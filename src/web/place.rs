//! Where the page was, kept across a reload: a restart onto new code
//! reloads the page (`net`'s `ver`), as does the menu's "Reload page", and
//! both used to land on the trackpad. Saved to `sessionStorage` just before
//! the reload, read back (and cleared) as the app starts, so a page opened
//! afresh starts where it always did.
//!
//! What is kept: the live view's being on, and the Agent view's screen (the
//! list, a session, a new session's folder) with its draft. Not the
//! keyboard: a page can raise it only inside a tap.

use serde_json::{json, Value};

use super::agent::{self, Screen};
use super::state;

const KEY: &str = "cce-remote-place";

pub struct Place {
    pub view: bool,
    pub agent: Option<(Screen, String)>,
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok().flatten()
}

fn save() {
    let view = state::with(|s| s.view);
    let agent = agent::place().map(|(screen, draft)| {
        let mut v = match screen {
            Screen::List => json!({"screen": "list"}),
            Screen::Session { id, title } => json!({"screen": "session", "id": id, "title": title}),
            Screen::New { cwd, label } => json!({"screen": "new", "cwd": cwd, "label": label}),
        };
        v["draft"] = Value::from(draft);
        v
    });
    if let Some(st) = storage() {
        let _ = st.set_item(KEY, &json!({"view": view, "agent": agent}).to_string());
    }
}

/// Reload the page, to come back where it is.
pub fn reload() {
    save();
    let _ = web_sys::window().unwrap().location().reload();
}

/// Where the last reload left the page, once: the next start is afresh.
pub fn take() -> Option<Place> {
    let st = storage()?;
    let raw = st.get_item(KEY).ok().flatten()?;
    let _ = st.remove_item(KEY);
    let v: Value = serde_json::from_str(&raw).ok()?;
    let s = |a: &Value, k: &str| a[k].as_str().map(str::to_string);
    let agent = v.get("agent").filter(|a| a.is_object()).and_then(|a| {
        let screen = match a["screen"].as_str()? {
            "list" => Screen::List,
            "session" => Screen::Session { id: s(a, "id")?, title: s(a, "title").unwrap_or_default() },
            "new" => Screen::New { cwd: s(a, "cwd")?, label: s(a, "label").unwrap_or_default() },
            _ => return None,
        };
        Some((screen, s(a, "draft").unwrap_or_default()))
    });
    Some(Place { view: v["view"].as_bool().unwrap_or(false), agent })
}
