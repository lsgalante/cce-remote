//! Consumer for the compositor's window-stream socket — the first-choice
//! frame source. The compositor pushes damage-driven RGBA frames of the
//! focused window (`window focused` subscription follows focus server-side,
//! works off-viewport/occluded, and a truly idle window sends nothing but a
//! 15s keepalive). We downscale + JPEG each frame into the MJPEG response.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

fn stream_socket_path() -> String {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
    format!("/tmp/cce-stream-{display}.sock")
}

/// Bridge the compositor stream into `tcp` as MJPEG parts. Ok(()) = the HTTP
/// client went away; Err = the stream source is unavailable/broke (caller
/// falls back to screencopy).
pub fn stream_mjpeg(tcp: &mut std::net::TcpStream) -> Result<(), String> {
    let path = stream_socket_path();
    let sock = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    // The compositor keepalives every ≤15s; a 40s silence means it's gone.
    sock.set_read_timeout(Some(Duration::from_secs(40))).ok();
    let mut sock = BufReader::new(sock);
    sock.get_mut()
        .write_all(b"window focused\n")
        .map_err(|e| e.to_string())?;

    let mut header = String::new();
    loop {
        header.clear();
        if sock.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
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
        sock.read_exact(&mut rgba).map_err(|e| e.to_string())?;

        let jpeg = crate::screencopy::downscale_encode(&rgba, w, h, w * 4, (0, 1, 2))?;
        let part = format!(
            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        );
        if tcp.write_all(part.as_bytes()).is_err()
            || tcp.write_all(&jpeg).is_err()
            || tcp.write_all(b"\r\n").is_err()
        {
            return Ok(()); // HTTP client gone — unsubscribes by dropping the socket
        }
    }
}
