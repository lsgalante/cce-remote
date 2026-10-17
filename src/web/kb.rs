//! The phone keyboard: a hidden `<textarea id="kb">` of the page's own, not
//! cce-ui's keyboard sink. iOS edits it natively, and the page reads each
//! edit back, in one of two modes (`sync_mode`):
//!
//! - **The prompt** (`TEXT_MODE`), while a Code Agent prompt is being
//!   written: the textarea holds the draft itself, and the Agent view draws
//!   the draft, selection and caret it reads back (`agent::edited`).
//! - **The desktop's mirror**, everywhere else: the textarea holds a copy
//!   of the desktop field around its caret — the text its app reports
//!   (`tt`), or what was typed here when it reports none — and each edit is
//!   replayed on the desktop as evdev keycodes (`crate::mirror`: arrows to
//!   the change, Backspace over what it replaced, the new characters; a US
//!   layout, Shift held around shifted ones).
//!
//! Either way the edits are iOS's own, so autocorrect, predictions,
//! dictation and the space bar's press-and-hold trackpad work: each needs
//! real text in a field nothing prevents. In the mirror autocorrect and
//! capitalisation are on only for a field that reports its contents: one
//! that reports none is a terminal, near enough, where "ls" corrected is
//! "Is". iOS reads those settings as the keyboard comes up, so they are set
//! before every focus (`prepare`), and a tap that opens a field brings the
//! contents with its `/click` answer.
//!
//! The mirror keeps its own idea of the desktop's field: the report it was
//! built from with every key sent since applied (`expect`). A new report
//! that disagrees, once the keys have been quiet a moment, is the truth —
//! the app did something else, or the caret moved on the desktop — and the
//! copy is rebuilt from it.
//!
//! **Padding.** iOS never fires a delete on an EMPTY field, so without
//! something to delete, Backspace would do nothing at a field's start (or
//! in a terminal, whose text the page never sees). The copy sits after
//! `PAD` line breaks, standing for the unknown text before it; deleting one
//! is a Backspace on the desktop, and the padding is topped up as it runs
//! low. Line breaks, so a field's first word still starts a "paragraph" for
//! capitalisation. Lines break only at line breaks (`white-space: pre` in
//! index.html), so the trackpad's up and down move by them, not by the
//! hidden 1px-wide box's columns.
//!
//! The keyboard rises only from a focus made inside the user's own gesture:
//! the keyboard button and the type bar (cce-ui buttons, whose clicks run in
//! the `pointerup` handler) and a trackpad tap that opened a field
//! (`pad.rs`, in `touchend`). A field closing drops a keyboard a field
//! raised, never one opened with the keyboard button — as cce-compositor's
//! `osk.rs` leaves a board summoned by hand.

use std::cell::{Cell, RefCell};

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlTextAreaElement;

use super::{agent, net, state};
use crate::mirror;
use crate::predict::{self, Edit, Field};
use crate::wrap::{byte_to_utf16, utf16_to_byte};

/// Line breaks before the copy: what Backspace deletes past its start.
const PAD: usize = 8;
/// Topped up when the caret comes this close to the value's start.
const PAD_LOW: usize = 2;
/// How long after the last key a disagreeing report is taken as the truth:
/// before it, the keys may simply not have landed yet.
const QUIET_MS: f64 = 400.0;

thread_local! {
    /// The textarea holds a prompt's text (true) or the desktop's mirror.
    static TEXT_MODE: Cell<bool> = const { Cell::new(false) };
    static MIRROR: RefCell<Mirror> = RefCell::new(Mirror::default());
}

#[derive(Default)]
struct Mirror {
    /// The textarea's value as the mirror last left it.
    value: String,
    /// The desktop's caret, in chars of `value`.
    desk: usize,
    /// The desktop's selection, in chars, if the report had one: collapsed
    /// by the first key.
    sel: Option<(usize, usize)>,
    /// What the desktop's field should hold: the report the copy was built
    /// from, with every key since applied. `None`: unknown (no report, or a
    /// key the model cannot follow).
    expect: Option<Field>,
    /// The report count (`state::field_seq`) last compared.
    seen: u64,
    /// When a key last went out.
    sent_at: f64,
}

fn kb() -> HtmlTextAreaElement {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("kb"))
        .and_then(|e| e.dyn_into().ok())
        .expect("the page has #kb")
}

fn text_mode() -> bool {
    TEXT_MODE.with(Cell::get)
}

fn chars_to(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}

fn char_byte(text: &str, ch: usize) -> usize {
    text.char_indices().nth(ch).map_or(text.len(), |(b, _)| b)
}

/// The textarea's selection, in chars of its value.
fn selection(kb: &HtmlTextAreaElement, value: &str) -> (usize, usize) {
    let at = |u: Option<u32>| chars_to(value, utf16_to_byte(value, u.unwrap_or(0)));
    (at(kb.selection_start().ok().flatten()), at(kb.selection_end().ok().flatten()))
}

fn select(kb: &HtmlTextAreaElement, value: &str, a: usize, b: usize) {
    let u = |c: usize| byte_to_utf16(value, char_byte(value, c));
    let _ = kb.set_selection_range(u(a), u(b));
}

/// Autocorrect, capitalisation and spellcheck on or off. iOS reads them as
/// the keyboard comes up.
fn set_traits(kb: &HtmlTextAreaElement, on: bool) {
    let v = if on { "on" } else { "off" };
    let _ = kb.set_attribute("autocorrect", v);
    let _ = kb.set_attribute("autocomplete", v);
    let _ = kb.set_attribute("spellcheck", if on { "true" } else { "false" });
    let _ = kb.set_attribute("autocapitalize", if on { "sentences" } else { "off" });
}

/// The textarea as the keyboard is about to come up: its mode's text and
/// traits. Called before every focus, while iOS still reads them.
fn prepare(kb: &HtmlTextAreaElement) {
    sync_mode();
    if text_mode() {
        set_traits(kb, true);
        show_draft(kb);
    } else {
        rebuild(kb);
    }
}

/// The textarea as the screen wants it: the prompt's text while one is
/// being written, else the desktop's mirror. Called before every layout (so
/// before any tap that raises the keyboard), cheap when nothing changed.
/// Leaving a prompt drops the keyboard that was for it.
pub fn sync_mode() {
    let want = agent::composing();
    if want == text_mode() {
        return;
    }
    TEXT_MODE.with(|t| t.set(want));
    let kb = kb();
    if want {
        set_traits(&kb, true);
        show_draft(&kb);
    } else {
        if is_up() {
            let _ = kb.blur();
        }
        rebuild(&kb);
    }
}

/// The draft and its selection into the textarea.
fn show_draft(kb: &HtmlTextAreaElement) {
    let (draft, a, b) = agent::draft();
    if kb.value() != draft {
        kb.set_value(&draft);
    }
    let _ = kb.set_selection_range(byte_to_utf16(&draft, a), byte_to_utf16(&draft, b));
}

/// The draft changed outside the keyboard (Send cleared it, a tap placed
/// the caret): the textarea follows.
pub fn draft_changed() {
    if text_mode() {
        show_draft(&kb());
    }
}

/// The textarea's text and selection, as bytes, into the draft.
fn read_draft(kb: &HtmlTextAreaElement) {
    let text = kb.value();
    let a = utf16_to_byte(&text, kb.selection_start().ok().flatten().unwrap_or(0));
    let b = utf16_to_byte(&text, kb.selection_end().ok().flatten().unwrap_or(0));
    agent::edited(text, a, b);
}

// ── The desktop's mirror.

/// The copy built afresh: the field as its app last reported it, or
/// nothing (a field that reports none, or no field), after the padding.
fn rebuild(kb: &HtmlTextAreaElement) {
    let (report, seq) = state::with(|s| (s.field_text.clone(), s.field_seq));
    let pad = "\n".repeat(PAD);
    let (value, desk, sel) = match &report {
        Some((text, cursor, anchor)) => {
            let (c, a) = (PAD + chars_to(text, *cursor), PAD + chars_to(text, *anchor));
            (format!("{pad}{text}"), c, (c != a).then_some((a.min(c), a.max(c))))
        }
        None => (pad, PAD, None),
    };
    set_traits(kb, report.is_some());
    kb.set_value(&value);
    match sel {
        Some((lo, hi)) => select(kb, &value, lo, hi),
        None => select(kb, &value, desk, desk),
    }
    MIRROR.with(|m| {
        *m.borrow_mut() = Mirror { value, desk, sel, expect: report, seen: seq, sent_at: 0.0 };
    });
    echo();
}

/// The strip's local echo, for a field that reports nothing: the copy's
/// line up to the caret.
fn echo() {
    let line = MIRROR.with(|m| {
        let m = m.borrow();
        let at = char_byte(&m.value, m.desk);
        let start = m.value[..at].rfind('\n').map_or(0, |i| i + 1);
        m.value[start..at].to_string()
    });
    state::with(|s| s.typed = line);
}

/// Send one key of a plan, with the strip's prediction and the mirror's
/// expectation of it. Whether the mirror can still follow the field.
fn send_key(edit: Edit) -> bool {
    let mods = state::with(|s| !s.mods.is_empty());
    let follow = match edit {
        Edit::Left => {
            predict(Edit::Left);
            net::send("k 105");
            true
        }
        Edit::Right => {
            predict(Edit::Right);
            net::send("k 106");
            true
        }
        Edit::Backspace => {
            predict(Edit::Backspace);
            net::send("k 14");
            true
        }
        Edit::Insert('\n') => {
            // A new line, or the field's submit: not followed.
            predict(Edit::Opaque);
            net::send("k 28");
            false
        }
        Edit::Insert('\t') => {
            predict(Edit::Opaque);
            net::send("k 15");
            false
        }
        Edit::Insert(ch) => type_char(ch),
        Edit::Delete | Edit::Opaque => false,
    };
    // With a modifier latched a key is a shortcut, not an edit.
    let follow = follow && !mods;
    MIRROR.with(|m| {
        let mut m = m.borrow_mut();
        m.sent_at = state::now_ms();
        m.expect = m.expect.take().filter(|_| follow).and_then(|mut f| predict::apply(&mut f, edit).then_some(f));
    });
    follow
}

/// The textarea changed (an edit) or its caret moved: replay it on the
/// desktop. `edited` is false for a caret move alone, which a selection
/// does not make (the desktop is not given one; a later edit is replayed
/// over it from the caret).
fn mirror_changed(kb: &HtmlTextAreaElement, edited: bool) {
    let new = kb.value();
    let (a, b) = selection(kb, &new);
    let (old, desk, sel) = MIRROR.with(|m| {
        let m = m.borrow();
        (m.value.clone(), m.desk, m.sel)
    });
    if !edited && (new != old || a != b || a == desk) {
        return;
    }
    let plan = mirror::plan(&old, desk, sel, &new, (a == b).then_some(a));
    let mut follow = true;
    for k in &plan.keys {
        follow &= send_key(*k);
    }
    // Enter, Tab or a shortcut: where the field is now is the desktop's to
    // say. A field that reports its contents will; one that does not (a
    // terminal, whose Enter runs the line) starts over empty.
    if !follow {
        let reports = state::with(|s| s.field_text.is_some());
        if !reports {
            rebuild(kb);
            return;
        }
    }
    let (mut value, mut desk) = (plan.text, plan.desk);
    if desk < PAD_LOW {
        value = format!("{}{value}", "\n".repeat(PAD));
        desk += PAD;
    }
    if value != new || a != b || a != desk {
        kb.set_value(&value);
        select(kb, &value, desk, desk);
    }
    MIRROR.with(|m| {
        let mut m = m.borrow_mut();
        m.value = value;
        m.desk = desk;
        m.sel = None;
    });
    echo();
}

/// A report from the desktop (`tt`), or the timer: a new one that
/// disagrees with what the mirror expects is the truth, once the keys have
/// been quiet a moment.
pub fn reported() {
    if text_mode() || !is_up() {
        return;
    }
    let (report, seq) = state::with(|s| (s.field_text.clone(), s.field_seq));
    let stale = MIRROR.with(|m| {
        let mut m = m.borrow_mut();
        if m.seen == seq || state::now_ms() - m.sent_at < QUIET_MS {
            return false;
        }
        m.seen = seq;
        m.expect != report
    });
    if stale {
        rebuild(&kb());
    }
}

/// A key button (Esc, Tab, the arrows): sent as it was, with the mirror's
/// caret following Left and Right; the rest it cannot follow.
pub fn key_button(code: u32) {
    let edit = match code {
        105 => Edit::Left,
        106 => Edit::Right,
        _ => Edit::Opaque,
    };
    predict(edit);
    net::send(&format!("k {code}"));
    if text_mode() {
        return;
    }
    let follow = matches!(edit, Edit::Left | Edit::Right) && state::with(|s| s.mods.is_empty());
    let kb = kb();
    let (value, desk) = MIRROR.with(|m| {
        let mut m = m.borrow_mut();
        m.sent_at = state::now_ms();
        m.expect = m.expect.take().filter(|_| follow).and_then(|mut f| predict::apply(&mut f, edit).then_some(f));
        if follow {
            let len = m.value.chars().count();
            m.desk = if edit == Edit::Left { m.desk.saturating_sub(1) } else { (m.desk + 1).min(len) };
            m.sel = None;
        }
        (m.value.clone(), m.desk)
    });
    if follow {
        select(&kb, &value, desk, desk);
        echo();
    } else if state::with(|s| s.field_text.is_none()) {
        rebuild(&kb);
    }
}

// ── The keyboard itself.

pub fn is_up() -> bool {
    let active = web_sys::window().and_then(|w| w.document()).and_then(|d| d.active_element());
    active.is_some_and(|a| a == **kb())
}

/// The keyboard button.
pub fn toggle() {
    let kb = kb();
    if is_up() {
        let _ = kb.blur();
    } else {
        prepare(&kb);
        let _ = kb.focus();
    }
}

/// Raise the keyboard if it is down, as the keyboard button would: called
/// inside a tap, the only place the phone lets a page raise it.
pub fn open() {
    if !is_up() {
        let kb = kb();
        prepare(&kb);
        let _ = kb.focus();
    }
}

/// Raise the keyboard for a desktop field (the type bar, a tap that opened
/// one); a closing field may then drop it.
pub fn open_for_field() {
    if is_up() {
        return;
    }
    let kb = kb();
    prepare(&kb);
    let _ = kb.focus();
    state::with(|s| s.kb_by_field = true);
    state::changed();
}

/// `ti 1` / `ti 0`.
pub fn set_field(open: bool) {
    let drop = state::with(|s| {
        s.field_open = open;
        !open && s.kb_by_field && s.kb_up
    });
    if drop {
        let _ = kb().blur();
    }
    state::changed();
}

pub fn install() {
    let kb = kb();
    let focus = Closure::<dyn FnMut()>::new(move || {
        state::with(|s| s.kb_up = true);
        state::changed();
    });
    let blur = Closure::<dyn FnMut()>::new(move || {
        state::with(|s| {
            s.kb_up = false;
            s.kb_by_field = false;
            s.typed.clear();
        });
        state::changed();
    });
    let k = kb.clone();
    let input = Closure::<dyn FnMut()>::new(move || {
        if text_mode() {
            read_draft(&k);
        } else {
            mirror_changed(&k, true);
        }
        state::changed();
    });
    // The space bar's trackpad, or a selection: the caret moves, nothing is
    // typed.
    let k = kb.clone();
    let selected = Closure::<dyn FnMut()>::new(move || {
        if !is_up() {
            return;
        }
        if text_mode() {
            read_draft(&k);
        } else {
            mirror_changed(&k, false);
        }
        state::changed();
    });
    let tick = Closure::<dyn FnMut()>::new(move || {
        reported();
        // A prediction the desktop never reflected gives way to its report.
        if state::with(|s| s.expire_prediction()) {
            state::changed();
        }
    });
    let _ = kb.add_event_listener_with_callback("focus", focus.as_ref().unchecked_ref());
    let _ = kb.add_event_listener_with_callback("blur", blur.as_ref().unchecked_ref());
    let _ = kb.add_event_listener_with_callback("input", input.as_ref().unchecked_ref());
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        let _ = doc.add_event_listener_with_callback("selectionchange", selected.as_ref().unchecked_ref());
    }
    let _ = web_sys::window()
        .unwrap()
        .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 250);
    // For the page's life.
    focus.forget();
    blur.forget();
    input.forget();
    selected.forget();
    tick.forget();
    rebuild(&kb);
}

/// Show `edit` in the strip before the desktop reports it. With a modifier
/// latched a key is a shortcut, not text: the prediction cannot follow it.
fn predict(edit: Edit) {
    state::with(|s| {
        let edit = if s.mods.is_empty() { edit } else { Edit::Opaque };
        s.predict(edit);
    });
}

/// Type one character; whether it was one the keys can type.
fn type_char(ch: char) -> bool {
    let Some((code, shift)) = keycode(ch) else { return false };
    predict(Edit::Insert(ch));
    if shift {
        net::send("kd 42");
        net::send(&format!("k {code}"));
        net::send("ku 42");
    } else {
        net::send(&format!("k {code}"));
    }
    true
}

/// A character's evdev keycode on a US layout, and whether it needs Shift.
fn keycode(ch: char) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    if ch.is_ascii_lowercase() {
        return Some((LETTERS[(ch as u8 - b'a') as usize], false));
    }
    if ch.is_ascii_uppercase() {
        return Some((LETTERS[(ch as u8 - b'A') as usize], true));
    }
    if ch.is_ascii_digit() {
        return Some((if ch == '0' { 11 } else { 1 + (ch as u8 - b'0') as u32 }, false));
    }
    Some(match ch {
        ' ' => (57, false),
        '-' => (12, false),
        '=' => (13, false),
        '[' => (26, false),
        ']' => (27, false),
        '\\' => (43, false),
        ';' => (39, false),
        '\'' => (40, false),
        '`' => (41, false),
        ',' => (51, false),
        '.' => (52, false),
        '/' => (53, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '_' => (12, true),
        '+' => (13, true),
        '{' => (26, true),
        '}' => (27, true),
        '|' => (43, true),
        ':' => (39, true),
        '"' => (40, true),
        '~' => (41, true),
        '<' => (51, true),
        '>' => (52, true),
        '?' => (53, true),
        _ => return None,
    })
}
