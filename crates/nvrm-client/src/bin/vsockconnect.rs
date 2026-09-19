// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! SSH ProxyCommand for Cloud Hypervisor's hybrid vsock Unix socket.
//!
//! ```text
//! vsockconnect <unix-socket> <guest-port>
//! ```
//!
//! Sends `CONNECT <port>\n`, reads `OK <hostport>\n`, then relays stdin/stdout.
//! The buffered handshake preserves payload bytes received with the reply.

use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

/// Bound the proxy handshake; SSH ConnectTimeout does not cover ProxyCommand.
/// Allow 30 seconds for a guest still starting its vsock listener.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_MAX_BYTES: usize = 64;

/// Read the reply while preserving buffered payload for the relay.
fn handshake<R: BufRead>(r: &mut R, line: &mut Vec<u8>) -> io::Result<()> {
    line.clear();
    let n = r.take(HANDSHAKE_MAX_BYTES as u64).read_until(b'\n', line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "vsock: the socket closed before answering CONNECT",
        ));
    }
    if !line.ends_with(b"\n") {
        if n == HANDSHAKE_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "vsock: CONNECT reply exceeds 64 bytes",
            ));
        }
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "vsock: the socket closed mid-line while answering CONNECT",
        ));
    }
    if !line.starts_with(b"OK ") {
        // The guest side refuses with a bare "ERROR" or closes; report what
        // was actually said rather than a guess about why.
        let said = String::from_utf8_lossy(line).trim_end().to_string();
        return Err(io::Error::other(format!(
            "vsock: CONNECT refused -- the socket answered {said:?}. \
             Nothing is listening on that guest port (is sshd's vsock socket up?)."
        )));
    }
    let port = &line[3..line.len() - 1];
    if port.is_empty()
        || !port.iter().all(u8::is_ascii_digit)
        || std::str::from_utf8(port)
            .ok()
            .and_then(|p| p.parse::<u32>().ok())
            .is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vsock: CONNECT reply has no valid host port",
        ));
    }
    Ok(())
}

/// Relay both directions; stdin EOF half-closes the socket for the peer.
fn pump(mut sock: UnixStream, surplus: Vec<u8>) -> io::Result<()> {
    let mut out = io::stdout();
    if !surplus.is_empty() {
        out.write_all(&surplus)?;
        out.flush()?;
    }
    let mut up = sock.try_clone()?;
    let writer = thread::spawn(move || -> io::Result<()> {
        let mut buf = [0u8; 65536];
        let mut inp = io::stdin();
        loop {
            match inp.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => up.write_all(&buf[..n])?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // Best effort: the peer may already be gone, which is not an error
        // on the way out.
        let _ = up.shutdown(Shutdown::Write);
        Ok(())
    });

    let mut buf = [0u8; 65536];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.write_all(&buf[..n])?;
                out.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    // Process exit ends a writer still blocked on stdin after the peer closes.
    drop(writer);
    Ok(())
}

fn run() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        return Err(io::Error::other(format!(
            "usage: {} <unix-socket> <guest-port>\n\
             \n\
             Opens one port in a cloud-hypervisor guest through its hybrid\n\
             vsock socket and joins it to stdin/stdout. Meant for\n\
             `ssh -o ProxyCommand`.",
            args.first().map(String::as_str).unwrap_or("vsockconnect")
        )));
    }
    let path = &args[1];
    let port: u32 = args[2]
        .parse()
        .map_err(|_| io::Error::other(format!("vsock: {:?} is not a port number", args[2])))?;

    // Linux sun_path holds 108 bytes including the null terminator.
    if path.len() >= 108 {
        return Err(io::Error::other(format!(
            "vsock: the socket path is {} bytes and AF_UNIX allows 107 -- {}\n\
             Put LEA_VM_DIR somewhere shorter; the path is built from it.",
            path.len(),
            path
        )));
    }

    let sock = UnixStream::connect(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("vsock: cannot connect to {path} -- {e}. Is the VM running?"),
        )
    })?;
    (&sock).write_all(format!("CONNECT {port}\n").as_bytes())?;

    // Idle SSH sessions may remain silent after the handshake.
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

    // Preserve any payload received in the same read as the handshake.
    let mut r = BufReader::new(sock);
    let mut line = Vec::new();
    handshake(&mut r, &mut line).map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => io::Error::new(
            e.kind(),
            format!(
                "vsock: {path} accepted a connection but did not answer CONNECT within {}s. \
                 The VM is running but not serving its vsock port -- still booting, or wedged.",
                HANDSHAKE_TIMEOUT.as_secs()
            ),
        ),
        _ => e,
    })?;
    let surplus = r.buffer().to_vec();
    let sock = r.into_inner();
    sock.set_read_timeout(None)?;
    pump(sock, surplus)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Read the handshake the way `run` does, and return what was left over.
    fn split(input: &[u8]) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let mut r = BufReader::new(Cursor::new(input.to_vec()));
        let mut line = Vec::new();
        handshake(&mut r, &mut line)?;
        // Include both buffered payload and bytes not yet read from the source.
        let mut rest = r.buffer().to_vec();
        let mut tail = Vec::new();
        r.into_inner().read_to_end(&mut tail).unwrap();
        rest.extend_from_slice(&tail);
        Ok((line, rest))
    }

    #[test]
    fn payload_in_the_same_write_survives() {
        let banner = b"SSH-2.0-OpenSSH_10.4\r\n";
        let mut stream = Vec::from(&b"OK 1073741824\n"[..]);
        stream.extend_from_slice(banner);
        let (line, rest) = split(&stream).expect("handshake");
        assert_eq!(line, b"OK 1073741824\n");
        assert_eq!(
            rest, banner,
            "the SSH banner must survive the handshake read"
        );
    }

    #[test]
    fn only_the_first_line_is_consumed() {
        let payload = b"SSH-2.0-x\r\nsecond\nthird\n";
        let mut stream = Vec::from(&b"OK 7\n"[..]);
        stream.extend_from_slice(payload);
        let (line, rest) = split(&stream).expect("handshake");
        assert_eq!(line, b"OK 7\n");
        assert_eq!(rest, payload);
    }

    #[test]
    fn unterminated_payload_survives() {
        let stream = b"OK 1\nabc";
        let (line, rest) = split(stream).expect("handshake");
        assert_eq!(line, b"OK 1\n");
        assert_eq!(rest, b"abc");
    }

    #[test]
    fn reply_alone_leaves_nothing_over() {
        let (line, rest) = split(b"OK 1073741824\n").expect("handshake");
        assert_eq!(line, b"OK 1073741824\n");
        assert!(rest.is_empty());
    }

    #[test]
    fn refusal_is_reported_verbatim() {
        let e = split(b"ERROR bad port\n").expect_err("must fail");
        let m = e.to_string();
        assert!(m.contains("CONNECT refused"), "{m}");
        assert!(m.contains("ERROR bad port"), "{m}");
    }

    #[test]
    fn eof_before_the_reply_is_an_error() {
        let e = split(b"").expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_reply_is_an_error() {
        let e = split(b"OK 107374").expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn malformed_success_ports_are_rejected() {
        for reply in [
            b"OK \n".as_slice(),
            b"OK text\n",
            b"OK 4294967296\n",
            b"OK 7 extra\n",
        ] {
            assert_eq!(split(reply).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn unterminated_replies_have_a_fixed_memory_bound() {
        let input = vec![b'x'; 4096];
        let mut reader = BufReader::new(Cursor::new(input));
        let mut line = Vec::new();
        let error = handshake(&mut reader, &mut line).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(line.len(), HANDSHAKE_MAX_BYTES);
    }
}
