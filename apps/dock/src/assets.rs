//! Shell icons and names are loaded away from the compositor's UI thread.
//! Only plain pixel buffers cross threads; composition brushes stay on the UI.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicIsize, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};

use anyhow::Result;
use muelle_shell::{
    icon_cache::{AppVisual, IconCache},
    power,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
    UI::WindowsAndMessaging::PostMessageW,
};

const WORKERS: usize = 2;
const QUEUE_CAPACITY: usize = 64;
const ICON_SIZE: u32 = 256;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Request {
    id: String,
    variant: &'static str,
    generation: u64,
}

struct Loaded {
    request: Request,
    visual: Option<AppVisual>,
}

pub struct Visuals {
    requests: SyncSender<Request>,
    results: Receiver<Loaded>,
    ready: HashMap<Request, Option<AppVisual>>,
    pending: HashSet<Request>,
    avatars: HashMap<String, Rc<muelle_shell::icons::IconImage>>,
    generations: HashMap<String, u64>,
    generation: u64,
    window: Arc<AtomicIsize>,
}

impl Visuals {
    pub fn new(window: HWND, message: u32) -> Result<Self> {
        Self::start(window, message, |id, variant| {
            IconCache::default()
                .get_variant(id, variant, ICON_SIZE)
                .inspect_err(|error| eprintln!("muelle: {error}"))
                .ok()
        })
    }

    fn start(
        window: HWND,
        message: u32,
        load: impl Fn(&str, &str) -> Option<AppVisual> + Send + Sync + 'static,
    ) -> Result<Self> {
        let (requests, incoming) = sync_channel::<Request>(QUEUE_CAPACITY);
        let (outgoing, results) = sync_channel(QUEUE_CAPACITY);
        let incoming = Arc::new(Mutex::new(incoming));
        let load = Arc::new(load);
        let window = Arc::new(AtomicIsize::new(window.0 as isize));
        for index in 0..WORKERS {
            let incoming = Arc::clone(&incoming);
            let outgoing = outgoing.clone();
            let load = Arc::clone(&load);
            let window = Arc::clone(&window);
            thread::Builder::new()
                .name(format!("muelle-icons-{index}"))
                .spawn(move || {
                    power::set_thread_efficiency(true);
                    // SAFETY: Each worker owns its COM apartment and balances
                    // initialization after all shell interfaces are released.
                    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
                    loop {
                        let request = {
                            let Ok(receiver) = incoming.lock() else { break };
                            receiver.recv()
                        };
                        let Ok(request) = request else { break };
                        // Never hold the queue mutex while calling shell extensions.
                        let visual = com
                            .is_ok()
                            .then(|| load(&request.id, request.variant))
                            .flatten();
                        if outgoing.send(Loaded { request, visual }).is_err() {
                            break;
                        }
                        let hwnd = window.load(Ordering::Relaxed);
                        if hwnd != 0 {
                            // SAFETY: Notification carries no pointer. Drop clears
                            // the target before the application's windows disappear.
                            unsafe {
                                let _ = PostMessageW(
                                    Some(HWND(hwnd as *mut _)),
                                    message,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            }
                        }
                    }
                    if com.is_ok() {
                        // SAFETY: Balances successful initialization on this thread.
                        unsafe { CoUninitialize() };
                    }
                })?;
        }
        Ok(Self {
            requests,
            results,
            ready: HashMap::new(),
            pending: HashSet::new(),
            avatars: HashMap::new(),
            generations: HashMap::new(),
            generation: 0,
            window,
        })
    }

    /// Never waits for disk, COM or a worker, even when the queue is full.
    pub fn get(&mut self, id: &str, variant: &'static str) -> Option<&AppVisual> {
        let request = Request {
            id: id.to_owned(),
            variant,
            generation: self.generations.get(id).copied().unwrap_or(0),
        };
        if !self.ready.contains_key(&request)
            && !self.pending.contains(&request)
            && self.requests.try_send(request.clone()).is_ok()
        {
            self.pending.insert(request.clone());
        }
        self.cached(&request)
    }

    /// Like `get`, but only reads what is loaded already: several loaded
    /// visuals can then be borrowed at once (a group's tile).
    pub fn peek(&self, id: &str, variant: &'static str) -> Option<&AppVisual> {
        self.cached(&Request {
            id: id.to_owned(),
            variant,
            generation: self.generations.get(id).copied().unwrap_or(0),
        })
    }

    fn cached(&self, request: &Request) -> Option<&AppVisual> {
        self.ready
            .get(request)
            .or_else(|| {
                self.ready.iter().find_map(|(old, visual)| {
                    (old.id == request.id && old.variant == request.variant).then_some(visual)
                })
            })
            .and_then(Option::as_ref)
    }

    pub fn collect(&mut self) -> Vec<String> {
        let mut changed = Vec::new();
        for mut loaded in self.results.try_iter() {
            self.pending.remove(&loaded.request);
            if loaded.request.generation
                != self
                    .generations
                    .get(&loaded.request.id)
                    .copied()
                    .unwrap_or(0)
            {
                continue;
            }
            let avatar = loaded
                .visual
                .as_mut()
                .and_then(|visual| visual.avatar.take());
            let avatar_changed =
                self.avatars.get(&loaded.request.id).map(Rc::as_ref) != avatar.as_ref();
            if avatar_changed && let Some(avatar) = avatar {
                self.avatars
                    .insert(loaded.request.id.clone(), Rc::new(avatar));
            } else if avatar_changed {
                self.avatars.remove(&loaded.request.id);
            }
            let same = self
                .ready
                .iter()
                .find(|(old, _)| {
                    old.id == loaded.request.id && old.variant == loaded.request.variant
                })
                .is_some_and(|(_, visual)| visual == &loaded.visual);
            if !same || avatar_changed {
                changed.push(loaded.request.id.clone());
            }
            self.ready.retain(|old, _| {
                old.id != loaded.request.id || old.variant != loaded.request.variant
            });
            self.ready.insert(loaded.request, loaded.visual);
        }
        changed
    }

    pub fn retain(&mut self, mut needed: impl FnMut(&str) -> bool) {
        self.ready.retain(|request, _| needed(&request.id));
        self.avatars.retain(|id, _| needed(id));
        self.generations
            .retain(|id, _| needed(id) || self.pending.iter().any(|request| request.id == *id));
    }

    /// Keep the previous pixels visible until their replacement arrives. A
    /// generation prevents an older in-flight load overwriting a changed avatar.
    pub fn invalidate(&mut self, id: &str) {
        self.generation = self.generation.wrapping_add(1);
        self.generations.insert(id.to_owned(), self.generation);
        // Retain the previous pixels until replacement. An unchanged native
        // profile check must not rebuild the Dock or upload the same avatar.
    }

    pub fn avatar(&self, id: &str) -> Option<Rc<muelle_shell::icons::IconImage>> {
        self.avatars.get(id).cloned()
    }
}

impl Drop for Visuals {
    fn drop(&mut self) {
        self.window.store(0, Ordering::Relaxed);
        // Do not join shell workers: an unresponsive extension must not hold
        // the UI during shutdown. Disconnecting the channels ends idle workers.
    }
}

/// A temporary label that does no file access or shell lookup.
pub fn fallback_name(id: &str) -> String {
    if let Some(profile) = muelle_shell::browser_profiles::Profile::parse(id) {
        return profile.fallback_name();
    }
    let file = id.rsplit(['\\', '/']).next().unwrap_or(id);
    file.strip_suffix(".exe").unwrap_or(file).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Condvar, mpsc::channel},
        time::{Duration, Instant},
    };

    #[test]
    fn an_unchanged_profile_refresh_reuses_its_avatar_and_needs_no_recomposition() -> Result<()> {
        let mut visuals = Visuals::start(HWND::default(), 0, |_, _| {
            let image = muelle_shell::icons::IconImage {
                width: 1,
                height: 1,
                bgra: vec![0, 0, 0, 255],
            };
            Some(AppVisual {
                name: "Work".into(),
                icon: image.clone(),
                unbadged: Some(image.clone()),
                avatar: Some(image),
            })
        })?;
        visuals.get("profile", "");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !visuals.pending.is_empty() && Instant::now() < deadline {
            visuals.collect();
            thread::sleep(Duration::from_millis(1));
        }
        let avatar = visuals
            .avatar("profile")
            .ok_or_else(|| anyhow::anyhow!("initial avatar missing"))?;
        visuals.invalidate("profile");
        assert!(
            visuals.get("profile", "").is_some(),
            "keep the previous icon during refresh"
        );
        let mut changed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !visuals.pending.is_empty() && Instant::now() < deadline {
            changed.extend(visuals.collect());
            thread::sleep(Duration::from_millis(1));
        }
        assert!(visuals.pending.is_empty());
        assert!(changed.is_empty());
        assert!(
            visuals
                .avatar("profile")
                .is_some_and(|new| Rc::ptr_eq(&avatar, &new))
        );
        Ok(())
    }

    #[test]
    fn a_stale_avatar_load_cannot_replace_a_new_generation() -> Result<()> {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let (started, waiting) = channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::clone(&gate);
        let counter = Arc::clone(&calls);
        let mut visuals = Visuals::start(HWND::default(), 0, move |_, _| {
            let version = counter.fetch_add(1, Ordering::Relaxed);
            if version == 0 {
                let _ = started.send(());
                let open = release
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(
                    release
                        .1
                        .wait_while(open, |open| !*open)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            }
            Some(AppVisual {
                name: version.to_string(),
                icon: muelle_shell::icons::IconImage {
                    width: 1,
                    height: 1,
                    bgra: vec![0, 0, 0, 255],
                },
                unbadged: None,
                avatar: Some(muelle_shell::icons::IconImage {
                    width: 1,
                    height: 1,
                    bgra: vec![version as u8, 0, 0, 255],
                }),
            })
        })?;
        visuals.get("profile", "");
        waiting.recv_timeout(Duration::from_secs(2))?;
        visuals.invalidate("profile");
        visuals.get("profile", "");
        let deadline = Instant::now() + Duration::from_secs(2);
        while visuals.peek("profile", "").is_none() && Instant::now() < deadline {
            visuals.collect();
            thread::sleep(Duration::from_millis(1));
        }
        *gate
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        gate.1.notify_all();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !visuals.pending.is_empty() && Instant::now() < deadline {
            visuals.collect();
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            visuals
                .peek("profile", "")
                .map(|visual| visual.name.as_str()),
            Some("1")
        );
        assert_eq!(
            visuals.avatar("profile").map(|image| image.bgra.clone()),
            Some(vec![1, 0, 0, 255])
        );
        Ok(())
    }

    #[test]
    fn blocked_shell_workers_do_not_block_requests_or_duplicate_work() -> Result<()> {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::clone(&gate);
        let (started, waiting) = channel();
        let mut visuals = Visuals::start(HWND::default(), 0, move |id, _| {
            let _ = started.send(id.to_owned());
            let (lock, wake) = &*gate;
            let open = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(
                wake.wait_while(open, |open| !*open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            None
        })?;
        visuals.get("first", "");
        visuals.get("second", "");
        // Both workers are inside the simulated shell extension. Requests
        // below must complete even though neither worker can make progress.
        for _ in 0..WORKERS {
            waiting.recv_timeout(Duration::from_secs(2))?;
        }
        for _ in 0..1_000 {
            assert!(visuals.get("first", "").is_none());
        }
        assert_eq!(visuals.pending.len(), 2);
        for index in 0..QUEUE_CAPACITY * 2 {
            visuals.get(&format!("queued-{index}"), "");
        }
        assert_eq!(visuals.pending.len(), QUEUE_CAPACITY + WORKERS);
        assert!(visuals.collect().is_empty());
        // Shutdown must also finish while the shell is still blocked.
        drop(visuals);
        *release
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        release.1.notify_all();
        Ok(())
    }

    #[test]
    fn completed_variants_are_cached_without_reloading_or_mixing_them() -> Result<()> {
        let mut visuals = Visuals::start(HWND::default(), 0, |_, variant| {
            Some(AppVisual {
                name: variant.to_owned(),
                icon: muelle_shell::icons::IconImage {
                    width: 1,
                    height: 1,
                    bgra: vec![0, 0, 0, 255],
                },
                unbadged: None,
                avatar: None,
            })
        })?;
        visuals.get("bin", "empty");
        visuals.get("bin", "full");
        let deadline = Instant::now() + Duration::from_secs(2);
        while visuals.ready.len() < 2 && Instant::now() < deadline {
            visuals.collect();
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            visuals
                .get("bin", "empty")
                .map(|visual| visual.name.as_str()),
            Some("empty")
        );
        assert_eq!(
            visuals
                .get("bin", "full")
                .map(|visual| visual.name.as_str()),
            Some("full")
        );
        assert!(visuals.pending.is_empty());
        visuals.retain(|_| false);
        assert!(visuals.ready.is_empty());
        Ok(())
    }

    #[test]
    fn fallback_name_needs_no_shell_or_file_access() {
        assert_eq!(fallback_name(r"Z:\offline\example.exe"), "example");
        assert_eq!(
            fallback_name("shell:AppsFolder\\Package!App"),
            "Package!App"
        );
    }

    #[test]
    #[ignore = "Measures real Windows shell work; run explicitly with --nocapture"]
    fn measure_shell_load_and_nonblocking_requests() -> Result<()> {
        let id = format!(r"{}\explorer.exe", std::env::var("WINDIR")?);
        let shell_start = Instant::now();
        let mut visuals = Visuals::new(HWND::default(), 0)?;
        let enqueue = Instant::now();
        for _ in 0..1_000 {
            visuals.get(&id, "");
        }
        let enqueue_elapsed = enqueue.elapsed();
        let loaded = visuals.results.recv_timeout(Duration::from_secs(10))?;
        assert!(loaded.visual.is_some());
        eprintln!(
            "1000 UI requests: {enqueue_elapsed:?}; worker result: {:?}",
            shell_start.elapsed()
        );
        Ok(())
    }
}
