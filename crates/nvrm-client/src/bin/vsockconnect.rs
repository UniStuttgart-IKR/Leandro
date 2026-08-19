// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! One guest port, opened through cloud-hypervisor's HYBRID vsock socket --
//! an `ssh -o ProxyCommand` helper, and nothing else.
//!
//! ```text
//! vsockconnect <unix-socket> <guest-port>     # stdin/stdout become the stream
//! ```
//!
//! WHY THIS EXISTS AT ALL, rather than a line of `socat`. cloud-hypervisor's
//! `--vsock cid=N,socket=PATH` does NOT put an `AF_VSOCK` socket on the host.
//! It puts a UNIX socket there speaking the *hybrid* protocol Firecracker
//! defined: the host connects to that unix socket and asks, in text, for a
//! guest port. `socat`'s `VSOCK-CONNECT` address speaks real `AF_VSOCK` to a
//! real host kernel vsock device and is therefore the wrong tool -- it is not
//! that it is awkward here, it does not fit at all.
//!
//! THE PROTOCOL, host -> guest:
//!
//! ```text
//!   connect(unix socket)
//!   write  "CONNECT <port>\n"
//!   read   ONE LINE:  "OK <hostport>\n"  = success, anything else = failure
//!   from here the stream is raw and bidirectional
//! ```
//!
//! Measured 2026-08-19 against cloud-hypervisor v53.0.0, a NixOS guest on
//! CID 43: the handshake line is `OK 1073741824\n` and the very next bytes
//! are `SSH-2.0-OpenSSH_10.4\r\n`.
//!
//! THE BUG THIS FILE IS SHAPED AROUND. The reply line and the first payload
//! bytes arrive in ONE read. A `read(&mut [0u8; 64])` therefore takes the
//! `OK ...\n` *and* the beginning of the SSH banner, and the banner bytes are
//! then gone -- ssh reports a protocol error, which reads like a broken
//! transport and is a lost afternoon. There are two honest fixes: read one
//! byte at a time until `\n`, or buffer and push the remainder back. This
//! takes the second, because a byte-at-a-time read is a syscall per byte on
//! the one path every SSH session opens with; `read_until` over a `BufReader`
//! does the same job in one read and hands the surplus back through
//! `into_inner`/`buffer`. The unit tests below feed exactly that shape --
//! reply and payload in a single write -- and assert not one byte is lost.
//!
//! Ships in `LEA_BIN_DIR` beside `mmapping` and `smipids`, so `build.sh
//! cargo`, the nix package and the store install all carry it without a
//! second rule anywhere.

use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

/// How long to wait for the `OK ...` line before giving up.
///
/// WHY THERE IS A TIMEOUT AT ALL. `connect(2)` to a listening AF_UNIX socket
/// succeeds whether or not anyone ever calls `accept`, so a cloud-hypervisor
/// that is alive but wedged leaves a socket that connects and then says
/// nothing. Without a deadline the read blocks forever -- and nothing above
/// recovers: ssh's `ConnectTimeout` applies to its OWN connect, never to a
/// ProxyCommand (which is already "connected", being a pipe pair), and
/// `lea_wait_ssh`'s per-attempt loop and its pidfile-liveness escape both sit
/// AFTER the call that is blocked. One hung VM would hang the rig, and on a
/// batch node it would hang until the allocation expired.
///
/// 30 s rather than something tight: this is the cold-start path, where the
/// guest's sshd socket unit may genuinely not have been reached yet, and a
/// deadline that fires during a normal boot would be worse than none.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Reads the one reply line into `line`. Bytes read together with the
/// reply line but belonging to the peer's payload stay in the
/// `BufReader`; the caller drains them with `buffer()` before handing the
/// socket on. Dropping them is the bug in the module comment.
fn handshake<R: BufRead>(r: &mut R, line: &mut Vec<u8>) -> io::Result<()> {
    line.clear();
    let n = r.read_until(b'\n', line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "vsock: the socket closed before answering CONNECT",
        ));
    }
    if !line.ends_with(b"\n") {
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
    Ok(())
}

/// `stdin -> socket` and `socket -> stdout`, until either side is done.
///
/// Half-close matters: ssh signals end of input by closing stdin, and a
/// proxy that does not pass that on leaves the guest's sshd waiting forever
/// on a session that is over.
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
    // The writer thread outlives us only when ssh keeps our stdin open
    // without writing; it is a daemon in effect and the process is about
    // to exit.
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

    // AF_UNIX sun_path is 108 bytes INCLUDING the terminator, and the error
    // for exceeding it is a bare ENAMETOOLONG that names nothing. A cluster
    // scratch directory nests deeply enough to hit it, so say which limit
    // was crossed and by how much rather than passing the kernel's answer on.
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

    // A deadline for the handshake ONLY. It is lifted again before the pump,
    // where a long silence is an idle SSH session rather than a fault.
    sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

    // BufReader, then take its unread buffer back: that is the push-back the
    // module comment is about.
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
        // The surplus is what BufReader already pulled in, plus anything the
        // source has not been asked for yet -- in a live socket the second
        // part arrives later, so the test drains both to model the whole
        // stream.
        let mut rest = r.buffer().to_vec();
        let mut tail = Vec::new();
        r.into_inner().read_to_end(&mut tail).unwrap();
        rest.extend_from_slice(&tail);
        Ok((line, rest))
    }

    /// THE BUG THIS BINARY EXISTS TO NOT HAVE. Reply line and payload arrive
    /// in ONE write; not a byte of the payload may be eaten.
    #[test]
    fn payload_in_the_same_write_survives() {
        let banner = b"SSH-2.0-OpenSSH_10.4\r\n";
        let mut stream = Vec::from(&b"OK 1073741824\n"[..]);
        stream.extend_from_slice(banner);
        let (line, rest) = split(&stream).expect("handshake");
        assert_eq!(line, b"OK 1073741824\n");
        assert_eq!(rest, banner, "the SSH banner must survive the handshake read");
    }

    /// A payload that itself contains newlines must not be split further --
    /// only the FIRST line belongs to the handshake.
    #[test]
    fn only_the_first_line_is_consumed() {
        let payload = b"SSH-2.0-x\r\nsecond\nthird\n";
        let mut stream = Vec::from(&b"OK 7\n"[..]);
        stream.extend_from_slice(payload);
        let (line, rest) = split(&stream).expect("handshake");
        assert_eq!(line, b"OK 7\n");
        assert_eq!(rest, payload);
    }

    /// A payload arriving with NO trailing newline of its own is still
    /// returned whole (the banner is not the last thing on the stream).
    #[test]
    fn unterminated_payload_survives() {
        let stream = b"OK 1\nabc";
        let (line, rest) = split(stream).expect("handshake");
        assert_eq!(line, b"OK 1\n");
        assert_eq!(rest, b"abc");
    }

    /// Exactly the reply and nothing else: the common case where the peer
    /// has not spoken yet.
    #[test]
    fn reply_alone_leaves_nothing_over() {
        let (line, rest) = split(b"OK 1073741824\n").expect("handshake");
        assert_eq!(line, b"OK 1073741824\n");
        assert!(rest.is_empty());
    }

    /// A refusal is an error naming what was said, not a silent hang.
    #[test]
    fn refusal_is_reported_verbatim() {
        let e = split(b"ERROR bad port\n").expect_err("must fail");
        let m = e.to_string();
        assert!(m.contains("CONNECT refused"), "{m}");
        assert!(m.contains("ERROR bad port"), "{m}");
    }

    /// A socket that closes instead of answering must not look like success.
    #[test]
    fn eof_before_the_reply_is_an_error() {
        let e = split(b"").expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A half-written reply (no newline) is EOF, not an "OK".
    #[test]
    fn truncated_reply_is_an_error() {
        let e = split(b"OK 107374").expect_err("must fail");
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }
}
