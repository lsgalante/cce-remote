//! cce-remote — use a phone as a trackpad + keyboard for the cce desktop.
//!
//! One small server, no GUI: it serves the embedded `index.html` (a touch
//! trackpad + keyboard page) over HTTP on the LAN, accepts a WebSocket at
//! `/ws`, and translates the page's compact input events into the
//! compositor's line-oriented control socket (`/tmp/cce-{WAYLAND_DISPLAY}.sock`
//! — the same channel `ccectl` uses, so injection goes through the real
//! compositor input path: `pointer-move-by`, `pointer-scroll`,
//! `pointer-press/release/click`, `keypress`, `key-down`/`key-up`).
//!
//! Wire protocol (WS text frames, space-separated, one event per frame):
//!   m <dx> <dy>          relative pointer move (logical px)
//!   s <dy> <dx>          scroll (wayland axis units)
//!   b <btn> <down|up|click>   btn = left|right|middle
//!   k <keycode>          tap an evdev keycode
//!   kd <keycode> / ku <keycode>   hold / release (modifiers)
//!
//! Security model: pairing PIN. A persistent 6-digit PIN (generated on first
//! run, stored 0600 under ~/.config/cce/cce-remote.pin, printed at startup)
//! must arrive as the FIRST WebSocket frame (`auth <pin>`) before any input
//! event is accepted; anything else closes the connection. The page remembers
//! the PIN in localStorage after the first pairing.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::Duration;

mod screencopy;

const INDEX_HTML: &str = include_str!("../index.html");
const DEFAULT_PORT: u16 = 17017;

fn control_socket_path() -> String {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    format!("/tmp/cce-{display}.sock")
}

fn pin_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(home).join(".config")
        });
    base.join("cce").join("cce-remote.pin")
}

/// The pairing PIN: read from disk, or generated (6 digits from /dev/urandom)
/// and stored 0600 on first run.
fn load_or_create_pin() -> std::io::Result<String> {
    let path = pin_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let mut bytes = [0u8; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let pin = format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(f, "{pin}")?;
    }
    Ok(pin)
}

/// One control-socket command, one connection: the compositor's IPC server is
/// one-shot (read → reply → close), so a fresh connect per command is the
/// correct framing — the reply is everything until EOF (commands like
/// `windows --json` reply with multiple lines).
fn control_command(cmd: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(control_socket_path())?;
    s.write_all(cmd.as_bytes())?;
    s.write_all(b"\n")?;
    let mut reply = String::new();
    s.read_to_string(&mut reply)?;
    Ok(reply)
}

/// True for tokens safe to splice into a control command (window queries:
/// numeric ids or app_ids). The WS payload is untrusted — nothing unvalidated
/// reaches the compositor.
fn safe_token(t: &str) -> bool {
    !t.is_empty() && t.len() <= 128
        && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

/// Translate one WS frame into a control-socket command. Returns None for
/// frames that don't parse — they're dropped, never forwarded raw (the WS
/// payload is untrusted; only these fixed shapes reach the compositor).
fn translate(frame: &str) -> Option<String> {
    let mut it = frame.split_ascii_whitespace();
    let cmd = match it.next()? {
        "m" => {
            let dx: f64 = it.next()?.parse().ok()?;
            let dy: f64 = it.next()?.parse().ok()?;
            format!("pointer-move-by {dx:.2} {dy:.2}")
        }
        "s" => {
            let dy: f64 = it.next()?.parse().ok()?;
            let dx: f64 = it.next().unwrap_or("0").parse().ok()?;
            format!("pointer-scroll {dy:.3} {dx:.3}")
        }
        "b" => {
            let btn = match it.next()? {
                b @ ("left" | "right" | "middle") => b,
                _ => return None,
            };
            match it.next()? {
                "down" => format!("pointer-press {btn}"),
                "up" => format!("pointer-release {btn}"),
                "click" => format!("pointer-click {btn}"),
                _ => return None,
            }
        }
        "k" => format!("keypress {}", it.next()?.parse::<u32>().ok()?),
        "kd" => format!("key-down {}", it.next()?.parse::<u32>().ok()?),
        "ku" => format!("key-up {}", it.next()?.parse::<u32>().ok()?),
        "wf" => {
            let target = it.next()?;
            if !safe_token(target) {
                return None;
            }
            format!("focus-window {target}")
        }
        _ => return None,
    };
    Some(cmd)
}

/// The `windows --json` reply (one JSON object per line) as a JSON array
/// for the page's switcher.
fn window_list_json() -> String {
    let reply = control_command("windows --json").unwrap_or_default();
    let objs: Vec<&str> = reply.lines().filter(|l| l.trim_start().starts_with('{')).collect();
    format!("windows [{}]", objs.join(","))
}

/// Pull a numeric field out of one windows-json line (no serde — the values
/// are flat numbers on a single line per window).
fn json_num(line: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let rest = &line[line.find(&pat)? + pat.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The focused app window as (id, x, y, w, h) in layout px.
fn focused_window() -> Option<(u64, f64, f64, f64, f64)> {
    let reply = control_command("windows --json").ok()?;
    for line in reply.lines() {
        if line.contains("\"focused\":true") && !line.contains("\"mode\":\"Status\"") {
            return Some((
                json_num(line, "id")? as u64,
                json_num(line, "x")?,
                json_num(line, "y")?,
                json_num(line, "w")?,
                json_num(line, "h")?,
            ));
        }
    }
    None
}

/// Screenshot the focused window via the compositor (it replies with the PNG
/// path), read the bytes, and DELETE the file — the remote view must not
/// litter ~/Pictures/screenshots.
fn take_screenshot() -> Option<(Vec<u8>, (u64, f64, f64, f64, f64))> {
    let win = focused_window()?;
    let reply = control_command(&format!("screenshot window {}", win.0)).ok()?;
    let path = reply.trim().strip_prefix("ok ")?.trim().to_string();
    let mut bytes = None;
    for _ in 0..5 {
        match std::fs::read(&path) {
            Ok(b) if !b.is_empty() => {
                bytes = Some(b);
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(60)),
        }
    }
    let _ = std::fs::remove_file(&path);
    Some((bytes?, win))
}

fn handle_ws(stream: TcpStream, pin: &str) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    // Unauthenticated clients can hold the socket only briefly.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[cce-remote] ws handshake failed ({peer}): {e}");
            return;
        }
    };
    // First frame MUST be `auth <pin>` — anything else (or a timeout, or a
    // wrong PIN) closes the connection before any input can be injected.
    let authed = matches!(
        ws.read(),
        Ok(tungstenite::Message::Text(t))
            if t.strip_prefix("auth ").map(str::trim) == Some(pin)
    );
    if !authed {
        eprintln!("[cce-remote] auth failed: {peer}");
        let _ = ws.send(tungstenite::Message::Text("auth fail".into()));
        let _ = ws.close(None);
        return;
    }
    let _ = ws.get_ref().set_read_timeout(None);
    let _ = ws.send(tungstenite::Message::Text("auth ok".into()));
    println!("[cce-remote] client connected: {peer}");
    loop {
        match ws.read() {
            Ok(msg) => {
                if let tungstenite::Message::Text(text) = msg {
                    if text.trim() == "wl" {
                        // Window-list request: the one message with a reply.
                        let _ = ws.send(tungstenite::Message::Text(window_list_json()));
                    } else if let Some((coords, btn)) = text
                        .strip_prefix("tap ")
                        .map(|r| (r, "left"))
                        .or_else(|| text.strip_prefix("tapr ").map(|r| (r, "right")))
                    {
                        // Window-view tap: absolute move + click.
                        let mut it = coords.split_ascii_whitespace();
                        if let (Some(Ok(x)), Some(Ok(y))) =
                            (it.next().map(str::parse::<f64>), it.next().map(str::parse::<f64>))
                        {
                            let _ = control_command(&format!("pointer-move-to {x:.1} {y:.1}"));
                            let _ = control_command(&format!("pointer-click {btn}"));
                        }
                    } else if let Some(cmd) = translate(&text) {
                        let _ = control_command(&cmd);
                    }
                }
            }
            Err(_) => break,
        }
    }
    println!("[cce-remote] client disconnected: {peer}");
}

/// MJPEG stream of the focused window: multipart/x-mixed-replace with one
/// JPEG part per grim capture (region = the focused window's layout rect,
/// re-resolved every few frames so the stream follows focus). ~3 fps for a
/// full-size window — the screencopy dominates, not the encode. Runs until
/// the client closes the socket. PIN via X-Pin header or ?pin= query (an
/// <img src> can't carry headers).
fn handle_stream(mut stream: TcpStream, request_head: &str, pin: &str) {
    let pin_ok = request_head.lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        lower.starts_with("x-pin:") && l[6..].trim() == pin
    }) || request_head
        .split_whitespace()
        .nth(1)
        .is_some_and(|target| target.contains(&format!("pin={pin}")));
    if !pin_ok {
        let _ = write!(stream, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return;
    }
    // Primary: persistent damage-driven screencopy (idle = zero frames,
    // active = compositor-paced). Fallback: the original grim loop.
    let rect = || focused_window().map(|(_, x, y, w, h)| (x as i32, y as i32, w as i32, h as i32));
    match screencopy::stream_mjpeg(&mut stream, rect) {
        Ok(()) => return, // client disconnected
        Err(e) => eprintln!("[cce-remote] screencopy stream failed ({e}), falling back to grim"),
    }
    let mut win = focused_window();
    let mut tick = 0u32;
    loop {
        if tick % 4 == 0 {
            if let Some(w) = focused_window() {
                win = Some(w);
            }
        }
        tick = tick.wrapping_add(1);
        let Some((_, x, y, w, h)) = win else {
            std::thread::sleep(Duration::from_millis(400));
            continue;
        };
        let out = std::process::Command::new("grim")
            .args([
                "-g",
                &format!("{},{} {}x{}", x as i32, y as i32, w as i32, h as i32),
                "-t", "jpeg", "-q", "65", "-s", "0.5", "-",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() && o.stdout.starts_with(&[0xff, 0xd8]) => {
                let part = format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    o.stdout.len()
                );
                if stream.write_all(part.as_bytes()).is_err()
                    || stream.write_all(&o.stdout).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return; // client gone — the loop (and grim spawning) stops
                }
            }
            _ => std::thread::sleep(Duration::from_millis(400)),
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn handle_http(mut stream: TcpStream, request_head: &str, pin: &str) {
    if request_head.starts_with("GET /stream") {
        handle_stream(stream, request_head, pin);
        return;
    }
    // /shot: the focused window's screenshot, PIN-gated via the X-Pin header
    // (the page fetch()es it — an <img src> couldn't carry a header).
    if request_head.starts_with("GET /shot") {
        let pin_ok = request_head.lines().any(|l| {
            let lower = l.to_ascii_lowercase();
            lower.starts_with("x-pin:") && l[6..].trim() == pin
        });
        if !pin_ok {
            let _ = write!(stream, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        match take_screenshot() {
            Some((bytes, (id, x, y, w, h))) => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nX-Win: {id} {x} {y} {w} {h}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len(),
                );
                let _ = stream.write_all(&bytes);
            }
            None => {
                let _ = write!(stream, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        }
        return;
    }
    let ok = request_head.starts_with("GET / ") || request_head.starts_with("GET /index.html ");
    let (status, body) = if ok {
        ("200 OK", INDEX_HTML)
    } else {
        ("404 Not Found", "not found")
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
}

fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|a| a.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let pin = match load_or_create_pin() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[cce-remote] cannot read/create PIN file {:?}: {e}", pin_path());
            std::process::exit(1);
        }
    };
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[cce-remote] cannot bind port {port}: {e}");
            std::process::exit(1);
        }
    };
    println!("[cce-remote] serving on http://0.0.0.0:{port} (control socket: {})", control_socket_path());
    println!("[cce-remote] pairing PIN: {pin}   (stored in {:?})", pin_path());

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let pin = pin.clone();
        std::thread::spawn(move || {
            // Peek the request head without consuming it, so a WS upgrade can
            // be handed to tungstenite with the handshake bytes intact.
            let mut buf = [0u8; 1024];
            let n = match stream.peek(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => return,
            };
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            if head.starts_with("GET /ws") {
                handle_ws(stream, &pin);
            } else {
                // Consume the request before replying (keeps curl happy).
                let mut sink = [0u8; 1024];
                let mut s = stream;
                let _ = s.read(&mut sink);
                handle_http(s, &head, &pin);
            }
        });
    }
}
