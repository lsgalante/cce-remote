//! What the page knows, shared by the app (which paints it) and the page's
//! own listeners (the trackpad, the keyboard, the sockets), which run outside
//! cce-ui's loop. One per page, in a thread-local: wasm has one thread.
//!
//! **Never call into the DOM from inside [`with`].** Focusing or blurring the
//! keyboard raises `focus`/`blur` synchronously, their listeners take the
//! state again, and a nested borrow panics. Read what you need, drop the
//! borrow, then act.

use std::cell::RefCell;

use cce_ui::engine::AppSender;
use crate::predict::{self, Edit, Field};
use cce_ui::scene::layout::Rect;

/// The app's message: the state changed, paint it.
#[derive(Debug, Clone)]
pub enum Msg {
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Paired,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Switcher,
    Menu,
    /// The menu's System list.
    System,
}

/// A row of the window switcher: an app window from `windows --json`.
#[derive(Debug, Clone)]
pub struct Win {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
}

/// The focused window's layout rect, which the cursor ring maps through.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WinRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

pub struct State {
    pub pin: String,
    pub sender: Option<AppSender<Msg>>,
    pub conn: Conn,
    /// The page version the server named on the first pairing; a different
    /// one later means it restarted onto new code.
    pub page_ver: Option<String>,

    // ── Published by the app's layout, in canvas px: where the trackpad is,
    // and what covers it.
    pub pad: Rect,
    pub overlay: Option<Overlay>,
    pub overlay_rect: Option<Rect>,
    pub windows: Vec<Win>,
    /// A `wl` was sent for the switcher; its reply opens it.
    pub want_switcher: bool,

    // ── Live view.
    pub view: bool,
    /// The frame texture, once one has arrived: (image id, width, height).
    pub frame: Option<(u32, u32, u32)>,
    pub frame_seq: u64,
    /// Frames painted and acknowledged; the stream sends the next on an ack.
    pub acked_seq: u64,
    pub last_frame_ms: f64,
    pub zoom: f32,
    pub pan: (f32, f32),
    pub win_rect: Option<WinRect>,
    /// The pointer in layout px: the last `ploc`, moved by prediction since.
    pub ptr: Option<(f64, f64)>,

    // ── Keyboard.
    /// A desktop text field is open (`ti 1`).
    pub field_open: bool,
    /// The phone keyboard is up because a field raised it (the type bar or
    /// a tap's `/click`), so a closing field may drop it.
    pub kb_by_field: bool,
    pub kb_up: bool,
    /// The local echo of what has been typed since the keyboard came up:
    /// shown when the field does not report its contents.
    pub typed: String,
    /// What the desktop field holds, as its app last reported it (`tt`):
    /// text and the caret's and anchor's byte offsets in it.
    pub field_text: Option<Field>,
    /// Edits typed here that the last report does not reflect yet: the strip
    /// shows them at once (`crate::predict`), and a later report retires them.
    pub pending: Vec<Edit>,
    /// When the oldest pending edit was made, to give up on one the desktop
    /// never reflects (`expire_prediction`).
    pub pending_at: f64,
    /// Latched modifiers (evdev codes), held down on the desktop.
    pub mods: Vec<u32>,
}

/// How long a predicted edit may go unreflected before the strip stops
/// showing it and trusts the desktop's last report.
const PREDICTION_TTL_MS: f64 = 1500.0;

impl State {
    /// An edit typed here, toward a field that reports its contents.
    pub fn predict(&mut self, edit: Edit) {
        if self.field_text.is_none() {
            return;
        }
        if self.pending.is_empty() {
            self.pending_at = now_ms();
        }
        self.pending.push(edit);
    }

    /// A report from the desktop (`tt`): it becomes the base, and the edits
    /// it reflects stop being pending.
    pub fn confirm_field(&mut self, now: Option<Field>) {
        self.pending = match (&self.field_text, &now) {
            (Some(base), Some(n)) => predict::remaining(base, &self.pending, n),
            _ => Vec::new(),
        };
        if !self.pending.is_empty() {
            self.pending_at = now_ms();
        }
        self.field_text = now;
    }

    /// Drop predictions the desktop has not reflected in time. Whether
    /// anything changed.
    pub fn expire_prediction(&mut self) -> bool {
        let stale = !self.pending.is_empty() && now_ms() - self.pending_at > PREDICTION_TTL_MS;
        if stale {
            self.pending.clear();
        }
        stale
    }

    /// What the strip shows for the field: the last report with the
    /// pending edits applied.
    pub fn shown_field(&self) -> Option<Field> {
        self.field_text.as_ref().map(|base| predict::shown(base, &self.pending))
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn init(pin: String) {
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            pin,
            sender: None,
            conn: Conn::Connecting,
            page_ver: None,
            pad: Rect::ZERO,
            overlay: None,
            overlay_rect: None,
            windows: Vec::new(),
            want_switcher: false,
            view: false,
            frame: None,
            frame_seq: 0,
            acked_seq: 0,
            last_frame_ms: 0.0,
            zoom: 1.0,
            pan: (0.0, 0.0),
            win_rect: None,
            ptr: None,
            field_open: false,
            kb_by_field: false,
            kb_up: false,
            typed: String::new(),
            field_text: None,
            pending: Vec::new(),
            pending_at: 0.0,
            mods: Vec::new(),
        })
    });
}

/// Run `f` on the state. See the module note: no DOM calls inside.
pub fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("state::init before use")))
}

/// Ask the app to paint the state again.
pub fn changed() {
    if let Some(sender) = with(|s| s.sender.clone()) {
        let _ = sender.send(Msg::Changed);
    }
}

/// Milliseconds on the page's clock (std's `Instant` panics on wasm32).
pub fn now_ms() -> f64 {
    js_sys::Date::now()
}
