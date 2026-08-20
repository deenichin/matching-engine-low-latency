//! Ingress transport abstraction (SPEC §3). A compile-time choice, not an
//! architectural commitment: a DPDK implementation would be a second
//! `impl` of [`Transport`], with any `unsafe` FFI confined to that module
//! behind an RAII mbuf wrapper. Nothing in `core`, `wire`, or `risk`
//! changes with the choice of transport.
//!
//! No thread topology here yet (that's stage 3 part two) — just binding,
//! accepting, and the stale-socket lifecycle SPEC §3 requires.

use std::io::{Read, Write};
use std::path::Path;

/// One transport's connection type must support ordinary byte-stream I/O
/// regardless of what the transport looks like underneath.
pub trait Transport: Sized {
    type Connection: Read + Write;

    /// Bind to `path`, ready to accept connections.
    ///
    /// Implementations backed by a filesystem path (like UDS) must remove
    /// any stale file left at `path` — from an unclean previous shutdown —
    /// before binding (SPEC §3), rather than failing with "address
    /// already in use" against a path nothing is actually listening on.
    fn bind(path: &Path) -> std::io::Result<Self>;

    /// Block until a new connection arrives, and accept it.
    fn accept(&self) -> std::io::Result<Self::Connection>;
}

/// Unix Domain Socket transport, `SOCK_STREAM` (SPEC §3).
pub struct UdsTransport {
    listener: std::os::unix::net::UnixListener,
    path: std::path::PathBuf,
}

impl Transport for UdsTransport {
    type Connection = std::os::unix::net::UnixStream;

    fn bind(path: &Path) -> std::io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    fn accept(&self) -> std::io::Result<Self::Connection> {
        let (stream, _addr) = self.listener.accept()?;
        Ok(stream)
    }
}

impl Drop for UdsTransport {
    /// Stale socket files are cleaned up on shutdown (SPEC §3). Errors are
    /// deliberately swallowed here — `Drop` cannot propagate them, and the
    /// next process to `bind()` at this path will remove any leftover
    /// file anyway, so a failed unlink here is not a correctness problem,
    /// only a tidiness one.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A short, collision-free path under `/tmp` directly — UDS paths are
    /// limited to ~100 bytes (`sockaddr_un.sun_path`), and macOS's
    /// `std::env::temp_dir()` (under `TMPDIR`) is often already most of
    /// that budget on its own.
    fn test_socket_path() -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::path::PathBuf::from(format!("/tmp/wire-gw-test-{}-{n}.sock", std::process::id()))
    }

    #[test]
    fn bind_creates_the_socket_file() {
        let path = test_socket_path();
        let transport = UdsTransport::bind(&path).expect("bind should succeed");
        assert!(path.exists());
        drop(transport);
    }

    #[test]
    fn bind_removes_a_stale_socket_file() {
        let path = test_socket_path();
        // Simulate a leftover from an unclean previous shutdown: a file
        // sitting at the path that nothing is listening on.
        std::fs::write(&path, b"stale").expect("failed to create stale file");

        let transport = UdsTransport::bind(&path);
        assert!(
            transport.is_ok(),
            "bind must remove a stale file rather than failing with address-in-use"
        );

        // And the result is a real, working listener, not just a path
        // that happened not to error.
        let transport = transport.unwrap();
        let client = UnixStream::connect(&path).expect("client should be able to connect");
        let accepted = transport
            .accept()
            .expect("transport should accept the connection");
        drop(client);
        drop(accepted);
    }

    #[test]
    fn accept_serves_a_connecting_client() {
        let path = test_socket_path();
        let transport = UdsTransport::bind(&path).expect("bind should succeed");

        let connect_path = path.clone();
        let client_thread = std::thread::spawn(move || {
            let mut client = UnixStream::connect(&connect_path).expect("client connect failed");
            client.write_all(b"ping").expect("client write failed");
            let mut response = [0u8; 4];
            client
                .read_exact(&mut response)
                .expect("client read failed");
            response
        });

        let mut server_side = transport.accept().expect("accept should succeed");
        let mut request = [0u8; 4];
        server_side
            .read_exact(&mut request)
            .expect("server read failed");
        assert_eq!(&request, b"ping");
        server_side.write_all(b"pong").expect("server write failed");

        let response = client_thread.join().expect("client thread panicked");
        assert_eq!(&response, b"pong");
    }

    #[test]
    fn second_connection_is_served_independently() {
        let path = test_socket_path();
        let transport = UdsTransport::bind(&path).expect("bind should succeed");

        let mut client1 = UnixStream::connect(&path).expect("client 1 connect failed");
        let mut server1 = transport.accept().expect("accept 1 should succeed");

        let mut client2 = UnixStream::connect(&path).expect("client 2 connect failed");
        let mut server2 = transport.accept().expect("accept 2 should succeed");

        client1.write_all(b"one").expect("client 1 write failed");
        client2.write_all(b"two").expect("client 2 write failed");

        let mut buf1 = [0u8; 3];
        server1.read_exact(&mut buf1).expect("server 1 read failed");
        let mut buf2 = [0u8; 3];
        server2.read_exact(&mut buf2).expect("server 2 read failed");

        assert_eq!(&buf1, b"one");
        assert_eq!(&buf2, b"two");
    }

    #[test]
    fn drop_removes_the_socket_file() {
        let path = test_socket_path();
        let transport = UdsTransport::bind(&path).expect("bind should succeed");
        assert!(path.exists());
        drop(transport);
        assert!(!path.exists(), "socket file must be cleaned up on shutdown");
    }
}
