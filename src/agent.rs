//! The Agent view's server side: the page's `ag <json>` frames, checked,
//! rebuilt and carried to Code Agent (cce-code-agent), whose sessions the
//! phone lists, reads, prompts, stops, answers, starts and archives.
//!
//! Code Agent's control socket is the second channel this crate writes to,
//! and the same discipline holds as for the compositor's (`translate`):
//! **nothing from the network is forwarded raw.** [`parse`] accepts a fixed
//! set of shapes, checks every id against [`token`] and caps the prompt's
//! length, and [`Op::line`] rebuilds the request from those checked values;
//! anything else is dropped. The prompt's text does go through as text — it
//! is what a paired phone is for — exactly as typed keys go to the desktop.
//!
//! Where a request goes ([`run`]): each Code Agent window listens on
//! `$XDG_RUNTIME_DIR/cce/code-agent/<pid>.sock` (its `remote.rs`). A
//! session open in a window is that window's; one that is not opens in the
//! newest window. With no window running, the list and a transcript come
//! from `cce-code-agent --list` / `--transcript <id>`, read-only: the user's
//! choice, so that nothing starts on the desktop from the phone.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

/// The longest prompt a frame may carry (Code Agent's own limit).
const MAX_TEXT: usize = 64 * 1024;
/// How long a window has to answer one request.
const WINDOW_WAIT: Duration = Duration::from_secs(12);

/// What the page may ask.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    List,
    Transcript { id: String, since: Option<String> },
    Send { id: Option<String>, cwd: Option<String>, text: String },
    Stop { id: String },
    Answer { id: String, request_id: String, choice: &'static str },
    Archive { id: String, archived: bool },
    Open { id: String },
}

/// A session or request id: letters, digits, `-`, `_`; nothing else names
/// one, so none can carry a path or a second request.
fn token(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    (!s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then(|| s.to_string())
}

/// A version the page has seen: what `token` takes, plus `:` and `,`.
fn version(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    (s.len() <= 512 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | ','))).then(|| s.to_string())
}

/// An `ag` frame's JSON: its sequence number (echoed in the reply) and the
/// request. `None` drops the frame.
pub fn parse(frame: &str) -> Option<(u64, Op)> {
    let v: Value = serde_json::from_str(frame).ok()?;
    let n = v["n"].as_u64()?;
    let op = match v["op"].as_str()? {
        "list" => Op::List,
        "transcript" => Op::Transcript { id: token(&v["id"])?, since: if v["since"].is_null() { None } else { Some(version(&v["since"])?) } },
        "send" => {
            let text = v["text"].as_str()?;
            if text.trim().is_empty() || text.len() > MAX_TEXT {
                return None;
            }
            let id = if v["id"].is_null() { None } else { Some(token(&v["id"])?) };
            let cwd = match &v["cwd"] {
                Value::Null => None,
                c => Some(c.as_str().filter(|c| c.starts_with('/') && c.len() <= 4096 && !c.contains(['\n', '\0']))?.to_string()),
            };
            if id.is_none() && cwd.is_none() {
                return None;
            }
            Op::Send { id, cwd, text: text.to_string() }
        }
        "stop" => Op::Stop { id: token(&v["id"])? },
        "answer" => Op::Answer {
            id: token(&v["id"])?,
            request_id: token(&v["request_id"])?,
            choice: match v["choice"].as_str()? {
                "allow" => "allow",
                "always" => "always",
                "deny" => "deny",
                _ => return None,
            },
        },
        "archive" => Op::Archive { id: token(&v["id"])?, archived: v["archived"].as_bool()? },
        "open" => Op::Open { id: token(&v["id"])? },
        _ => return None,
    };
    Some((n, op))
}

impl Op {
    /// The request line for Code Agent's socket, built from the checked
    /// values alone.
    pub fn line(&self) -> String {
        match self {
            Op::List => json!({"op": "list"}),
            Op::Transcript { id, since } => json!({"op": "transcript", "id": id, "since": since}),
            Op::Send { id, cwd, text } => json!({"op": "send", "id": id, "cwd": cwd, "text": text}),
            Op::Stop { id } => json!({"op": "stop", "id": id}),
            Op::Answer { id, request_id, choice } => json!({"op": "answer", "id": id, "request_id": request_id, "choice": choice}),
            Op::Archive { id, archived } => json!({"op": "archive", "id": id, "archived": archived}),
            Op::Open { id } => json!({"op": "open", "id": id}),
        }
        .to_string()
    }

    /// The session it names, if any.
    fn id(&self) -> Option<&str> {
        match self {
            Op::List => None,
            Op::Send { id, .. } => id.as_deref(),
            Op::Transcript { id, .. } | Op::Stop { id } | Op::Answer { id, .. } | Op::Archive { id, .. } | Op::Open { id } => Some(id),
        }
    }
}

fn socket_dir() -> PathBuf {
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    run.join("cce").join("code-agent")
}

/// One request to a window's socket: its reply, parsed.
fn ask(sock: &std::path::Path, line: &str) -> Result<Value, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(WINDOW_WAIT));
    let _ = s.set_write_timeout(Some(WINDOW_WAIT));
    s.write_all(line.as_bytes()).and_then(|_| s.write_all(b"\n")).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).map_err(|e| e.to_string())?;
    serde_json::from_str(&reply).map_err(|_| "a window answered nonsense".to_string())
}

/// The running windows, newest first, with what each has open (`state`).
fn windows() -> Vec<(PathBuf, Value)> {
    let mut socks: Vec<(PathBuf, std::time::SystemTime)> = std::fs::read_dir(socket_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .filter(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists()))
        .map(|p| {
            let t = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            (p, t)
        })
        .collect();
    socks.sort_by(|a, b| b.1.cmp(&a.1));
    socks.into_iter().filter_map(|(p, _)| ask(&p, r#"{"op":"state"}"#).ok().map(|s| (p, s))).collect()
}

/// Whether a window's `state` lists session `id`.
fn has(state: &Value, id: &str) -> bool {
    state["sessions"].as_array().is_some_and(|l| l.iter().any(|s| s["id"].as_str() == Some(id)))
}

/// `cce-code-agent <args>`'s JSON: the read-only answers with no window.
fn offline(args: &[&str]) -> Value {
    let out = std::process::Command::new(cce_core::process::de_bin("cce-code-agent")).args(args).output();
    match out {
        Ok(o) if o.status.success() => serde_json::from_slice(&o.stdout).unwrap_or_else(|_| json!({"error": "cce-code-agent answered nonsense"})),
        Ok(o) => json!({"error": String::from_utf8_lossy(&o.stderr).trim().to_string()}),
        Err(e) => json!({"error": format!("cce-code-agent: {e}")}),
    }
}

/// Carry out `op` where it belongs; the reply for the page.
pub fn run(op: &Op) -> Value {
    let wins = windows();
    if let Op::List = op {
        let Some((sock, _)) = wins.first() else {
            let mut v = offline(&["--list"]);
            v["readonly"] = json!(true);
            return v;
        };
        let mut v = ask(sock, &op.line()).unwrap_or_else(|e| json!({"error": e}));
        // What every window is doing, by session: one list, all windows.
        let open: serde_json::Map<String, Value> = wins
            .iter()
            .flat_map(|(_, st)| st["sessions"].as_array().cloned().unwrap_or_default())
            .filter_map(|s| Some((s["id"].as_str()?.to_string(), json!({"state": s["state"], "shown": s["shown"], "title": s["title"], "cwd": s["cwd"]}))))
            .collect();
        v["open"] = Value::Object(open);
        v["readonly"] = json!(false);
        return v;
    }
    let owner = op.id().and_then(|id| wins.iter().find(|(_, st)| has(st, id)));
    match (op, owner, wins.first()) {
        (_, Some((sock, _)), _) => ask(sock, &op.line()).unwrap_or_else(|e| json!({"error": e})),
        (Op::Transcript { id, since }, None, _) => {
            let mut v = offline(&["--transcript", id]);
            // The page's copy is current: say so, as a window would.
            if since.is_some() && v["version"].as_str() == since.as_deref() {
                return json!({"unchanged": true, "version": since});
            }
            v["readonly"] = json!(true);
            v
        }
        // Not open anywhere: the newest window opens it.
        (Op::Send { .. } | Op::Open { .. } | Op::Archive { .. }, None, Some((sock, _))) => ask(sock, &op.line()).unwrap_or_else(|e| json!({"error": e})),
        (Op::Stop { .. } | Op::Answer { .. }, None, _) => json!({"error": "That session is not running in a Code Agent window."}),
        // No window at all (a list returned above).
        _ => json!({"error": "No Code Agent window is open. Start one on the desktop to send, stop or archive."}),
    }
}

/// An `ag` frame, start to finish: the reply frame, carrying the page's
/// sequence number. `None` for a frame [`parse`] drops.
pub fn handle(frame: &str) -> Option<String> {
    let (n, op) = parse(frame)?;
    let mut v = run(&op);
    if !v.is_object() {
        v = json!({"error": "a window answered nonsense"});
    }
    v["n"] = json!(n);
    Some(format!("ag {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_only_its_shapes_and_rebuilds_them() {
        let (n, op) = parse(r#"{"n":7,"op":"answer","id":"abc-1","request_id":"perm_2","choice":"always","extra":"x"}"#).unwrap();
        assert_eq!(n, 7);
        assert_eq!(op, Op::Answer { id: "abc-1".into(), request_id: "perm_2".into(), choice: "always" });
        // The line carries the checked fields only: "extra" is gone.
        let line: Value = serde_json::from_str(&op.line()).unwrap();
        assert_eq!(line, json!({"op": "answer", "id": "abc-1", "request_id": "perm_2", "choice": "always"}));
        let (_, send) = parse(r#"{"n":1,"op":"send","cwd":"/home/u/p","text":"fix it\nplease"}"#).unwrap();
        assert_eq!(send, Op::Send { id: None, cwd: Some("/home/u/p".into()), text: "fix it\nplease".into() });
        assert!(parse(r#"{"n":2,"op":"transcript","id":"s","since":"50:96:0:perm-1,perm-2"}"#).is_some());
    }

    #[test]
    fn refuses_everything_else() {
        for bad in [
            r#"{"op":"list"}"#,
            r#"{"n":1,"op":"exec"}"#,
            r#"{"n":1,"op":"state"}"#,
            r#"{"n":1,"op":"stop","id":"../../x"}"#,
            r#"{"n":1,"op":"stop","id":"a\nb"}"#,
            r#"{"n":1,"op":"answer","id":"s","request_id":"r","choice":"yes"}"#,
            r#"{"n":1,"op":"send","id":"s","text":"  "}"#,
            r#"{"n":1,"op":"send","text":"x"}"#,
            r#"{"n":1,"op":"send","text":"x","cwd":"relative/dir"}"#,
            r#"{"n":1,"op":"send","text":"x","cwd":"/a\nb"}"#,
            r#"{"n":1,"op":"transcript","id":"s","since":"a b"}"#,
            r#"{"n":1,"op":"archive","id":"s","archived":"yes"}"#,
            "not json",
        ] {
            assert!(parse(bad).is_none(), "{bad}");
        }
        let long = format!(r#"{{"n":1,"op":"send","id":"s","text":"{}"}}"#, "x".repeat(MAX_TEXT + 1));
        assert!(parse(&long).is_none());
    }

    #[test]
    fn a_line_is_one_line() {
        // A prompt's newlines are escaped in the JSON, never a second request.
        let (_, op) = parse(r#"{"n":1,"op":"send","id":"s","text":"a\n{\"op\":\"stop\"}\n"}"#).unwrap();
        assert!(!op.line().contains('\n'));
    }
}
