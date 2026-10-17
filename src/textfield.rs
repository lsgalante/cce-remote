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

use std::sync::Mutex;
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
}

static FIELD: Mutex<Field> = Mutex::new(Field { active: false, since: None });

fn publish(active: bool) {
    let mut f = FIELD.lock().unwrap();
    if f.active != active {
        *f = Field { active, since: Some(Instant::now()) };
    }
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
            publish(false);
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
            zwp_input_method_v2::Event::Activate => state.pending = true,
            zwp_input_method_v2::Event::Deactivate => state.pending = false,
            zwp_input_method_v2::Event::Done => publish(state.pending),
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
    fn nothing_is_repeated() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(5);
        assert_eq!(decide(true, Some(t0), true, later), None);
        assert_eq!(decide(false, Some(t0), false, later), None);
        // A page that connects before any field ever opened hears nothing.
        assert_eq!(decide(false, None, false, t0), None);
    }
}
