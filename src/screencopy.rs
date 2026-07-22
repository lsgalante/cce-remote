//! Persistent wlr-screencopy capture — the robust replacement for forking
//! `grim` per frame.
//!
//! One long-lived Wayland connection per stream: each frame is a
//! `capture_output_region` of the focused window's rect, throttled by
//! `copy_with_damage` — the compositor withholds the frame until the region
//! actually changes, so an idle desktop costs nothing and an active one
//! streams at compositor pace instead of fork/exec pace. Shm buffers are
//! reused across frames; pixels are box-downscaled and JPEG-encoded
//! in-process (`jpeg-encoder`).
//!
//! Single-output assumption: the region is passed in output-local logical
//! coordinates, which equals layout coordinates when the (only) output sits
//! at 0,0 — true for this DE's eDP-1 setup, same assumption grim ran under.

use std::io::Write;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// Longest edge of the encoded frame, in px — the downscale factor is chosen
/// per frame to stay under this.
const MAX_EDGE: u32 = 700;
const JPEG_QUALITY: u8 = 70;

#[derive(Default)]
struct CapState {
    // per-frame handshake state, reset before each request
    buffer_meta: Option<(u32, u32, u32, wl_shm::Format)>, // w, h, stride, format
    buffer_done: bool,
    ready: bool,
    failed: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CapState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(CapState: ignore wl_shm::WlShm);
delegate_noop!(CapState: ignore wl_output::WlOutput);
delegate_noop!(CapState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CapState: ignore wl_buffer::WlBuffer);
delegate_noop!(CapState: ignore ZwlrScreencopyManagerV1);

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CapState {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format, width, height, stride } => {
                if let WEnum::Value(f) = format {
                    // prefer 32-bit formats we know how to swizzle
                    if matches!(
                        f,
                        wl_shm::Format::Xrgb8888
                            | wl_shm::Format::Argb8888
                            | wl_shm::Format::Xbgr8888
                            | wl_shm::Format::Abgr8888
                    ) {
                        state.buffer_meta = Some((width, height, stride, f));
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

struct ShmSlot {
    _file: std::fs::File,
    map: memmap2::MmapMut,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    meta: (u32, u32, u32, wl_shm::Format),
}

pub struct CaptureSession {
    conn: Connection,
    queue: EventQueue<CapState>,
    qh: QueueHandle<CapState>,
    state: CapState,
    manager: ZwlrScreencopyManagerV1,
    output: wl_output::WlOutput,
    shm: wl_shm::WlShm,
    slot: Option<ShmSlot>,
}

impl CaptureSession {
    pub fn new() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, queue) =
            registry_queue_init::<CapState>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .map_err(|e| format!("screencopy v3 unavailable: {e}"))?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let output: wl_output::WlOutput =
            globals.bind(&qh, 1..=4, ()).map_err(|e| e.to_string())?;
        Ok(Self {
            conn,
            queue,
            qh,
            state: CapState::default(),
            manager,
            output,
            shm,
            slot: None,
        })
    }

    /// Pump the event queue until `pred(state)` holds or the deadline passes.
    /// Returns false on timeout (Wayland connection still healthy).
    fn wait_until(
        &mut self,
        deadline: Instant,
        pred: impl Fn(&CapState) -> bool,
    ) -> Result<bool, String> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|e| e.to_string())?;
            if pred(&self.state) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.conn.flush().map_err(|e| e.to_string())?;
            if let Some(guard) = self.queue.prepare_read() {
                let fd = guard.connection_fd();
                let mut fds = [rustix::event::PollFd::new(
                    &fd,
                    rustix::event::PollFlags::IN,
                )];
                let remaining = deadline.saturating_duration_since(Instant::now());
                let ms = remaining.as_millis().min(200) as i32;
                let _ = rustix::event::poll(&mut fds, ms.max(1));
                let readable = fds[0].revents().contains(rustix::event::PollFlags::IN);
                drop(fds);
                if readable {
                    let _ = guard.read();
                } // else: drop the guard without reading and re-check
            }
        }
    }

    fn ensure_slot(&mut self, meta: (u32, u32, u32, wl_shm::Format)) -> Result<(), String> {
        if let Some(s) = &self.slot {
            if s.meta == meta {
                return Ok(());
            }
        }
        if let Some(old) = self.slot.take() {
            old.buffer.destroy();
            old.pool.destroy();
        }
        let (w, h, stride, format) = meta;
        let size = (stride * h) as usize;
        let dir = std::path::Path::new("/dev/shm");
        let dir = if dir.is_dir() { dir } else { std::path::Path::new("/tmp") };
        let path = dir.join(format!("cce-remote-shm-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        // unlink immediately — the fd keeps it alive, nothing lingers on disk
        let _ = std::fs::remove_file(&path);
        file.set_len(size as u64).map_err(|e| e.to_string())?;
        let map = unsafe { memmap2::MmapMut::map_mut(&file) }.map_err(|e| e.to_string())?;
        let pool = self.shm.create_pool(file.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            stride as i32,
            format,
            &self.qh,
            (),
        );
        self.slot = Some(ShmSlot { _file: file, map, pool, buffer, meta });
        Ok(())
    }

    /// Capture one frame of `rect` (output-local logical px). With
    /// `use_damage`, blocks until the region changes or `timeout` — a timeout
    /// returns Ok(None) so the caller can force a keepalive frame. The frame
    /// is returned already JPEG-encoded.
    pub fn next_frame(
        &mut self,
        rect: (i32, i32, i32, i32),
        use_damage: bool,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        self.state = CapState::default();
        let frame = self.manager.capture_output_region(
            1, // overlay the cursor — the remote wants to see it
            &self.output,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            &self.qh,
            (),
        );
        // Phase 1: buffer negotiation (always fast).
        let ok = self.wait_until(Instant::now() + Duration::from_secs(5), |s| {
            s.buffer_done || s.failed
        })?;
        let Some(meta) = self.state.buffer_meta else {
            frame.destroy();
            return Err("no usable shm format offered".into());
        };
        if !ok || self.state.failed {
            frame.destroy();
            return if self.state.failed { Err("capture failed".into()) } else { Ok(None) };
        }
        self.ensure_slot(meta)?;
        let buffer = &self.slot.as_ref().unwrap().buffer;
        if use_damage {
            frame.copy_with_damage(buffer);
        } else {
            frame.copy(buffer);
        }
        // Phase 2: damage-gated (this is the idle throttle).
        let ok = self.wait_until(Instant::now() + timeout, |s| s.ready || s.failed)?;
        let failed = self.state.failed;
        if !ok || failed {
            frame.destroy();
            return if failed { Err("copy failed".into()) } else { Ok(None) };
        }
        frame.destroy();
        let slot = self.slot.as_ref().unwrap();
        Ok(Some(encode_jpeg(&slot.map, slot.meta)?))
    }
}

/// Box-downscale the 32-bit shm pixels to ≤ MAX_EDGE and encode as JPEG.
fn encode_jpeg(
    map: &memmap2::MmapMut,
    (w, h, stride, format): (u32, u32, u32, wl_shm::Format),
) -> Result<Vec<u8>, String> {
    // byte offsets of R,G,B within each little-endian 32-bit pixel
    let (ri, gi, bi) = match format {
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2usize, 1usize, 0usize),
        _ => (0usize, 1usize, 2usize), // Xbgr8888 / Abgr8888
    };
    let f = ((w.max(h) + MAX_EDGE - 1) / MAX_EDGE).max(1);
    let (ow, oh) = (w / f, h / f);
    let mut rgb = Vec::with_capacity((ow * oh * 3) as usize);
    let fsq = (f * f) as u32;
    for oy in 0..oh {
        for ox in 0..ow {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for sy in 0..f {
                let row = ((oy * f + sy) * stride) as usize;
                for sx in 0..f {
                    let px = row + ((ox * f + sx) * 4) as usize;
                    r += map[px + ri] as u32;
                    g += map[px + gi] as u32;
                    b += map[px + bi] as u32;
                }
            }
            rgb.push((r / fsq) as u8);
            rgb.push((g / fsq) as u8);
            rgb.push((b / fsq) as u8);
        }
    }
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder
        .encode(&rgb, ow as u16, oh as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Drive `session` frames into an MJPEG multipart writer until the client
/// disconnects. `rect_of_focused` re-resolves the focused window (layout px).
pub fn stream_mjpeg(
    tcp: &mut std::net::TcpStream,
    rect_of_focused: impl Fn() -> Option<(i32, i32, i32, i32)>,
) -> Result<(), String> {
    let mut session = CaptureSession::new()?;
    let mut rect = rect_of_focused();
    let mut rect_at = Instant::now();
    let mut force_full = true; // first frame immediately; also after timeouts
    loop {
        if rect_at.elapsed() > Duration::from_millis(500) {
            if let Some(r) = rect_of_focused() {
                if Some(r) != rect {
                    force_full = true; // focus moved: don't wait for damage
                }
                rect = Some(r);
            }
            rect_at = Instant::now();
        }
        let Some(r) = rect else {
            std::thread::sleep(Duration::from_millis(400));
            rect = rect_of_focused();
            continue;
        };
        // 20s damage timeout doubles as a keepalive: the forced frame's write
        // is what detects a silently-gone client.
        let timeout = if force_full { Duration::from_secs(5) } else { Duration::from_secs(20) };
        match session.next_frame(r, !force_full, timeout) {
            Ok(Some(jpeg)) => {
                force_full = false;
                let head = format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg.len()
                );
                if tcp.write_all(head.as_bytes()).is_err()
                    || tcp.write_all(&jpeg).is_err()
                    || tcp.write_all(b"\r\n").is_err()
                {
                    return Ok(()); // client gone
                }
                // cap runaway damage bursts (~30 fps)
                std::thread::sleep(Duration::from_millis(33));
            }
            Ok(None) => force_full = true,
            Err(e) => return Err(e),
        }
    }
}
