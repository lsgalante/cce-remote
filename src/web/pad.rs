//! The trackpad: raw touches on the pad, turned into pointer frames.
//!
//! cce-ui's browser shell follows only the first finger and turns it into a
//! click, a scroll or a held drag — a touchscreen's reading. A trackpad needs
//! every finger and relative motion, so the pad reads the page's own touch
//! events, and keeps the matching pointer events from reaching cce-ui at all
//! (a capture-phase listener on the window stops them before the canvas's
//! own listeners run). Everything off the pad — the buttons, the open
//! switcher or menu — is cce-ui's as usual.
//!
//! - one finger drags the pointer (relative, ×`ACCEL`), coalesced on a 12ms
//!   timer so a fast drag is ~80 frames a second, not one per touch event;
//! - a short still tap clicks where the pointer IS (never at the finger),
//!   through `/click` so a click that opened a text field raises the
//!   keyboard (`net::click_opened_field`); two fingers right-click;
//! - a still finger held 350ms presses the button for a drag;
//! - two fingers scroll — or, in the live view, pinch-zoom and pan the
//!   picture (a transform of the view alone).
//!
//! Every touch on the canvas is `preventDefault`ed: a touch's defaults would
//! take the focus off the keyboard's textarea and drop the phone keyboard
//! mid-typing. Pointer events, which cce-ui reads, come before touch events
//! and are not cancelled by it.

use std::cell::RefCell;
use std::collections::HashMap;

use cce_ui::scene::layout::Rect;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{AddEventListenerOptions, HtmlCanvasElement, PointerEvent, TouchEvent};

use super::{kb, net, state};

/// Finger px → pointer px.
pub const ACCEL: f32 = 1.6;
/// Finger px → wheel units (~surface px): near 1:1 is the trackpad feel.
const SCROLL: f32 = 0.8;
/// A tap moves less than this (total finger travel, px) …
const TAP_SLOP: f32 = 10.0;
/// … and lifts within this.
const TAP_MS: f64 = 300.0;
/// A finger held this still for this long presses for a drag.
const HOLD_MS: i32 = 350;
const HOLD_SLOP: f32 = 8.0;
const FLUSH_MS: i32 = 12;
/// The live view's zoom range.
const MAX_ZOOM: f32 = 8.0;

#[derive(Default)]
struct Pad {
    /// The gesture in progress started on the pad.
    owned: bool,
    touches: HashMap<i32, (f32, f32)>,
    moved: f32,
    start_ms: f64,
    max_fingers: usize,
    dragging: bool,
    hold_timer: Option<i32>,
    pend: (f32, f32),
    pend_scroll: f32,
    flush_timer: Option<i32>,
    /// Two-finger pinch in the live view: (finger distance, centroid x, y).
    pinch: Option<(f32, f32, f32)>,
}

thread_local! {
    static PAD: RefCell<Pad> = RefCell::new(Pad::default());
    /// Pointer ids whose events the pad keeps from cce-ui.
    static OWNED_POINTERS: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

fn canvas() -> HtmlCanvasElement {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("app"))
        .and_then(|e| e.dyn_into().ok())
        .expect("the page has #app")
}

/// Client px → canvas px (the app's logical px).
fn local(cx: f32, cy: f32) -> (f32, f32) {
    let r = canvas().get_bounding_client_rect();
    (cx - r.left() as f32, cy - r.top() as f32)
}

fn inside(r: Rect, (x, y): (f32, f32)) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// Whether a touch landing at `p` is the pad's: on it, and not on the open
/// switcher or menu standing over it.
fn pad_owns(p: (f32, f32)) -> bool {
    state::with(|s| inside(s.pad, p) && !s.overlay_rect.is_some_and(|o| inside(o, p)))
}

pub fn install() {
    let canvas = canvas();
    let opts = AddEventListenerOptions::new();
    opts.set_passive(false);
    let add = |name: &str, f: fn(&TouchEvent)| {
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            e.prevent_default();
            f(&e);
        });
        let _ = canvas.add_event_listener_with_callback_and_add_event_listener_options(name, cb.as_ref().unchecked_ref(), &opts);
        cb.forget();
    };
    add("touchstart", touch_start);
    add("touchmove", touch_move);
    add("touchend", touch_end);
    add("touchcancel", touch_cancel);

    // Capture phase on the window: runs before the canvas's own (cce-ui's)
    // listeners, and stopping propagation here keeps them from running.
    let capture = AddEventListenerOptions::new();
    capture.set_capture(true);
    let window = web_sys::window().unwrap();
    for name in ["pointerdown", "pointermove", "pointerup", "pointercancel"] {
        let cb = Closure::<dyn FnMut(PointerEvent)>::new(move |e: PointerEvent| gate_pointer(&e));
        let _ = window.add_event_listener_with_callback_and_add_event_listener_options(name, cb.as_ref().unchecked_ref(), &capture);
        cb.forget();
    }
}

fn gate_pointer(e: &PointerEvent) {
    if e.pointer_type() != "touch" {
        return;
    }
    let id = e.pointer_id();
    let owned = match e.type_().as_str() {
        "pointerdown" => {
            let mine = pad_owns(local(e.client_x() as f32, e.client_y() as f32));
            if mine {
                OWNED_POINTERS.with(|o| o.borrow_mut().push(id));
            }
            mine
        }
        "pointerup" | "pointercancel" => OWNED_POINTERS.with(|o| {
            let mut o = o.borrow_mut();
            let had = o.contains(&id);
            o.retain(|&p| p != id);
            had
        }),
        _ => OWNED_POINTERS.with(|o| o.borrow().contains(&id)),
    };
    if owned {
        e.stop_propagation();
    }
}

fn touch_start(e: &TouchEvent) {
    let changed = e.changed_touches();
    let first = PAD.with(|p| p.borrow().touches.is_empty());
    if first {
        let Some(t) = changed.get(0) else { return };
        let at = local(t.client_x() as f32, t.client_y() as f32);
        // A touch on the pad beside an open panel dismisses it, and is a
        // trackpad touch as well.
        let closed = state::with(|s| {
            let beside = s.overlay.is_some() && inside(s.pad, at) && !s.overlay_rect.is_some_and(|o| inside(o, at));
            if beside {
                s.overlay = None;
            }
            beside
        });
        if closed {
            state::changed();
        }
        let owned = pad_owns(at);
        PAD.with(|p| p.borrow_mut().owned = owned);
    }
    if !PAD.with(|p| p.borrow().owned) {
        return;
    }
    let start_hold = PAD.with(|p| {
        let mut p = p.borrow_mut();
        for i in 0..changed.length() {
            if let Some(t) = changed.get(i) {
                p.touches.insert(t.identifier(), (t.client_x() as f32, t.client_y() as f32));
            }
        }
        p.max_fingers = p.max_fingers.max(p.touches.len());
        if p.touches.len() == 1 {
            p.moved = 0.0;
            p.start_ms = state::now_ms();
            p.pinch = None;
            true
        } else {
            if let Some(id) = p.hold_timer.take() {
                web_sys::window().unwrap().clear_timeout_with_handle(id);
            }
            false
        }
    });
    if start_hold {
        let cb = Closure::once_into_js(|| {
            let press = PAD.with(|p| {
                let mut p = p.borrow_mut();
                p.hold_timer = None;
                let press = p.moved < HOLD_SLOP && p.touches.len() == 1;
                if press {
                    p.dragging = true;
                }
                press
            });
            if press {
                net::send("b left down");
            }
        });
        let id = web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), HOLD_MS)
            .ok();
        PAD.with(|p| p.borrow_mut().hold_timer = id);
    }
}

fn touch_move(e: &TouchEvent) {
    if !PAD.with(|p| p.borrow().owned) {
        return;
    }
    let changed = e.changed_touches();
    let view = state::with(|s| s.view);
    let mut predict = None;
    let mut zoom_to = None;
    PAD.with(|p| {
        let mut p = p.borrow_mut();
        if p.touches.len() == 1 {
            let Some(t) = changed.get(0) else { return };
            let Some(&(px, py)) = p.touches.get(&t.identifier()) else { return };
            let (x, y) = (t.client_x() as f32, t.client_y() as f32);
            let (dx, dy) = (x - px, y - py);
            p.pend.0 += dx;
            p.pend.1 += dy;
            p.moved += dx.abs() + dy.abs();
            p.touches.insert(t.identifier(), (x, y));
            if view {
                predict = Some((dx * ACCEL, dy * ACCEL));
            }
        } else if p.touches.len() == 2 {
            let (mut dy, mut n) = (0.0, 0);
            for i in 0..changed.length() {
                let Some(t) = changed.get(i) else { continue };
                if let Some(&(_, py)) = p.touches.get(&t.identifier()) {
                    let (x, y) = (t.client_x() as f32, t.client_y() as f32);
                    dy += y - py;
                    n += 1;
                    p.touches.insert(t.identifier(), (x, y));
                }
            }
            if n > 0 {
                p.moved += dy.abs();
            }
            if view {
                let pts: Vec<_> = p.touches.values().copied().collect();
                let dist = ((pts[0].0 - pts[1].0).powi(2) + (pts[0].1 - pts[1].1).powi(2)).sqrt();
                let (cx, cy) = ((pts[0].0 + pts[1].0) / 2.0, (pts[0].1 + pts[1].1) / 2.0);
                if let Some(pinch) = p.pinch {
                    zoom_to = Some((pinch, (dist, cx, cy)));
                }
                p.pinch = Some((dist, cx, cy));
            } else if n > 0 {
                // Natural direction: content follows the fingers.
                p.pend_scroll += (-dy / n as f32) * SCROLL;
            }
        }
    });
    if let Some((dx, dy)) = predict {
        predict_marker(dx as f64, dy as f64);
    }
    if let Some((from, to)) = zoom_to {
        pinch_view(from, to);
    }
    queue_flush();
}

fn touch_end(e: &TouchEvent) {
    if !PAD.with(|p| p.borrow().owned) {
        return;
    }
    let changed = e.changed_touches();
    let gesture_over = PAD.with(|p| {
        let mut p = p.borrow_mut();
        for i in 0..changed.length() {
            if let Some(t) = changed.get(i) {
                p.touches.remove(&t.identifier());
            }
        }
        if p.touches.len() < 2 {
            p.pinch = None;
        }
        p.touches.is_empty()
    });
    if !gesture_over {
        return;
    }
    let (tap, fingers, dragging) = PAD.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(id) = p.hold_timer.take() {
            web_sys::window().unwrap().clear_timeout_with_handle(id);
        }
        let tap = !p.dragging && p.moved < TAP_SLOP && state::now_ms() - p.start_ms < TAP_MS;
        // A left tap's click goes over HTTP, which can overtake the input
        // socket: drop the tap's few px of jitter rather than let it land
        // after the click.
        if tap && p.max_fingers < 2 {
            p.pend = (0.0, 0.0);
        }
        let out = (tap, p.max_fingers, p.dragging);
        p.dragging = false;
        p.max_fingers = 0;
        p.owned = false;
        out
    });
    flush();
    if dragging {
        net::send("b left up");
    } else if tap {
        if fingers >= 2 {
            net::send("b right click");
        } else if net::click_opened_field() && !kb::is_up() {
            // Still inside the tap: the one place iOS lets a page raise
            // the keyboard.
            kb::open_for_field();
        }
    }
}

fn touch_cancel(_: &TouchEvent) {
    let dragging = PAD.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(id) = p.hold_timer.take() {
            web_sys::window().unwrap().clear_timeout_with_handle(id);
        }
        let d = p.dragging;
        *p = Pad::default();
        d
    });
    if dragging {
        net::send("b left up");
    }
}

fn queue_flush() {
    let due = PAD.with(|p| p.borrow().flush_timer.is_none());
    if !due {
        return;
    }
    let cb = Closure::once_into_js(flush);
    let id = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), FLUSH_MS)
        .ok();
    PAD.with(|p| p.borrow_mut().flush_timer = id);
}

fn flush() {
    let (pend, scroll) = PAD.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(id) = p.flush_timer.take() {
            web_sys::window().unwrap().clear_timeout_with_handle(id);
        }
        (std::mem::take(&mut p.pend), std::mem::take(&mut p.pend_scroll))
    });
    if pend != (0.0, 0.0) {
        net::send(&format!("m {:.1} {:.1}", pend.0 * ACCEL, pend.1 * ACCEL));
    }
    if scroll != 0.0 {
        net::send(&format!("s {scroll:.3} 0"));
    }
}

/// Move the cursor ring with the finger at once: the window stream is
/// damage-driven and does not repaint for a pointer move, so waiting for the
/// `pl` poll felt broken. The next `ploc` reconciles it.
fn predict_marker(dx: f64, dy: f64) {
    let moved = state::with(|s| {
        let (Some((x, y)), Some(r)) = (s.ptr, s.win_rect) else { return false };
        s.ptr = Some(((x + dx).clamp(r.x, r.x + r.w), (y + dy).clamp(r.y, r.y + r.h)));
        true
    });
    if moved {
        state::changed();
    }
}

/// Zoom about the pinch's centroid, then follow its drag; pan is clamped so
/// the picture always covers the pad, and a zoom back to 1 resets it.
fn pinch_view((d0, cx0, cy0): (f32, f32, f32), (d1, cx1, cy1): (f32, f32, f32)) {
    if d0 <= 0.0 {
        return;
    }
    let (ox, oy) = {
        let r = canvas().get_bounding_client_rect();
        (r.left() as f32, r.top() as f32)
    };
    state::with(|s| {
        let pad = s.pad;
        let nz = (s.zoom * (d1 / d0)).clamp(1.0, MAX_ZOOM);
        let applied = nz / s.zoom;
        // The centroid in pad px.
        let (px, py) = (cx1 - ox - pad.x, cy1 - oy - pad.y);
        let (mut panx, mut pany) = s.pan;
        panx = px - (px - panx) * applied + (cx1 - cx0);
        pany = py - (py - pany) * applied + (cy1 - cy0);
        s.zoom = nz;
        s.pan = clamp_pan(nz, (panx, pany), pad);
    });
    state::changed();
}

fn clamp_pan(zoom: f32, (x, y): (f32, f32), pad: Rect) -> (f32, f32) {
    if zoom <= 1.001 {
        return (0.0, 0.0);
    }
    (x.clamp(pad.width - pad.width * zoom, 0.0), y.clamp(pad.height - pad.height * zoom, 0.0))
}
