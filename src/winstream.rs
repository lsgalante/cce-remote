//! Consumer for the compositor's window-stream socket — the first-choice
//! frame source. The compositor pushes damage-driven RGBA frames of the
//! focused window (`window focused` subscription follows focus server-side,
//! works off-viewport/occluded, and a truly idle window sends nothing but a
//! ≤15s keepalive). Frames land in the latest-wins slot (`stream::Slot`);
//! encoding happens at send time, not here.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

fn stream_socket_path() -> String {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    format!("/tmp/cce-stream-{display}.sock")
}

pub struct Reader {
    sock: BufReader<UnixStream>,
}

impl Reader {
    pub fn connect() -> Result<Self, String> {
        let path = stream_socket_path();
        let sock = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
        // The compositor keepalives every ≤15s; 20s of silence means it's
        // gone. This timeout is also what bounds how long a stopped
        // producer thread lingers.
        sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let mut sock = BufReader::new(sock);
        sock.get_mut()
            .write_all(b"window focused\n")
            .map_err(|e| e.to_string())?;
        Ok(Self { sock })
    }

    /// The next frame. Err = the source is unavailable/broke (caller falls
    /// back or gives up) — including a read timeout, which given the
    /// keepalive cadence means a dead compositor, not an idle window.
    pub fn next(&mut self) -> Result<crate::stream::Payload, String> {
        let mut header = String::new();
        loop {
            header.clear();
            if self.sock.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
                return Err("stream socket closed".into());
            }
            let mut it = header.split_ascii_whitespace();
            if it.next() != Some("frame") {
                continue;
            }
            let (Some(w), Some(h), Some(len)) = (
                it.next().and_then(|v| v.parse::<u32>().ok()),
                it.next().and_then(|v| v.parse::<u32>().ok()),
                it.next().and_then(|v| v.parse::<u32>().ok()),
            ) else {
                return Err(format!("bad frame header: {header:?}"));
            };
            if len != w.saturating_mul(h).saturating_mul(4) || len > MAX_FRAME_BYTES {
                return Err(format!("implausible frame: {header:?}"));
            }
            let mut rgba = vec![0u8; len as usize];
            self.sock.read_exact(&mut rgba).map_err(|e| e.to_string())?;
            return Ok(crate::stream::Payload::Raw {
                data: rgba,
                w,
                h,
                stride: w * 4,
                rgb: (0, 1, 2),
            });
        }
    }
}
