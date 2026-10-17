//! One accepted connection, plain or TLS, on the same port.
//!
//! The first byte decides: a TLS ClientHello begins with 0x16, which no HTTP
//! request does. So `https://` and `http://` share the port, and the classic
//! page keeps working at its old plain-HTTP address while the cce-ui app
//! (which needs a secure page) is served over TLS (`tls.rs`).
//!
//! The request head is read before anyone knows what the request is — a TLS
//! stream cannot be peeked the way a socket can — and kept for replay: reads
//! hand it out again first, so tungstenite still sees a WebSocket's whole
//! handshake.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use rustls::{ServerConnection, StreamOwned};

/// How long a client may take to open: the TLS handshake and the request head.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// A request head larger than this is not one this server answers.
const MAX_HEAD: usize = 16 * 1024;

enum Io {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

pub struct Conn {
    io: Io,
    replay: Vec<u8>,
    pos: usize,
}

impl Conn {
    /// Take an accepted socket: TLS if it opens with a ClientHello and there
    /// is a certificate to answer with, plain otherwise. A ClientHello with
    /// no certificate configured is refused.
    pub fn accept(tcp: TcpStream) -> io::Result<Conn> {
        tcp.set_read_timeout(Some(OPEN_TIMEOUT))?;
        let mut first = [0u8; 1];
        if tcp.peek(&mut first)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let io = if first[0] == 0x16 {
            let cfg = crate::tls::config().ok_or_else(|| io::Error::other("TLS client, but no certificate"))?;
            let session = ServerConnection::new(cfg).map_err(io::Error::other)?;
            Io::Tls(Box::new(StreamOwned::new(session, tcp)))
        } else {
            Io::Plain(tcp)
        };
        Ok(Conn { io, replay: Vec::new(), pos: 0 })
    }

    fn tcp(&self) -> &TcpStream {
        match &self.io {
            Io::Plain(s) => s,
            Io::Tls(s) => s.get_ref(),
        }
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.tcp().peer_addr()
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.tcp().set_read_timeout(d)
    }

    /// Read the request head (through its blank line), keep it for replay,
    /// and return it. Anything read past the head is kept as well.
    pub fn read_head(&mut self) -> io::Result<String> {
        let mut buf = [0u8; 4096];
        while !self.replay.windows(4).any(|w| w == b"\r\n\r\n") {
            if self.replay.len() > MAX_HEAD {
                return Err(io::Error::other("request head too large"));
            }
            let n = self.read_io(&mut buf)?;
            if n == 0 {
                break;
            }
            self.replay.extend_from_slice(&buf[..n]);
        }
        if self.replay.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(String::from_utf8_lossy(&self.replay).into_owned())
    }

    /// The request head has been handled: nothing to replay.
    pub fn consume_head(&mut self) {
        self.pos = self.replay.len();
    }

    fn read_io(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.io {
            Io::Plain(s) => s.read(buf),
            Io::Tls(s) => s.read(buf),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.replay.len() {
            let n = buf.len().min(self.replay.len() - self.pos);
            buf[..n].copy_from_slice(&self.replay[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.read_io(buf)
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.io {
            Io::Plain(s) => s.write(buf),
            Io::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.io {
            Io::Plain(s) => s.flush(),
            Io::Tls(s) => s.flush(),
        }
    }
}

impl Drop for Conn {
    /// A TLS peer is told the stream ended (close_notify), so a response
    /// read to EOF is not taken for a truncation.
    fn drop(&mut self) {
        if let Io::Tls(s) = &mut self.io {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connected pair over loopback: the server side as a `Conn`.
    fn pair(client_sends: &[u8]) -> (Conn, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.write_all(client_sends).unwrap();
        let (server, _) = listener.accept().unwrap();
        (Conn::accept(server).unwrap(), client)
    }

    #[test]
    fn the_head_is_replayed_to_whoever_reads_next() {
        let req = b"GET /ws HTTP/1.1\r\nHost: x\r\n\r\nafter";
        let (mut conn, _client) = pair(req);
        let head = conn.read_head().unwrap();
        assert!(head.starts_with("GET /ws HTTP/1.1\r\n"));
        // tungstenite reads the handshake again, and what followed it.
        let mut again = vec![0u8; req.len()];
        conn.read_exact(&mut again).unwrap();
        assert_eq!(&again[..], &req[..]);
    }

    #[test]
    fn a_consumed_head_is_not_replayed() {
        let (mut conn, mut client) = pair(b"GET / HTTP/1.1\r\n\r\n");
        conn.read_head().unwrap();
        conn.consume_head();
        client.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        conn.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"x");
    }
}
