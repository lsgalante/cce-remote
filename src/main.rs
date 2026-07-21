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

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::Duration;

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

/// One persistent line-oriented connection to the compositor's control socket.
struct Control {
    stream: BufReader<UnixStream>,
}

impl Control {
    fn connect() -> std::io::Result<Self> {
        let s = UnixStream::connect(control_socket_path())?;
        Ok(Self { stream: BufReader::new(s) })
    }

    fn send(&mut self, cmd: &str) -> std::io::Result<()> {
        self.stream.get_mut().write_all(cmd.as_bytes())?;
        self.stream.get_mut().write_all(b"\n")?;
        // Drain the reply line so the socket never backs up. Errors in the
        // reply text are ignored — input injection is fire-and-forget.
        let mut reply = String::new();
        self.stream.read_line(&mut reply)?;
        Ok(())
    }
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
        _ => return None,
    };
    Some(cmd)
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
    let mut control = match Control::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[cce-remote] control socket unavailable: {e}");
            let _ = ws.close(None);
            return;
        }
    };
    println!("[cce-remote] client connected: {peer}");
    loop {
        match ws.read() {
            Ok(msg) => {
                if let tungstenite::Message::Text(text) = msg {
                    if let Some(cmd) = translate(&text) {
                        if control.send(&cmd).is_err() {
                            // Compositor went away; try one reconnect.
                            match Control::connect() {
                                Ok(c) => control = c,
                                Err(_) => break,
                            }
                            let _ = control.send(&cmd);
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    println!("[cce-remote] client disconnected: {peer}");
}

fn handle_http(mut stream: TcpStream, request_head: &str) {
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
                handle_http(s, &head);
            }
        });
    }
}
