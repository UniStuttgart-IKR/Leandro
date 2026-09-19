// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Input event encoding and nonblocking source reads.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

/// One `struct virtio_input_event`: le16 type, le16 code, le32 value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: u32,
}

impl InputEvent {
    pub const WIRE_LEN: usize = 8;

    pub fn to_bytes(self) -> [u8; Self::WIRE_LEN] {
        let mut b = [0u8; Self::WIRE_LEN];
        b[0..2].copy_from_slice(&self.kind.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }

    /// Parse decimal/hex `type code value`; skip blank lines and `#` comments.
    pub fn parse_line(line: &str) -> anyhow::Result<Option<Self>> {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            return Ok(None);
        }
        let mut it = line.split_whitespace();
        let mut next = |what: &str| -> anyhow::Result<i64> {
            let tok = it
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing {what} in {line:?}"))?;
            let (radix, digits) = match tok.strip_prefix("0x") {
                Some(rest) => (16, rest),
                None => (10, tok),
            };
            i64::from_str_radix(digits, radix)
                .map_err(|e| anyhow::anyhow!("bad {what} {tok:?}: {e}"))
        };
        let kind = next("type")?;
        let code = next("code")?;
        let value = next("value")?;
        if it.next().is_some() {
            anyhow::bail!("trailing junk in {line:?}");
        }
        if !(i32::MIN as i64..=u32::MAX as i64).contains(&value) {
            anyhow::bail!("value {value} out of range");
        }
        Ok(Some(InputEvent {
            kind: u16::try_from(kind).map_err(|_| anyhow::anyhow!("type {kind} out of range"))?,
            code: u16::try_from(code).map_err(|_| anyhow::anyhow!("code {code} out of range"))?,
            // Preserve signed motion and explicit hexadecimal bit patterns.
            value: value as u32,
        }))
    }
}

pub(crate) enum Source {
    Evdev(File),
    /// Retain an incomplete line across nonblocking reads.
    Fifo(BufReader<File>, String),
}

// Drop the evdev timestamp; virtio-input carries only type, code and value.
const EVDEV_FRAME: usize = std::mem::size_of::<libc::input_event>();
const EVDEV_TYPE_OFF: usize = std::mem::offset_of!(libc::input_event, type_);

impl Source {
    pub fn open(spec: &SourceSpec) -> anyhow::Result<Self> {
        match spec {
            SourceSpec::Evdev(path) => {
                let f = File::open(path)
                    .map_err(|e| anyhow::anyhow!("open evdev {}: {e}", path.display()))?;
                set_nonblocking(f.as_raw_fd())?;
                Ok(Source::Evdev(f))
            }
            SourceSpec::Fifo(path) => Ok(Source::Fifo(open_fifo(path)?, String::new())),
        }
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        match self {
            Source::Evdev(f) => f.as_raw_fd(),
            Source::Fifo(r, _) => r.get_ref().as_raw_fd(),
        }
    }

    /// Read available events without blocking the device worker.
    pub(crate) fn drain(&mut self, out: &mut VecDeque<InputEvent>) -> std::io::Result<()> {
        match self {
            Source::Evdev(f) => {
                let mut buf = [0u8; EVDEV_FRAME * 32];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => return Ok(()),
                        Ok(n) => {
                            for frame in buf[..n].chunks_exact(EVDEV_FRAME) {
                                let t = &frame[EVDEV_TYPE_OFF..];
                                out.push_back(InputEvent {
                                    kind: u16::from_ne_bytes([t[0], t[1]]),
                                    code: u16::from_ne_bytes([t[2], t[3]]),
                                    value: u32::from_ne_bytes([t[4], t[5], t[6], t[7]]),
                                });
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            Source::Fifo(reader, line) => {
                loop {
                    match reader.read_line(line) {
                        // O_RDWR keeps a writer open, so ordinary FIFO writer exits
                        // cannot produce EOF. Keep the FD registered with epoll.
                        Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                        Ok(_) => {
                            match InputEvent::parse_line(line) {
                                Ok(Some(ev)) => out.push_back(ev),
                                Ok(None) => {}
                                Err(e) => eprintln!("vhost-user-input: {e}"),
                            }
                            line.clear();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
}

/// O_RDWR prevents EOF when external writers disconnect.
fn open_fifo(path: &Path) -> anyhow::Result<BufReader<File>> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: c is a valid NUL-terminated path.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| anyhow::anyhow!("open fifo {}: {e}", path.display()))?;
    anyhow::ensure!(
        f.metadata()?.file_type().is_fifo(),
        "{} is not a FIFO",
        path.display()
    );
    Ok(BufReader::new(f))
}

fn set_nonblocking(fd: RawFd) -> anyhow::Result<()> {
    // SAFETY: plain fcntl on a fd we own.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(anyhow::anyhow!(
            "O_NONBLOCK: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub enum SourceSpec {
    Evdev(std::path::PathBuf),
    Fifo(std::path::PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EV_KEY, EV_REL};

    #[test]
    fn event_wire_layout() {
        // le16 type, le16 code, le32 value -- the guest reads exactly this.
        let ev = InputEvent {
            kind: EV_KEY,
            code: 30,
            value: 1,
        };
        assert_eq!(
            ev.to_bytes(),
            [0x01, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn negative_relative_motion_survives() {
        // A mouse moving left is value -1, and it must reach the guest as
        // the two's complement pattern, not as a parse error.
        let ev = InputEvent::parse_line("2 0 -1").unwrap().unwrap();
        assert_eq!(ev.kind, EV_REL);
        assert_eq!(ev.value, 0xffff_ffff);
    }

    #[test]
    fn lines_may_be_hex_commented_or_blank() {
        assert!(InputEvent::parse_line("   ").unwrap().is_none());
        assert!(InputEvent::parse_line("# a comment").unwrap().is_none());
        let ev = InputEvent::parse_line("0x01 0x1e 1  # KEY_A down")
            .unwrap()
            .unwrap();
        assert_eq!((ev.kind, ev.code, ev.value), (1, 30, 1));
    }

    #[test]
    fn malformed_lines_are_errors_not_silent_events() {
        assert!(InputEvent::parse_line("1 30").is_err());
        assert!(InputEvent::parse_line("1 30 1 1").is_err());
        assert!(InputEvent::parse_line("1 99999 1").is_err());
    }

    #[test]
    fn fifo_source_rejects_other_file_types() {
        assert!(open_fifo(Path::new("/dev/null")).is_err());
    }

    #[test]
    fn fifo_keeps_a_partial_line_until_the_next_write() {
        use std::io::Write;
        use std::os::fd::FromRawFd;

        let mut fds = [-1; 2];
        // SAFETY: pipe2 initializes both entries; each successful FD gets one owner.
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let mut writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let mut source = Source::Fifo(BufReader::new(reader), Default::default());
        let mut events = VecDeque::new();

        writer.write_all(b"1 30 ").unwrap();
        source.drain(&mut events).unwrap();
        assert!(events.is_empty());
        writer.write_all(b"1\n1 30 0\n").unwrap();
        source.drain(&mut events).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            InputEvent {
                kind: 1,
                code: 30,
                value: 1
            }
        );
        assert_eq!(
            events[1],
            InputEvent {
                kind: 1,
                code: 30,
                value: 0
            }
        );
    }

    #[test]
    fn event_values_must_fit_the_wire_field() {
        assert!(InputEvent::parse_line("2 0 4294967296").is_err());
        assert!(InputEvent::parse_line("2 0 -2147483649").is_err());
        assert_eq!(
            InputEvent::parse_line("2 0 0xffffffff")
                .unwrap()
                .unwrap()
                .value,
            u32::MAX
        );
        assert_eq!(
            InputEvent::parse_line("2 0 -2147483648")
                .unwrap()
                .unwrap()
                .value,
            0x80000000
        );
    }
}
