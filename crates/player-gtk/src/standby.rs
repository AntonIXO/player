//! Standby battery saver (Poco F1 / postmarketOS). When **no USB DAC is
//! connected** and the player has been **idle past a timeout**, suspend the
//! system (`systemctl suspend` → s2idle on this kernel). A logind **sleep
//! inhibitor** is held the whole time a DAC is attached *or* audio is
//! playing/paused, so the phone can never suspend mid-session.
//!
//! Audio is the priority and is never touched: this only acts when nobody can
//! be listening (no DAC, nothing playing/paused, no recent UI input). On resume
//! the system-sleep hook (`packaging/aports/hifi-player`) re-asserts the audio
//! environment *before* any device is reopened, so the first post-wake track is
//! as clean as a cold boot.
//!
//! Diagnosis note: on this mainline SDM845 kernel the SoC does not reach full
//! AOSS/CX deep sleep, and s2idle is only effective once the radios are quiet —
//! so for a real battery win WiFi/BT should be off in standby. Wire that with
//! `PLAYER_STANDBY_ENTER_CMD` (run on entry; the resume hook restores it).
//!
//! **Off by default.** Enabled only when `PLAYER_STANDBY_TIMEOUT_SECS` is set to
//! a positive value, so the desktop dev box never suspends itself. Optional env:
//! `PLAYER_STANDBY_POLL_SECS` (default 20) and `PLAYER_STANDBY_ENTER_CMD`
//! (a shell command run via `sh -c` just before suspending, e.g.
//! `"nmcli radio wifi off"`).

use std::cell::{Cell, RefCell};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4 as gtk;

use gtk::glib;

use crate::state::SharedState;

/// Shared, cheaply-clonable standby coordinator. All fields are `Rc`, so clones
/// handed to input/event closures share the same activity clock and inhibitor.
#[derive(Clone)]
pub(crate) struct Standby {
    last_activity: Rc<Cell<Instant>>,
    inhibitor: Rc<RefCell<Option<Child>>>,
    timeout: Duration,
    poll: Duration,
    enter_cmd: Option<Rc<str>>,
}

impl Standby {
    /// Build from the environment, or `None` if the feature is disabled
    /// (`PLAYER_STANDBY_TIMEOUT_SECS` unset or `0`).
    pub(crate) fn from_env() -> Option<Standby> {
        let timeout = env_secs("PLAYER_STANDBY_TIMEOUT_SECS")?;
        if timeout.is_zero() {
            return None;
        }
        // Poll often enough to track DAC plug/unplug and idle; default 20 s.
        let poll = env_secs("PLAYER_STANDBY_POLL_SECS").unwrap_or(Duration::from_secs(20));
        let poll = poll.max(Duration::from_secs(1));
        let enter_cmd = std::env::var("PLAYER_STANDBY_ENTER_CMD")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| Rc::from(s.as_str()));
        eprintln!(
            "[standby] enabled: suspend after {}s idle with no DAC (poll {}s)",
            timeout.as_secs(),
            poll.as_secs()
        );
        Some(Standby {
            last_activity: Rc::new(Cell::new(Instant::now())),
            inhibitor: Rc::new(RefCell::new(None)),
            timeout,
            poll,
            enter_cmd,
        })
    }

    /// Record user/playback activity, resetting the idle clock. Cheap; safe to
    /// call from every input event.
    pub(crate) fn note_activity(&self) {
        self.last_activity.set(Instant::now());
    }

    /// Install the periodic evaluator on the GTK main loop. Holds a clone of
    /// `state` (read each tick for play/pause) for the app's lifetime.
    pub(crate) fn start(&self, state: SharedState) {
        let me = self.clone();
        glib::timeout_add_seconds_local(self.poll.as_secs().max(1) as u32, move || {
            me.tick(&state);
            glib::ControlFlow::Continue
        });
    }

    fn tick(&self, state: &SharedState) {
        let (playing, paused) = {
            let s = state.borrow();
            (s.playing, s.paused)
        };
        let dac = player_core::has_usb_dac();
        // A session is "busy" while audio is playing or held paused (the device
        // is open / silence keep-alive running) — never suspend then.
        let busy = playing || paused;

        // Keep the idle clock fresh while anything is going on.
        if busy || dac {
            self.note_activity();
        }
        // Block sleep whenever a DAC is attached or audio is active.
        self.set_inhibited(busy || dac);

        let idle = self.last_activity.get().elapsed();
        if should_suspend(idle, dac, busy, self.timeout) {
            eprintln!(
                "[standby] idle {}s, no DAC — suspending",
                idle.as_secs()
            );
            // Drop our own inhibitor first so it can't block the suspend.
            self.set_inhibited(false);
            if let Some(cmd) = self.enter_cmd.as_deref() {
                run_shell(cmd);
            }
            trigger_suspend();
            // Fresh idle window after the system resumes (we were frozen, not
            // restarted), so we don't immediately re-suspend on the next tick.
            self.note_activity();
        }
    }

    /// Acquire/release the logind sleep inhibitor by holding a blocking
    /// `systemd-inhibit … sleep infinity` child. Idempotent.
    fn set_inhibited(&self, want: bool) {
        let mut slot = self.inhibitor.borrow_mut();
        match (want, slot.is_some()) {
            (true, false) => match spawn_inhibitor() {
                Ok(child) => *slot = Some(child),
                Err(e) => eprintln!("[standby] inhibitor spawn failed: {e}"),
            },
            (false, true) => {
                if let Some(mut child) = slot.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            _ => {}
        }
    }
}

/// Pure suspend policy (unit-tested): suspend only when no DAC is present, no
/// audio is playing/paused, and the player has been idle at least `timeout`.
fn should_suspend(idle: Duration, dac_present: bool, session_busy: bool, timeout: Duration) -> bool {
    !dac_present && !session_busy && idle >= timeout
}

fn spawn_inhibitor() -> std::io::Result<Child> {
    Command::new("systemd-inhibit")
        .args([
            "--what=sleep",
            "--who=Bit-Perfect Player",
            "--why=DAC connected / audio active",
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn trigger_suspend() {
    // logind allows the active local session to suspend without a password.
    if let Err(e) = Command::new("systemctl").arg("suspend").spawn() {
        eprintln!("[standby] `systemctl suspend` failed: {e}");
    }
}

fn run_shell(cmd: &str) {
    if let Err(e) = Command::new("sh").arg("-c").arg(cmd).spawn() {
        eprintln!("[standby] enter-cmd `{cmd}` failed: {e}");
    }
}

fn env_secs(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(600);

    #[test]
    fn suspends_only_when_idle_no_dac_and_not_busy() {
        // Past the timeout, no DAC, nothing playing → suspend.
        assert!(should_suspend(Duration::from_secs(601), false, false, T));
    }

    #[test]
    fn dac_connected_never_suspends() {
        assert!(!should_suspend(Duration::from_secs(10_000), true, false, T));
    }

    #[test]
    fn playing_or_paused_never_suspends() {
        // `session_busy` covers both playing and held-paused.
        assert!(!should_suspend(Duration::from_secs(10_000), false, true, T));
    }

    #[test]
    fn before_timeout_does_not_suspend() {
        assert!(!should_suspend(Duration::from_secs(599), false, false, T));
        // Exactly at the timeout it is allowed.
        assert!(should_suspend(T, false, false, T));
    }
}
