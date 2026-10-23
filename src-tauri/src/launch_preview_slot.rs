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

use crate::process_streaming::ProcessExitSignal;

pub(crate) struct LaunchSlot {
    preparing: AtomicBool,
    running: Mutex<Option<RunningGame>>,
    /// An update is being downloaded and installed, and the launcher will
    /// restart: no launch may start meanwhile (review-frontend-shell-23).
    updating: AtomicBool,
}

/// Holds launches off while an update installs; dropped if the install fails.
pub(crate) struct UpdateGuard<'slot> {
    slot: &'slot LaunchSlot,
}

impl Drop for UpdateGuard<'_> {
    fn drop(&mut self) {
        self.slot.updating.store(false, Ordering::Release);
    }
}

/// The running game: its PID, and the signal its monitor raises when the
/// process has really exited, which STOP waits on before using force
/// (windows-1).
struct RunningGame {
    pid: u32,
    exit_signal: ProcessExitSignal,
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
            running: Mutex::new(None),
            updating: AtomicBool::new(false),
        }
    }

    fn pid_guard(&self) -> std::sync::MutexGuard<'_, Option<RunningGame>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes the slot for a new launch, or says why not. The PID lock is held
    /// across both checks, so a spawn cannot slip in between them.
    pub(crate) fn claim(&self) -> Result<PreparingGuard<'_>, String> {
        let running = self.pid_guard();
        if let Some(RunningGame { pid, .. }) = *running {
            return Err(format!(
                "Minecraft is already running (PID {pid}). Stop it or close the game before launching again."
            ));
        }
        if self.updating.load(Ordering::Acquire) {
            return Err("Cubic Launcher is installing an update and will restart. Launch again after it restarts.".to_string());
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

    /// Stops new launches for an update install, and says what is going on
    /// right now: a game running or a launch preparing. Under the PID lock, as
    /// `claim`, so no launch slips in between the two (review-frontend-shell-23).
    pub(crate) fn hold_for_update(&self) -> Result<(UpdateGuard<'_>, LaunchStatus), String> {
        let running = self.pid_guard();
        if self.updating.swap(true, Ordering::AcqRel) {
            return Err("An update is already being installed.".to_string());
        }
        let status = LaunchStatus {
            preparing: self.preparing.load(Ordering::Acquire),
            running_pid: running.as_ref().map(|game| game.pid),
        };
        drop(running);
        Ok((UpdateGuard { slot: self }, status))
    }

    pub(crate) fn set_running(&self, pid: u32, exit_signal: ProcessExitSignal) {
        *self.pid_guard() = Some(RunningGame { pid, exit_signal });
    }

    /// Clears the PID only if it is still `pid`: the exit of one game must
    /// never forget the PID of another.
    pub(crate) fn clear_running_if(&self, pid: u32) -> bool {
        let mut running = self.pid_guard();
        if running.as_ref().map(|game| game.pid) == Some(pid) {
            *running = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn running_pid(&self) -> Option<u32> {
        self.pid_guard().as_ref().map(|game| game.pid)
    }

    /// The exit signal of `pid`, if it is still the running game: a PID no
    /// longer here has already exited.
    pub(crate) fn exit_signal_of(&self, pid: u32) -> Option<ProcessExitSignal> {
        self.pid_guard()
            .as_ref()
            .filter(|game| game.pid == pid)
            .map(|game| game.exit_signal.clone())
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
    use crate::process_streaming::ProcessExitSignal;

    /// review-frontend-shell-23: while an update installs no launch starts,
    /// and the install sees a launch that is preparing.
    #[test]
    fn an_update_install_holds_launches_off_and_sees_a_preparing_launch() {
        let slot = LaunchSlot::new();
        let (guard, status) = slot.hold_for_update().expect("nothing in the way");
        assert_eq!(
            status,
            LaunchStatus {
                preparing: false,
                running_pid: None
            }
        );
        let refusal = slot.claim().err().expect("no launch during the install");
        assert!(refusal.contains("installing an update"), "{refusal}");
        assert!(slot.hold_for_update().is_err());
        drop(guard);

        let preparing = slot.claim().expect("a failed install frees the slot");
        let (_guard, status) = slot.hold_for_update().unwrap();
        assert!(status.preparing);
        drop(preparing);
    }

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
        slot.set_running(4242, ProcessExitSignal::default());
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
        slot.set_running(200, ProcessExitSignal::default());

        assert!(!slot.clear_running_if(100));
        assert_eq!(slot.running_pid(), Some(200));

        assert!(slot.clear_running_if(200));
        assert_eq!(slot.running_pid(), None);
    }
}
