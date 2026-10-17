//! cce-icons glyphs for the page. cce-ui's `upload_icon` reads the icon
//! directory from disk, which a browser has none of, so the glyphs this page
//! draws are copied into `web/icons/` (from `cce-icons/svg/`; copy again if
//! one is redrawn), compiled in, rasterized with cce-ui's own `rasterize_svg`
//! and tinted the way `upload_icon_tinted` tints: the artwork is white, so
//! multiplying is tinting.

use std::cell::RefCell;
use std::collections::HashMap;

fn svg(name: &str) -> Option<&'static [u8]> {
    Some(match name {
        "arrow-left" => include_bytes!("../../web/icons/arrow-left.svg"),
        "arrow-up" => include_bytes!("../../web/icons/arrow-up.svg"),
        "arrow-down" => include_bytes!("../../web/icons/arrow-down.svg"),
        "arrow-right" => include_bytes!("../../web/icons/arrow-right.svg"),
        "menu" => include_bytes!("../../web/icons/menu.svg"),
        "more-horizontal" => include_bytes!("../../web/icons/more-horizontal.svg"),
        "keyboard" => include_bytes!("../../web/icons/keyboard.svg"),
        "circle" => include_bytes!("../../web/icons/circle.svg"),
        "circle-outline" => include_bytes!("../../web/icons/circle-outline.svg"),
        _ => return None,
    })
}

/// (glyph, px, tint) → its image id, or None for a glyph that failed.
type Cache = HashMap<(&'static str, u32, [u8; 3]), Option<u32>>;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(HashMap::new());
}

/// `name` rasterized at `px` and tinted `rgb`, as an image id; uploaded once
/// per (name, px, tint) for the page's life.
pub fn icon(name: &'static str, px: u32, rgb: [u8; 3]) -> Option<u32> {
    let key = (name, px, rgb);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).copied()) {
        return hit;
    }
    let id = svg(name).and_then(|data| cce_ui::rasterize_svg(data, px)).map(|(mut rgba, w, h)| {
        for p in rgba.as_chunks_mut::<4>().0 {
            for (c, &t) in p[..3].iter_mut().zip(rgb.iter()) {
                *c = ((*c as u16 * t as u16 + 127) / 255) as u8;
            }
        }
        cce_ui::draw::upload_rgba(rgba, w, h)
    });
    CACHE.with(|c| c.borrow_mut().insert(key, id));
    id
}
