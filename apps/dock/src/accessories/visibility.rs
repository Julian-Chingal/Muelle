//! Kernel signals pause USB queries while the menu bar has no visible consumer.
//! No pipe writes or waits on the UI thread; no polling to notice a reveal.
use super::worker::Event;
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::SyncSender,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        System::Threading::{
            CreateEventW, OpenEventW, RegisterWaitForSingleObject, ResetEvent,
            SYNCHRONIZATION_SYNCHRONIZE, SetEvent, UnregisterWaitEx, WT_EXECUTEDEFAULT,
            WaitForSingleObject,
        },
    },
    core::{HSTRING, Owned},
};

pub struct Controller {
    active: isize,
    changed: isize,
    audio: isize,
    pub name: String,
    visible: AtomicBool,
}
impl Controller {
    pub fn new(visible: bool) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "Local\\Muelle.Accessories.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        unsafe {
            let mut owner = Self {
                active: CreateEventW(None, true, visible, &HSTRING::from(&name))?.0 as isize,
                changed: 0,
                audio: 0,
                name,
                visible: AtomicBool::new(visible),
            };
            owner.changed = CreateEventW(
                None,
                false,
                false,
                &HSTRING::from(format!("{}.Changed", owner.name)),
            )?
            .0 as isize;
            owner.audio = CreateEventW(
                None,
                false,
                false,
                &HSTRING::from(format!("{}.Audio", owner.name)),
            )?
            .0 as isize;
            Ok(owner)
        }
    }
    pub fn set(&self, visible: bool) {
        if self.visible.swap(visible, Ordering::AcqRel) == visible {
            return;
        }
        // Own handles remain live through Shared's lifetime. Never wait here.
        unsafe {
            let active = HANDLE(self.active as *mut _);
            let _ = if visible {
                SetEvent(active)
            } else {
                ResetEvent(active)
            };
            let _ = SetEvent(HANDLE(self.changed as *mut _));
        }
    }
    pub fn audio_changed(&self) {
        // Auto-reset coalesces pending route changes; no UI pipe IO or wait.
        unsafe {
            let _ = SetEvent(HANDLE(self.audio as *mut _));
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        unsafe {
            for handle in [self.active, self.changed, self.audio] {
                if handle != 0 {
                    let _ = CloseHandle(HANDLE(handle as *mut _));
                }
            }
        }
    }
}

struct Signal {
    tx: SyncSender<Event>,
    full: Arc<AtomicBool>,
    audio: bool,
}
pub struct Watch {
    active: Owned<HANDLE>,
    _signals: [Registration; 2],
}
struct Registration {
    _event: Owned<HANDLE>,
    wait: HANDLE,
    _signal: Arc<Signal>,
}
impl Registration {
    fn new(name: &str, tx: SyncSender<Event>, full: Arc<AtomicBool>, audio: bool) -> Result<Self> {
        unsafe {
            let event = Owned::new(OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &HSTRING::from(name),
            )?);
            let signal = Arc::new(Signal { tx, full, audio });
            let mut wait = HANDLE::default();
            RegisterWaitForSingleObject(
                &mut wait,
                *event,
                Some(notify),
                Some(Arc::as_ptr(&signal).cast()),
                u32::MAX,
                WT_EXECUTEDEFAULT,
            )?;
            Ok(Self {
                _event: event,
                wait,
                _signal: signal,
            })
        }
    }
}
impl Watch {
    pub fn new(name: &str, tx: SyncSender<Event>, full: Arc<AtomicBool>) -> Result<Self> {
        ensure!(
            name.starts_with("Local\\Muelle.Accessories.")
                && name.len() <= 96
                && name
                    .trim_start_matches("Local\\Muelle.Accessories.")
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.'),
            "invalid visibility channel"
        );
        unsafe {
            let active = Owned::new(OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE,
                false,
                &HSTRING::from(name),
            )?);
            Ok(Self {
                active,
                _signals: [
                    Registration::new(&format!("{name}.Changed"), tx.clone(), full.clone(), false)?,
                    Registration::new(&format!("{name}.Audio"), tx, full, true)?,
                ],
            })
        }
    }
    pub fn active(&self) -> bool {
        unsafe { WaitForSingleObject(*self.active, 0) == WAIT_OBJECT_0 }
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE));
        }
    }
}
unsafe extern "system" fn notify(context: *mut std::ffi::c_void, _: bool) {
    // Context and both events outlive registration; Drop synchronizes callbacks.
    let signal = unsafe { &*context.cast::<Signal>() };
    let event = if signal.audio {
        Event::AudioChanged
    } else {
        Event::Visibility
    };
    if matches!(
        signal.tx.try_send(event),
        Err(std::sync::mpsc::TrySendError::Full(_))
    ) {
        signal.full.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visibility_signals_wake_once_without_a_polling_timer() -> Result<()> {
        let control = Controller::new(false)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let watch = Watch::new(&control.name, tx, Arc::new(AtomicBool::new(false)))?;
        assert!(!watch.active());
        control.set(true);
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1))?,
            Event::Visibility
        ));
        assert!(watch.active());
        control.set(true);
        assert!(rx.try_recv().is_err());
        control.set(false);
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1))?,
            Event::Visibility
        ));
        assert!(!watch.active());
        control.audio_changed();
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1))?,
            Event::AudioChanged
        ));
        assert!(!watch.active(), "audio routing must not change visibility");
        assert!(rx.try_recv().is_err());
        Ok(())
    }
}
