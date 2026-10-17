//! The page's two sockets and its one request, all reached through web-sys
//! (cce-ui has no networking).
//!
//! - **`/ws`**, the input socket: `auth <pin>` first, then input frames out
//!   and `auth ok` / `ver` / `ti` / `windows` / `ploc` in. Reconnects a
//!   second after any close.
//! - **`/wstream`**, the live view: JPEG frames in, decoded by the browser
//!   (`createImageBitmap`, then read back through an `OffscreenCanvas`) into
//!   RGBA for the frame texture. **Ack-clocked**: the server sends the next
//!   frame only after an `n`, which the app sends once it has PAINTED one
//!   ([`ack_painted`]), so at most one frame is ever in flight and the
//!   server's adaptation times decode and paint as well as the network.
//! - **`POST /click`**, synchronous, from inside the tap that asked for it:
//!   see [`click_opened_field`].

use std::cell::{Cell, RefCell};

use js_sys::{Array, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    BinaryType, Blob, BlobPropertyBag, CloseEvent, ImageBitmap, MessageEvent, OffscreenCanvas,
    OffscreenCanvasRenderingContext2d, WebSocket, XmlHttpRequest,
};

use super::state::{self, Conn, Win, WinRect};

/// A socket and the callbacks it holds, dropped together when replaced.
struct Sock {
    ws: WebSocket,
    _open: Closure<dyn FnMut()>,
    _msg: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
}

impl Drop for Sock {
    /// Unhook first: a socket closed on replacement still fires `close`
    /// later, and a dropped closure called from JS throws.
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
    }
}

/// An interval's handle and the callback it runs.
type Timer = (i32, Closure<dyn FnMut()>);

thread_local! {
    static INPUT: RefCell<Option<Sock>> = const { RefCell::new(None) };
    static STREAM: RefCell<Option<Sock>> = const { RefCell::new(None) };
    /// Bumped per stream connection, so a frame decoded for an old one is dropped.
    static STREAM_GEN: Cell<u32> = const { Cell::new(0) };
    static DECODER: RefCell<Option<(OffscreenCanvas, OffscreenCanvasRenderingContext2d)>> = const { RefCell::new(None) };
    /// The live view's timers: the `wl` / `pl` polls and the stream watchdog.
    static VIEW_TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
}

fn url(path: &str) -> String {
    let loc = web_sys::window().unwrap().location();
    let scheme = if loc.protocol().is_ok_and(|p| p == "https:") { "wss" } else { "ws" };
    format!("{scheme}://{}{path}", loc.host().unwrap_or_default())
}

fn set_timeout(ms: i32, f: impl FnOnce() + 'static) {
    let cb = Closure::once_into_js(f);
    let _ = web_sys::window().unwrap().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), ms);
}

// ── The input socket ────────────────────────────────────────────────────

pub fn connect() {
    let ws = match WebSocket::new(&url("/ws")) {
        Ok(ws) => ws,
        Err(_) => {
            set_timeout(1000, connect);
            return;
        }
    };
    let w = ws.clone();
    let open = Closure::<dyn FnMut()>::new(move || {
        let pin = state::with(|s| s.pin.clone());
        let _ = w.send_with_str(&format!("auth {pin}"));
    });
    let msg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(text) = e.data().as_string() {
            on_input_message(&text);
        }
    });
    let close = Closure::<dyn FnMut(CloseEvent)>::new(move |_| {
        state::with(|s| {
            s.conn = Conn::Off;
            // The reconnect repeats `ti 1` if the field is still open.
            s.field_open = false;
            s.field_text = None;
        });
        state::changed();
        set_timeout(1000, connect);
    });
    ws.set_onopen(Some(open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(msg.as_ref().unchecked_ref()));
    ws.set_onclose(Some(close.as_ref().unchecked_ref()));
    INPUT.with(|i| *i.borrow_mut() = Some(Sock { ws, _open: open, _msg: msg, _close: close }));
}

fn on_input_message(text: &str) {
    if text == "auth ok" {
        state::with(|s| s.conn = Conn::Paired);
    } else if text == "auth fail" {
        // The loader asks for the PIN when it has none: forget this one and
        // start over there.
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.remove_item("cce-remote-pin");
        }
        let _ = web_sys::window().unwrap().location().reload();
        return;
    } else if let Some(v) = text.strip_prefix("ver ") {
        // The first pairing pins the version; a different one on a later
        // reconnect means the service restarted onto new code.
        let reload = state::with(|s| match &s.page_ver {
            None => {
                s.page_ver = Some(v.to_string());
                false
            }
            Some(old) => old != v,
        });
        if reload {
            let _ = web_sys::window().unwrap().location().reload();
        }
        return;
    } else if text == "ti 1" || text == "ti 0" {
        super::kb::set_field(text == "ti 1");
        return;
    } else if let Some(rest) = text.strip_prefix("tt") {
        state::with(|s| s.field_text = parse_contents(rest));
    } else if let Some(json) = text.strip_prefix("windows ") {
        if !on_windows(json) {
            return;
        }
    } else if let Some(xy) = text.strip_prefix("ploc ") {
        // Polled 8x a second in the live view: repaint only for a move.
        let mut it = xy.split(' ').filter_map(|n| n.parse::<f64>().ok());
        let (Some(x), Some(y)) = (it.next(), it.next()) else { return };
        if !state::with(|s| s.ptr.replace((x, y)) != Some((x, y))) {
            return;
        }
    } else {
        return;
    }
    state::changed();
}

/// A `tt` frame after its verb: ` <cursor> <anchor> <text>` (byte offsets;
/// the text is the rest of the frame), or nothing for no contents. Offsets
/// off a char boundary or past the end are refused rather than trusted.
fn parse_contents(rest: &str) -> Option<(String, usize, usize)> {
    let mut it = rest.strip_prefix(' ')?.splitn(3, ' ');
    let cursor: usize = it.next()?.parse().ok()?;
    let anchor: usize = it.next()?.parse().ok()?;
    let text = it.next().unwrap_or("").to_string();
    (text.is_char_boundary(cursor) && text.is_char_boundary(anchor)).then_some((text, cursor, anchor))
}

/// A `windows [...]` reply: refresh the focused window's rect (the cursor
/// ring's frame of reference) and, if the switcher asked, open it. Whether
/// anything the page shows changed (the live view polls this every 2s).
fn on_windows(json: &str) -> bool {
    let Ok(list) = js_sys::JSON::parse(json) else { return false };
    let get = |o: &JsValue, k: &str| Reflect::get(o, &JsValue::from_str(k)).unwrap_or(JsValue::UNDEFINED);
    let mut wins = Vec::new();
    let mut rect = None;
    for o in Array::from(&list).iter() {
        let mode = get(&o, "mode").as_string().unwrap_or_default();
        let focused = get(&o, "focused").as_bool().unwrap_or(false);
        if mode == "Status" {
            continue;
        }
        let num = |k| get(&o, k).as_f64().unwrap_or(0.0);
        if focused {
            rect = Some(WinRect { x: num("x"), y: num("y"), w: num("w"), h: num("h") });
        }
        if get(&o, "has_parent").as_bool().unwrap_or(false) || get(&o, "minimized").as_bool().unwrap_or(false) {
            continue;
        }
        wins.push(Win {
            id: num("id") as u64,
            title: get(&o, "title").as_string().unwrap_or_default(),
            app_id: get(&o, "app_id").as_string().unwrap_or_default(),
            focused,
        });
    }
    state::with(|s| {
        let mut changed = false;
        if rect.is_some() && rect != s.win_rect {
            s.win_rect = rect;
            changed = true;
        }
        if s.want_switcher {
            s.want_switcher = false;
            s.windows = wins;
            s.overlay = Some(state::Overlay::Switcher);
            changed = true;
        }
        changed
    })
}

/// Send an input frame, if paired.
pub fn send(frame: &str) {
    if state::with(|s| s.conn != Conn::Paired) {
        return;
    }
    INPUT.with(|i| {
        if let Some(sock) = i.borrow().as_ref() {
            if sock.ws.ready_state() == WebSocket::OPEN {
                let _ = sock.ws.send_with_str(frame);
            }
        }
    });
}

/// Left-click, and say whether that click opened a desktop text field.
///
/// A SYNCHRONOUS `POST /click`: the server clicks, waits up to 150ms for a
/// field to activate, and answers `1`/`0`. It has to block, because the
/// caller is the tap's own event handler, and iOS raises the keyboard only
/// for a focus made inside one — a WS reply arrives after the tap is over.
/// Not paired: no click (as the input socket would send none).
pub fn click_opened_field() -> bool {
    let pin = state::with(|s| (s.conn == Conn::Paired).then(|| s.pin.clone()));
    let Some(pin) = pin else { return false };
    let Ok(x) = XmlHttpRequest::new() else { return false };
    if x.open_with_async("POST", "/click", false).is_err() || x.set_request_header("X-Pin", &pin).is_err() || x.send().is_err() {
        return false;
    }
    x.status().is_ok_and(|s| s == 200) && x.response_text().ok().flatten().is_some_and(|t| t == "1")
}

// ── The live view ───────────────────────────────────────────────────────

/// Open (or reopen) the frame stream.
pub fn stream_start() {
    let gen = STREAM_GEN.with(|g| {
        g.set(g.get().wrapping_add(1));
        g.get()
    });
    let Ok(ws) = WebSocket::new(&url("/wstream")) else { return };
    ws.set_binary_type(BinaryType::Arraybuffer);
    state::with(|s| s.last_frame_ms = state::now_ms());
    let paired = std::rc::Rc::new(Cell::new(false));
    let w = ws.clone();
    let open = Closure::<dyn FnMut()>::new(move || {
        let pin = state::with(|s| s.pin.clone());
        let _ = w.send_with_str(&format!("auth {pin}"));
    });
    let w = ws.clone();
    let p = paired.clone();
    let msg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Some(text) = e.data().as_string() {
            if text == "auth ok" {
                p.set(true);
                let _ = w.send_with_str("n");
            }
            return;
        }
        if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
            state::with(|s| s.last_frame_ms = state::now_ms());
            wasm_bindgen_futures::spawn_local(decode(buf, gen));
        }
    });
    let close = Closure::<dyn FnMut(CloseEvent)>::new(move |_| {
        // Reconnect only a connection that paired: a stale PIN would
        // otherwise retry-loop failures into the server's rate limiter and
        // lock this phone's address out of the input socket too.
        let current = STREAM_GEN.with(|g| g.get()) == gen;
        if current && paired.get() && state::with(|s| s.view) {
            set_timeout(600, stream_start);
        }
    });
    ws.set_onopen(Some(open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(msg.as_ref().unchecked_ref()));
    ws.set_onclose(Some(close.as_ref().unchecked_ref()));
    let old = STREAM.with(|st| st.borrow_mut().replace(Sock { ws, _open: open, _msg: msg, _close: close }));
    if let Some(old) = old {
        let _ = old.ws.close();
    }
}

pub fn stream_stop() {
    STREAM_GEN.with(|g| g.set(g.get().wrapping_add(1)));
    if let Some(old) = STREAM.with(|st| st.borrow_mut().take()) {
        let _ = old.ws.close();
    }
}

/// The app painted frame `seq`: ask for the next.
pub fn ack_painted(seq: u64) {
    let due = state::with(|s| {
        let due = seq > s.acked_seq;
        s.acked_seq = s.acked_seq.max(seq);
        due
    });
    if due {
        send_ack();
    }
}

fn send_ack() {
    STREAM.with(|st| {
        if let Some(sock) = st.borrow().as_ref() {
            if sock.ws.ready_state() == WebSocket::OPEN {
                let _ = sock.ws.send_with_str("n");
            }
        }
    });
}

/// One JPEG into the frame texture. A frame that fails to decode is acked
/// anyway, or the stream would stall waiting for the ack.
async fn decode(buf: js_sys::ArrayBuffer, gen: u32) {
    match decode_rgba(buf).await {
        Ok((rgba, w, h)) => {
            if STREAM_GEN.with(|g| g.get()) != gen {
                return;
            }
            let id = state::with(|s| s.frame.map(|f| f.0));
            let id = match id {
                Some(id) => {
                    cce_ui::draw::update_pixels(id, rgba, w, h, cce_ui::draw::PixelFormat::Rgba);
                    id
                }
                None => cce_ui::draw::upload_rgba(rgba, w, h),
            };
            state::with(|s| {
                s.frame = Some((id, w, h));
                s.frame_seq += 1;
            });
            state::changed();
        }
        Err(_) => send_ack(),
    }
}

async fn decode_rgba(buf: js_sys::ArrayBuffer) -> Result<(Vec<u8>, u32, u32), JsValue> {
    let parts = Array::of1(&Uint8Array::new(&buf));
    let opts = BlobPropertyBag::new();
    opts.set_type("image/jpeg");
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &opts)?;
    let promise = web_sys::window().unwrap().create_image_bitmap_with_blob(&blob)?;
    let bmp: ImageBitmap = wasm_bindgen_futures::JsFuture::from(promise).await?.dyn_into()?;
    let (w, h) = (bmp.width(), bmp.height());
    let rgba = DECODER.with(|d| -> Result<Vec<u8>, JsValue> {
        let mut d = d.borrow_mut();
        if d.as_ref().is_none_or(|(c, _)| c.width() != w || c.height() != h) {
            let canvas = OffscreenCanvas::new(w, h)?;
            let ctx: OffscreenCanvasRenderingContext2d = canvas.get_context("2d")?.ok_or("no 2d context")?.dyn_into()?;
            *d = Some((canvas, ctx));
        }
        let (_, ctx) = d.as_ref().unwrap();
        ctx.draw_image_with_image_bitmap(&bmp, 0.0, 0.0)?;
        Ok(ctx.get_image_data(0, 0, w as i32, h as i32)?.data().0)
    });
    bmp.close();
    Ok((rgba?, w, h))
}

/// The live view's polls: the window rect every 2s (the ring's frame of
/// reference), the pointer every 120ms (reconciling the ring's prediction),
/// and a watchdog that reopens a stream silent for 30s — the sources send a
/// keepalive at least every 20s, so that silence is a dead connection.
pub fn view_polls(on: bool) {
    let old: Vec<_> = VIEW_TIMERS.with(|t| t.borrow_mut().drain(..).collect());
    for (id, _cb) in old {
        web_sys::window().unwrap().clear_interval_with_handle(id);
    }
    if !on {
        return;
    }
    send("wl");
    let every = |ms: i32, f: Box<dyn FnMut()>| {
        let cb = Closure::<dyn FnMut()>::new(f);
        let id = web_sys::window()
            .unwrap()
            .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms)
            .unwrap_or(0);
        VIEW_TIMERS.with(|t| t.borrow_mut().push((id, cb)));
    };
    every(2000, Box::new(|| send("wl")));
    every(120, Box::new(|| send("pl")));
    every(
        5000,
        Box::new(|| {
            if state::with(|s| s.view && state::now_ms() - s.last_frame_ms > 30_000.0) {
                stream_start();
            }
        }),
    );
}
