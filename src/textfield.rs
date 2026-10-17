//! Whether a text field on the desktop is open for typing — the signal behind
//! the page's "tap to type" prompt.
//!
//! The compositor relays text-input-v3 to one input method per seat
//! (`input_relay.rs` in cce-compositor): `activate` when a field is enabled,
//! `deactivate` when it lets go, each batch closed by `done`. This module
//! registers as that input method on its own long-lived Wayland connection
//! and only listens: it commits no text, sets no preedit and grabs no
//! keyboard, so apps behave exactly as they did without it. Typing still goes
//! through the control socket as keycodes.
//!
//! The seat takes ONE input method. If something else holds it (fcitx, say)
//! the compositor answers `unavailable`; the watcher then retries quietly, the
//! prompt never shows, and everything else keeps working.
//!
//! State is published to a static and read by each `/ws` connection's loop,
//! which applies the same field-switch grace `osk.rs` does before telling the
//! page a field closed — see [`decide`].

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};

/// How long a field may be gone before the page hears it closed. Moving from
/// one field to the next disables one text input and enables the other; the
/// prompt (or the phone keyboard it opened) must not flicker in between.
/// Matches cce-compositor's `osk.rs` `HIDE_DELAY_MS`.
const CLOSE_GRACE: Duration = Duration::from_millis(250);

/// Between attempts to (re)register: the compositor restarting, or another
/// input method holding the seat.
const RETRY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
struct Field {
    active: bool,
    /// When `active` last changed; None until it first does.
    since: Option<Instant>,
    /// Count of activations: a field enabled, or focus moving to another.
    /// Bumped even when `active` was already true (a field switch), which
    /// is what lets `/click` tell "this click opened a field" from "one was
    /// already open".
    opened: u64,
}

static FIELD: Mutex<Field> = Mutex::new(Field { active: false, since: None, opened: 0 });
/// Signalled on every activation, for [`wait_opened`].
static OPENED: Condvar = Condvar::new();

/// Apply one `done`: the field's state, and whether that batch carried an
/// `activate`.
fn publish(active: bool, activated: bool) {
    let mut f = FIELD.lock().unwrap();
    if f.active != active {
        f.active = active;
        f.since = Some(Instant::now());
    }
    if active && activated {
        f.opened += 1;
        OPENED.notify_all();
    }
}

/// The activation count, to hand [`wait_opened`] before acting.
pub fn opened() -> u64 {
    FIELD.lock().unwrap().opened
}

/// Wait up to `timeout` for an activation after `opened()` read `after`.
/// True as soon as one happens.
pub fn wait_opened(after: u64, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut f = FIELD.lock().unwrap();
    while f.opened == after {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        f = OPENED.wait_timeout(f, left).unwrap().0;
    }
    true
}

/// What a connection that last told its page `sent` should send now, if
/// anything: `Some(true)` for "a field opened", `Some(false)` for "it closed".
pub fn notice(sent: bool, now: Instant) -> Option<bool> {
    let f = *FIELD.lock().unwrap();
    decide(f.active, f.since, sent, now)
}

/// An opening is reported at once; a closing only once it has lasted
/// `CLOSE_GRACE`, so a field switch — closed, then open again a frame
/// later — reads as no change at all.
fn decide(active: bool, since: Option<Instant>, sent: bool, now: Instant) -> Option<bool> {
    if active == sent {
        return None;
    }
    if active {
        return Some(true);
    }
    match since {
        Some(t) if now.saturating_duration_since(t) < CLOSE_GRACE => None,
        _ => Some(false),
    }
}

/// Register as the seat's input method and keep the published state current,
/// for the life of the process.
pub fn spawn_watcher() {
    std::thread::spawn(|| {
        let mut last_err = String::new();
        loop {
            let err = watch().err().unwrap_or_else(|| "connection closed".into());
            publish(false, false);
            // Retrying every RETRY would repeat the same line forever while
            // another input method holds the seat; say it once.
            if err != last_err {
                eprintln!("[cce-remote] text-field watcher: {err} (retrying every {}s)", RETRY.as_secs());
                last_err = err;
            }
            std::thread::sleep(RETRY);
        }
    });
}

#[derive(Default)]
struct ImState {
    /// `activate`/`deactivate` are double-buffered until `done`.
    pending: bool,
    /// This batch carried an `activate`.
    activated: bool,
    unavailable: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ImState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(ImState: ignore wl_seat::WlSeat);
delegate_noop!(ImState: ignore ZwpInputMethodManagerV2);

impl Dispatch<ZwpInputMethodV2, ()> for ImState {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => {
                state.pending = true;
                state.activated = true;
            }
            zwp_input_method_v2::Event::Deactivate => state.pending = false,
            zwp_input_method_v2::Event::Done => {
                publish(state.pending, state.activated);
                state.activated = false;
            }
            zwp_input_method_v2::Event::Unavailable => state.unavailable = true,
            _ => {}
        }
    }
}

/// One registration, until the connection fails or the seat refuses us.
fn watch() -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<ImState>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
    let manager: ZwpInputMethodManagerV2 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("input-method-v2 unavailable: {e}"))?;
    let im = manager.get_input_method(&seat, &qh, ());
    let mut state = ImState::default();
    loop {
        queue.blocking_dispatch(&mut state).map_err(|e| e.to_string())?;
        if state.unavailable {
            im.destroy();
            let _ = conn.flush();
            return Err("the seat already has an input method".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_opening_field_is_reported_at_once() {
        let t0 = Instant::now();
        assert_eq!(decide(true, Some(t0), false, t0), Some(true));
    }

    #[test]
    fn a_closing_field_waits_out_the_grace() {
        let t0 = Instant::now();
        assert_eq!(decide(false, Some(t0), true, t0), None);
        assert_eq!(decide(false, Some(t0), true, t0 + CLOSE_GRACE - Duration::from_millis(1)), None);
        assert_eq!(decide(false, Some(t0), true, t0 + CLOSE_GRACE), Some(false));
    }

    #[test]
    fn a_field_switch_is_no_change() {
        // Closed, then open again inside the grace: by the time anyone looks,
        // the state matches what the page was told.
        let t0 = Instant::now();
        assert_eq!(decide(false, Some(t0), true, t0 + Duration::from_millis(50)), None);
        assert_eq!(decide(true, Some(t0 + Duration::from_millis(60)), true, t0 + Duration::from_millis(100)), None);
    }

    #[test]
    fn a_click_hears_an_activation_or_times_out() {
        // The only test that touches the statics.
        let before = opened();
        let t0 = Instant::now();
        assert!(!wait_opened(before, Duration::from_millis(30)));
        assert!(t0.elapsed() >= Duration::from_millis(30));
        // A batch that keeps a field open without an activate is not one.
        publish(true, false);
        assert!(!wait_opened(before, Duration::from_millis(10)));
        let waker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            publish(true, true);
        });
        let t0 = Instant::now();
        assert!(wait_opened(before, Duration::from_secs(5)));
        assert!(t0.elapsed() < Duration::from_secs(1), "woken, not timed out");
        waker.join().unwrap();
        publish(false, false);
    }

    #[test]
    fn nothing_is_repeated() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(5);
        assert_eq!(decide(true, Some(t0), true, later), None);
        assert_eq!(decide(false, Some(t0), false, later), None);
        // A page that connects before any field ever opened hears nothing.
        assert_eq!(decide(false, None, false, t0), None);
    }
}
