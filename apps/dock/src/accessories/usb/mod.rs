mod headsets;
mod io;
pub(super) mod protocol;

use super::{WorkerJob, worker::Event};
use anyhow::{Context, Result, ensure};
use muelle_shell::accessories::{Device, Kind};
use protocol::{Model, Reading};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Devices::HumanInterfaceDevice::HidDevice,
    Win32::{
        Foundation::{HANDLE, WAIT_OBJECT_0},
        System::{JobObjects::AssignProcessToJobObject, Threading::WaitForSingleObject},
    },
};

pub const FLAG: &str = "--accessory-usb-query";
const INTERVAL: Duration = Duration::from_secs(60);
const LIMIT: usize = 8;
/// A device behind a receiver stops answering while it sleeps: a G305 does
/// after a few seconds without movement, which is most of the time someone
/// types. Its last reading stays this long, as the reference monitor does
/// (`graceSec = 1800` for receiver devices), so the indicator does not come
/// and go with every query. Unplugging the receiver still clears it at once.
const RECEIVER_GRACE: Duration = Duration::from_secs(30 * 60);
// A transport error is not proof that a directly attached headset disconnected.
// A successful protocol-level disconnected result still clears it immediately.
const TRANSPORT_GRACE: Duration = Duration::from_secs(180);
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    paths: Vec<String>,
}

pub fn query_main() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().take(4097).read_to_string(&mut input)?;
    ensure!(input.len() <= 4096, "USB request too large");
    let request: Request = serde_json::from_str(&input)?;
    let readings =
        io::query(&request.paths).map_err(|error| super::diagnostic(&format!("{error:#}")));
    serde_json::to_writer(std::io::stdout().lock(), &readings)?;
    Ok(())
}

pub fn selector() -> Result<String> {
    let mut selectors = Vec::new();
    for profile in protocol::PROFILES {
        if profile.page == 0 {
            // Only the two exact HyperX revisions whose usage is not fixed in
            // their protocol notes. No global HID enumeration or vendor-wide IO.
            selectors.push(format!("System.Devices.InterfaceClassGuid:=\"{{4d1e55b2-f16f-11cf-88cb-001111000030}}\" AND System.DeviceInterface.Hid.VendorId:={} AND System.DeviceInterface.Hid.ProductId:={} AND System.DeviceInterface.Hid.UsagePage:>=65280", profile.vendor, profile.product));
            continue;
        }
        selectors.push(
            HidDevice::GetDeviceSelectorVidPid(
                profile.page,
                profile.usage,
                profile.vendor,
                profile.product,
            )?
            .to_string(),
        );
        if profile.model == Model::Logitech {
            selectors.push(
                HidDevice::GetDeviceSelectorVidPid(
                    profile.page,
                    1,
                    profile.vendor,
                    profile.product,
                )?
                .to_string(),
            );
        }
    }
    Ok(selectors
        .iter()
        .map(|query| format!("({query})"))
        .collect::<Vec<_>>()
        .join(" OR "))
}

fn probe(paths: &[String]) -> Result<Vec<Reading>> {
    let job = WorkerJob::new()?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg(FLAG)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // The non-inherited job also cleans up if the watcher process crashes.
    unsafe {
        if let Err(error) = AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) {
            let _ = child.kill();
            return Err(error.into());
        }
    }
    query_child(&mut child, paths, 4000)
}

fn query_child(child: &mut Child, paths: &[String], timeout: u32) -> Result<Vec<Reading>> {
    if let Some(mut input) = child.stdin.take() {
        serde_json::to_writer(
            &mut input,
            &Request {
                paths: paths.to_vec(),
            },
        )?;
        input.flush()?;
    }
    ensure!(
        unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), timeout) } == WAIT_OBJECT_0,
        "USB provider exceeded process deadline"
    );
    ensure!(child.wait()?.success(), "USB provider failed");
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .context("USB output unavailable")?
        .take(4097)
        .read_to_end(&mut output)?;
    decode_response(&output, paths)
}

fn decode_response(output: &[u8], paths: &[String]) -> Result<Vec<Reading>> {
    ensure!(output.len() <= 4096, "USB output too large");
    let readings: std::result::Result<Vec<Reading>, String> = serde_json::from_slice(output)?;
    let readings =
        readings.map_err(|error| anyhow::anyhow!("USB query: {}", super::diagnostic(&error)))?;
    let profile = paths
        .first()
        .and_then(|path| protocol::profile_path(path))
        .context("unknown USB response target")?;
    let mut slots = std::collections::BTreeSet::new();
    ensure!(
        readings.len() <= 6
            && readings.iter().all(|r| slots.insert(r.slot)
                && r.percent.is_none_or(|n| n <= 100)
                && if profile.model == Model::Logitech {
                    (1..=profile.slots).contains(&r.slot)
                } else {
                    r.slot == 0 && r.kind == Kind::Headset
                }),
        "invalid USB response"
    );
    Ok(readings)
}

pub(super) struct Completed {
    pub path: String,
    pub generation: u64,
    pub result: Result<Vec<Reading>>,
}
struct Job {
    path: String,
    paths: Vec<String>,
    generation: u64,
}
struct Entry {
    id: String,
    paths: BTreeSet<String>,
    generation: u64,
    next: Instant,
    failures: u8,
    devices: Vec<Held>,
}
/// One device's last reading and when its receiver last reported it.
struct Held {
    slot: u8,
    seen: Instant,
    device: Device,
}
#[derive(Default)]
pub struct Sources {
    entries: BTreeMap<String, Entry>,
    generation: u64,
    in_flight: Option<(String, u64)>,
    pool: Option<SyncSender<Job>>,
}
impl Sources {
    pub fn add(&mut self, path: String, id: String) {
        let Some(key) = protocol::receiver_key(&path) else {
            return;
        };
        let path = path.to_ascii_lowercase();
        if let Some(entry) = self.entries.get_mut(&key) {
            if entry.paths.contains(&path) || entry.paths.len() >= 2 {
                return;
            }
            self.generation = self.generation.wrapping_add(1);
            entry.paths.insert(path);
            entry.generation = self.generation;
            if entry.devices.is_empty() {
                entry.next = Instant::now();
            }
            return;
        }
        if self.entries.len() >= LIMIT {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.entries.insert(
            key,
            Entry {
                id,
                paths: BTreeSet::from([path]),
                generation: self.generation,
                next: Instant::now(),
                failures: 0,
                devices: Vec::new(),
            },
        );
    }
    pub fn remove(&mut self, path: &str) {
        if let Some(key) = protocol::receiver_key(path)
            && let Some(entry) = self.entries.get_mut(&key)
            && entry.paths.remove(&path.to_ascii_lowercase())
        {
            if entry.paths.is_empty() {
                self.entries.remove(&key);
            } else {
                self.generation = self.generation.wrapping_add(1);
                entry.generation = self.generation;
                if entry.devices.is_empty() {
                    entry.next = Instant::now();
                }
            }
        }
        if self.entries.is_empty() && self.in_flight.is_none() {
            self.pool = None;
        }
    }
    pub fn schedule(&mut self, tx: &SyncSender<Event>, full: &Arc<AtomicBool>) -> Result<()> {
        // Start no thread or USB query process unless an allowlisted interface exists.
        if self.entries.is_empty() {
            if self.in_flight.is_none() {
                self.pool = None;
            }
            return Ok(());
        }
        if self.pool.is_none() {
            let (jobs, receive) = mpsc::sync_channel::<Job>(1);
            let (tx, full) = (tx.clone(), full.clone());
            thread::Builder::new()
                .name("muelle-usb-query".into())
                .spawn(move || {
                    while let Ok(job) = receive.recv() {
                        let result = probe(&job.paths);
                        if tx
                            .try_send(Event::Usb(Completed {
                                path: job.path,
                                generation: job.generation,
                                result,
                            }))
                            .is_err()
                        {
                            full.store(true, Ordering::Release);
                            break;
                        }
                    }
                })?;
            self.pool = Some(jobs);
        }
        // Exactly one in-flight operation. Neither hotplug nor metadata noise
        // can create an unbounded work queue or override its rate limit.
        if self.in_flight.is_some() {
            return Ok(());
        }
        if let Some((path, entry)) = self
            .entries
            .iter_mut()
            .filter(|(_, entry)| entry.next <= Instant::now())
            .min_by_key(|(_, entry)| entry.next)
            && let Some(pool) = &self.pool
        {
            pool.try_send(Job {
                path: path.clone(),
                paths: entry.paths.iter().cloned().collect(),
                generation: entry.generation,
            })?;
            self.in_flight = Some((path.clone(), entry.generation));
        }
        Ok(())
    }
    pub fn deadline(&self) -> Option<Instant> {
        if self.in_flight.is_some() {
            None
        } else {
            self.entries.values().map(|entry| entry.next).min()
        }
    }
    pub fn expiry_deadline(&self) -> Option<Instant> {
        self.entries
            .values()
            .flat_map(|entry| &entry.devices)
            .map(|held| {
                held.seen
                    + if held.slot == 0 {
                        TRANSPORT_GRACE
                    } else {
                        RECEIVER_GRACE
                    }
            })
            .min()
    }
    pub fn expire(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for entry in self.entries.values_mut() {
            let before = entry.devices.len();
            entry.devices.retain(|held| {
                now.saturating_duration_since(held.seen)
                    < if held.slot == 0 {
                        TRANSPORT_GRACE
                    } else {
                        RECEIVER_GRACE
                    }
            });
            changed |= before != entry.devices.len();
        }
        changed
    }
    pub fn clear(&mut self) {
        // Keep the sequence and in-flight operation: an old result must neither
        // alias a rediscovered receiver nor let two probes run simultaneously.
        self.entries.clear();
        if self.in_flight.is_none() {
            self.pool = None;
        }
    }
    pub fn complete(&mut self, response: Completed) -> Option<String> {
        if self.in_flight.as_ref().is_some_and(|(path, generation)| {
            *path == response.path && *generation == response.generation
        }) {
            self.in_flight = None;
        }
        if self.entries.is_empty() && self.in_flight.is_none() {
            self.pool = None;
        }
        let entry = self
            .entries
            .get_mut(&response.path)
            .filter(|entry| entry.generation == response.generation)?;
        let now = Instant::now();
        let previous = std::mem::take(&mut entry.devices);
        let failed = response.result.is_err();
        let mut diagnostic = None;
        match response.result {
            Ok(readings) => {
                if entry.failures != 0 {
                    diagnostic = Some("USB accessory query recovered".into());
                }
                entry.failures = 0;
                entry.devices = readings
                    .into_iter()
                    .map(|reading| Held {
                        slot: reading.slot,
                        seen: now,
                        device: Device {
                            id: if reading.slot == 0 {
                                entry.id.clone()
                            } else {
                                format!("{}/{}", entry.id, reading.slot)
                            },
                            name: if reading.kind == Kind::Mouse {
                                "Logitech HID++".into()
                            } else {
                                "Wireless headset".into()
                            },
                            kind: reading.kind,
                            percent: reading.percent,
                            conflicting: false,
                            active_audio: false,
                        },
                    })
                    .collect();
            }
            Err(error) => {
                entry.failures = entry.failures.saturating_add(1);
                // At most 1, 2, 4, ... 128 consecutive failures are logged.
                if entry.failures.is_power_of_two() {
                    let model = protocol::profile_path(&response.path)
                        .map(|profile| format!(" {:04X}:{:04X}", profile.vendor, profile.product))
                        .unwrap_or_default();
                    diagnostic = Some(format!(
                        "USB accessory{model} query failed (attempt {}): {error:#}",
                        entry.failures
                    ));
                }
            }
        }
        // Receiver slots may sleep. Direct headsets retain a reading only after
        // transport failure, never after an explicit disconnected response.
        for held in previous {
            let grace = if held.slot != 0 {
                RECEIVER_GRACE
            } else {
                TRANSPORT_GRACE
            };
            if (held.slot != 0 || failed)
                && now.saturating_duration_since(held.seen) < grace
                && !entry.devices.iter().any(|fresh| fresh.slot == held.slot)
            {
                entry.devices.push(held);
            }
        }
        entry.next = now + retry_delay(entry.failures);
        diagnostic
    }
    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.entries
            .values()
            .flat_map(|entry| entry.devices.iter().map(|held| &held.device))
    }
}
fn retry_delay(failures: u8) -> Duration {
    if failures == 0 {
        INTERVAL
    } else {
        Duration::from_secs((60u64 << failures.min(4)).min(900))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_ipc_preserves_failure_reasons_and_validates_successful_readings() -> Result<()> {
        let paths = [r"\\?\hid#vid_046d&pid_c53f&mi_02#instance".into()];
        let encode =
            |result: std::result::Result<Vec<Reading>, String>| serde_json::to_vec(&result);
        let failure = encode(Err("HID++ ping: read timed out".into()))?;
        let error = decode_response(&failure, &paths)
            .err()
            .context("failure decoded as success")?;
        assert!(error.to_string().contains("HID++ ping: read timed out"));
        assert!(decode_response(&encode(Ok(Vec::new()))?, &paths)?.is_empty());
        let reading = Reading {
            slot: 1,
            kind: Kind::Mouse,
            percent: Some(90),
        };
        assert_eq!(
            decode_response(&encode(Ok(vec![reading.clone()]))?, &paths)?,
            vec![reading.clone()]
        );
        assert!(
            decode_response(&encode(Ok(vec![reading.clone(), reading.clone()]))?, &paths).is_err()
        );
        assert!(
            decode_response(
                &encode(Ok(vec![Reading {
                    percent: Some(101),
                    ..reading
                }]))?,
                &paths
            )
            .is_err()
        );
        assert!(decode_response(br#"[{"slot":1,"kind":"mouse","percent":90}]"#, &paths).is_err());
        Ok(())
    }

    #[test]
    fn headset_errors_expire_without_extra_queries_but_disconnect_clears_immediately() {
        let path = r"\\?\hid#vid_1038&pid_22a1&mi_03#instance";
        let mut sources = Sources::default();
        sources.add(path.into(), "headset".into());
        let generation = sources.entries[path].generation;
        let done = |result| Completed {
            path: path.into(),
            generation,
            result,
        };
        let reading = Reading {
            slot: 0,
            kind: Kind::Headset,
            percent: Some(65),
        };
        assert!(sources.complete(done(Ok(vec![reading.clone()]))).is_none());
        let first = sources.complete(done(Err(anyhow::anyhow!("read timed out"))));
        assert!(first.is_some_and(|message| message.contains("read timed out")));
        assert_eq!(sources.devices().next().and_then(|d| d.percent), Some(65));
        if let Some(entry) = sources.entries.get_mut(path) {
            entry.devices[0].seen -= TRANSPORT_GRACE;
        }
        let next = sources.deadline();
        assert!(sources.expire());
        assert_eq!(sources.devices().count(), 0);
        assert_eq!(
            sources.deadline(),
            next,
            "expiry must not request a new query"
        );
        assert!(sources.complete(done(Ok(vec![reading]))).is_some());
        assert!(sources.complete(done(Ok(Vec::new()))).is_none());
        assert_eq!(sources.devices().count(), 0);
    }

    #[test]
    fn watcher_recovery_rejects_old_results_and_keeps_one_probe_in_flight() {
        let path = r"\\?\hid#vid_046d&pid_c53f&mi_02#instance";
        let mut sources = Sources::default();
        sources.add(path.into(), "g305".into());
        let generation = sources.entries[path].generation;
        sources.in_flight = Some((path.into(), generation));
        sources.clear();
        sources.add(path.into(), "g305".into());
        assert!(sources.in_flight.is_some());
        assert!(sources.deadline().is_none());
        let _ = sources.complete(Completed {
            path: path.into(),
            generation,
            result: Ok(vec![Reading {
                slot: 1,
                kind: Kind::Mouse,
                percent: Some(90),
            }]),
        });
        assert!(sources.in_flight.is_none());
        assert!(sources.deadline().is_some());
        assert_eq!(sources.devices().count(), 0);
    }

    #[test]
    fn changing_receiver_collection_keeps_the_reading_and_query_deadline() {
        let short = r"\\?\hid#vid_046d&pid_c53f&mi_02&col01#7&12345678&0&0000#{hid-guid}";
        let long = r"\\?\hid#vid_046d&pid_c53f&mi_02&col02#7&12345678&0&0001#{hid-guid}";
        let mut sources = Sources::default();
        sources.add(short.into(), "g305".into());
        let key = protocol::receiver_key(short).unwrap_or_default();
        let generation = sources.entries[&key].generation;
        let _ = sources.complete(Completed {
            path: key,
            generation,
            result: Ok(vec![Reading {
                slot: 1,
                kind: Kind::Mouse,
                percent: Some(90),
            }]),
        });
        let next = sources.deadline();
        sources.add(long.into(), "g305".into());
        assert_eq!(sources.deadline(), next);
        assert_eq!(
            sources.devices().next().and_then(|device| device.percent),
            Some(90)
        );
        sources.remove(short);
        assert_eq!(sources.deadline(), next);
        assert_eq!(sources.devices().count(), 1);
        sources.remove(long);
        assert_eq!(sources.devices().count(), 0);
    }

    #[test]
    fn diagnostics_are_throttled_and_recovery_is_logged_once() {
        let path = r"\\?\hid#vid_046d&pid_c53f&mi_02#instance";
        let mut sources = Sources::default();
        sources.add(path.into(), "g305".into());
        let generation = sources.entries[path].generation;
        let done = |result| Completed {
            path: path.into(),
            generation,
            result,
        };
        for attempt in 1u8..=12 {
            assert_eq!(
                sources
                    .complete(done(Err(anyhow::anyhow!("timeout"))))
                    .is_some(),
                attempt.is_power_of_two()
            );
        }
        assert!(
            sources
                .complete(done(Ok(Vec::new())))
                .is_some_and(|text| text.contains("recovered"))
        );
        assert!(sources.complete(done(Ok(Vec::new()))).is_none());
    }

    #[test]
    fn receiver_collections_are_polled_once_and_hotplug_invalidates_partial_queries() {
        let short = r"\\?\hid#vid_046d&pid_c53f&mi_02&col01#7&12345678&0&0000#{hid-guid}";
        let long = r"\\?\hid#vid_046d&pid_c53f&mi_02&col02#7&12345678&0&0001#{hid-guid}";
        let mut sources = Sources::default();
        sources.add(short.into(), "g305-a".into());
        let key = protocol::receiver_key(short).unwrap_or_default();
        let old_generation = sources.entries[&key].generation;
        sources.add(long.into(), "g305-a".into());
        assert_eq!(sources.entries.len(), 1);
        assert_eq!(sources.entries[&key].paths.len(), 2);
        sources.complete(Completed {
            path: key.clone(),
            generation: old_generation,
            result: Ok(vec![Reading {
                slot: 1,
                kind: Kind::Mouse,
                percent: Some(90),
            }]),
        });
        assert_eq!(sources.devices().count(), 0);
        let generation = sources.entries[&key].generation;
        sources.complete(Completed {
            path: key.clone(),
            generation,
            result: Ok(vec![Reading {
                slot: 1,
                kind: Kind::Mouse,
                percent: Some(90),
            }]),
        });
        assert_eq!(sources.devices().count(), 1);
        let deadline = sources.deadline();
        sources.add(short.to_ascii_uppercase(), "g305-a".into());
        assert_eq!(sources.deadline(), deadline);
        sources.add(long.replace("12345678", "87654321"), "g305-b".into());
        assert_eq!(sources.entries.len(), 2);
        sources.remove(short);
        assert_eq!(sources.entries[&key].devices.len(), 1);
        assert_eq!(sources.entries[&key].paths.len(), 1);
        sources.remove(&long.to_ascii_uppercase());
        assert!(!sources.entries.contains_key(&key));
        assert_eq!(sources.entries.len(), 1);
    }
    #[test]
    fn hotplug_noise_does_not_override_rate_limits_and_removal_clears_readings() {
        let path = r"\\?\hid#vid_1038&pid_22a1&mi_03#instance";
        let mut sources = Sources::default();
        assert!(sources.deadline().is_none() && sources.pool.is_none());
        sources.add(path.into(), "headset".into());
        let generation = sources.entries[path].generation;
        sources.complete(Completed {
            path: path.into(),
            generation,
            result: Ok(vec![Reading {
                slot: 0,
                kind: Kind::Headset,
                percent: Some(0),
            }]),
        });
        let next = sources.deadline();
        sources.add(path.into(), "headset".into());
        assert_eq!(sources.deadline(), next);
        assert_eq!(
            sources.devices().next().and_then(|device| device.percent),
            Some(0)
        );
        sources.complete(Completed {
            path: path.into(),
            generation,
            result: Err(anyhow::anyhow!("timeout")),
        });
        assert_eq!(sources.devices().count(), 1);
        sources.remove(path);
        assert_eq!(sources.devices().count(), 0);
        assert!(sources.deadline().is_none());
    }

    #[test]
    fn a_sleeping_receiver_device_keeps_its_last_reading_for_a_while() {
        let path = r"\\?\hid#vid_046d&pid_c53f&mi_02&col02#7&12345678&0&0001#{hid-guid}";
        let mut sources = Sources::default();
        sources.add(path.into(), "g305".into());
        let key = protocol::receiver_key(path).unwrap_or_default();
        let generation = sources.entries[&key].generation;
        let mouse = Reading {
            slot: 1,
            kind: Kind::Mouse,
            percent: Some(90),
        };
        let done = |result| Completed {
            path: key.clone(),
            generation,
            result,
        };
        sources.complete(done(Ok(vec![mouse.clone()])));
        // Asleep: the receiver answers, the mouse does not.
        sources.complete(done(Ok(Vec::new())));
        assert_eq!(sources.devices().next().and_then(|d| d.percent), Some(90));
        // A failed query does not drop it either.
        sources.complete(done(Err(anyhow::anyhow!("timeout"))));
        assert_eq!(sources.devices().count(), 1);
        // A fresh reading replaces it, never duplicates it.
        sources.complete(done(Ok(vec![Reading {
            percent: Some(85),
            ..mouse
        }])));
        assert_eq!(sources.devices().count(), 1);
        assert_eq!(sources.devices().next().and_then(|d| d.percent), Some(85));
        // Past the grace period it is gone.
        if let Some(entry) = sources.entries.get_mut(&key) {
            for held in &mut entry.devices {
                held.seen = Instant::now()
                    .checked_sub(RECEIVER_GRACE + Duration::from_secs(1))
                    .unwrap_or(held.seen);
            }
        }
        sources.complete(done(Ok(Vec::new())));
        assert_eq!(sources.devices().count(), 0);
    }

    #[test]
    #[ignore = "Native fixture, run only as the child of the deadline test"]
    fn stalled_usb_fixture() {
        thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore = "Starts and terminates its own stalled test child; no hardware access"]
    fn usb_process_deadline_terminates_a_stalled_provider() -> Result<()> {
        let job = WorkerJob::new()?;
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "accessories::usb::tests::stalled_usb_fixture",
                "--exact",
                "--ignored",
            ])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let handle = HANDLE(child.as_raw_handle());
        unsafe {
            AssignProcessToJobObject(job.0, handle)?;
        }
        let start = Instant::now();
        assert!(query_child(&mut child, &["fixture".into()], 100).is_err());
        drop(job);
        assert_eq!(unsafe { WaitForSingleObject(handle, 1000) }, WAIT_OBJECT_0);
        assert!(start.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[test]
    fn unplug_replug_rejects_late_results_and_backoff_is_bounded() {
        let path = r"\\?\hid#vid_046d&pid_c52b&mi_02#instance";
        let mut sources = Sources::default();
        sources.add(path.into(), "receiver-a".into());
        let generation = sources.entries[path].generation;
        sources.remove(path);
        sources.add(path.into(), "receiver-a".into());
        sources.complete(Completed {
            path: path.into(),
            generation,
            result: Ok(vec![Reading {
                slot: 1,
                kind: Kind::Mouse,
                percent: Some(50),
            }]),
        });
        assert_eq!(sources.devices().count(), 0);
        assert_eq!(retry_delay(0), Duration::from_secs(60));
        assert!(retry_delay(1) >= Duration::from_secs(60));
        assert_eq!(retry_delay(255), Duration::from_secs(900));
    }
}
