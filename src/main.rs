//! cce-remote — use a phone as a trackpad + keyboard for the cce desktop.
//!
//! One small server, no GUI: it serves the embedded page (a cce-ui app built
//! for the browser, `src/web`; see `site.rs`) over HTTP and HTTPS on the
//! tailnet (see `reachable`, `conn.rs`), accepts a WebSocket at
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
//! And from the server: `ti 1` / `ti 0` when a desktop text field opens /
//! closes (`textfield.rs`), so the page can offer its keyboard.
//!
//! Security model: pairing PIN. A persistent 6-digit PIN (generated on first
//! run, stored 0600 under ~/.config/cce/cce-remote.pin, printed at startup)
//! must arrive as the FIRST WebSocket frame (`auth <pin>`) before any input
//! event is accepted; anything else closes the connection. The page remembers
//! the PIN in localStorage after the first pairing.

use std::io::{Read, Write};
use std::net::TcpListener;

use conn::Conn;
use std::time::Duration;

use cce_core::ipc::ctl::{Request, WindowInfo};

mod agent;
mod conn;
mod screencopy;
mod site;
mod stream;
mod textfield;
mod tls;
mod winstream;

const DEFAULT_PORT: u16 = 17017;

/// How often an idle `/ws` connection checks whether a desktop text field
/// opened, closed or changed — the latency of the "tap to type" prompt and of
/// the strip's confirmation of what was typed. Short, because the strip's
/// text waits on it: at 100ms a fast typist's strip visibly trailed. 50
/// wakeups a second per connected phone is nothing.
const FIELD_POLL: Duration = Duration::from_millis(20);

/// How long `/click` waits for the click to open a text field before
/// answering no. The page is frozen inside the tap for this long whenever a
/// click opens nothing, so it is kept short: activations measured 1-16ms
/// after the click (cce-ui, shadow session, 2026-10-17), and one that misses
/// the window only costs the user the "tap to type" bar instead.
const CLICK_FIELD_WAIT: Duration = Duration::from_millis(150);

/// `$WAYLAND_DISPLAY`, else `wayland-0`: run from a service or a login shell
/// with no display set, this still finds the session's compositor.
fn display() -> String {
    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string())
}

fn control_socket_path() -> String {
    cce_core::ipc::ctl::control_socket_for(Some(&display()))
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
    cce_core::ipc::ctl::send_to(&control_socket_path(), cmd, None)
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

/// Loopback, or an address Tailscale hands out: 100.64.0.0/10 and
/// fd7a:115c:a1e0::/48.
fn tailnet_or_loopback(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback() || (s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0)
        }
    }
}

/// Whether a connection may be served at all, before any PIN is asked for.
///
/// The PIN crosses the wire in clear — HTTP, a WS frame, a `/stream` URL — so
/// on a network someone else can watch, anyone there who sees one pairing
/// owns the keyboard. By default, then, only the tailnet is served: WireGuard
/// carries it encrypted end to end, and loopback never leaves the machine.
/// Both ends are checked. The PEER must be a tailnet (or loopback) address,
/// and so must the LOCAL address it reached: Linux will accept a packet for
/// the tailnet address arriving on the Wi-Fi interface, and a LAN peer
/// spoofing a 100.x source cannot finish the handshake, since the reply is
/// routed into the tunnel. `lan` (`--lan` / `CCE_REMOTE_LAN=1`) serves every
/// interface, the old behaviour, for a network you trust.
fn reachable(peer: std::net::IpAddr, local: std::net::IpAddr, lan: bool) -> bool {
    lan || (tailnet_or_loopback(peer) && tailnet_or_loopback(local))
}

/// Failed PIN attempts allowed back-to-back from one peer before it must wait.
/// Sized for a human mistyping a PIN, not for a client retry loop.
const PIN_ATTEMPT_BURST: f64 = 5.0;
/// Sustained rate a peer recovers attempts at: one per 30s. That caps a
/// brute-force at ~2/min, so walking a 6-digit space takes on the order of a
/// year rather than the couple of hours an unthrottled LAN socket allows.
const PIN_ATTEMPT_REFILL_PER_SEC: f64 = 1.0 / 30.0;
/// Backstop on the tracking table so the limiter cannot itself be turned into
/// a memory-exhaustion vector by cycling source addresses.
const PIN_MAX_TRACKED_PEERS: usize = 4096;
/// Failed attempts allowed back-to-back across ALL peers. The per-peer budget
/// alone is per address, and anyone who can claim many addresses (a /22 LAN
/// has a thousand) gets a budget for each — which brought walking the PIN
/// space down from a year to hours. Room for a few devices mistyping at once.
const PIN_GLOBAL_BURST: f64 = 20.0;
/// What every peer together recovers: the same one per 30s a single peer
/// does, so the year-long walk holds however many addresses do the guessing.
const PIN_GLOBAL_REFILL_PER_SEC: f64 = 1.0 / 30.0;

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last: std::time::Instant,
}

/// Per-peer token bucket over failed PIN attempts.
///
/// The PIN is ~20 bits and every gate compares it with `==`, so the thing that
/// actually makes it a credential is that an attacker cannot try often. Only
/// FAILURES consume tokens — a paired client reconnecting its stream, which
/// the page does on every hiccup, must never be throttled — and a success
/// clears the peer's record entirely.
///
/// `now` is a parameter rather than read inside, so the behavior is testable
/// without sleeping.
///
/// Behind the per-peer buckets sits one GLOBAL bucket every failure also
/// drains, so guessing from many addresses buys no more attempts than
/// guessing from one. The price: whoever drains it locks out everyone,
/// a paired phone included, until it refills. That is the trade — a denial
/// of a trackpad for a while, against keyboard control of the desktop — and
/// with the default reach (loopback and the tailnet, see `reachable`) only
/// your own devices can drain it.
struct RateLimiter {
    peers: std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, Bucket>>,
    global: std::sync::Mutex<Bucket>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            peers: std::sync::Mutex::new(std::collections::HashMap::new()),
            global: std::sync::Mutex::new(Bucket {
                tokens: PIN_GLOBAL_BURST,
                last: std::time::Instant::now(),
            }),
        }
    }

    fn refilled(bucket: Bucket, now: std::time::Instant) -> Bucket {
        Self::refilled_at(bucket, now, PIN_ATTEMPT_REFILL_PER_SEC, PIN_ATTEMPT_BURST)
    }

    fn refilled_at(bucket: Bucket, now: std::time::Instant, rate: f64, burst: f64) -> Bucket {
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        Bucket {
            tokens: (bucket.tokens + elapsed * rate).min(burst),
            last: now,
        }
    }

    fn global_refilled(bucket: Bucket, now: std::time::Instant) -> Bucket {
        Self::refilled_at(bucket, now, PIN_GLOBAL_REFILL_PER_SEC, PIN_GLOBAL_BURST)
    }

    /// May this peer attempt a PIN right now? Does not consume anything —
    /// a correct PIN costs nothing.
    fn allow(&self, ip: std::net::IpAddr, now: std::time::Instant) -> bool {
        if Self::global_refilled(*self.global.lock().unwrap(), now).tokens < 1.0 {
            return false;
        }
        let peers = self.peers.lock().unwrap();
        match peers.get(&ip) {
            Some(&b) => Self::refilled(b, now).tokens >= 1.0,
            None => true,
        }
    }

    /// Charge this peer — and the global budget — for a wrong PIN.
    fn record_failure(&self, ip: std::net::IpAddr, now: std::time::Instant) {
        {
            let mut g = self.global.lock().unwrap();
            let mut b = Self::global_refilled(*g, now);
            b.tokens = (b.tokens - 1.0).max(0.0);
            *g = b;
        }
        let mut peers = self.peers.lock().unwrap();
        // A fully refilled bucket is indistinguishable from an absent one, so
        // dropping those keeps the table proportional to peers currently being
        // penalized rather than to every peer ever seen.
        peers.retain(|_, b| Self::refilled(*b, now).tokens < PIN_ATTEMPT_BURST);
        if peers.len() >= PIN_MAX_TRACKED_PEERS && !peers.contains_key(&ip) {
            // At capacity: evict whoever is closest to having recovered.
            if let Some(&victim) = peers
                .iter()
                .max_by(|a, b| a.1.tokens.total_cmp(&b.1.tokens))
                .map(|(k, _)| k)
            {
                peers.remove(&victim);
            }
        }
        let entry = peers
            .entry(ip)
            .or_insert(Bucket { tokens: PIN_ATTEMPT_BURST, last: now });
        let mut b = Self::refilled(*entry, now);
        b.tokens = (b.tokens - 1.0).max(0.0);
        *entry = b;
    }

    /// A correct PIN clears the peer's record.
    fn record_success(&self, ip: std::net::IpAddr) {
        self.peers.lock().unwrap().remove(&ip);
    }
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
/// compositor). A list rather than one command so a verb can expand to
/// several commands without the expansion living inline at the call site (the
/// retired view-mode tap did: absolute move, then click); keeping every
/// expansion here is what makes this the single place input is validated.
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
        // Named commands, individually whitelisted — never pass-through.
        // The page's System menu. Each name maps to a FIXED command,
        // matched against a literal: none takes anything from the frame
        // (the focused window is the compositor's to resolve), and the wire
        // names are deliberately not the compositor's own, so a frame naming
        // a compositor command (`cmd exit`) is dropped like any other.
        "cmd" => match it.next()? {
            "restart-focused-app" => "restart-app".to_string(),
            "close-focused" => "close".to_string(),
            "fullscreen" => "fullscreen".to_string(),
            "overview" => "overview".to_string(),
            "screenshot" => "screenshot".to_string(),
            "reload-config" => "reload".to_string(),
            "lock" => "lock".to_string(),
            "display-off" => "idle display off".to_string(),
            "sleep" => "idle sleep".to_string(),
            "log-out" => "exit".to_string(),
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
    let reply = control_command(&Request::WindowsJson.to_string()).unwrap_or_default();
    let objs: Vec<&str> = reply.lines().filter(|l| l.trim_start().starts_with('{')).collect();
    format!("windows [{}]", objs.join(","))
}

/// A window as (id, x, y, w, h) in layout px.
type WindowBox = (u64, f64, f64, f64, f64);

/// The focused app window.
fn focused_window() -> Option<WindowBox> {
    let reply = control_command(&Request::WindowsJson.to_string()).ok()?;
    let w = WindowInfo::parse_all(&reply).into_iter().find(|w| w.focused && w.mode != "Status")?;
    Some((w.id, w.x as f64, w.y as f64, w.w as f64, w.h as f64))
}

/// Screenshot the focused window via the compositor (it replies with the PNG
/// path), read the bytes, and DELETE the file — the remote view must not
/// litter ~/Pictures/screenshots.
fn take_screenshot() -> Option<(Vec<u8>, WindowBox)> {
    let win = focused_window()?;
    let reply = control_command(&Request::ScreenshotWindow { id: win.0 }.to_string()).ok()?;
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

fn handle_ws(stream: Conn, pin: &str, ip: std::net::IpAddr, limiter: &RateLimiter) {
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
    if !limiter.allow(ip, std::time::Instant::now()) {
        // Close WITHOUT "auth fail": that message makes the page drop its
        // stored PIN and prompt, so sending it here would punish a correctly
        // paired client for someone else's guessing on the same address. It
        // reconnects on close and succeeds once the bucket refills.
        eprintln!("[cce-remote] auth rate-limited: {peer}");
        let _ = ws.close(None);
        return;
    }
    let authed = matches!(
        ws.read(),
        Ok(tungstenite::Message::Text(t)) if auth_frame_ok(&t, pin)
    );
    if !authed {
        limiter.record_failure(ip, std::time::Instant::now());
        eprintln!("[cce-remote] auth failed: {peer}");
        let _ = ws.send(tungstenite::Message::Text("auth fail".into()));
        let _ = ws.close(None);
        return;
    }
    limiter.record_success(ip);
    // Short reads from here on: an idle read returns every FIELD_POLL so the
    // loop can tell the page a text field opened or closed. tungstenite keeps
    // a frame that a timeout split, so input is unaffected.
    let _ = ws.get_ref().set_read_timeout(Some(FIELD_POLL));
    let _ = ws.send(tungstenite::Message::Text("auth ok".into()));
    let _ = ws.send(tungstenite::Message::Text(format!("ver {}", site::version())));
    println!("[cce-remote] client connected: {peer}");
    // What this page was last told; a fresh page assumes no field is open,
    // and has seen no contents.
    let mut field_sent = false;
    let mut contents_seen = 0u64;
    // The Agent view's replies (`agent`), made on threads of their own: a
    // window's answer or `cce-code-agent --list` can take most of a second,
    // and the trackpad must not wait behind it.
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<String>();
    loop {
        let msg = match ws.read() {
            Ok(msg) => Some(msg),
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                None
            }
            Err(_) => break,
        };
        if let Some(open) = textfield::notice(field_sent, std::time::Instant::now()) {
            // Fixed strings, nothing from the frame: not a translate() concern.
            let notice = if open { "ti 1" } else { "ti 0" };
            if ws.send(tungstenite::Message::Text(notice.into())).is_err() {
                break;
            }
            field_sent = open;
        }
        // The field's contents, for the page's echo strip: `tt <cursor>
        // <anchor> <text>` (byte offsets; the text is the rest of the frame,
        // spaces and newlines included), or a bare `tt` for none. Outbound
        // only — nothing here reaches the control socket.
        if let Some((seen, contents)) = textfield::contents_since(contents_seen) {
            let frame = match contents {
                Some(c) => format!("tt {} {} {}", c.cursor, c.anchor, c.text),
                None => "tt".to_string(),
            };
            if ws.send(tungstenite::Message::Text(frame)).is_err() {
                break;
            }
            contents_seen = seen;
        }
        while let Ok(reply) = agent_rx.try_recv() {
            if ws.send(tungstenite::Message::Text(reply)).is_err() {
                break;
            }
        }
        if let Some(tungstenite::Message::Text(text)) = msg {
            if let Some(frame) = text.strip_prefix("ag ") {
                // Code Agent's socket, not the compositor's: agent.rs checks
                // and rebuilds the request, as translate() does for commands.
                let (frame, tx) = (frame.to_string(), agent_tx.clone());
                std::thread::spawn(move || {
                    if let Some(reply) = agent::handle(&frame) {
                        let _ = tx.send(reply);
                    }
                });
            } else if text.trim() == "wl" {
                // Window-list request: the one message with a reply.
                let _ = ws.send(tungstenite::Message::Text(window_list_json()));
            } else if text.trim() == "pl" {
                // Pointer location (view mode's cursor marker):
                // "x=N y=N" → "ploc N N".
                if let Some((x, y)) = control_command(&Request::PointerLocation.to_string())
                    .ok()
                    .and_then(|reply| cce_core::ipc::ctl::parse_pointer_location(&reply))
                {
                    let _ = ws.send(tungstenite::Message::Text(format!("ploc {x} {y}")));
                }
            } else if let Some(cmds) = translate(&text) {
                // Every input-bearing frame goes through translate() —
                // `wl` and `pl` above are the only exceptions, and they
                // send fixed commands with no caller-supplied content.
                if text.starts_with("cmd ") {
                    // A System-menu action may reply only when done
                    // (`restart-app` waits up to 10s for the app to exit,
                    // `lock` for every output to show the lock): off this
                    // thread, so the trackpad does not stall behind it.
                    std::thread::spawn(move || {
                        for cmd in cmds {
                            let _ = control_command(&cmd);
                        }
                    });
                } else {
                    for cmd in cmds {
                        let _ = control_command(&cmd);
                    }
                }
            }
        }
    }
    println!("[cce-remote] client disconnected: {peer}");
}

/// MJPEG stream of the focused window: multipart/x-mixed-replace, one JPEG
/// part per frame taken from the same latest-wins slot `/wstream` uses (the
/// producer picks the source and follows focus). Fixed 560/q60, no acks — the
/// curl-debuggable endpoint, not the page's path. Runs until the client closes
/// the socket. PIN via X-Pin header or ?pin= query (an <img src> can't carry
/// headers).
fn handle_stream(mut stream: Conn, request_head: &str, pin: &str, ip: std::net::IpAddr, limiter: &RateLimiter) {
    if !limiter.allow(ip, std::time::Instant::now()) {
        let _ = write!(stream, "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    // Header OR query: an <img src> cannot carry a header, so /stream accepts
    // the PIN in the URL. /shot does not — see handle_http.
    let pin_ok = header_pin_ok(request_head, pin) || query_pin_ok(request_head, pin);
    if !pin_ok {
        limiter.record_failure(ip, std::time::Instant::now());
        let _ = write!(stream, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    limiter.record_success(ip);
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return;
    }
    // Source selection (winstream → screencopy → grim) lives in the producer;
    // this endpoint is the curl-debuggable MJPEG view over the same slot the
    // page's ack-clocked /wstream uses.
    let slot = stream::Slot::new();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    stream::spawn_producer(std::sync::Arc::clone(&slot), std::sync::Arc::clone(&stop));
    stream::run_mjpeg_sender(&mut stream, &slot);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The page's live view: ack-clocked latest-wins frame delivery over a
/// dedicated WebSocket. Same first-frame `auth <pin>` gate as the input WS —
/// and a separate socket on purpose: frames are 30-150KB and input events are
/// bytes, so sharing one TCP stream would head-of-line-block pointer motion
/// behind every frame on a slow link.
fn handle_wstream(stream: Conn, pin: &str, ip: std::net::IpAddr, limiter: &RateLimiter) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[cce-remote] wstream handshake failed ({peer}): {e}");
            return;
        }
    };
    if !limiter.allow(ip, std::time::Instant::now()) {
        let _ = ws.close(None);
        return;
    }
    let authed = matches!(
        ws.read(),
        Ok(tungstenite::Message::Text(t)) if auth_frame_ok(&t, pin)
    );
    if !authed {
        limiter.record_failure(ip, std::time::Instant::now());
        eprintln!("[cce-remote] wstream auth failed: {peer}");
        let _ = ws.close(None);
        return;
    }
    limiter.record_success(ip);
    let _ = ws.send(tungstenite::Message::Text("auth ok".into()));
    let slot = stream::Slot::new();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    stream::spawn_producer(std::sync::Arc::clone(&slot), std::sync::Arc::clone(&stop));
    stream::run_ws_sender(&mut ws, &slot);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = ws.close(None);
}

/// The `X-Pin` header gate shared by `/shot` and `/click`: rate limit, then
/// the PIN. Answers 429 or 403 itself and returns false when the request may
/// not proceed. Header only: neither is loaded as an `<img src>`, so unlike
/// `/stream` there is no reason to let the PIN travel in a URL (where it
/// lands in logs).
fn header_gate(stream: &mut Conn, request_head: &str, pin: &str, ip: std::net::IpAddr, limiter: &RateLimiter) -> bool {
    if !limiter.allow(ip, std::time::Instant::now()) {
        let _ = write!(stream, "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return false;
    }
    if !header_pin_ok(request_head, pin) {
        limiter.record_failure(ip, std::time::Instant::now());
        let _ = write!(stream, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return false;
    }
    limiter.record_success(ip);
    true
}

fn handle_http(mut stream: Conn, request_head: &str, pin: &str, ip: std::net::IpAddr, limiter: &RateLimiter) {
    if request_head.starts_with("GET /stream") {
        handle_stream(stream, request_head, pin, ip, limiter);
        return;
    }
    // /click: the page's left tap, answering whether it opened a desktop
    // text field. The page waits on it synchronously because iOS raises the
    // keyboard only from inside the tap's own handler; a WS round trip
    // returns after the tap is over. The click itself is the fixed
    // `b left click` through translate(); the request carries nothing else.
    if request_head.starts_with("POST /click ") {
        if !header_gate(&mut stream, request_head, pin, ip, limiter) {
            return;
        }
        let before = textfield::opened();
        for cmd in translate("b left click").unwrap_or_default() {
            let _ = control_command(&cmd);
        }
        let opened = if textfield::wait_opened(before, CLICK_FIELD_WAIT) { "1" } else { "0" };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{opened}"
        );
        return;
    }
    // /shot: the focused window's screenshot, PIN-gated via the X-Pin header.
    // A debug endpoint now — the page's live view rides /wstream and nothing
    // in the page fetches this.
    if request_head.starts_with("GET /shot") {
        if !header_gate(&mut stream, request_head, pin, ip, limiter) {
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
    let Some(path) = get_path(request_head) else {
        respond(&mut stream, "404 Not Found", "text/plain", "", b"not found");
        return;
    };
    // The DE's look for the cce-ui page: PIN-gated, as /shot is.
    let private = |path: &str| path == "/boot" || path == "/config" || path.starts_with("/font/");
    if private(path) {
        if !header_gate(&mut stream, request_head, pin, ip, limiter) {
            return;
        }
        // A font is the same bytes for the server's life: let the phone keep it.
        const KEEP: &str = "Cache-Control: private, max-age=86400\r\n";
        match path {
            "/boot" => respond(&mut stream, "200 OK", "text/plain; charset=utf-8", "Cache-Control: no-store\r\n", site::boot().as_bytes()),
            "/config" => respond(&mut stream, "200 OK", "text/plain; charset=utf-8", "Cache-Control: no-store\r\n", site::config().as_bytes()),
            _ => match path["/font/".len()..].parse().ok().and_then(site::font) {
                Some((bytes, ctype)) => respond(&mut stream, "200 OK", ctype, KEEP, &bytes),
                None => respond(&mut stream, "404 Not Found", "text/plain", "", b"not found"),
            },
        }
        return;
    }
    let gzip = request_head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("accept-encoding:") && l.contains("gzip")
    });
    match site::asset(path, gzip) {
        Some(reply) => respond(&mut stream, "200 OK", reply.ctype, &reply.headers, &reply.body),
        None => respond(&mut stream, "404 Not Found", "text/plain", "", b"not found"),
    }
}

/// The path of a `GET` request, without its query.
fn get_path(request_head: &str) -> Option<&str> {
    let target = request_head.strip_prefix("GET ")?.split_whitespace().next()?;
    Some(target.split('?').next().unwrap_or(target))
}

/// A whole response: `headers` are further header lines, each ending in CRLF.
fn respond(stream: &mut Conn, status: &str, ctype: &str, headers: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n", body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn main() {
    let mut port = DEFAULT_PORT;
    let mut lan = std::env::var("CCE_REMOTE_LAN").is_ok_and(|v| v == "1");
    for arg in std::env::args().skip(1) {
        if arg == "--lan" {
            lan = true;
        } else if let Ok(p) = arg.parse::<u16>() {
            port = p;
        } else {
            eprintln!("[cce-remote] usage: cce-remote [--lan] [port]");
            std::process::exit(2);
        }
    }
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
    let limiter = std::sync::Arc::new(RateLimiter::new());
    textfield::spawn_watcher();
    if lan {
        println!("[cce-remote] serving EVERY interface on :{port} (--lan: the PIN crosses the network in clear)");
    } else {
        println!("[cce-remote] serving loopback and the tailnet on :{port} (--lan or CCE_REMOTE_LAN=1 for every interface)");
    }
    println!("[cce-remote] control socket: {}", control_socket_path());
    println!("[cce-remote] pairing PIN: {pin}   (stored in {:?})", pin_path());

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Identify the peer once, here: every PIN gate is rate-limited per
        // source address, and a socket whose peer cannot be resolved cannot be
        // held accountable for its guesses, so it is refused rather than
        // exempted.
        let Ok(ip) = stream.peer_addr().map(|a| a.ip()) else { continue };
        let Ok(local) = stream.local_addr().map(|a| a.ip()) else { continue };
        if !reachable(ip, local, lan) {
            // Say why rather than just dropping it, so a phone still holding
            // the old LAN address learns where to go; nothing past this line
            // is reachable from here, the request is never parsed.
            eprintln!("[cce-remote] refused {ip} -> {local}: not the tailnet (--lan to serve it)");
            std::thread::spawn(move || {
                let mut s = stream;
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut sink = [0u8; 1024];
                let _ = s.read(&mut sink);
                let body = "cce-remote serves the tailnet only: open it at this machine's Tailscale address.\n";
                let _ = write!(
                    s,
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
            continue;
        }
        // Input events and stream acks are tiny and latency-critical; Nagle
        // would batch them behind delayed ACKs.
        let _ = stream.set_nodelay(true);
        let pin = pin.clone();
        let limiter = std::sync::Arc::clone(&limiter);
        std::thread::spawn(move || {
            // Plain or TLS by the first byte (conn.rs). The request head is
            // read once and kept, so a WS upgrade is handed to tungstenite
            // with its handshake bytes intact.
            let Ok(mut stream) = Conn::accept(stream) else { return };
            let Ok(head) = stream.read_head() else { return };
            if head.starts_with("GET /wstream") {
                // before /ws: "GET /ws" is a prefix of this
                handle_wstream(stream, &pin, ip, &limiter);
            } else if head.starts_with("GET /ws") {
                handle_ws(stream, &pin, ip, &limiter);
            } else {
                stream.consume_head();
                handle_http(stream, &head, &pin, ip, &limiter);
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

    // ---- rate limiting ----
    //
    // What actually makes a 6-digit PIN a credential: not the comparison, but
    // that a peer cannot try often. Time is injected, so none of this sleeps.

    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
    }

    #[test]
    fn a_peer_gets_a_burst_then_must_wait() {
        let rl = RateLimiter::new();
        let t0 = Instant::now();
        let peer = ip(10);

        for i in 0..PIN_ATTEMPT_BURST as u32 {
            assert!(rl.allow(peer, t0), "attempt {i} should be allowed");
            rl.record_failure(peer, t0);
        }
        assert!(!rl.allow(peer, t0), "the burst must be exhausted");

        // Still locked out just short of the refill interval, allowed after it.
        assert!(!rl.allow(peer, t0 + Duration::from_secs(29)));
        assert!(rl.allow(peer, t0 + Duration::from_secs(31)));
    }

    #[test]
    fn recovery_is_capped_at_the_burst_size() {
        // Asserted on refilled() directly rather than through the public API:
        // record_failure() prunes recovered peers and re-creates them at full,
        // which masks a missing cap end-to-end. (It did — this test passed
        // against a build with the .min() removed until it was written this
        // way.) Idle time must not bank attempts.
        let t0 = Instant::now();
        let drained = Bucket { tokens: 0.0, last: t0 };
        // Full recovery takes BURST * 30s = 150s; well past that, nothing accrues.
        for idle in [300u64, 3600, 86_400] {
            let b = RateLimiter::refilled(drained, t0 + Duration::from_secs(idle));
            assert_eq!(
                b.tokens, PIN_ATTEMPT_BURST,
                "{idle}s idle banked {} attempts", b.tokens
            );
        }
        // Partial recovery is proportional, not all-or-nothing.
        let b = RateLimiter::refilled(drained, t0 + Duration::from_secs(60));
        assert!(b.tokens > 1.0);
        let b = RateLimiter::refilled(drained, t0 + Duration::from_secs(15));
        assert!(b.tokens < 1.0, "half an interval must not buy a whole attempt");
    }

    #[test]
    fn peers_are_limited_independently() {
        let rl = RateLimiter::new();
        let t0 = Instant::now();
        let (attacker, phone) = (ip(20), ip(21));
        for _ in 0..PIN_ATTEMPT_BURST as u32 {
            rl.record_failure(attacker, t0);
        }
        assert!(!rl.allow(attacker, t0));
        assert!(rl.allow(phone, t0), "one peer's guessing must not lock out another");
    }

    #[test]
    fn a_correct_pin_costs_nothing_and_clears_the_record() {
        let rl = RateLimiter::new();
        let t0 = Instant::now();
        let peer = ip(30);

        // The page reconnects its stream on every hiccup, each time presenting
        // a correct PIN. If that consumed budget it would throttle itself.
        for _ in 0..1000 {
            assert!(rl.allow(peer, t0));
        }

        for _ in 0..(PIN_ATTEMPT_BURST as u32 - 1) {
            rl.record_failure(peer, t0);
        }
        rl.record_success(peer);
        for _ in 0..PIN_ATTEMPT_BURST as u32 {
            assert!(rl.allow(peer, t0), "success should restore the full burst");
            rl.record_failure(peer, t0);
        }
    }

    #[test]
    fn the_tracking_table_does_not_grow_without_bound() {
        let rl = RateLimiter::new();
        let t0 = Instant::now();

        // Recovered peers carry no information and must not be retained.
        for i in 0..200u8 {
            rl.record_failure(ip(i), t0);
        }
        assert!(rl.peers.lock().unwrap().len() > 1);
        rl.record_failure(ip(255), t0 + Duration::from_secs(3600));
        assert_eq!(
            rl.peers.lock().unwrap().len(),
            1,
            "fully refilled peers should have been pruned"
        );

        // And the table is capped even when every entry is still penalized.
        let mut rl2 = RateLimiter::new();
        // One failure per peer is enough to create (and hold) an entry.
        for i in 0..(PIN_MAX_TRACKED_PEERS + 50) {
            rl2.record_failure(IpAddr::V4(Ipv4Addr::from((i as u32).to_be_bytes())), t0);
        }
        assert!(
            rl2.peers.get_mut().unwrap().len() <= PIN_MAX_TRACKED_PEERS,
            "table exceeded its cap"
        );
    }

    #[test]
    fn many_addresses_share_one_global_budget() {
        // One failure from each of a LAN's worth of addresses: every per-peer
        // bucket still has four left, and still the guessing stops.
        let rl = RateLimiter::new();
        let t0 = Instant::now();
        for i in 0..PIN_GLOBAL_BURST as u8 {
            assert!(rl.allow(ip(i), t0), "global attempt {i} should be allowed");
            rl.record_failure(ip(i), t0);
        }
        assert!(!rl.allow(ip(200), t0), "a fresh address must not bring a fresh budget");
        // The documented price: a paired phone waits too, then recovers.
        assert!(!rl.allow(ip(201), t0 + Duration::from_secs(29)));
        assert!(rl.allow(ip(201), t0 + Duration::from_secs(31)));
        // A success does not refund the global budget.
        rl.record_success(ip(0));
        assert!(!rl.allow(ip(0), t0));
    }

    #[test]
    fn only_loopback_and_the_tailnet_are_served_by_default() {
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        let me_ts = v4(100, 105, 214, 102);
        let me_lan = v4(192, 168, 68, 55);
        let phone_ts = v4(100, 90, 1, 2);
        let phone_lan = v4(192, 168, 68, 77);
        let lo = v4(127, 0, 0, 1);

        assert!(reachable(phone_ts, me_ts, false));
        assert!(reachable(lo, lo, false));
        assert!(!reachable(phone_lan, me_lan, false), "the LAN is opt-in");
        // Linux takes a packet for the tailnet address off the Wi-Fi; the
        // peer is what gives it away.
        assert!(!reachable(phone_lan, me_ts, false));
        // A tailnet-shaped source aimed at the LAN address is not the tunnel.
        assert!(!reachable(phone_ts, me_lan, false));
        // Just outside 100.64.0.0/10 on either side.
        assert!(!reachable(v4(100, 63, 255, 255), me_ts, false));
        assert!(!reachable(v4(100, 128, 0, 1), me_ts, false));
        // Tailscale's IPv6 range, and v4 seen through a dual-stack socket.
        let ts6: IpAddr = "fd7a:115c:a1e0::dd34:d667".parse().unwrap();
        let other6: IpAddr = "fd7a:115c:a1e1::1".parse().unwrap();
        assert!(reachable(ts6, ts6, false));
        assert!(!reachable(other6, ts6, false));
        let mapped: IpAddr = "::ffff:100.90.1.2".parse().unwrap();
        assert!(reachable(mapped, me_ts, false));

        assert!(reachable(phone_lan, me_lan, true), "--lan serves everything");
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
            // retired 2026-08-23: view-mode taps are plain trackpad clicks,
            // so the verbs left the whitelist rather than lingering as
            // unused injection surface
            "tap 100 200", "tapr 5 5",
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
        // The System menu: each name to its fixed command.
        for (name, cmd) in [
            ("restart-focused-app", "restart-app"),
            ("close-focused", "close"),
            ("fullscreen", "fullscreen"),
            ("overview", "overview"),
            ("screenshot", "screenshot"),
            ("reload-config", "reload"),
            ("lock", "lock"),
            ("display-off", "idle display off"),
            ("sleep", "idle sleep"),
            ("log-out", "exit"),
            ("restart-compositor", "restart-compositor"),
        ] {
            assert_eq!(one(&format!("cmd {name}")), cmd);
        }
        // Restarting the focused app names no app: a target in the frame is
        // dropped, never forwarded (`restart-app foot` would restart foot).
        assert_eq!(one("cmd restart-focused-app foot"), "restart-app");
        assert_eq!(one("cmd restart-compositor"), "restart-compositor");
        // Trailing junk is discarded, not appended.
        assert_eq!(one("cmd restart-compositor rm -rf"), "restart-compositor");
        for frame in [
            "cmd exit", "cmd reload", "cmd spawn foot", "cmd RESTART-COMPOSITOR",
            // The compositor's own names for the menu's actions.
            "cmd restart-app", "cmd close", "cmd idle", "cmd lock-screen", "cmd exit force",
        ] {
            assert!(translate(frame).is_none(), "{frame:?} should be dropped");
        }
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
            "tap 1 2\nexit",
        ] {
            for out in translate(frame).unwrap_or_default() {
                assert!(!out.contains('\n'), "{frame:?} produced a multi-line command: {out:?}");
                assert!(!out.contains('\r'), "{frame:?} produced a CR: {out:?}");
            }
        }
    }
}
