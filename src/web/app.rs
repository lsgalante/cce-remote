//! The page's cce-ui app: the remote's controls laid out and painted on the
//! standard root plate, from the shared [`state`](super::state).
//!
//! Top to bottom: two rows of keys (Esc, Tab, the sticky modifiers; the
//! arrows, the window switcher, the menu, the keyboard) with the connection
//! mark at the end of the first; the "tap to type" bar while a desktop field
//! is open and the keyboard is not; the typing echo while the keyboard is
//! up; the trackpad, a canvas well filling the rest (the live view draws
//! into it); and the two click buttons. The switcher and the menu are a pane
//! plate over the pad.
//!
//! The menu's "Code Agent" row swaps all of that for the Agent view
//! (`agent.rs`), which lays out, paints and takes the events while it is
//! on; the pad is then published empty, so `pad.rs` takes no touches.
//!
//! The app paints and handles its buttons; it does not read the pad's
//! touches (`pad.rs` does, before cce-ui sees them) or the keyboard's text
//! (`kb.rs`). It publishes where the pad and the open panel are, which is
//! how `pad.rs` tells its touches from cce-ui's.

use cce_ui::context::UiContext;
use cce_ui::engine::{AppSender, Application, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::arena::Arena;
use cce_ui::scene::layout::{compute_layout, LayoutBox, Length, Rect, Size as LSize, Style};
use cce_ui::scene::paint::{AlignH, AlignV, DisplayList, PaintCtx, TextAttrs, TextLayout};
use cce_ui::scene::Material;
use cce_ui::widget::{Adapted, Button, ElementState, Event, Handle, KeyEvent, MouseButton, MouseScrollDelta, WidgetHost, WidgetId};

use super::icons::icon;
use super::state::{self, Conn, Msg, Overlay};
use super::{agent, kb, net, pad};
use crate::predict::Edit;

/// A key or button row's height: a comfortable thumb target.
const BTN_H: f32 = 40.0;
const CLICK_H: f32 = 52.0;
const PREVIEW_H: f32 = 36.0;
const ROW_H: f32 = 40.0;
/// Switcher rows kept ready; more windows than this are not listed.
const ROWS: usize = 32;
/// A glyph's size on its button, and the size it is rasterized at (sharp
/// up to 3x).
const ICON: f32 = 20.0;
const ICON_PX: u32 = 64;
const MARK: f32 = 10.0;
const MARK_W: f32 = 16.0;
const HINT: &str = "drag = move · tap = click\n2-finger drag = scroll\n2-finger tap = right click · hold = drag";
/// The cursor ring, in the DE's highlight cyan.
const RING: [f32; 4] = [0.49, 1.0, 1.0, 1.0];
const RING_R: f32 = 7.0;

#[derive(Debug, Clone, Copy)]
enum Action {
    Key(u32),
    Mod(u32),
    Windows,
    Menu,
    Keyboard,
}

#[derive(Debug, Clone, Copy)]
enum Row {
    Window(u64),
    ToggleView,
    /// The Agent view: Code Agent's sessions.
    Agent,
    Reload,
    /// Opens the System list.
    System,
    /// Back from it to the menu.
    Back,
    Sys(Sys),
}

/// The System list: compositor actions the server whitelists by these wire
/// names (`translate`'s `cmd` arm), each a fixed command with nothing from
/// the page in it. The focused app is the desktop's focused window, which
/// the compositor resolves.
#[derive(Debug, Clone, Copy)]
enum Sys {
    RestartApp,
    CloseFocused,
    Fullscreen,
    Overview,
    Screenshot,
    ReloadConfig,
    Lock,
    DisplayOff,
    Sleep,
    LogOut,
    RestartCompositor,
}

impl Sys {
    const ALL: [Sys; 11] = [
        Sys::RestartApp,
        Sys::CloseFocused,
        Sys::Fullscreen,
        Sys::Overview,
        Sys::Screenshot,
        Sys::ReloadConfig,
        Sys::Lock,
        Sys::DisplayOff,
        Sys::Sleep,
        Sys::LogOut,
        Sys::RestartCompositor,
    ];

    fn wire(self) -> &'static str {
        match self {
            Sys::RestartApp => "restart-focused-app",
            Sys::CloseFocused => "close-focused",
            Sys::Fullscreen => "fullscreen",
            Sys::Overview => "overview",
            Sys::Screenshot => "screenshot",
            Sys::ReloadConfig => "reload-config",
            Sys::Lock => "lock",
            Sys::DisplayOff => "display-off",
            Sys::Sleep => "sleep",
            Sys::LogOut => "log-out",
            Sys::RestartCompositor => "restart-compositor",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Sys::RestartApp => "Restart focused app",
            Sys::CloseFocused => "Close focused window",
            Sys::Fullscreen => "Toggle fullscreen",
            Sys::Overview => "Overview",
            Sys::Screenshot => "Screenshot",
            Sys::ReloadConfig => "Reload config",
            Sys::Lock => "Lock screen",
            Sys::DisplayOff => "Turn display off",
            Sys::Sleep => "Sleep…",
            Sys::LogOut => "Log out…",
            Sys::RestartCompositor => "Restart compositor…",
        }
    }

    /// Asked before the action (the `…` in its label): the ones that end
    /// the session or drop the remote's own connection. Restarting an app is
    /// not one, by the user's choice: its windows get the compositor's close,
    /// as their close buttons would, and it exits through its own `on_exit`
    /// (where notes saves, and spreadsheet keeps a recovery copy).
    fn confirm(self) -> Option<&'static str> {
        Some(match self {
            Sys::Sleep => "Put the computer to sleep? The remote disconnects until it wakes.",
            Sys::LogOut => "Log out? Every app closes and the session ends, the remote with it.",
            Sys::RestartCompositor => "Restart the compositor? Every app in the session will close, then the session relaunches.",
            _ => return None,
        })
    }
}

type Btn = Handle<Adapted<Button>>;

pub struct RemoteApp {
    ui: UiContext,
    /// The keys: the first `ROW1` on the first row, the rest on the second.
    bar: Vec<(Btn, Action)>,
    typebar: Btn,
    lclick: Btn,
    rclick: Btn,
    rows: Vec<Btn>,
    row_actions: Vec<Row>,
    list_scroll: f32,
    /// The list the panel showed last: another one starts at its top.
    shown_overlay: Option<Overlay>,
    /// The Agent view's widgets and layout (shown while `agent::is_on`).
    agent: agent::View,
    /// The widgets that take events this frame.
    roots: Vec<WidgetId>,

    width: f32,
    height: f32,
    scale: f64,
    needs_rebuild: bool,
    mark_rect: Rect,
    preview_rect: Option<Rect>,
    pad_rect: Rect,
    overlay_rect: Option<Rect>,
    typebar_shown: bool,
    /// The screen's corner radii where the canvas meets them (`screen_corners`).
    corners: (Option<f32>, Option<f32>),
}

const ROW1: usize = 5;

/// The label colour as sRGB bytes (the style holds it linear).
fn label_rgb() -> [u8; 3] {
    cce_ui::color::control_label_color_u8()
}

fn key(ui: &mut UiContext, label: &str) -> Btn {
    ui.insert(Button::new(0.0, 0.0, 0.0, 0.0).with_label(label))
}

fn glyph(ui: &mut UiContext, name: &'static str, fallback: &str) -> Btn {
    match icon(name, ICON_PX, label_rgb()) {
        Some(id) => ui.insert(Button::new(0.0, 0.0, 0.0, 0.0).with_icon(id, ICON, ICON)),
        None => key(ui, fallback),
    }
}

fn label_font() -> (String, f32) {
    cce_ui::layout::control_label_font_parsed()
}

/// The phone's safe areas as `[top, right, bottom, left]` px: the notch or
/// status bar, the home indicator. The canvas covers them, so the root
/// plate reaches the screen's edges; the controls are laid out clear of
/// them. Read from `#safe` (index.html), whose padding is the `env()`
/// insets. Above the keyboard the bottom one is the keyboard's: the canvas
/// already ends where it begins.
///
/// Each is less the root plate's own inset, which the layout puts inside
/// the frame anyway: both keep the controls off the screen's edge, and
/// stacked they left a band below the click buttons. So the controls end
/// at the safe area's line, not a plate inset short of it.
fn safe_insets() -> [f32; 4] {
    let Some(window) = web_sys::window() else { return [0.0; 4] };
    let Some(doc) = window.document() else { return [0.0; 4] };
    let px = |v: String| v.trim().trim_end_matches("px").parse::<f32>().unwrap_or(0.0).max(0.0);
    // app.js's word, not `--kb`: a home-screen app's viewport shrinks with
    // the keyboard, so there the canvas needs no offset and --kb is 0.
    let kb_up = doc
        .document_element()
        .and_then(|e| window.get_computed_style(&e).ok().flatten())
        .is_some_and(|st| st.get_property_value("--kb-up").unwrap_or_default().trim() == "1");
    let Some(st) = doc.get_element_by_id("safe").and_then(|e| window.get_computed_style(&e).ok().flatten()) else { return [0.0; 4] };
    let side = |name: &str| px(st.get_property_value(name).unwrap_or_default());
    let bottom = if kb_up { 0.0 } else { side("padding-bottom") };
    let plate = cce_ui::layout::root_plate_inset();
    [side("padding-top"), side("padding-right"), bottom, side("padding-left")].map(|v| (v - plate).max(0.0))
}

/// The screen's corner radius at the canvas's top and bottom corners, where
/// they are the screen's own (app.js: a home-screen app, its bottom ones
/// while the keyboard is down); `None` where the DE's window radius holds.
fn screen_corners() -> (Option<f32>, Option<f32>) {
    let Some(window) = web_sys::window() else { return (None, None) };
    let Some(st) = window.document().and_then(|d| d.document_element()).and_then(|e| window.get_computed_style(&e).ok().flatten()) else {
        return (None, None);
    };
    let px = |name: &str| st.get_property_value(name).ok().and_then(|v| v.trim().trim_end_matches("px").parse::<f32>().ok()).filter(|r| *r > 0.0);
    (px("--corner-top"), px("--corner-bottom"))
}

/// The root plate, its corners the screen's where they meet it: what
/// `PaintCtx::root_plate` draws (the same rect, material, roll and nominal
/// window radius), with those corners' radii swapped in. Nominal, as
/// `Prim::Plate` takes them: the span factor widens each into the squircle
/// the screen's continuous corner is.
fn root_plate(pc: &mut PaintCtx, width: f32, height: f32, corners: (Option<f32>, Option<f32>)) {
    let window = cce_ui::layout::window_corner_radius();
    let (top, bottom) = (corners.0.unwrap_or(window), corners.1.unwrap_or(window));
    let material = Material::root().for_role(cce_ui::scene::PlateRole::Root);
    pc.plate(Rect { x: 0.0, y: 0.0, width, height }, (top, top, bottom, bottom), &material, cce_ui::layout::bevel_width());
}

/// One line of `text` as the paint shapes it: where the caret stands at
/// each char boundary. `measure_text_width` is an inked extent, not an
/// advance, and a caret placed by it walked off the glyphs it followed.
pub(super) fn shaped(text: &str, family: &str, size: f32) -> cce_ui::text::ShapedRun {
    let Ok(mut fs) = cce_ui::geometry_font_system().try_lock() else { return Default::default() };
    let buffer = cce_ui::text::get_text_buffer(&mut fs, text, size, Some(family));
    cce_ui::text::shaped_run(&buffer, text, cce_ui::scale::scale_factor())
}

impl RemoteApp {
    fn set_view(on: bool) {
        state::with(|s| {
            s.view = on;
            s.zoom = 1.0;
            s.pan = (0.0, 0.0);
        });
        if on {
            net::stream_start();
        } else {
            net::stream_stop();
        }
        net::view_polls(on);
        state::changed();
    }

    /// Lay the frame out from the state: which bands show, where the pad
    /// and the panel are, what the panel lists.
    fn layout(&mut self) {
        self.corners = screen_corners();
        let [it, ir, ib, il] = safe_insets();
        let frame = Rect { x: il, y: it, width: (self.width - il - ir).max(0.0), height: (self.height - it - ib).max(0.0) };
        if agent::is_on() {
            self.agent.layout(&mut self.ui, frame);
            self.roots = self.agent.roots().to_vec();
            // No trackpad and no panel: pad.rs leaves every touch to cce-ui.
            state::with(|s| {
                s.pad = Rect::ZERO;
                s.overlay = None;
                s.overlay_rect = None;
            });
            return;
        }
        let (typebar, preview, overlay, mods) =
            state::with(|s| (s.field_open && !s.kb_up, s.kb_up, s.overlay, s.mods.clone()));
        let mut arena: Arena<LayoutBox> = Arena::new();
        let root = arena.insert(LayoutBox::container(Style::root_column()));
        // The key rows are spaced at the list rung, not the controls one: the
        // control gap is sized for a desktop form, and at a phone's width it
        // left seven keys too narrow for a thumb.
        let row = |arena: &mut Arena<LayoutBox>, h: f32, gap: f32| {
            let r = arena.insert(LayoutBox::container(Style::controls_row().gap(gap).height(Length::Fixed(h))));
            arena.append_child(root, r);
            r
        };
        let (keys_gap, controls_gap) = (cce_ui::layout::list_gap(), cce_ui::layout::control_gap());
        let leaf = |arena: &mut Arena<LayoutBox>, parent, w: f32, h: f32, grow: f32| {
            let l = arena.insert(LayoutBox::leaf(Style::row().grow(grow).shrink(1.0), LSize::new(w, h)));
            arena.append_child(parent, l);
            l
        };
        let row1 = row(&mut arena, BTN_H, keys_gap);
        let mut keys: Vec<_> = (0..ROW1).map(|_| leaf(&mut arena, row1, 0.0, BTN_H, 1.0)).collect();
        let mark = leaf(&mut arena, row1, MARK_W, BTN_H, 0.0);
        let row2 = row(&mut arena, BTN_H, keys_gap);
        keys.extend((ROW1..self.bar.len()).map(|_| leaf(&mut arena, row2, 0.0, BTN_H, 1.0)));
        let typebar_leaf = typebar.then(|| leaf(&mut arena, root, 0.0, BTN_H, 0.0));
        let preview_leaf = preview.then(|| leaf(&mut arena, root, 0.0, PREVIEW_H, 0.0));
        let pad = arena.insert(LayoutBox::leaf(Style::column().grow(1.0), LSize::new(0.0, 0.0)));
        arena.append_child(root, pad);
        let clicks = row(&mut arena, CLICK_H, controls_gap);
        let l = leaf(&mut arena, clicks, 0.0, CLICK_H, 1.0);
        let r = leaf(&mut arena, clicks, 0.0, CLICK_H, 1.0);
        compute_layout(&mut arena, root, LSize::new(frame.width, frame.height));
        let rect = |id| {
            let r: Rect = arena.value(id).unwrap().rect;
            Rect { x: r.x + frame.x, y: r.y + frame.y, ..r }
        };

        let ui = &mut self.ui;
        self.roots.clear();
        for ((h, act), k) in self.bar.iter().zip(&keys) {
            let b = rect(*k);
            ui[*h].set_rect(b.x, b.y, b.width, b.height);
            if let Action::Mod(code) = act {
                ui[*h].selected = mods.contains(code);
            }
            self.roots.push(h.id());
        }
        self.mark_rect = rect(mark);
        self.typebar_shown = typebar;
        if let Some(t) = typebar_leaf {
            let b = rect(t);
            ui[self.typebar].set_rect(b.x, b.y, b.width, b.height);
            self.roots.push(self.typebar.id());
        }
        self.preview_rect = preview_leaf.map(rect);
        self.pad_rect = rect(pad);
        for (h, id) in [(self.lclick, l), (self.rclick, r)] {
            let b = rect(id);
            ui[h].set_rect(b.x, b.y, b.width, b.height);
            self.roots.push(h.id());
        }

        // The switcher or the menu, a panel inset in the pad.
        let gap = cce_ui::layout::root_plate_gap();
        let p = self.pad_rect;
        self.overlay_rect = overlay.map(|_| Rect { x: p.x + gap, y: p.y + gap, width: p.width - 2.0 * gap, height: p.height - 2.0 * gap });
        self.row_actions = match overlay {
            None => Vec::new(),
            Some(Overlay::Switcher) => state::with(|s| s.windows.iter().map(|w| Row::Window(w.id)).take(ROWS).collect()),
            Some(Overlay::Menu) => vec![Row::ToggleView, Row::Agent, Row::Reload, Row::System],
            Some(Overlay::System) => std::iter::once(Row::Back).chain(Sys::ALL.into_iter().map(Row::Sys)).collect(),
        };
        let (view, windows) = state::with(|s| (s.view, s.windows.clone()));
        let inner = cce_ui::layout::plate_padding();
        let rgap = cce_ui::layout::list_gap();
        let o = self.overlay_rect.unwrap_or(Rect::ZERO);
        let max_scroll = (self.row_actions.len() as f32 * (ROW_H + rgap) - rgap + 2.0 * inner - o.height).max(0.0);
        if overlay != self.shown_overlay {
            self.shown_overlay = overlay;
            self.list_scroll = 0.0;
        }
        self.list_scroll = self.list_scroll.clamp(0.0, max_scroll);
        for (i, h) in self.rows.iter().enumerate() {
            let action = self.row_actions.get(i).copied();
            let y = o.y + inner - self.list_scroll + i as f32 * (ROW_H + rgap);
            let shown = action.is_some() && y >= o.y && y + ROW_H <= o.y + o.height;
            if let Some(w) = ui.get_widget_mut(h.id()) {
                w.set_visible(shown);
            }
            let Some(action) = action else { continue };
            let (label, selected) = match action {
                Row::Window(id) => {
                    let w = windows.iter().find(|w| w.id == id);
                    let title = w.map(|w| if w.title.is_empty() { w.app_id.clone() } else { w.title.clone() });
                    (title.unwrap_or_default(), w.is_some_and(|w| w.focused))
                }
                Row::ToggleView => ((if view { "Switch to trackpad" } else { "Show focused window (live view)" }).to_string(), false),
                Row::Agent => ("Code Agent sessions".to_string(), false),
                Row::Reload => ("Reload page".to_string(), false),
                Row::System => ("System…".to_string(), false),
                Row::Back => ("Back".to_string(), false),
                Row::Sys(sys) => (sys.label().to_string(), false),
            };
            ui[*h].set_label(&label);
            ui[*h].selected = selected;
            ui[*h].set_rect(o.x + inner, y, o.width - 2.0 * inner, ROW_H);
            if shown {
                self.roots.push(h.id());
            }
        }
        ui.rebuild_spatial_grid();

        let (pad_rect, overlay_rect) = (self.pad_rect, self.overlay_rect);
        state::with(|s| {
            s.pad = pad_rect;
            s.overlay_rect = overlay_rect;
        });
    }

    /// Act on what was clicked. Runs inside the pointer event that clicked
    /// it, which is what lets the keyboard button, the type bar and the left
    /// button raise the phone keyboard.
    fn drain(&mut self) {
        let ui = &mut self.ui;
        let mut acts: Vec<Action> = Vec::new();
        for (h, act) in &self.bar {
            if ui[*h].take_click() {
                acts.push(*act);
            }
        }
        let typebar = ui[self.typebar].take_click();
        let left = ui[self.lclick].take_click();
        let right = ui[self.rclick].take_click();
        let row = self.rows.iter().position(|h| ui[*h].take_click()).and_then(|i| self.row_actions.get(i).copied());

        for act in acts {
            match act {
                Action::Key(code) => {
                    // The arrows the strip can follow on one line; the rest
                    // (Esc, Tab, Up, Down) it cannot.
                    kb::predict(match code {
                        105 => Edit::Left,
                        106 => Edit::Right,
                        _ => Edit::Opaque,
                    });
                    net::send(&format!("k {code}"));
                }
                Action::Mod(code) => {
                    let on = state::with(|s| {
                        let on = !s.mods.contains(&code);
                        if on {
                            s.mods.push(code);
                        } else {
                            s.mods.retain(|&m| m != code);
                        }
                        on
                    });
                    net::send(&format!("{} {code}", if on { "kd" } else { "ku" }));
                }
                Action::Windows => {
                    let open = state::with(|s| {
                        let open = s.overlay == Some(Overlay::Switcher);
                        s.overlay = None;
                        s.want_switcher = !open;
                        open
                    });
                    if !open {
                        // The reply opens it (net::on_windows).
                        net::send("wl");
                    }
                }
                Action::Menu => state::with(|s| {
                    s.overlay = if matches!(s.overlay, Some(Overlay::Menu | Overlay::System)) { None } else { Some(Overlay::Menu) };
                }),
                Action::Keyboard => kb::toggle(),
            }
        }
        if typebar {
            kb::open_for_field();
        }
        if left && net::click_opened_field() && !kb::is_up() {
            kb::open_for_field();
        }
        if right {
            net::send("b right click");
        }
        if let Some(row) = row {
            state::with(|s| s.overlay = None);
            let window = web_sys::window().unwrap();
            match row {
                Row::Window(id) => net::send(&format!("wf {id}")),
                Row::ToggleView => Self::set_view(!state::with(|s| s.view)),
                Row::Agent => {
                    // The live view's stream is not needed under it.
                    if state::with(|s| s.view) {
                        Self::set_view(false);
                    }
                    agent::show();
                }
                Row::Reload => super::place::reload(),
                Row::System => state::with(|s| s.overlay = Some(Overlay::System)),
                Row::Back => state::with(|s| s.overlay = Some(Overlay::Menu)),
                Row::Sys(sys) => {
                    let ok = sys.confirm().is_none_or(|q| window.confirm_with_message(q).unwrap_or(false));
                    if ok {
                        net::send(&format!("cmd {}", sys.wire()));
                    }
                }
            }
        }
        self.needs_rebuild = true;
    }

    fn route(&mut self, ev: &Event) -> bool {
        let mut changed = false;
        for &root in &self.roots {
            if self.ui.propagate_event(ev, root) {
                changed = true;
            }
        }
        changed
    }

    fn paint_pad(&self, pc: &mut PaintCtx) {
        let pad = self.pad_rect;
        let radius = cce_ui::layout::panel_corner_radius();
        pc.well_floor(pad, radius, &Material::pane(), false);
        let (view, frame, seq, zoom, pan, win, ptr) =
            state::with(|s| (s.view, s.frame, s.frame_seq, s.zoom, s.pan, s.win_rect, s.ptr));
        match (view, frame) {
            (true, Some((id, fw, fh))) => {
                // Contain-fit in the pad, then the pinch's zoom and pan.
                let scale = (pad.width / fw as f32).min(pad.height / fh as f32);
                let (dw, dh) = (fw as f32 * scale, fh as f32 * scale);
                let (ix, iy) = (pad.x + (pad.width - dw) / 2.0, pad.y + (pad.height - dh) / 2.0);
                let tx = |x: f32| pad.x + pan.0 + (x - pad.x) * zoom;
                let ty = |y: f32| pad.y + pan.1 + (y - pad.y) * zoom;
                pc.clip_rounded(pad, radius, |pc| {
                    pc.image(id, Rect { x: tx(ix), y: ty(iy), width: dw * zoom, height: dh * zoom }, 1.0);
                    // The frames carry no cursor: ring where the pointer is.
                    if let (Some(w), Some((px, py))) = (win, ptr) {
                        let (u, v) = ((px - w.x) / w.w, (py - w.y) / w.h);
                        if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
                            let (cx, cy) = (tx(ix + u as f32 * dw), ty(iy + v as f32 * dh));
                            pc.arc(cx, cy, RING_R + 1.0, 2.0, 0.0, std::f32::consts::TAU, [0.0, 0.0, 0.0, 0.6]);
                            pc.arc(cx, cy, RING_R, 2.0, 0.0, std::f32::consts::TAU, RING);
                        }
                    }
                });
                net::ack_painted(seq);
            }
            // The hint, unless a panel covers the pad: text draws above
            // shapes, so it would show through the panel.
            _ if self.overlay_rect.is_none() => {
                let (family, size) = label_font();
                let inset = cce_ui::layout::plate_padding();
                pc.text_boxed(
                    HINT,
                    pad.x + inset,
                    pad.y,
                    size * 0.85,
                    cce_ui::color::control_label_color_detached_u8(),
                    Some(family),
                    None,
                    TextAttrs::default(),
                    TextLayout { wrap_width: Some(pad.width - 2.0 * inset), box_height: pad.height, align_h: AlignH::Center, align_v: AlignV::Middle },
                );
            }
            _ => {}
        }
        pc.well_rim(pad, radius, cce_ui::layout::control_relief());
    }

    fn paint_preview(&self, pc: &mut PaintCtx, r: Rect) {
        let radius = cce_ui::layout::control_corner_radius();
        pc.canvas_well(r, radius, &Material::pane(), cce_ui::layout::control_relief(), false);
        let (family, size) = label_font();
        let inset = cce_ui::layout::plate_padding();
        let (typed, field) = state::with(|s| (s.typed.clone(), s.shown_field()));
        let caret_w = 2.0;
        let text_r = Rect { x: r.x + inset, y: r.y, width: r.width - 2.0 * inset - caret_w - 1.0, height: r.height };
        // What the strip shows: the desktop field's own line around its
        // caret when the app reports its contents, else the local echo of
        // what was typed here (caret at its end).
        let (line, caret, anchor) = match &field {
            Some((text, cursor, anchor)) => {
                let start = text[..*cursor].rfind('\n').map_or(0, |i| i + 1);
                let end = text[*cursor..].find('\n').map_or(text.len(), |i| cursor + i);
                (text[start..end].to_string(), cursor - start, (*anchor).clamp(start, end) - start)
            }
            None => (typed.clone(), typed.len(), typed.len()),
        };
        // From the left like a field; when the caret would fall past the
        // right edge, the line starts later, so the caret stays in view.
        let run = shaped(&line, &family, size);
        let mut from = 0;
        while from < caret && run.x_of(caret) - run.x_of(from) > text_r.width {
            from += line[from..].chars().next().map_or(1, char::len_utf8);
        }
        let x_at = |i: usize| text_r.x + run.x_of(i.max(from)) - run.x_of(from);
        let ch = size * 1.1;
        let cy = r.y + (r.height - ch) / 2.0;
        if anchor != caret {
            let (a, b) = (x_at(anchor.min(caret)), x_at(anchor.max(caret)).min(text_r.x + text_r.width));
            pc.quad(Rect { x: a, y: cy, width: (b - a).max(0.0), height: ch }, [RING[0], RING[1], RING[2], 0.3]);
        }
        if from < line.len() {
            pc.text_boxed(
                line[from..].to_string(),
                text_r.x,
                text_r.y,
                size,
                label_rgb(),
                Some(family.clone()),
                Some([text_r.x, text_r.y, text_r.x + text_r.width + caret_w, text_r.y + text_r.height]),
                TextAttrs::default(),
                TextLayout { wrap_width: None, box_height: text_r.height, align_h: AlignH::Left, align_v: AlignV::Middle },
            );
        }
        pc.quad(Rect { x: x_at(caret) - caret_w / 2.0, y: cy, width: caret_w, height: ch }, RING);
    }

    fn paint_overlay(&self, pc: &mut PaintCtx, o: Rect) {
        let radius = cce_ui::layout::panel_corner_radius();
        pc.plate(o, (radius, radius, radius, radius), &Material::pane(), cce_ui::layout::bevel_width());
        if self.row_actions.is_empty() {
            let (family, size) = label_font();
            pc.text_boxed(
                "no windows",
                o.x,
                o.y,
                size,
                cce_ui::color::control_label_color_detached_u8(),
                Some(family),
                None,
                TextAttrs::default(),
                TextLayout { wrap_width: Some(o.width), box_height: o.height, align_h: AlignH::Center, align_v: AlignV::Middle },
            );
        }
        for h in &self.rows {
            if let Some(w) = self.ui.get_widget(h.id()) {
                if w.visible() {
                    cce_ui::widget::painter::paint_root_into(&self.ui, w, pc);
                }
            }
        }
    }

    fn paint_mark(&self, pc: &mut PaintCtx) {
        let (name, rgb) = match state::with(|s| s.conn) {
            Conn::Paired => ("circle", [0x5f, 0xbf, 0x6f]),
            Conn::Off => ("circle-outline", [0xbf, 0x5f, 0x5f]),
            Conn::Connecting => ("circle-outline", [0x6a, 0x6d, 0x7c]),
        };
        if let Some(id) = icon(name, ICON_PX / 2, rgb) {
            let m = self.mark_rect;
            pc.image(id, Rect { x: m.x + (m.width - MARK) / 2.0, y: m.y + (m.height - MARK) / 2.0, width: MARK, height: MARK }, 1.0);
        }
    }
}

impl Application for RemoteApp {
    type Message = Msg;

    fn create(sender: AppSender<Msg>) -> Self {
        cce_ui::scale::set_scale_factor(1.0);
        state::with(|s| s.sender = Some(sender));
        kb::install();
        pad::install();
        net::connect();

        let mut ui = UiContext::new();
        let mut bar = vec![
            (key(&mut ui, "esc"), Action::Key(1)),
            (key(&mut ui, "tab"), Action::Key(15)),
            (key(&mut ui, "ctrl"), Action::Mod(29)),
            (key(&mut ui, "alt"), Action::Mod(56)),
            (key(&mut ui, "sup"), Action::Mod(125)),
        ];
        bar.extend([
            (glyph(&mut ui, "arrow-left", "left"), Action::Key(105)),
            (glyph(&mut ui, "arrow-up", "up"), Action::Key(103)),
            (glyph(&mut ui, "arrow-down", "down"), Action::Key(108)),
            (glyph(&mut ui, "arrow-right", "right"), Action::Key(106)),
            (glyph(&mut ui, "menu", "windows"), Action::Windows),
            (glyph(&mut ui, "more-horizontal", "menu"), Action::Menu),
            (glyph(&mut ui, "keyboard", "kbd"), Action::Keyboard),
        ]);
        let typebar = key(&mut ui, "tap to type into the field");
        let lclick = key(&mut ui, "left");
        let rclick = key(&mut ui, "right");
        let rows = (0..ROWS)
            .map(|_| ui.insert(Button::new(0.0, 0.0, 0.0, 0.0).with_label("")))
            .collect();
        let agent = agent::View::new(&mut ui);
        // Back where a reload left the page (a restart onto new code).
        if let Some(p) = super::place::take() {
            if p.view {
                Self::set_view(true);
            }
            if let Some((screen, draft)) = p.agent {
                agent::restore(screen, draft);
            }
        }
        Self {
            agent,
            ui,
            bar,
            typebar,
            lclick,
            rclick,
            rows,
            row_actions: Vec::new(),
            list_scroll: 0.0,
            shown_overlay: None,
            roots: Vec::new(),
            width: 0.0,
            height: 0.0,
            scale: 1.0,
            needs_rebuild: true,
            mark_rect: Rect::ZERO,
            preview_rect: None,
            pad_rect: Rect::ZERO,
            overlay_rect: None,
            typebar_shown: false,
            corners: (None, None),
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings { title: "cce remote".to_string(), app_id: "cce-remote".to_string(), width: 390, height: 760, fullscreen: false, min_size: None }
    }

    fn update(&mut self, msg: Msg, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            Msg::Changed => {
                self.needs_rebuild = true;
                *needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.ui.tick(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        let resized = self.width != size.width || self.height != size.height || self.scale != scale;
        if self.needs_rebuild || resized {
            self.width = size.width;
            self.height = size.height;
            self.scale = scale;
            cce_ui::scale::set_scale_factor(scale as f32);
            self.layout();
            self.needs_rebuild = false;
        }

        let mut pc = PaintCtx::new();
        root_plate(&mut pc, self.width, self.height, self.corners);
        if agent::is_on() {
            self.agent.paint(&self.ui, &mut pc);
            return Some(pc.finish());
        }
        let mut painted: Vec<WidgetId> = self.bar.iter().map(|(h, _)| h.id()).collect();
        if self.typebar_shown {
            painted.push(self.typebar.id());
        }
        painted.extend([self.lclick.id(), self.rclick.id()]);
        for id in painted {
            if let Some(w) = self.ui.get_widget(id) {
                cce_ui::widget::painter::paint_root_into(&self.ui, w, &mut pc);
            }
        }
        self.paint_mark(&mut pc);
        if let Some(r) = self.preview_rect {
            self.paint_preview(&mut pc, r);
        }
        self.paint_pad(&mut pc);
        if let Some(o) = self.overlay_rect {
            self.paint_overlay(&mut pc, o);
        }
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn ui_context(&self) -> Option<&UiContext> {
        Some(&self.ui)
    }

    fn ui_context_mut(&mut self) -> Option<&mut UiContext> {
        Some(&mut self.ui)
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let ev = Event::PointerMove { x: pos.x, y: pos.y, local_x: pos.x, local_y: pos.y };
        if self.route(&ev) {
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(&mut self, button: MouseButton, state: ElementState, pos: LogicalPosition, needs_rebuild: &mut bool) -> Option<Msg> {
        let ev = Event::MouseButton { button, state, x: pos.x, y: pos.y, local_x: pos.x, local_y: pos.y };
        self.route(&ev);
        if agent::is_on() {
            self.agent.drain(&mut self.ui);
            self.agent.mouse(state == ElementState::Pressed, pos.x, pos.y);
            self.needs_rebuild = true;
            *needs_rebuild = true;
            return None;
        }
        self.drain();
        *needs_rebuild = true;
        None
    }

    /// A drag on the open panel scrolls its list (cce-ui reads a finger's
    /// drag as a scroll).
    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        if agent::is_on() {
            let dy = match delta {
                MouseScrollDelta::PixelDelta(p) => p.y as f32,
                MouseScrollDelta::LineDelta(_, y) => *y * ROW_H,
            };
            if self.agent.wheel(dy, pos.x, pos.y) {
                *needs_rebuild = true;
            }
            return;
        }
        let Some(o) = self.overlay_rect else { return };
        if pos.x < o.x || pos.x > o.x + o.width || pos.y < o.y || pos.y > o.y + o.height {
            return;
        }
        let dy = match delta {
            MouseScrollDelta::PixelDelta(p) => p.y as f32,
            MouseScrollDelta::LineDelta(_, y) => *y * ROW_H,
        };
        self.list_scroll -= dy;
        self.needs_rebuild = true;
        *needs_rebuild = true;
    }

    fn handle_key_input(&mut self, _event: &KeyEvent, _needs_rebuild: &mut bool) -> Option<Msg> {
        None
    }
}
