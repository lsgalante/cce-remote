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
mod winstream;

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

/// Whether `candidate` is the pairing PIN.
///
/// An empty PIN never authorizes anything. `load_or_create_pin` cannot produce
/// one — it regenerates on an empty file — but that is a property of a
/// different function, and if it ever stopped holding, a bare `X-Pin:` header
/// or an `auth ` frame with nothing after it would authenticate every request.
/// Gate on the dangerous state here rather than trusting the caller.
fn pin_matches(candidate: &str, pin: &str) -> bool {
    !pin.is_empty() && candidate == pin
}

/// The WebSocket auth gate: the FIRST frame must be `auth <pin>`. Everything
/// else — a wrong PIN, a different verb, an input event sent before pairing —
/// closes the connection, so no input can be injected unauthenticated.
fn auth_frame_ok(frame: &str, pin: &str) -> bool {
    frame
        .strip_prefix("auth ")
        .is_some_and(|candidate| pin_matches(candidate.trim(), pin))
}

/// PIN carried by an `X-Pin` header. The header name is matched
/// case-insensitively (HTTP field names are), the value is not.
fn header_pin_ok(request_head: &str, pin: &str) -> bool {
    request_head.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("x-pin:")
            .then(|| line["x-pin:".len()..].trim())
            .is_some_and(|candidate| pin_matches(candidate, pin))
    })
}

/// PIN carried as a `?pin=` query parameter — needed because an `<img src>`
/// cannot send headers, so `/stream` has no other way to authenticate.
///
/// Parsed as an actual parameter rather than searched for as a substring: the
/// old `target.contains("pin=<pin>")` also accepted `?notpin=<pin>` and
/// `?pin=<pin>trailing-garbage`. Neither is exploitable without already knowing
/// the PIN, but "close enough to the right string" is not a check.
fn query_pin_ok(request_head: &str, pin: &str) -> bool {
    request_head
        .split_whitespace()
        .nth(1)
        .and_then(|target| target.split_once('?'))
        .is_some_and(|(_, query)| {
            query
                .split('&')
                .any(|kv| kv.strip_prefix("pin=").is_some_and(|c| pin_matches(c, pin)))
        })
}

/// `f64::from_str` accepts "NaN" / "inf" / "infinity", and `{:.2}` formats them
/// straight back out, so without this a frame of `m NaN NaN` would reach the
/// compositor's pointer math verbatim. Reject rather than clamp: no legitimate
/// frame from the page contains one.
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// Translate one WS frame into the control-socket commands it means. Returns
/// None for frames that don't parse — they're dropped, never forwarded raw
/// (the WS payload is untrusted; only these fixed shapes reach the
/// compositor). A list rather than one command because a window-view tap is
/// one frame and two commands; keeping that here rather than inline at the
/// call site is what makes this the single place input is validated.
fn translate(frame: &str) -> Option<Vec<String>> {
    let mut it = frame.split_ascii_whitespace();
    let cmd = match it.next()? {
        "m" => {
            let dx = finite(it.next()?.parse().ok()?)?;
            let dy = finite(it.next()?.parse().ok()?)?;
            format!("pointer-move-by {dx:.2} {dy:.2}")
        }
        "s" => {
            let dy = finite(it.next()?.parse().ok()?)?;
            let dx = finite(it.next().unwrap_or("0").parse().ok()?)?;
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
        // Window-view tap: absolute move, then click. The one frame that means
        // two commands — hence the Vec return.
        verb @ ("tap" | "tapr") => {
            let btn = if verb == "tapr" { "right" } else { "left" };
            let x = finite(it.next()?.parse().ok()?)?;
            let y = finite(it.next()?.parse().ok()?)?;
            return Some(vec![
                format!("pointer-move-to {x:.1} {y:.1}"),
                format!("pointer-click {btn}"),
            ]);
        }
        // Named commands, individually whitelisted — never pass-through.
        "cmd" => match it.next()? {
            "restart-compositor" => "restart-compositor".to_string(),
            _ => return None,
        },
        _ => return None,
    };
    Some(vec![cmd])
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
        Ok(tungstenite::Message::Text(t)) if auth_frame_ok(&t, pin)
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
                    } else if text.trim() == "pl" {
                        // Pointer location (view mode's cursor marker):
                        // "x=N y=N" → "ploc N N".
                        if let Ok(reply) = control_command("pointer-location") {
                            let coords: String = reply
                                .split_whitespace()
                                .filter_map(|kv| kv.strip_prefix("x=").or_else(|| kv.strip_prefix("y=")))
                                .collect::<Vec<_>>()
                                .join(" ");
                            if !coords.is_empty() {
                                let _ = ws.send(tungstenite::Message::Text(format!("ploc {coords}")));
                            }
                        }
                    } else if let Some(cmds) = translate(&text) {
                        // Every input-bearing frame goes through translate() —
                        // `wl` and `pl` above are the only exceptions, and they
                        // send fixed commands with no caller-supplied content.
                        for cmd in cmds {
                            let _ = control_command(&cmd);
                        }
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
    // Header OR query: an <img src> cannot carry a header, so /stream accepts
    // the PIN in the URL. /shot does not — see handle_http.
    let pin_ok = header_pin_ok(request_head, pin) || query_pin_ok(request_head, pin);
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
    // Source preference: the compositor's window stream (per-window damage,
    // follows focus, works off-screen) → screencopy (output damage) → grim.
    match winstream::stream_mjpeg(&mut stream) {
        Ok(()) => return, // client disconnected
        Err(e) => eprintln!("[cce-remote] window stream unavailable ({e}), trying screencopy"),
    }
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
        // Header only: the page fetch()es this one, so unlike /stream there is
        // no reason to let the PIN travel in a URL (where it lands in logs).
        if !header_pin_ok(request_head, pin) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // `translate` is the security boundary of this crate: it is the only thing
    // between an untrusted WebSocket frame and a control socket that can move
    // the pointer and type into whatever the user has focused. These tests
    // cover the two halves of that job — the fixed shapes it accepts, and
    // everything it must refuse — because a regression here is not a wrong
    // pixel, it is remote input injection.

    /// A frame expected to mean exactly one command.
    fn one(frame: &str) -> String {
        let cmds = translate(frame).expect("frame should translate");
        assert_eq!(cmds.len(), 1, "{frame:?} yielded {cmds:?}, expected one command");
        cmds.into_iter().next().unwrap()
    }

    // ---- the pairing PIN ----
    //
    // The other half of the security model: translate() decides what a paired
    // client may say, these decide who is paired at all. Both are reachable by
    // anyone who can open a socket to this port.

    const PIN: &str = "123456";

    fn head(lines: &[&str]) -> String {
        format!("{}\r\n\r\n", lines.join("\r\n"))
    }

    #[test]
    fn ws_auth_requires_exactly_auth_then_pin() {
        assert!(auth_frame_ok("auth 123456", PIN));
        assert!(auth_frame_ok("auth   123456  ", PIN)); // value is trimmed
        for frame in [
            "auth 123457",      // wrong PIN
            "auth 12345",       // prefix of it
            "auth 1234567",     // superstring of it
            "auth ",            // empty candidate
            "auth",             // no separator
            "AUTH 123456",      // verb is case-sensitive
            "auth123456",
            " auth 123456",     // must be the whole frame, unprefixed
            "m 1 2",            // an input event before pairing
            "",
        ] {
            assert!(!auth_frame_ok(frame, PIN), "{frame:?} must not authenticate");
        }
    }

    #[test]
    fn an_empty_pin_authorizes_nothing() {
        // load_or_create_pin() regenerates on an empty file, so this should be
        // unreachable — which is exactly why it is worth pinning. A truncated
        // PIN file must fail closed, not open.
        assert!(!auth_frame_ok("auth ", ""));
        assert!(!auth_frame_ok("auth", ""));
        assert!(!header_pin_ok(&head(&["GET /shot HTTP/1.1", "X-Pin:"]), ""));
        assert!(!header_pin_ok(&head(&["GET /shot HTTP/1.1", "X-Pin: "]), ""));
        assert!(!query_pin_ok(&head(&["GET /stream?pin= HTTP/1.1"]), ""));
    }

    #[test]
    fn x_pin_header_is_matched_case_insensitively_by_name_only() {
        for name in ["X-Pin", "x-pin", "X-PIN", "x-PiN"] {
            let h = head(&["GET /shot HTTP/1.1", &format!("{name}: {PIN}"), "Host: x"]);
            assert!(header_pin_ok(&h, PIN), "{name} should be accepted");
        }
        // Value whitespace is trimmed; the value itself must match exactly.
        assert!(header_pin_ok(&head(&["GET /shot HTTP/1.1", "X-Pin:   123456  "]), PIN));
        for bad in ["X-Pin: 123457", "X-Pin: 12345", "X-Pin: 1234567", "X-Pin:", "X-Pinx: 123456"] {
            let h = head(&["GET /shot HTTP/1.1", bad]);
            assert!(!header_pin_ok(&h, PIN), "{bad:?} must not authenticate");
        }
        // No header at all.
        assert!(!header_pin_ok(&head(&["GET /shot HTTP/1.1", "Host: x"]), PIN));
    }

    #[test]
    fn query_pin_is_a_parameter_not_a_substring() {
        assert!(query_pin_ok(&head(&["GET /stream?pin=123456 HTTP/1.1"]), PIN));
        assert!(query_pin_ok(&head(&["GET /stream?pin=123456&g=7 HTTP/1.1"]), PIN));
        assert!(query_pin_ok(&head(&["GET /stream?g=7&pin=123456 HTTP/1.1"]), PIN));
        for bad in [
            "GET /stream?notpin=123456 HTTP/1.1",  // substring match used to pass this
            "GET /stream?pin=1234567 HTTP/1.1",    // and this
            "GET /stream?xpin=123456 HTTP/1.1",
            "GET /stream?pin=12345 HTTP/1.1",
            "GET /stream?pin= HTTP/1.1",
            "GET /stream?pin HTTP/1.1",
            "GET /stream HTTP/1.1",                // no query at all
            "GET /pin=123456 HTTP/1.1",            // in the PATH, not the query
        ] {
            assert!(!query_pin_ok(&head(&[bad]), PIN), "{bad:?} must not authenticate");
        }
    }

    #[test]
    fn the_two_http_gates_are_not_interchangeable() {
        // /stream takes either (an <img src> cannot send headers); /shot takes
        // the header only, so the PIN stays out of URLs and logs where it can.
        let query_only = head(&["GET /stream?pin=123456 HTTP/1.1", "Host: x"]);
        assert!(query_pin_ok(&query_only, PIN));
        assert!(!header_pin_ok(&query_only, PIN), "/shot must not accept a URL PIN");

        let header_only = head(&["GET /shot HTTP/1.1", "X-Pin: 123456"]);
        assert!(header_pin_ok(&header_only, PIN));
        assert!(!query_pin_ok(&header_only, PIN));
    }

    #[test]
    fn pointer_and_scroll_carry_fixed_precision() {
        assert_eq!(one("m 1 -2"), "pointer-move-by 1.00 -2.00");
        assert_eq!(one("m 0.126 -0.126"), "pointer-move-by 0.13 -0.13");
        // Exact .5 ties round half-to-even, not away from zero — sub-pixel
        // detail the page never notices, but pin it so a formatting change
        // shows up here rather than as drifting pointer feel.
        assert_eq!(one("m 0.125 0.135"), "pointer-move-by 0.12 0.14");
        // `s` takes dy first; dx is optional and defaults to 0.
        assert_eq!(one("s 5"), "pointer-scroll 5.000 0.000");
        assert_eq!(one("s 5 -1.5"), "pointer-scroll 5.000 -1.500");
    }

    #[test]
    fn buttons_map_to_press_release_click() {
        assert_eq!(one("b left down"), "pointer-press left");
        assert_eq!(one("b left up"), "pointer-release left");
        assert_eq!(one("b right click"), "pointer-click right");
        assert_eq!(one("b middle click"), "pointer-click middle");
    }

    #[test]
    fn keys_map_to_tap_and_hold() {
        assert_eq!(one("k 28"), "keypress 28");
        assert_eq!(one("kd 42"), "key-down 42");
        assert_eq!(one("ku 42"), "key-up 42");
    }

    #[test]
    fn unknown_verbs_are_dropped() {
        // Note the compositor's own command names: a frame naming one directly
        // must NOT be honored, or the whitelist would be decorative.
        for frame in [
            "", "   ", "x 1", "exit", "spawn foot", "reload",
            "pointer-click left", "keypress 28", "restart-compositor",
        ] {
            assert!(translate(frame).is_none(), "{frame:?} should be dropped");
        }
    }

    #[test]
    fn missing_or_malformed_arguments_are_dropped() {
        for frame in [
            "m", "m 1", "m a b", "m 1 b",
            "s", "s abc", "s 1 abc",
            "k", "k abc", "k -1", "k 1.5", "k 99999999999999999999",
            "kd", "ku",
            "b", "b left", "b left bogus", "b sideways click", "b LEFT click",
            "wf", "cmd",
            "tap", "tapr", "tap 1", "tapr 1", "tap a b", "tap 1 b",
        ] {
            assert!(translate(frame).is_none(), "{frame:?} should be dropped");
        }
    }

    #[test]
    fn non_finite_coordinates_are_dropped() {
        // The hazard is real rather than theoretical — this is exactly what
        // finite() exists to stop, and it is why parse().ok() alone is not
        // enough validation for a float.
        assert!("NaN".parse::<f64>().is_ok());
        assert_eq!(format!("{:.2}", "NaN".parse::<f64>().unwrap()), "NaN");
        assert_eq!(format!("{:.2}", "inf".parse::<f64>().unwrap()), "inf");

        for frame in [
            "m NaN 1", "m 1 NaN", "m inf 0", "m -inf 0", "m 1 infinity",
            "s NaN", "s 1 inf", "s nan 0",
            "tap NaN 1", "tap 1 inf", "tapr -inf 0", "tapr 1 nan",
        ] {
            assert!(translate(frame).is_none(), "{frame:?} should be dropped");
        }
    }

    #[test]
    fn focus_target_is_restricted_to_safe_tokens() {
        assert_eq!(one("wf 12"), "focus-window 12");
        assert_eq!(one("wf org.cce.files"), "focus-window org.cce.files");
        assert_eq!(one("wf a-b_c:d.1"), "focus-window a-b_c:d.1");
        for frame in [
            "wf ../etc", "wf a/b", "wf a;b", "wf a$b", "wf a*b",
            "wf a'b", "wf a\"b", "wf a|b", "wf a&b", "wf a\\b",
        ] {
            assert!(translate(frame).is_none(), "{frame:?} should be dropped");
        }
        // safe_token's length bound, exercised on both sides.
        assert!(translate(&format!("wf {}", "a".repeat(128))).is_some());
        assert!(translate(&format!("wf {}", "a".repeat(129))).is_none());
    }

    #[test]
    fn named_commands_are_whitelisted_never_passed_through() {
        assert_eq!(one("cmd restart-compositor"), "restart-compositor");
        // Trailing junk is discarded, not appended.
        assert_eq!(one("cmd restart-compositor rm -rf"), "restart-compositor");
        for frame in ["cmd exit", "cmd reload", "cmd spawn foot", "cmd RESTART-COMPOSITOR"] {
            assert!(translate(frame).is_none(), "{frame:?} should be dropped");
        }
    }

    #[test]
    fn a_tap_is_one_frame_and_two_commands() {
        // The window-view tap: move the pointer somewhere absolute, then click
        // it. Both commands, in that order — a click without the move lands
        // wherever the pointer happened to be.
        assert_eq!(
            translate("tap 100 200.5").unwrap(),
            ["pointer-move-to 100.0 200.5", "pointer-click left"]
        );
        assert_eq!(
            translate("tapr 0 0").unwrap(),
            ["pointer-move-to 0.0 0.0", "pointer-click right"]
        );
        // Negative coords are legal: the layout origin is not the only anchor.
        assert_eq!(
            translate("tap -5.25 -0.04").unwrap(),
            ["pointer-move-to -5.2 -0.0", "pointer-click left"]
        );
        // Trailing junk is discarded, exactly as for the one-command verbs.
        assert_eq!(
            translate("tap 1 2 pointer-press left").unwrap(),
            ["pointer-move-to 1.0 2.0", "pointer-click left"]
        );
        // A partial tap must emit NOTHING — not a bare move, and above all not
        // a click at whatever position the pointer already had.
        assert!(translate("tap 1").is_none());
        assert!(translate("tap").is_none());
    }

    #[test]
    fn a_newline_can_never_smuggle_a_second_command() {
        // control_command() appends "\n", so an embedded newline in the output
        // would be a second command on the socket. split_ascii_whitespace()
        // eats it and every command is REBUILT from re-parsed values, so
        // trailing tokens are discarded rather than forwarded.
        assert_eq!(one("m 1 2\npointer-click left"), "pointer-move-by 1.00 2.00");
        assert_eq!(one("wf 12\nexit"), "focus-window 12");

        // The property that matters, over every shape the page can send plus
        // deliberate junk: whatever comes out is a single line.
        for frame in [
            "m 1 2\nexit", "m 1\n2", "s 1\nexit", "b left\nclick", "b left click\nexit",
            "k 28\nexit", "kd 42\nexit", "ku 42\nexit", "wf 1\nexit",
            "cmd restart-compositor\nexit", "m\t1\t2", "wf\n12", "  m   1   2  ",
        ] {
            for out in translate(frame).unwrap_or_default() {
                assert!(!out.contains('\n'), "{frame:?} produced a multi-line command: {out:?}");
                assert!(!out.contains('\r'), "{frame:?} produced a CR: {out:?}");
            }
        }
    }
}
