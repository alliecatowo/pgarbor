//! Socket abstraction over TCP and Unix-domain streams. Fully implemented, frozen (v1).
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

/// An accepted client connection.
// CONTRACT
#[derive(Debug)]
pub enum Stream {
    #[cfg(test)]
    Memory { input: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<u8>>>, output: std::sync::Arc<std::sync::Mutex<Vec<u8>>> },
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

/// Who is on the other end (used by the auth policy and `client_addr`/`client_port` of the session).
// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    /// `None` for Unix-domain connections.
    pub addr: Option<SocketAddr>,
    pub is_unix: bool,
}

impl PeerInfo {
    /// True for Unix sockets and for 127.0.0.0/8, ::1 (and IPv4-mapped loopback) TCP peers.
    pub fn is_local(&self) -> bool {
        match self.addr {
            None => self.is_unix,
            Some(a) => match a.ip() {
                std::net::IpAddr::V4(v4) => v4.is_loopback(),
                std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v| v.is_loopback()),
            },
        }
    }
    /// `client_addr` for `SessionOptions` (`None` for Unix sockets, as in `pg_stat_activity`).
    pub fn client_addr(&self) -> Option<String> {
        self.addr.map(|a| a.ip().to_string())
    }
    pub fn client_port(&self) -> Option<i32> {
        self.addr.map(|a| i32::from(a.port()))
    }
}

impl Stream {
    pub fn try_clone(&self) -> std::io::Result<Stream> {
        match self {
            #[cfg(test)]
            Stream::Memory { input, output } => Ok(Stream::Memory { input: input.clone(), output: output.clone() }),
            Stream::Tcp(s) => Ok(Stream::Tcp(s.try_clone()?)),
            #[cfg(unix)]
            Stream::Unix(s) => Ok(Stream::Unix(s.try_clone()?)),
        }
    }
    /// `None` = block forever. A zero duration is rejected by std, so it is mapped to 1 ms.
    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        let d = d.map(|d| if d.is_zero() { Duration::from_millis(1) } else { d });
        match self {
            #[cfg(test)]
            Stream::Memory { .. } => Ok(()),
            Stream::Tcp(s) => s.set_read_timeout(d),
            #[cfg(unix)]
            Stream::Unix(s) => s.set_read_timeout(d),
        }
    }
    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        let d = d.map(|d| if d.is_zero() { Duration::from_millis(1) } else { d });
        match self {
            #[cfg(test)]
            Stream::Memory { .. } => Ok(()),
            Stream::Tcp(s) => s.set_write_timeout(d),
            #[cfg(unix)]
            Stream::Unix(s) => s.set_write_timeout(d),
        }
    }
    /// TCP_NODELAY (no-op on Unix sockets).
    pub fn set_nodelay(&self, on: bool) -> std::io::Result<()> {
        match self {
            #[cfg(test)]
            Stream::Memory { .. } => Ok(()),
            Stream::Tcp(s) => s.set_nodelay(on),
            #[cfg(unix)]
            Stream::Unix(_) => Ok(()),
        }
    }
    /// Shut down both directions (wakes a thread blocked in `read` on a clone of this stream).
    pub fn shutdown_both(&self) -> std::io::Result<()> {
        match self {
            #[cfg(test)]
            Stream::Memory { .. } => Ok(()),
            Stream::Tcp(s) => s.shutdown(Shutdown::Both),
            #[cfg(unix)]
            Stream::Unix(s) => s.shutdown(Shutdown::Both),
        }
    }
    pub fn peer_info(&self) -> PeerInfo {
        match self {
            #[cfg(test)]
            Stream::Memory { .. } => PeerInfo { addr: None, is_unix: true },
            Stream::Tcp(s) => PeerInfo { addr: s.peer_addr().ok(), is_unix: false },
            #[cfg(unix)]
            Stream::Unix(_) => PeerInfo { addr: None, is_unix: true },
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(test)]
            Stream::Memory { input, .. } => {
                let mut q=input.lock().map_err(|_|std::io::Error::other("script input poisoned"))?;
                let n=buf.len().min(q.len());for slot in &mut buf[..n] {if let Some(b)=q.pop_front(){*slot=b;}}
                Ok(n)
            },
            Stream::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(test)]
            Stream::Memory { output, .. } => {output.lock().map_err(|_|std::io::Error::other("script output poisoned"))?.extend_from_slice(buf);Ok(buf.len())},
            Stream::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(test)]
            Stream::Memory { .. } => Ok(()),
            Stream::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
        }
    }
}

#[cfg(test)]
pub(crate) fn scripted(input: Vec<u8>) -> (Stream, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    let output=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    (Stream::Memory {input:std::sync::Arc::new(std::sync::Mutex::new(input.into())),output:output.clone()},output)
}
