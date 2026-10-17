//! The Agent view: Code Agent's sessions on the phone. The menu's "Code
//! Agent" row opens it in place of the trackpad; "‹ Trackpad" goes back.
//!
//! Two screens. **Sessions**: every session on disk by folder (archived
//! ones left out), each with what a Code Agent window is doing with it, or
//! its age; a folder's `+` starts a new session there. **A session**: its
//! transcript (replies as wrapped text, tool calls as one row each), a
//! waiting permission prompt with Deny / Always allow / Allow, and a prompt
//! strip with the keyboard and Send; Stop while a turn runs, Archive when
//! none does. With no Code Agent window on the desktop the server answers
//! from disk and the view is read-only (the user's choice).
//!
//! The server (`../agent.rs`) carries each request to Code Agent; the
//! requests go out as `ag <json>` frames on the input socket and come back
//! the same way, matched by their `n`. While the view is open a timer asks
//! for the list or the open transcript (with its version, so an unchanged
//! one costs a short reply).
//!
//! Its data is a thread-local of its own (`DATA`): the socket's listener
//! and the keyboard (which writes the draft while a prompt is being
//! written, `compose`) reach it outside the app, as they reach `state`.
//! The same rule holds: no DOM calls inside [`with`].
//!
//! Text wraps by character count: the page's face is the desktop's
//! monospace one, which `paint_preview` already relies on. Markdown is
//! shown as written.

use std::cell::RefCell;
use std::collections::HashMap;

use cce_ui::context::UiContext;
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{AlignH, AlignV, PaintCtx, TextAttrs, TextLayout};
use cce_ui::scene::Material;
use cce_ui::widget::{Adapted, Button, Handle, WidgetHost, WidgetId};
use serde_json::Value;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::{kb, net, state};
use crate::wrap::wrap;

type Btn = Handle<Adapted<Button>>;

const BTN_H: f32 = 40.0;
const ROW_H: f32 = 44.0;
const GROUP_H: f32 = 36.0;
/// How often the open screen is refreshed, in ms.
const POLL_MS: i32 = 1200;
/// Read-only, every this many ticks (about 5 s): nothing on the desktop
/// is changing the sessions, and each answer costs a process.
const READONLY_EVERY: u32 = 4;
/// The prompt strip shows at most this many lines of the draft.
const DRAFT_LINES: usize = 4;
/// A failed call's error, at most this many lines.
const ERROR_LINES: usize = 6;
/// A tap that moved more than this is a drag (a scroll), not a tap.
const TAP_SLOP: f32 = 12.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    List,
    Session { id: String, title: String },
    /// A first prompt for a new session in `cwd`.
    New { cwd: String, label: String },
}

#[derive(Debug, Clone, Default)]
struct Sess {
    id: String,
    title: String,
    active: f64,
    archived: bool,
    elsewhere: Option<String>,
    /// What a Code Agent window is doing with it: working, waiting, idle…
    state: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct Group {
    cwd: String,
    label: String,
    sessions: Vec<Sess>,
}

#[derive(Debug, Clone, Default)]
struct En {
    kind: String,
    text: String,
    name: String,
    summary: String,
    state: String,
    output: String,
    error: bool,
}

#[derive(Debug, Clone, Default)]
struct Pend {
    request_id: String,
    text: String,
    always: bool,
    question: bool,
}

#[derive(Debug, Clone, Default)]
struct Tr {
    id: String,
    version: String,
    busy: bool,
    readonly: bool,
    entries: Vec<En>,
    pending: Vec<Pend>,
}

/// What a request was, to file its reply.
#[derive(Debug, Clone)]
enum Asked {
    List,
    Transcript(String),
    Action,
}

#[derive(Default)]
struct Data {
    on: bool,
    screen: Option<Screen>,
    groups: Vec<Group>,
    readonly: bool,
    listed: bool,
    tr: Option<Tr>,
    /// A line at the top: an error from the server, or what was done.
    note: Option<(String, bool)>,
    draft: String,
    next_n: u64,
    asked: HashMap<u64, Asked>,
    /// A list or transcript request is out: the timer waits for it.
    in_flight: bool,
    /// The timer's ticks: read-only answers (with no Code Agent window,
    /// each one a `cce-code-agent` run over the session files) are asked
    /// for on every `READONLY_EVERY`th.
    ticks: u32,
    timer: Option<i32>,
    /// Bumped when the transcript changes, so the view relays it out.
    tr_rev: u64,
    /// The last list reply as it came (less its `n`): an unchanged one
    /// repaints nothing, as every poll reply on this page.
    list_raw: String,
}

thread_local! {
    static DATA: RefCell<Data> = RefCell::new(Data::default());
}

fn with<R>(f: impl FnOnce(&mut Data) -> R) -> R {
    DATA.with(|d| f(&mut d.borrow_mut()))
}

pub fn is_on() -> bool {
    with(|d| d.on)
}

/// Writing a prompt: the keyboard's text is the draft's, not the desktop's.
fn composing() -> bool {
    with(|d| d.on && matches!(d.screen, Some(Screen::Session { .. } | Screen::New { .. })) && !d.readonly_screen())
}

/// Where the body's content starts: its rows are drawn inside its padding.
fn content_top(body: Rect) -> f32 {
    body.y + cce_ui::layout::plate_padding()
}

impl Data {
    /// The screen shown is read-only: no Code Agent window answered (the
    /// list, a new session), or the transcript came from disk.
    fn readonly_screen(&self) -> bool {
        match &self.screen {
            Some(Screen::Session { .. }) => self.tr.as_ref().is_some_and(|t| t.readonly) || self.readonly,
            _ => self.readonly,
        }
    }
}

/// Send a request; its reply is filed by `n`.
fn ask(op: Value, asked: Asked) {
    let frame = with(|d| {
        d.next_n += 1;
        let n = d.next_n;
        d.asked.insert(n, asked);
        let mut op = op;
        op["n"] = Value::from(n);
        format!("ag {op}")
    });
    net::send(&frame);
}

/// The timer's tick: the open screen again, less often while read-only.
fn tick() {
    let skip = with(|d| {
        d.ticks = d.ticks.wrapping_add(1);
        d.readonly_screen() && d.ticks % READONLY_EVERY != 0
    });
    if !skip {
        refresh();
    }
}

fn refresh() {
    let (screen, version) = with(|d| {
        if d.in_flight {
            return (None, None);
        }
        d.in_flight = true;
        (d.screen.clone(), d.tr.as_ref().map(|t| t.version.clone()))
    });
    match screen {
        Some(Screen::Session { id, .. }) => {
            let mut op = serde_json::json!({"op": "transcript", "id": id});
            if let Some(v) = version.filter(|_| with(|d| d.tr.as_ref().is_some_and(|t| t.id == id))) {
                op["since"] = Value::from(v);
            }
            ask(op, Asked::Transcript(id));
        }
        Some(_) => ask(serde_json::json!({"op": "list"}), Asked::List),
        None => {}
    }
}

/// Open the view (the menu's row).
pub fn show() {
    with(|d| {
        d.on = true;
        d.screen = Some(Screen::List);
        d.in_flight = false;
    });
    refresh();
    let tick = Closure::<dyn FnMut()>::new(tick);
    let id = web_sys::window()
        .unwrap()
        .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), POLL_MS)
        .ok();
    tick.forget();
    if let Some(old) = with(|d| std::mem::replace(&mut d.timer, id)) {
        web_sys::window().unwrap().clear_interval_with_handle(old);
    }
    state::changed();
}

fn hide() {
    if let Some(t) = with(|d| {
        d.on = false;
        d.timer.take()
    }) {
        web_sys::window().unwrap().clear_interval_with_handle(t);
    }
    if kb::is_up() {
        kb::toggle();
    }
    state::changed();
}

fn go(screen: Screen) {
    with(|d| {
        if matches!(&screen, Screen::Session { id, .. } if d.tr.as_ref().is_none_or(|t| &t.id != id)) {
            d.tr = None;
            d.tr_rev += 1;
        }
        d.screen = Some(screen);
        d.in_flight = false;
        d.note = None;
    });
    refresh();
    state::changed();
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

/// An `ag` reply from the server. A poll's reply repaints only when it
/// changed something: the view polls every `POLL_MS`, and a repaint per
/// reply kept an idle page drawing.
pub fn on_reply(json: &str) {
    let Ok(mut v) = serde_json::from_str::<Value>(json) else { return };
    let Some(asked) = v["n"].as_u64().and_then(|n| with(|d| d.asked.remove(&n))) else { return };
    let error = v["error"].as_str().map(str::to_string);
    let changed = match asked {
        Asked::List => with(|d| {
            d.in_flight = false;
            if let Some(e) = error {
                let changed = d.note.as_ref().map(|n| &n.0) != Some(&e);
                d.note = Some((e, true));
                return changed;
            }
            if let Some(o) = v.as_object_mut() {
                o.remove("n");
            }
            let raw = v.to_string();
            if raw == d.list_raw {
                return false;
            }
            d.list_raw = raw;
            let open = v["open"].as_object().cloned().unwrap_or_default();
            d.readonly = v["readonly"].as_bool().unwrap_or(false);
            d.listed = true;
            d.groups = v["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| Group {
                    cwd: s(p, "cwd"),
                    label: s(p, "label"),
                    sessions: p["sessions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|x| {
                            let id = s(x, "id");
                            Sess {
                                state: open.get(&id).and_then(|o| o["state"].as_str()).map(str::to_string),
                                title: s(x, "title"),
                                active: x["active"].as_f64().unwrap_or(0.0),
                                archived: x["archived"].as_bool().unwrap_or(false),
                                elsewhere: x["elsewhere"].as_str().map(str::to_string),
                                id,
                            }
                        })
                        .collect(),
                })
                .collect();
            true
        }),
        Asked::Transcript(id) => with(|d| {
            d.in_flight = false;
            if let Some(e) = error {
                let changed = d.note.as_ref().map(|n| &n.0) != Some(&e);
                d.note = Some((e, true));
                return changed;
            }
            if v["unchanged"].as_bool() == Some(true) || !matches!(&d.screen, Some(Screen::Session { id: shown, .. }) if *shown == id) {
                return false;
            }
            let readonly = v["readonly"].as_bool().unwrap_or(false);
            d.tr = Some(Tr {
                id,
                version: s(&v, "version"),
                busy: v["busy"].as_bool().unwrap_or(false),
                readonly,
                entries: v["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| En {
                        kind: s(e, "kind"),
                        text: s(e, "text"),
                        name: s(e, "name"),
                        summary: s(e, "summary"),
                        state: s(e, "state"),
                        output: s(e, "output"),
                        error: e["error"].as_bool().unwrap_or(false),
                    })
                    .collect(),
                pending: v["pending"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| Pend {
                        request_id: s(p, "request_id"),
                        text: s(p, "text"),
                        always: p["always"].as_bool().unwrap_or(false),
                        question: p["question"].as_bool().unwrap_or(false),
                    })
                    .collect(),
            });
            d.tr_rev += 1;
            true
        }),
        Asked::Action => {
            with(|d| {
                d.note = error.map(|e| (e, true));
                d.in_flight = false;
            });
            refresh();
            true
        }
    };
    if changed {
        state::changed();
    }
}

/// The keyboard's input while a prompt is being written: it edits the
/// draft and says so (the keyboard then sends nothing to the desktop).
pub fn compose(kind: &str, data: &str) -> bool {
    if !composing() {
        return false;
    }
    with(|d| match kind {
        "insertText" | "insertReplacementText" => d.draft.push_str(data),
        "insertLineBreak" | "insertParagraph" => d.draft.push('\n'),
        "deleteContentBackward" => {
            d.draft.pop();
        }
        "deleteWordBackward" => {
            let t = d.draft.trim_end();
            let cut = t.rfind(char::is_whitespace).map_or(0, |i| i + 1);
            d.draft.truncate(cut);
        }
        _ => {}
    });
    true
}

// ── Painting helpers.

fn font() -> (String, f32) {
    cce_ui::layout::control_label_font_parsed()
}

fn fg() -> [u8; 3] {
    cce_ui::color::control_label_color_u8()
}

fn dim() -> [u8; 3] {
    cce_ui::color::control_label_color_detached_u8()
}

fn lin(c: [f32; 4]) -> [f32; 4] {
    cce_ui::color::to_linear(c)
}

const RUNNING: [f32; 4] = [0.95, 0.75, 0.30, 1.0];
const DONE: [f32; 4] = [0.45, 0.80, 0.50, 1.0];
const FAILED: [f32; 4] = [1.0, 0.45, 0.40, 1.0];
const STOPPED: [f32; 4] = [0.55, 0.57, 0.62, 1.0];
const WAITING: [f32; 4] = [0.49, 1.0, 1.0, 1.0];
const BUBBLE: [f32; 4] = [1.0, 1.0, 1.0, 0.06];

/// One line of text, left-aligned in a `h`-tall box at (x, y).
fn line(pc: &mut PaintCtx, text: &str, x: f32, y: f32, h: f32, size: f32, rgb: [u8; 3], clip: Rect) {
    if text.is_empty() {
        return;
    }
    pc.text_boxed(
        text.to_string(),
        x,
        y,
        size,
        rgb,
        Some(font().0),
        Some([clip.x, clip.y, clip.x + clip.width, clip.y + clip.height]),
        TextAttrs::default(),
        TextLayout { wrap_width: None, box_height: h, align_h: AlignH::Left, align_v: AlignV::Middle },
    );
}

/// `text` cut to `cols` characters, with an ellipsis.
fn elide(text: &str, cols: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= cols {
        return flat;
    }
    format!("{}…", flat.chars().take(cols.saturating_sub(1)).collect::<String>())
}

fn ago(active: f64) -> String {
    let now = state::now_ms() / 1000.0;
    let s = (now - active).max(0.0) as u64;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn state_word(st: &str) -> (&'static str, [f32; 4]) {
    match st {
        "working" => ("working…", RUNNING),
        "waiting" => ("needs you", WAITING),
        "starting" => ("starting…", STOPPED),
        "ended" => ("ended", STOPPED),
        _ => ("open", DONE),
    }
}

/// A hand-drawn row a tap acts on.
#[derive(Debug, Clone)]
enum Hit {
    Session(String, String),
    NewIn(String, String),
}

/// The Agent view's widgets and layout, held by the app.
pub struct View {
    back: Btn,
    /// Stop while a turn runs, Archive when none does.
    act: Btn,
    send: Btn,
    kbd: Btn,
    deny: Btn,
    always: Btn,
    allow: Btn,
    title_rect: Rect,
    body: Rect,
    card: Option<Rect>,
    strip: Option<Rect>,
    scroll: f32,
    /// The body's rows, in content coordinates, for taps.
    hits: Vec<(Rect, Hit)>,
    content_h: f32,
    stick_bottom: bool,
    seen_rev: u64,
    press: Option<(f32, f32)>,
    roots: Vec<WidgetId>,
}

fn button(ui: &mut UiContext, label: &str) -> Btn {
    ui.insert(Button::new(0.0, 0.0, 0.0, 0.0).with_label(label))
}

const OFF: Rect = Rect { x: -1000.0, y: -1000.0, width: 0.0, height: 0.0 };

impl View {
    pub fn new(ui: &mut UiContext) -> View {
        View {
            back: button(ui, "‹ Trackpad"),
            act: button(ui, "Stop"),
            send: button(ui, "Send"),
            kbd: button(ui, "Keyboard"),
            deny: button(ui, "Deny"),
            always: button(ui, "Always"),
            allow: button(ui, "Allow"),
            title_rect: Rect::ZERO,
            body: Rect::ZERO,
            card: None,
            strip: None,
            scroll: 0.0,
            hits: Vec::new(),
            content_h: 0.0,
            stick_bottom: true,
            seen_rev: 0,
            press: None,
            roots: Vec::new(),
        }
    }

    /// The widgets that take events this frame.
    pub fn roots(&self) -> &[WidgetId] {
        &self.roots
    }

    fn place(ui: &mut UiContext, b: Btn, r: Rect) {
        ui[b].set_rect(r.x, r.y, r.width, r.height);
    }

    /// Lay the screen out in a `w`×`h` frame.
    pub fn layout(&mut self, ui: &mut UiContext, w: f32, h: f32) {
        let pad = cce_ui::layout::plate_padding();
        let gap = cce_ui::layout::list_gap();
        let (screen, tr, readonly) = with(|d| (d.screen.clone().unwrap_or(Screen::List), d.tr.clone(), d.readonly_screen()));
        let session = matches!(screen, Screen::Session { .. });
        let writing = matches!(screen, Screen::Session { .. } | Screen::New { .. }) && !readonly;
        self.roots.clear();
        // Header: back, the title, and Stop / Archive in a session.
        let top = pad;
        let back_w = 116.0;
        ui[self.back].set_label(if matches!(screen, Screen::List) { "‹ Trackpad" } else { "‹ Sessions" });
        Self::place(ui, self.back, Rect { x: pad, y: top, width: back_w, height: BTN_H });
        self.roots.push(self.back.id());
        let act_w = 92.0;
        let busy = tr.as_ref().is_some_and(|t| t.busy);
        if session && !readonly {
            ui[self.act].set_label(if busy { "Stop" } else { "Archive" });
            Self::place(ui, self.act, Rect { x: w - pad - act_w, y: top, width: act_w, height: BTN_H });
            self.roots.push(self.act.id());
        } else {
            Self::place(ui, self.act, OFF);
        }
        let tx = pad + back_w + gap;
        let tw = if session && !readonly { w - pad - act_w - gap - tx } else { w - pad - tx };
        self.title_rect = Rect { x: tx, y: top, width: tw.max(0.0), height: BTN_H };

        // From the bottom: the prompt strip, then a waiting prompt's card.
        let (family, size) = font();
        let cw = char_w(&family, size);
        let line_h = (size * 1.45).ceil();
        let mut bottom = h - pad;
        self.strip = None;
        for b in [self.send, self.kbd] {
            Self::place(ui, b, OFF);
        }
        if writing {
            let draft = with(|d| d.draft.clone());
            let cols = ((w - 2.0 * pad - 2.0 * pad) / cw) as usize;
            let lines = wrap(&draft, cols).len().clamp(1, DRAFT_LINES);
            let row_y = bottom - BTN_H;
            let bw = (w - 2.0 * pad - gap) / 2.0;
            Self::place(ui, self.kbd, Rect { x: pad, y: row_y, width: bw, height: BTN_H });
            Self::place(ui, self.send, Rect { x: pad + bw + gap, y: row_y, width: bw, height: BTN_H });
            self.roots.extend([self.kbd.id(), self.send.id()]);
            let sh = lines as f32 * line_h + pad;
            let strip = Rect { x: pad, y: row_y - gap - sh, width: w - 2.0 * pad, height: sh };
            self.strip = Some(strip);
            bottom = strip.y - gap;
        }
        self.card = None;
        for b in [self.deny, self.always, self.allow] {
            Self::place(ui, b, OFF);
        }
        let front = tr.as_ref().and_then(|t| t.pending.first().cloned()).filter(|_| session && !readonly);
        if let Some(p) = front {
            let cols = ((w - 4.0 * pad) / cw) as usize;
            let lines = wrap(&p.text, cols).len().min(4);
            let ch = pad + (lines as f32 + if p.question { 1.0 } else { 0.0 }) * line_h + gap + BTN_H + pad;
            let card = Rect { x: pad, y: bottom - ch, width: w - 2.0 * pad, height: ch };
            let by = card.y + card.height - pad - BTN_H;
            let inner = card.width - 2.0 * pad;
            let n = if p.question { 1.0 } else if p.always { 3.0 } else { 2.0 };
            let bw = (inner - (n - 1.0) * gap) / n;
            let mut x = card.x + pad;
            ui[self.deny].set_label(if p.question { "Skip" } else { "Deny" });
            Self::place(ui, self.deny, Rect { x, y: by, width: bw, height: BTN_H });
            self.roots.push(self.deny.id());
            x += bw + gap;
            if !p.question {
                if p.always {
                    Self::place(ui, self.always, Rect { x, y: by, width: bw, height: BTN_H });
                    self.roots.push(self.always.id());
                    x += bw + gap;
                }
                Self::place(ui, self.allow, Rect { x, y: by, width: bw, height: BTN_H });
                self.roots.push(self.allow.id());
            }
            self.card = Some(card);
            bottom = card.y - gap;
        }
        let body_top = top + BTN_H + gap;
        self.body = Rect { x: pad, y: body_top, width: w - 2.0 * pad, height: (bottom - body_top).max(0.0) };
        ui.rebuild_spatial_grid();
    }

    /// Act on the buttons clicked. Runs in the pointer event, so the
    /// keyboard button can raise the phone keyboard.
    pub fn drain(&mut self, ui: &mut UiContext) {
        let screen = with(|d| d.screen.clone().unwrap_or(Screen::List));
        if ui[self.back].take_click() {
            match screen {
                Screen::List => hide(),
                _ => go(Screen::List),
            }
        }
        if ui[self.kbd].take_click() {
            kb::toggle();
        }
        let tr = with(|d| d.tr.clone());
        if ui[self.act].take_click() {
            if let Screen::Session { id, .. } = &screen {
                if tr.as_ref().is_some_and(|t| t.busy) {
                    ask(serde_json::json!({"op": "stop", "id": id}), Asked::Action);
                } else if web_sys::window().unwrap().confirm_with_message("Archive this session?").unwrap_or(false) {
                    ask(serde_json::json!({"op": "archive", "id": id, "archived": true}), Asked::Action);
                    go(Screen::List);
                }
            }
        }
        if ui[self.send].take_click() {
            let draft = with(|d| d.draft.trim().to_string());
            if !draft.is_empty() {
                match &screen {
                    Screen::Session { id, .. } => ask(serde_json::json!({"op": "send", "id": id, "text": draft}), Asked::Action),
                    Screen::New { cwd, label } => {
                        ask(serde_json::json!({"op": "send", "cwd": cwd, "text": draft}), Asked::Action);
                        with(|d| d.note = Some((format!("Started a new session in {label}; it is listed once it has saved."), false)));
                        go(Screen::List);
                    }
                    Screen::List => {}
                }
                with(|d| d.draft.clear());
                self.stick_bottom = true;
            }
        }
        let front = tr.and_then(|t| t.pending.first().cloned());
        for (b, choice) in [(self.deny, "deny"), (self.always, "always"), (self.allow, "allow")] {
            if ui[b].take_click() {
                if let (Screen::Session { id, .. }, Some(p)) = (&screen, &front) {
                    ask(serde_json::json!({"op": "answer", "id": id, "request_id": p.request_id, "choice": choice}), Asked::Action);
                }
            }
        }
    }

    /// A press or release in the body: a tap on a row opens it.
    pub fn mouse(&mut self, pressed: bool, x: f32, y: f32) {
        if pressed {
            self.press = Some((x, y));
            return;
        }
        let Some((px, py)) = self.press.take() else { return };
        if (x - px).abs() > TAP_SLOP || (y - py).abs() > TAP_SLOP || !contains(self.body, x, y) {
            return;
        }
        let cy = y - content_top(self.body) + self.scroll;
        let hit = self.hits.iter().find(|(r, _)| cy >= r.y && cy < r.y + r.height && x >= self.body.x + r.x && x <= self.body.x + r.x + r.width).map(|(_, h)| h.clone());
        match hit {
            Some(Hit::Session(id, title)) => {
                self.scroll = 0.0;
                self.stick_bottom = true;
                go(Screen::Session { id, title });
            }
            Some(Hit::NewIn(cwd, label)) => go(Screen::New { cwd, label }),
            None => {}
        }
    }

    /// A drag (the page reads one as a wheel) scrolls the body.
    pub fn wheel(&mut self, dy: f32, x: f32, y: f32) -> bool {
        if !contains(self.body, x, y) {
            return false;
        }
        let max = (self.content_h - self.body.height).max(0.0);
        self.scroll = (self.scroll - dy).clamp(0.0, max);
        self.stick_bottom = self.scroll >= max - 2.0;
        true
    }

    pub fn paint(&mut self, ui: &UiContext, pc: &mut PaintCtx) {
        let (family, size) = font();
        let small = (size * 0.88).round();
        let cw = char_w(&family, size);
        let line_h = (size * 1.45).ceil();
        let (screen, note, readonly, listed, tr_rev, draft) =
            with(|d| (d.screen.clone().unwrap_or(Screen::List), d.note.clone(), d.readonly_screen(), d.listed, d.tr_rev, d.draft.clone()));
        let title = match &screen {
            Screen::List => "Code Agent".to_string(),
            Screen::Session { title, .. } => title.clone(),
            Screen::New { label, .. } => format!("New session in {label}"),
        };
        let t = self.title_rect;
        line(pc, &elide(&title, (t.width / cw) as usize), t.x, t.y, t.height, size, fg(), t);
        for id in self.roots.clone() {
            if let Some(w) = ui.get_widget(id) {
                cce_ui::widget::painter::paint_root_into(ui, w, pc);
            }
        }

        let b = self.body;
        let radius = cce_ui::layout::panel_corner_radius();
        pc.well_floor(b, radius, &Material::pane(), false);
        let inset = cce_ui::layout::plate_padding();
        let inner = Rect { x: b.x + inset, y: b.y + inset, width: b.width - 2.0 * inset, height: b.height - 2.0 * inset };
        let cols = (inner.width / cw).max(8.0) as usize;
        // Lines to draw, in content coordinates: (y, h, text, colour, x, size).
        let mut rows: Vec<(f32, f32, String, [u8; 3], f32, f32)> = Vec::new();
        let mut dots: Vec<(f32, f32, [f32; 4])> = Vec::new();
        let mut bubbles: Vec<(f32, f32)> = Vec::new();
        let mut hits = Vec::new();
        let mut y = 0.0;
        let text_block = |rows: &mut Vec<_>, y: &mut f32, text: &str, rgb: [u8; 3], x: f32, cols: usize, max: usize| {
            let all = wrap(text, cols);
            let n = all.len();
            for (i, l) in all.into_iter().enumerate() {
                if max > 0 && i == max {
                    rows.push((*y, line_h, format!("… {} more lines", n - max), dim(), x, small));
                    *y += line_h;
                    break;
                }
                rows.push((*y, line_h, l, rgb, x, size));
                *y += line_h;
            }
        };
        if let Some((n, err)) = &note {
            text_block(&mut rows, &mut y, n, if *err { [0xff, 0x8a, 0x80] } else { dim() }, 0.0, cols, 0);
            y += inset;
        }
        if readonly && !matches!(screen, Screen::New { .. }) {
            text_block(&mut rows, &mut y, "Read-only: no Code Agent window is open on the desktop.", dim(), 0.0, cols, 0);
            y += inset;
        }
        match &screen {
            Screen::List => {
                if !listed {
                    text_block(&mut rows, &mut y, "Loading…", dim(), 0.0, cols, 0);
                }
                let groups = with(|d| d.groups.clone());
                let writable = !with(|d| d.readonly);
                for g in &groups {
                    let shown: Vec<&Sess> = g.sessions.iter().filter(|s| !s.archived || s.state.is_some()).collect();
                    if shown.is_empty() {
                        continue;
                    }
                    rows.push((y, GROUP_H, g.label.clone(), fg(), 0.0, size));
                    if writable {
                        rows.push((y, GROUP_H, "+ new".into(), dim(), inner.width - 6.0 * cw, small));
                        hits.push((Rect { x: inner.width - 8.0 * cw + inset, y, width: 8.0 * cw, height: GROUP_H }, Hit::NewIn(g.cwd.clone(), g.label.clone())));
                    }
                    y += GROUP_H;
                    for s in shown {
                        let (right, color) = match &s.state {
                            Some(st) => {
                                let (word, c) = state_word(st);
                                dots.push((y + ROW_H / 2.0, 10.0, c));
                                (word.to_string(), [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8])
                            }
                            None if s.elsewhere.is_some() => ("elsewhere".into(), dim()),
                            None => (ago(s.active), dim()),
                        };
                        let rcols = right.chars().count() + 1;
                        let tcols = cols.saturating_sub(rcols + 3);
                        rows.push((y, ROW_H, elide(&s.title, tcols), fg(), 2.0 * cw, size));
                        rows.push((y, ROW_H, right.clone(), color, inner.width - rcols as f32 * cw, small));
                        hits.push((Rect { x: inset, y, width: inner.width - 8.0 * cw, height: ROW_H }, Hit::Session(s.id.clone(), s.title.clone())));
                        y += ROW_H;
                    }
                    y += inset;
                }
            }
            Screen::New { .. } => {
                text_block(&mut rows, &mut y, "Write the first prompt below, then Send.", dim(), 0.0, cols, 0);
            }
            Screen::Session { .. } => {
                let tr = with(|d| d.tr.clone());
                match tr {
                    None => text_block(&mut rows, &mut y, "Loading…", dim(), 0.0, cols, 0),
                    Some(tr) => {
                        for e in &tr.entries {
                            match e.kind.as_str() {
                                "user" => {
                                    let top = y;
                                    y += inset * 0.5;
                                    text_block(&mut rows, &mut y, &e.text, fg(), inset, cols.saturating_sub(2), 0);
                                    y += inset * 0.5;
                                    bubbles.push((top, y - top));
                                }
                                "assistant" => text_block(&mut rows, &mut y, if e.text.is_empty() { "…" } else { &e.text }, fg(), 0.0, cols, 0),
                                "thinking" => {
                                    let gist = e.text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").trim_matches(['*', '#', ' ']);
                                    rows.push((y, line_h, elide(&format!("Thinking  {gist}"), cols), dim(), 0.0, size));
                                    y += line_h;
                                }
                                "tool" => {
                                    let c = match e.state.as_str() {
                                        "running" => RUNNING,
                                        "failed" => FAILED,
                                        "stopped" => STOPPED,
                                        _ => DONE,
                                    };
                                    dots.push((y + line_h / 2.0, 0.0, c));
                                    rows.push((y, line_h, elide(&format!("{}  {}", e.name, e.summary), cols.saturating_sub(2)), fg(), 2.0 * cw, size));
                                    y += line_h;
                                    if e.state == "failed" && !e.output.trim().is_empty() {
                                        text_block(&mut rows, &mut y, e.output.trim(), dim(), 2.0 * cw, cols.saturating_sub(2), ERROR_LINES);
                                    }
                                }
                                _ => text_block(&mut rows, &mut y, &e.text, if e.error { [0xff, 0x8a, 0x80] } else { dim() }, 0.0, cols, 0),
                            }
                            y += inset;
                        }
                        if tr.busy {
                            rows.push((y, line_h, "working…".into(), dim(), 0.0, small));
                            y += line_h;
                        }
                    }
                }
            }
        }
        self.content_h = y + 2.0 * inset;
        self.hits = hits;
        let max = (self.content_h - b.height).max(0.0);
        if tr_rev != self.seen_rev {
            self.seen_rev = tr_rev;
            if self.stick_bottom && matches!(screen, Screen::Session { .. }) {
                self.scroll = max;
            }
        }
        self.scroll = self.scroll.clamp(0.0, max);
        let oy = inner.y - self.scroll;
        pc.clip_rounded(b, radius, |pc| {
            for (by, bh) in &bubbles {
                pc.rounded_rect(Rect { x: inner.x, y: oy + by, width: inner.width, height: *bh }, 8.0, (true, true, true, true), BUBBLE);
            }
            for (cy, dx, c) in &dots {
                pc.circle(inner.x + dx.max(cw * 0.6), oy + cy, 4.0, lin(*c));
            }
            for (ry, rh, text, rgb, rx, rsize) in &rows {
                let top = oy + ry;
                if top + rh < b.y || top > b.y + b.height {
                    continue;
                }
                line(pc, text, inner.x + rx, top, *rh, *rsize, *rgb, b);
            }
        });
        pc.well_rim(b, radius, cce_ui::layout::control_relief());

        // The waiting prompt's card.
        if let (Some(c), Some(p)) = (self.card, with(|d| d.tr.as_ref().and_then(|t| t.pending.first().cloned()))) {
            pc.plate(c, (radius, radius, radius, radius), &Material::pane(), cce_ui::layout::bevel_width());
            let cols = ((c.width - 2.0 * inset) / cw) as usize;
            let mut ly = c.y + inset;
            for l in wrap(&p.text, cols).into_iter().take(4) {
                line(pc, &l, c.x + inset, ly, line_h, size, fg(), c);
                ly += line_h;
            }
            if p.question {
                line(pc, "Answer it at the desk, or skip it.", c.x + inset, ly, line_h, small, dim(), c);
            }
            for bt in [self.deny, self.always, self.allow] {
                if let Some(w) = ui.get_widget(bt.id()) {
                    if w.rect().2 > 0.0 {
                        cce_ui::widget::painter::paint_root_into(ui, w, pc);
                    }
                }
            }
        }
        // The prompt strip: the draft, its last lines, and a caret.
        if let Some(s) = self.strip {
            pc.canvas_well(s, cce_ui::layout::control_corner_radius(), &Material::pane(), cce_ui::layout::control_relief(), false);
            let cols = ((s.width - 2.0 * inset) / cw) as usize;
            let all = wrap(&draft, cols);
            let shown = &all[all.len().saturating_sub(DRAFT_LINES)..];
            let mut ly = s.y + inset * 0.5;
            if draft.is_empty() {
                line(pc, "Message Claude…", s.x + inset, ly, line_h, size, dim(), s);
            }
            for l in shown {
                line(pc, l, s.x + inset, ly, line_h, size, fg(), s);
                ly += line_h;
            }
            if kb::is_up() {
                let last = shown.last().map_or(0, |l| l.chars().count());
                let cx = s.x + inset + last as f32 * cw + 1.0;
                let cy = s.y + inset * 0.5 + (shown.len().max(1) - 1) as f32 * line_h + line_h * 0.15;
                pc.quad(Rect { x: cx, y: cy, width: 2.0, height: line_h * 0.7 }, lin(WAITING));
            }
        }
    }
}

/// The face's character width (it is monospace).
fn char_w(family: &str, size: f32) -> f32 {
    (cce_ui::widget::display::measure_text_width("MMMMMMMMMM", family, size) / 10.0).max(4.0)
}

fn contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height
}
