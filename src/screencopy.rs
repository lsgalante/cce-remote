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

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};


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
    /// returns Ok(None) so the caller can force a keepalive frame. The pixels
    /// are returned RAW (copied out of the shm slot, which is reused);
    /// encoding happens at send time so dropped frames cost nothing.
    pub fn next_frame(
        &mut self,
        rect: (i32, i32, i32, i32),
        use_damage: bool,
        timeout: Duration,
    ) -> Result<Option<crate::stream::Payload>, String> {
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
        let (w, h, stride, format) = slot.meta;
        // byte offsets of R,G,B within each little-endian 32-bit pixel
        let rgb = match format {
            wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2usize, 1usize, 0usize),
            _ => (0usize, 1usize, 2usize), // Xbgr8888 / Abgr8888
        };
        let size = (stride * h) as usize;
        Ok(Some(crate::stream::Payload::Raw {
            data: slot.map[..size].to_vec(),
            w,
            h,
            stride,
            rgb,
        }))
    }
}

/// Shared by both raw frame sources (screencopy shm and the compositor's
/// window-stream RGBA): box-downscale 32-bit pixels to ≤ `max_edge` and JPEG
/// them at `quality`. `rgb_at` gives the byte offsets of R,G,B within each
/// 4-byte pixel. Called per SENT frame, with the (edge, quality) the
/// adaptation ladder picked for the link.
pub fn downscale_encode(
    data: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    (ri, gi, bi): (usize, usize, usize),
    max_edge: u32,
    quality: u8,
) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 || (stride * h) as usize > data.len() {
        return Err("bad frame dimensions".into());
    }
    let f = ((w.max(h) + max_edge - 1) / max_edge).max(1);
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
                    r += data[px + ri] as u32;
                    g += data[px + gi] as u32;
                    b += data[px + bi] as u32;
                }
            }
            rgb.push((r / fsq) as u8);
            rgb.push((g / fsq) as u8);
            rgb.push((b / fsq) as u8);
        }
    }
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(&rgb, ow as u16, oh as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| e.to_string())?;
    Ok(out)
}
