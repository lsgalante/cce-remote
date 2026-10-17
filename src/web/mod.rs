//! The page half (wasm32 only): `start`, which the page's loader
//! (`web/app.js`) calls once it has the PIN, the DE's config and its fonts.
//!
//! | module | holds |
//! |---|---|
//! | `app` | the cce-ui `Application`: layout, paint, the buttons and panels |
//! | `state` | what the page knows, shared by the app and the listeners below |
//! | `net` | the input and stream sockets, frame decoding, `/click` |
//! | `pad` | the trackpad: raw touches into pointer frames |
//! | `kb` | the phone keyboard's hidden textarea |
//! | `icons` | the cce-icons glyphs, compiled in |
//! | `agent` | the Agent view: Code Agent's sessions, a transcript, a prompt |
//! | `place` | where the page was, kept across a reload |

mod agent;
mod app;
mod icons;
mod kb;
mod net;
mod pad;
mod place;
mod state;

use cce_ui::web::{Fonts, Sizing};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Run the remote on `canvas`. `fonts` is an array of font files' bytes,
/// `families` the generic serif / sans-serif / monospace families as
/// `"serif,sans,mono"` (any part may be empty), `config` the DE's
/// `config.kdl` (its colours and surfaces, so the page looks like the
/// desktop), `pin` the pairing PIN the loader holds.
#[wasm_bindgen]
pub async fn start(canvas: web_sys::HtmlCanvasElement, fonts: js_sys::Array, families: String, config: String, pin: String) -> Result<(), JsValue> {
    std::panic::set_hook(Box::new(|info| web_sys::console::error_1(&info.to_string().into())));
    if !config.is_empty() {
        cce_ui::color::reload_colors(&config);
    }
    match_page_to_plate();
    state::init(pin);
    let mut fonts = Fonts::new(fonts.iter().map(|f| js_sys::Uint8Array::new(&f).to_vec()).collect());
    let mut names = families.split(',').map(|f| Some(f.trim()).filter(|f| !f.is_empty()).map(String::from));
    fonts.serif = names.next().flatten();
    fonts.sans_serif = names.next().flatten();
    fonts.monospace = names.next().flatten();
    cce_ui::web::run::<app::RemoteApp>(canvas, fonts, Sizing::Page).await
}

/// The page behind the canvas in the root plate's colour, as the plate
/// shows over it (it may be translucent, and the canvas is composited
/// premultiplied): where the canvas does not reach — the band a home-screen
/// app's short viewport leaves under it — the page reads as the app, not a
/// gap.
fn match_page_to_plate() {
    const PAGE: [f32; 3] = [0x14 as f32 / 255.0, 0x15 as f32 / 255.0, 0x1c as f32 / 255.0];
    let t = cce_ui::color::to_srgb(cce_ui::scene::Material::root().tint);
    let a = t[3].clamp(0.0, 1.0);
    let [r, g, b] = [0, 1, 2].map(|i| ((t[i] * a + PAGE[i] * (1.0 - a)).clamp(0.0, 1.0) * 255.0).round() as u8);
    let css = format!("#{r:02x}{g:02x}{b:02x}");
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    for el in [doc.document_element(), doc.body().map(Into::into)].into_iter().flatten() {
        if let Some(el) = el.dyn_ref::<web_sys::HtmlElement>() {
            let _ = el.style().set_property("background", &css);
        }
    }
}

/// The next frame read back from the GPU, as `[width, height, rgba]`: how a
/// test sees the page, since headless Chromium leaves a WebGPU canvas out of
/// its screenshots (cce-ui's `web::capture`).
#[wasm_bindgen]
pub async fn capture() -> Result<js_sys::Array, JsValue> {
    let frame = cce_ui::web::capture().await?;
    Ok(js_sys::Array::of3(&frame.width.into(), &frame.height.into(), &js_sys::Uint8Array::from(&frame.rgba[..])))
}
