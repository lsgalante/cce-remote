//! The phone keyboard: a hidden `<textarea id="kb">` of the page's own, not
//! cce-ui's keyboard sink, because two iOS behaviours need handling the sink
//! does not do:
//!
//! - **Sentinel padding.** iOS never fires `deleteContentBackward` on an
//!   EMPTY field, so without something to delete, Backspace does nothing.
//!   The textarea holds `SENTINEL` with the caret at its end, kept intact by
//!   `preventDefault` on every `beforeinput` and re-armed on focus and by a
//!   1s drift repair (autocorrect ghosts).
//! - **`beforeinput`, not `keydown`**: iOS reports keyCode 229 for most keys.
//!
//! Each character becomes evdev keycodes on the input socket (a US layout,
//! Shift held around shifted ones), so typing reaches the desktop as key
//! presses, whatever app has focus.
//!
//! **Text mode**, while a Code Agent prompt is being written (`sync_mode`):
//! the textarea holds the draft itself, with autocorrect, capitalisation and
//! spellcheck on, and nothing is prevented. iOS edits it natively, so its
//! autocorrect, predictions, dictation and the space bar's trackpad (press
//! and hold, then slide the caret) all work; the page reads the value and
//! the selection back (`input`, `selectionchange`) and the Agent view draws
//! them. The sentinel's Backspace problem is moot there: an empty prompt has
//! nothing to delete. Lines break only at line breaks (`white-space: pre`
//! in index.html), so the trackpad's up and down move by them, not by the
//! hidden 1px-wide box's columns.
//!
//! The keyboard rises only from a focus made inside the user's own gesture:
//! the keyboard button and the type bar (cce-ui buttons, whose clicks run in
//! the `pointerup` handler) and a trackpad tap that opened a field
//! (`pad.rs`, in `touchend`). A field closing drops a keyboard a field
//! raised, never one opened with the keyboard button — as cce-compositor's
//! `osk.rs` leaves a board summoned by hand.

use std::cell::Cell;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlTextAreaElement, InputEvent};

use super::{agent, net, state};
use crate::predict::Edit;
use crate::wrap::{byte_to_utf16, utf16_to_byte};

thread_local! {
    /// The textarea holds a prompt's text (true) or the sentinel (false).
    static TEXT_MODE: Cell<bool> = const { Cell::new(false) };
}

const SENTINEL: &str = "········";

fn kb() -> HtmlTextAreaElement {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("kb"))
        .and_then(|e| e.dyn_into().ok())
        .expect("the page has #kb")
}

fn arm(kb: &HtmlTextAreaElement) {
    kb.set_value(SENTINEL);
    let end = SENTINEL.encode_utf16().count() as u32;
    let _ = kb.set_selection_range(end, end);
}

fn text_mode() -> bool {
    TEXT_MODE.with(Cell::get)
}

/// The textarea as the screen wants it: the prompt's text while one is
/// being written, else the sentinel. Called before every layout (so before
/// any tap that raises the keyboard), cheap when nothing changed. Leaving a
/// prompt drops the keyboard that was for it.
pub fn sync_mode() {
    let want = agent::composing();
    if want == text_mode() {
        return;
    }
    TEXT_MODE.with(|t| t.set(want));
    let kb = kb();
    let on = if want { "on" } else { "off" };
    let _ = kb.set_attribute("autocorrect", on);
    let _ = kb.set_attribute("autocomplete", on);
    let _ = kb.set_attribute("spellcheck", if want { "true" } else { "false" });
    let _ = kb.set_attribute("autocapitalize", if want { "sentences" } else { "off" });
    if want {
        show_draft(&kb);
    } else {
        if is_up() {
            let _ = kb.blur();
        }
        arm(&kb);
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
fn read_back(kb: &HtmlTextAreaElement) {
    let text = kb.value();
    let a = utf16_to_byte(&text, kb.selection_start().ok().flatten().unwrap_or(0));
    let b = utf16_to_byte(&text, kb.selection_end().ok().flatten().unwrap_or(0));
    agent::edited(text, a, b);
}

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
        let _ = kb.focus();
    }
}

/// Raise the keyboard if it is down, as the keyboard button would: called
/// inside a tap, the only place the phone lets a page raise it.
pub fn open() {
    if !is_up() {
        let _ = kb().focus();
    }
}

/// Raise the keyboard for a desktop field (the type bar, a tap that opened
/// one); a closing field may then drop it.
pub fn open_for_field() {
    if is_up() {
        return;
    }
    let _ = kb().focus();
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
    let k = kb.clone();
    let focus = Closure::<dyn FnMut()>::new(move || {
        if text_mode() {
            show_draft(&k);
        } else {
            arm(&k);
        }
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
    let input = Closure::<dyn FnMut(InputEvent)>::new(move |e: InputEvent| {
        // Text mode: the browser edits, `edited` reads it back.
        if text_mode() {
            return;
        }
        e.prevent_default();
        on_input(&e.input_type(), e.data().as_deref().unwrap_or(""));
        state::changed();
    });
    let k = kb.clone();
    let edited = Closure::<dyn FnMut()>::new(move || {
        if text_mode() {
            read_back(&k);
            state::changed();
        }
    });
    // The space bar's trackpad, a selection: the caret moves, nothing is typed.
    let k = kb.clone();
    let selected = Closure::<dyn FnMut()>::new(move || {
        if text_mode() && is_up() {
            read_back(&k);
            state::changed();
        }
    });
    let k = kb.clone();
    let drift = Closure::<dyn FnMut()>::new(move || {
        if is_up() && !text_mode() && k.value() != SENTINEL {
            arm(&k);
        }
        // A prediction the desktop never reflected gives way to its report.
        if state::with(|s| s.expire_prediction()) {
            state::changed();
        }
    });
    let _ = kb.add_event_listener_with_callback("focus", focus.as_ref().unchecked_ref());
    let _ = kb.add_event_listener_with_callback("blur", blur.as_ref().unchecked_ref());
    let _ = kb.add_event_listener_with_callback("beforeinput", input.as_ref().unchecked_ref());
    let _ = kb.add_event_listener_with_callback("input", edited.as_ref().unchecked_ref());
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        let _ = doc.add_event_listener_with_callback("selectionchange", selected.as_ref().unchecked_ref());
    }
    let _ = web_sys::window()
        .unwrap()
        .set_interval_with_callback_and_timeout_and_arguments_0(drift.as_ref().unchecked_ref(), 1000);
    // For the page's life.
    focus.forget();
    blur.forget();
    input.forget();
    edited.forget();
    selected.forget();
    drift.forget();
}

fn on_input(kind: &str, data: &str) {
    match kind {
        "insertText" | "insertReplacementText" => {
            for ch in data.chars() {
                type_char(ch);
            }
        }
        "insertLineBreak" => {
            // A new line, or the field's submit: not followed.
            predict(Edit::Opaque);
            net::send("k 28");
            state::with(|s| s.typed.clear());
        }
        "deleteContentBackward" => {
            predict(Edit::Backspace);
            net::send("k 14");
            state::with(|s| {
                s.typed.pop();
            });
        }
        "deleteWordBackward" => {
            // One Backspace per removed character, so the desktop and the
            // echo stay in step.
            let n = state::with(|s| {
                let rest = word_back(&s.typed).to_string();
                let n = (s.typed.chars().count() - rest.chars().count()).max(1);
                s.typed = rest;
                n
            });
            for _ in 0..n {
                predict(Edit::Backspace);
                net::send("k 14");
            }
        }
        "deleteContentForward" => {
            predict(Edit::Delete);
            net::send("k 111");
        }
        _ => {}
    }
}

/// `typed` without its last word and the spaces after it.
fn word_back(typed: &str) -> &str {
    let t = typed.trim_end();
    if t.is_empty() {
        // Only spaces: no word to take (the page sends one Backspace).
        return typed;
    }
    let cut = t.rfind(char::is_whitespace).map_or(0, |i| i + t[i..].chars().next().map_or(1, char::len_utf8));
    &typed[..cut.min(t.len())]
}

/// Show `edit` in the strip before the desktop reports it. With a modifier
/// latched a key is a shortcut, not text: the prediction cannot follow it.
pub fn predict(edit: Edit) {
    state::with(|s| {
        let edit = if s.mods.is_empty() { edit } else { Edit::Opaque };
        s.predict(edit);
    });
}

fn type_char(ch: char) {
    let Some((code, shift)) = keycode(ch) else { return };
    predict(Edit::Insert(ch));
    if shift {
        net::send("kd 42");
        net::send(&format!("k {code}"));
        net::send("ku 42");
    } else {
        net::send(&format!("k {code}"));
    }
    state::with(|s| s.typed.push(ch));
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
