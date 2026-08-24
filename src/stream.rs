//! Live-view delivery: latest-frame-wins, clocked by client acks.
//!
//! The failure this module exists to prevent: the original MJPEG path pushed
//! every frame, in order, into a blocking TCP write. Nothing between the
//! compositor and the phone ever dropped a frame, so the kernel's send buffer
//! (hundreds of KB, ~10-30 frames) became a queue — the moment wifi throughput
//! dipped below the frame rate, the queue filled, and every frame the phone
//! showed was queue-depth old. Latency accumulated and never drained: "fine at
//! first, unusable after a short time".
//!
//! The fix is sender-side flow control, the same shape VNC/RDP use:
//!
//! - A `Slot` holds only the NEWEST frame from the source (winstream →
//!   screencopy → grim, tried in that order by `spawn_producer`). Overwriting
//!   is the drop point: stale frames cease to exist before they cost anything.
//! - `run_ws_sender` sends one frame, then waits for the page's `n` ack before
//!   sending the newest frame available. In-flight is capped at ONE frame, so
//!   degraded wifi costs frame RATE, never growing latency.
//! - Encoding happens at send time, only for frames actually sent, at a
//!   (max_edge, jpeg_quality) picked by `adapt()` from the measured send→ack
//!   time — readable resolution when the link allows, graceful degradation
//!   when it doesn't.
//!
//! `/stream` (MJPEG over HTTP) remains as a curl-debuggable endpoint via
//! `run_mjpeg_sender`, thin over the same slot; the page no longer uses it.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One captured frame. `Raw` is encoded at send time; `Jpeg` (grim fallback)
/// is passed through as-is, so adaptation does not apply to it.
#[derive(Clone)]
pub enum Payload {
    Raw { data: Vec<u8>, w: u32, h: u32, stride: u32, rgb: (usize, usize, usize) },
    Jpeg(Vec<u8>),
}

/// The latest-wins seam between the frame source and however many senders are
/// consuming it. `push` overwrites; senders track the last seq they delivered.
pub struct Slot {
    inner: Mutex<Inner>,
    cv: Condvar,
}

struct Inner {
    seq: u64,
    frame: Option<Payload>,
    alive: bool,
}

impl Slot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner { seq: 0, frame: None, alive: true }),
            cv: Condvar::new(),
        })
    }

    pub fn push(&self, p: Payload) {
        let mut g = self.inner.lock().unwrap();
        g.seq += 1;
        g.frame = Some(p);
        self.cv.notify_all();
    }

    /// The producer died (source unavailable/broke). Senders return, the
    /// client reconnects, and the fresh producer re-picks a source.
    pub fn close(&self) {
        self.inner.lock().unwrap().alive = false;
        self.cv.notify_all();
    }

    /// Newest frame with seq > `last`: Ok(Some) on a new frame, Ok(None) on
    /// timeout (window idle — sources force keepalive frames ≤20s), Err when
    /// the producer is gone.
    pub fn wait_newer(&self, last: u64, timeout: Duration) -> Result<Option<(u64, Payload)>, ()> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.seq > last {
                return Ok(Some((g.seq, g.frame.clone().unwrap())));
            }
            if !g.alive {
                return Err(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (ng, _) = self.cv.wait_timeout(g, deadline - now).unwrap();
            g = ng;
        }
    }
}

fn encode(p: &Payload, max_edge: u32, quality: u8) -> Result<Vec<u8>, String> {
    match p {
        Payload::Raw { data, w, h, stride, rgb } => {
            crate::screencopy::downscale_encode(data, *w, *h, *stride, *rgb, max_edge, quality)
        }
        Payload::Jpeg(b) => Ok(b.clone()),
    }
}

// ---- adaptation ------------------------------------------------------------

/// (max_edge px, jpeg quality), best first. Resolution is held as long as
/// possible — quality drops before size does — because the point of the live
/// view is READING the window.
pub const LADDER: &[(u32, u8)] = &[
    (1400, 68),
    (1120, 68),
    (1120, 55),
    (840, 58),
    (840, 46),
    (560, 48),
];
pub const START_LEVEL: usize = 1;

/// Pick the next ladder level from the smoothed send→ack time. Downgrades are
/// immediate (lag is being felt NOW); upgrades need `acks_since_change` of
/// stability so the level does not oscillate at a threshold. The dead band
/// between the two thresholds is the hysteresis.
pub fn adapt(level: usize, ewma_ms: f64, acks_since_change: u32) -> usize {
    if ewma_ms > 220.0 {
        (level + 1).min(LADDER.len() - 1)
    } else if ewma_ms < 90.0 && acks_since_change >= 10 {
        level.saturating_sub(1)
    } else {
        level
    }
}

// ---- the frame source ------------------------------------------------------

/// Source preference is unchanged from the MJPEG design: compositor window
/// stream (per-window damage, follows focus) → screencopy (output damage) →
/// grim. The producer owns the source; on source death it closes the slot.
pub fn spawn_producer(slot: Arc<Slot>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let result = match crate::winstream::Reader::connect() {
            Ok(mut r) => produce_winstream(&mut r, &slot, &stop),
            Err(e) => {
                eprintln!("[cce-remote] window stream unavailable ({e}), trying screencopy");
                match crate::screencopy::CaptureSession::new() {
                    Ok(mut s) => produce_screencopy(&mut s, &slot, &stop),
                    Err(e2) => {
                        eprintln!("[cce-remote] screencopy unavailable ({e2}), falling back to grim");
                        produce_grim(&slot, &stop)
                    }
                }
            }
        };
        if let Err(e) = result {
            if !stop.load(Ordering::Relaxed) {
                eprintln!("[cce-remote] frame source ended: {e}");
            }
        }
        slot.close();
    });
}

fn produce_winstream(
    r: &mut crate::winstream::Reader,
    slot: &Slot,
    stop: &AtomicBool,
) -> Result<(), String> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Blocks ≤20s: the compositor keepalives every ≤15s, so a stopped
        // sender's producer lingers at most one keepalive interval.
        slot.push(r.next()?);
    }
}

fn produce_screencopy(
    sess: &mut crate::screencopy::CaptureSession,
    slot: &Slot,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut rect: Option<(i32, i32, i32, i32)> = None;
    let mut rect_at: Option<Instant> = None;
    let mut force_full = true; // first frame immediately; also after timeouts
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if rect_at.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
            if let Some((_, x, y, w, h)) = crate::focused_window() {
                let r = (x as i32, y as i32, w as i32, h as i32);
                if Some(r) != rect {
                    force_full = true; // focus moved: don't wait for damage
                }
                rect = Some(r);
            }
            rect_at = Some(Instant::now());
        }
        let Some(r) = rect else {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        };
        // The 20s damage timeout doubles as the keepalive cadence.
        let timeout = if force_full { Duration::from_secs(5) } else { Duration::from_secs(20) };
        match sess.next_frame(r, !force_full, timeout) {
            Ok(Some(frame)) => {
                force_full = false;
                slot.push(frame);
                // cap runaway damage bursts (~30 fps)
                std::thread::sleep(Duration::from_millis(33));
            }
            Ok(None) => force_full = true,
            Err(e) => return Err(e),
        }
    }
}

fn produce_grim(slot: &Slot, stop: &AtomicBool) -> Result<(), String> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some((_, x, y, w, h)) = crate::focused_window() else {
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
                slot.push(Payload::Jpeg(o.stdout));
                std::thread::sleep(Duration::from_millis(350));
            }
            _ => std::thread::sleep(Duration::from_millis(400)),
        }
    }
}

// ---- senders ---------------------------------------------------------------

/// Ack-clocked delivery over the page's stream WebSocket. The page sends `n`
/// after RENDERING each frame — so the measured send→ack time covers network,
/// decode and paint, i.e. what the user actually experiences — and only then
/// does the newest frame go out.
pub fn run_ws_sender(ws: &mut tungstenite::WebSocket<std::net::TcpStream>, slot: &Slot) {
    let mut last_seq = 0u64;
    let mut level = START_LEVEL;
    let mut ewma_ms = 120.0f64;
    let mut acks_since_change = 0u32;
    let mut sent_at: Option<Instant> = None;
    // Acks can legitimately stop for a long time (iOS suspends the page when
    // backgrounded); pings distinguish suspended-but-alive from gone. The
    // browser answers pings in its network stack, JS not required.
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_secs(75)));
    let mut silent = 0u32;
    loop {
        // 1. wait for the ack of the previous frame
        loop {
            match ws.read() {
                Ok(tungstenite::Message::Text(t)) if t == "n" => break,
                Ok(tungstenite::Message::Close(_)) => return,
                Ok(_) => {
                    silent = 0; // pong: peer alive, keep waiting
                    continue;
                }
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    silent += 1;
                    if silent >= 2 {
                        return; // two silent windows with no pong: gone
                    }
                    if ws.send(tungstenite::Message::Ping(Vec::new())).is_err() {
                        return;
                    }
                    continue;
                }
                Err(_) => return,
            }
        }
        silent = 0;
        if let Some(t0) = sent_at.take() {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            ewma_ms = 0.7 * ewma_ms + 0.3 * ms;
            acks_since_change += 1;
            let next = adapt(level, ewma_ms, acks_since_change);
            if next != level {
                level = next;
                acks_since_change = 0;
            }
        }
        // 2. newest frame (blocks while the window is idle; sources force
        //    keepalive frames ≤20s, so this wakes regularly)
        let (seq, payload) = loop {
            match slot.wait_newer(last_seq, Duration::from_secs(30)) {
                Ok(Some(x)) => break x,
                Ok(None) => {
                    if ws.send(tungstenite::Message::Ping(Vec::new())).is_err() {
                        return;
                    }
                }
                Err(()) => return,
            }
        };
        last_seq = seq;
        let (edge, quality) = LADDER[level];
        let Ok(jpeg) = encode(&payload, edge, quality) else { continue };
        sent_at = Some(Instant::now());
        if ws.send(tungstenite::Message::Binary(jpeg)).is_err() {
            return;
        }
    }
}

/// MJPEG over the slot, fixed 560/q60 — kept as the curl-debuggable endpoint.
/// Latest-wins still applies (each iteration encodes only the newest frame),
/// but without acks the TCP buffer can still queue a few frames; the page no
/// longer uses this path.
pub fn run_mjpeg_sender(tcp: &mut std::net::TcpStream, slot: &Slot) {
    let mut last = 0u64;
    loop {
        match slot.wait_newer(last, Duration::from_secs(25)) {
            Ok(Some((seq, p))) => {
                last = seq;
                let Ok(jpeg) = encode(&p, 560, 60) else { continue };
                let head = format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg.len()
                );
                if tcp.write_all(head.as_bytes()).is_err()
                    || tcp.write_all(&jpeg).is_err()
                    || tcp.write_all(b"\r\n").is_err()
                {
                    return; // client gone
                }
            }
            Ok(None) => continue, // idle; dead clients surface on the next write
            Err(()) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The slot IS the fix: if it ever queues instead of overwriting, the
    // unbounded-lag failure mode comes back silently.

    fn raw(tag: u8) -> Payload {
        Payload::Raw { data: vec![tag; 4], w: 1, h: 1, stride: 4, rgb: (0, 1, 2) }
    }
    fn tag_of(p: &Payload) -> u8 {
        match p {
            Payload::Raw { data, .. } => data[0],
            Payload::Jpeg(b) => b[0],
        }
    }

    #[test]
    fn slot_overwrites_never_queues() {
        let s = Slot::new();
        s.push(raw(1));
        s.push(raw(2));
        s.push(raw(3));
        // A consumer that fell behind gets the NEWEST frame, once — frames 1
        // and 2 are gone, not waiting their turn.
        let (seq, p) = s.wait_newer(0, Duration::from_millis(10)).unwrap().unwrap();
        assert_eq!(seq, 3);
        assert_eq!(tag_of(&p), 3);
        // Nothing newer: times out rather than re-delivering.
        assert!(s.wait_newer(seq, Duration::from_millis(10)).unwrap().is_none());
    }

    #[test]
    fn slot_close_wakes_and_errs() {
        let s = Slot::new();
        s.push(raw(9));
        let _ = s.wait_newer(0, Duration::from_millis(10)).unwrap().unwrap();
        s.close();
        assert!(s.wait_newer(99, Duration::from_secs(5)).is_err(), "close must wake, not time out");
    }

    #[test]
    fn slot_wakes_a_blocked_waiter() {
        let s = Slot::new();
        let s2 = Arc::clone(&s);
        let t = std::thread::spawn(move || s2.wait_newer(0, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(30));
        s.push(raw(7));
        let got = t.join().unwrap().unwrap().unwrap();
        assert_eq!(got.0, 1);
        assert_eq!(tag_of(&got.1), 7);
    }

    #[test]
    fn ladder_prefers_resolution_over_quality() {
        // The point of the live view is reading the window: stepping down the
        // ladder must drop quality before it drops size.
        for pair in LADDER.windows(2) {
            let ((e1, _), (e2, _)) = (pair[0], pair[1]);
            assert!(e2 <= e1, "ladder edge must be non-increasing: {pair:?}");
        }
        assert!(START_LEVEL < LADDER.len());
    }

    #[test]
    fn adapt_downgrades_immediately_upgrades_cautiously() {
        // Lag is felt now: no stability requirement to step down.
        assert_eq!(adapt(1, 300.0, 0), 2);
        // Upgrades need sustained headroom, or the level oscillates at the
        // threshold.
        assert_eq!(adapt(2, 50.0, 3), 2);
        assert_eq!(adapt(2, 50.0, 10), 1);
        // The dead band holds steady in both directions.
        assert_eq!(adapt(2, 150.0, 100), 2);
    }

    #[test]
    fn adapt_clamps_at_both_ends() {
        let worst = LADDER.len() - 1;
        assert_eq!(adapt(worst, 10_000.0, 0), worst);
        assert_eq!(adapt(0, 1.0, 1000), 0);
    }
}
