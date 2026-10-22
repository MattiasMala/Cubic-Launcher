//! The one launch this process may have at a time (concorrenza-4).
//!
//! Before, the state of a launch lived only in the frontend's signals: a
//! webview reload showed PLAY over a running game, a second Play launched on
//! the same instance, and the first game's exit cleared the PID of the second.
//! Here the backend owns it: a launch is "preparing" from the command to the
//! spawn, then "running" while its PID is alive. A second launch is refused in
//! both states, and the frontend reads the state at boot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Serialize;

pub(crate) struct LaunchSlot {
    preparing: AtomicBool,
    running_pid: Mutex<Option<u32>>,
}

/// What `launch_status_command` hands the frontend at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchStatus {
    pub preparing: bool,
    pub running_pid: Option<u32>,
}

/// Holds "preparing" until the pipeline is over, however it ends — an error,
/// a panic of the task, or the spawn that hands over to the PID.
pub(crate) struct PreparingGuard<'slot> {
    slot: &'slot LaunchSlot,
}

impl Drop for PreparingGuard<'_> {
    fn drop(&mut self) {
        self.slot.preparing.store(false, Ordering::Release);
    }
}

impl LaunchSlot {
    pub(crate) const fn new() -> Self {
        Self {
            preparing: AtomicBool::new(false),
            running_pid: Mutex::new(None),
        }
    }

    fn pid_guard(&self) -> std::sync::MutexGuard<'_, Option<u32>> {
        self.running_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes the slot for a new launch, or says why not. The PID lock is held
    /// across both checks, so a spawn cannot slip in between them.
    pub(crate) fn claim(&self) -> Result<PreparingGuard<'_>, String> {
        let running = self.pid_guard();
        if let Some(pid) = *running {
            return Err(format!(
                "Minecraft is already running (PID {pid}). Stop it or close the game before launching again."
            ));
        }
        if self
            .preparing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("A launch is already being prepared. Wait for it to start or fail before launching again.".to_string());
        }
        drop(running);
        Ok(PreparingGuard { slot: self })
    }

    pub(crate) fn set_running(&self, pid: u32) {
        *self.pid_guard() = Some(pid);
    }

    /// Clears the PID only if it is still `pid`: the exit of one game must
    /// never forget the PID of another.
    pub(crate) fn clear_running_if(&self, pid: u32) -> bool {
        let mut running = self.pid_guard();
        if *running == Some(pid) {
            *running = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn running_pid(&self) -> Option<u32> {
        *self.pid_guard()
    }

    pub(crate) fn status(&self) -> LaunchStatus {
        let running_pid = self.running_pid();
        LaunchStatus {
            preparing: self.preparing.load(Ordering::Acquire),
            running_pid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LaunchSlot, LaunchStatus};

    #[test]
    fn a_second_launch_is_refused_while_the_first_is_preparing() {
        let slot = LaunchSlot::new();
        let first = slot.claim().expect("the first launch takes the slot");
        assert!(slot.claim().is_err());
        assert_eq!(
            slot.status(),
            LaunchStatus {
                preparing: true,
                running_pid: None
            }
        );

        drop(first);
        assert!(slot.claim().is_ok(), "a failed preparation frees the slot");
    }

    #[test]
    fn a_second_launch_is_refused_while_the_game_runs() {
        let slot = LaunchSlot::new();
        let preparing = slot.claim().unwrap();
        slot.set_running(4242);
        drop(preparing);

        let refusal = slot.claim().err().expect("a running game keeps the slot");
        assert!(refusal.contains("4242"), "{refusal}");
        assert_eq!(
            slot.status(),
            LaunchStatus {
                preparing: false,
                running_pid: Some(4242)
            }
        );

        assert!(slot.clear_running_if(4242));
        assert!(slot.claim().is_ok());
    }

    #[test]
    fn the_exit_of_one_game_does_not_clear_the_pid_of_another() {
        let slot = LaunchSlot::new();
        slot.set_running(200);

        assert!(!slot.clear_running_if(100));
        assert_eq!(slot.running_pid(), Some(200));

        assert!(slot.clear_running_if(200));
        assert_eq!(slot.running_pid(), None);
    }
}
