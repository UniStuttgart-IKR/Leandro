// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! virtio-input as an external vhost-user backend.
//!
//! Why this exists at all: cloud-hypervisor has no virtio-input of its own,
//! and `--generic-vhost-user` does know `b"input" => VIRTIO_ID_INPUT` -- so
//! the device can live behind a socket like every other one here. The
//! obvious supplier for the far end would have been crosvm, which already
//! serves virtio-gpu that way. It cannot: `crosvm device` offers block,
//! gpu, net, snd, console, fs, vsock and wl, and no input. Its standalone
//! gpu device even hardcodes `event_devices = Vec::new()` with the comment
//! "These are only used when there is an input device".
//!
//! The device is deliberately small. virtio-input is a config space plus
//! two queues, and none of it touches the RM path:
//!
//!   eventq  (0)  device -> driver.  The driver parks empty 8-byte buffers
//!                here; the device fills one per input event.
//!   statusq (1)  driver -> device.  LEDs and force feedback. Consumed and
//!                completed, not acted on -- there is no lamp to light.
//!
//! Two sources, and the second one is the reason the gate can prove
//! anything:
//!
//!   --evdev PATH   forward a real host input device verbatim.
//!   --fifo PATH    read `type code value` lines. Scriptable, so a test can
//!                  press a key without a human and without synthesising
//!                  input into somebody's live desktop session.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, RwLock};

use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_queue::QueueT;
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;

type Mem = GuestMemoryAtomic<GuestMemoryMmap<()>>;
type InVring = VringRwLock<Mem>;

const VIRTIO_F_VERSION_1: u64 = 32;
const VIRTIO_RING_F_INDIRECT_DESC: u64 = 28;

/// virtio 1.4 sec 5.8: the two queues, and `data` values 0 and 1 in the
/// daemon's epoll. 2 is the exit event, so an extra source starts at 3.
const QUEUE_EVENT: u16 = 0;
const QUEUE_STATUS: u16 = 1;
const EVENT_SOURCE: u16 = 3;

// ---- config space (virtio 1.4 sec 5.8.4) ----------------------------------
const CFG_UNSET: u8 = 0x00;
const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

/// `struct virtio_input_config`: select, subsel, size, 5 reserved, then a
/// 128-byte union. The guest writes select/subsel and reads back size and
/// the payload, so the config space is a small state machine rather than a
/// constant.
const CFG_LEN: usize = 136;
const CFG_PAYLOAD: usize = 128;

// Linux input event types we announce (input-event-codes.h).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;

/// Highest key code we claim. 0x2ff covers the whole keyboard plus the
/// BTN_* range that mice use, which is what makes one device enough for
/// both -- the guest's evdev happily drives a device that has keys and
/// relative axes.
const KEY_MAX_CLAIMED: u16 = 0x2ff;

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

    /// Parse one `type code value` line. Empty lines and `#` comments are
    /// skipped by returning `Ok(None)` rather than an error, so a fifo can
    /// carry a readable script.
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
        Ok(Some(InputEvent {
            kind: u16::try_from(kind).map_err(|_| anyhow::anyhow!("type {kind} out of range"))?,
            code: u16::try_from(code).map_err(|_| anyhow::anyhow!("code {code} out of range"))?,
            // Negative values are the normal case for relative motion, and
            // they travel as the two's complement bit pattern.
            value: value as u32,
        }))
    }
}

/// Where events come from. Both end up as a stream of `InputEvent`; the
/// difference is only the framing.
pub enum Source {
    /// A host evdev node. Frames are `struct input_event`, whose size
    /// depends on the libc time representation, so it is read rather than
    /// assumed.
    Evdev(std::fs::File),
    /// A fifo of `type code value` lines.
    Fifo(BufReader<std::fs::File>, std::path::PathBuf),
}

/// `struct input_event` is `struct timeval` followed by type, code, value.
/// On 64-bit Linux that is 16 + 2 + 2 + 4 = 24 bytes; the timestamp is
/// dropped because virtio-input has no field for it.
const EVDEV_FRAME: usize = 2 * std::mem::size_of::<libc::time_t>() + 8;
const EVDEV_TYPE_OFF: usize = 2 * std::mem::size_of::<libc::time_t>();

impl Source {
    pub fn open(spec: &SourceSpec) -> anyhow::Result<Self> {
        match spec {
            SourceSpec::Evdev(path) => {
                let f = std::fs::File::open(path)
                    .map_err(|e| anyhow::anyhow!("open evdev {}: {e}", path.display()))?;
                set_nonblocking(f.as_raw_fd())?;
                Ok(Source::Evdev(f))
            }
            SourceSpec::Fifo(path) => Ok(Source::Fifo(open_fifo(path)?, path.clone())),
        }
    }

    fn as_raw_fd(&self) -> RawFd {
        match self {
            Source::Evdev(f) => f.as_raw_fd(),
            Source::Fifo(r, _) => r.get_ref().as_raw_fd(),
        }
    }

    /// Drain whatever is readable right now. Never blocks: both fds are
    /// O_NONBLOCK, and the caller is an epoll handler.
    fn drain(&mut self, out: &mut VecDeque<InputEvent>) -> std::io::Result<()> {
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
            Source::Fifo(reader, path) => {
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        // EOF on a fifo means the last writer closed it. That
                        // is normal -- every `echo > fifo` is a writer that
                        // comes and goes -- but the fd then stays readable
                        // forever and epoll would spin. Reopening parks it
                        // again until the next writer.
                        Ok(0) => {
                            *reader = open_fifo(path).map_err(std::io::Error::other)?;
                            return Ok(());
                        }
                        Ok(_) => match InputEvent::parse_line(&line) {
                            Ok(Some(ev)) => out.push_back(ev),
                            Ok(None) => {}
                            Err(e) => eprintln!("vhost-user-input: {e}"),
                        },
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }
}

/// A fifo is opened read-write on purpose. Opening read-only would block
/// until a writer appears -- before the VM even starts -- and would then
/// report EOF whenever the last writer left. O_RDWR keeps one writer (us)
/// permanently attached, so the fd is simply quiet when nothing is queued.
fn open_fifo(path: &std::path::Path) -> anyhow::Result<BufReader<std::fs::File>> {
    if !path.exists() {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| anyhow::anyhow!("fifo path: {e}"))?;
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(anyhow::anyhow!(
                "mkfifo {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("open fifo {}: {e}", path.display()))?;
    set_nonblocking(f.as_raw_fd())?;
    Ok(BufReader::new(f))
}

fn set_nonblocking(fd: RawFd) -> anyhow::Result<()> {
    // SAFETY: plain fcntl on a fd we own.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(anyhow::anyhow!("O_NONBLOCK: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

pub enum SourceSpec {
    Evdev(std::path::PathBuf),
    Fifo(std::path::PathBuf),
}

struct InputDevice {
    mem: Option<Mem>,
    source: Source,
    pending: VecDeque<InputEvent>,
    /// Written by the guest through SET_CONFIG, read back through
    /// GET_CONFIG. `size` is derived, never stored.
    select: u8,
    subsel: u8,
    name: Vec<u8>,
}

impl InputDevice {
    fn new(source: Source, name: &str) -> Self {
        Self {
            mem: None,
            source,
            pending: VecDeque::new(),
            select: CFG_UNSET,
            subsel: 0,
            name: name.as_bytes().to_vec(),
        }
    }

    /// The payload for the current (select, subsel), or empty when the pair
    /// names nothing. Empty is the correct answer, not an error: that is how
    /// the driver learns a capability is absent (`size == 0`).
    fn config_payload(&self) -> Vec<u8> {
        let bitmap = |bits: &[u16]| -> Vec<u8> {
            let max = bits.iter().copied().max().unwrap_or(0) as usize;
            let mut v = vec![0u8; max / 8 + 1];
            for b in bits {
                v[*b as usize / 8] |= 1 << (*b % 8);
            }
            v
        };
        match (self.select, self.subsel) {
            (CFG_ID_NAME, _) => self.name.clone(),
            (CFG_ID_SERIAL, _) => b"0".to_vec(),
            // bustype 0x06 = BUS_VIRTUAL, then vendor/product/version. A
            // virtual bus is what this is; claiming USB would make the
            // guest's udev rules look for things that are not there.
            (CFG_ID_DEVIDS, _) => {
                let mut v = Vec::with_capacity(8);
                for x in [0x06u16, 0x0000, 0x0000, 0x0001] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            // No INPUT_PROP_* -- not a pointing stick, not a touchpad.
            (CFG_PROP_BITS, _) => Vec::new(),
            // Which event types exist at all.
            (CFG_EV_BITS, 0) => bitmap(&[EV_SYN, EV_KEY, EV_REL]),
            // ...and which codes within a type. One device carrying both
            // keys and relative axes is deliberate: it keeps the backend to
            // a single virtqueue pair, and evdev has no objection.
            (CFG_EV_BITS, s) if s as u16 == EV_KEY => {
                bitmap(&(0..=KEY_MAX_CLAIMED).collect::<Vec<_>>())
            }
            (CFG_EV_BITS, s) if s as u16 == EV_REL => bitmap(&[REL_X, REL_Y, REL_WHEEL]),
            // No absolute axes, so no ABS_INFO. Answering with a zeroed
            // struct instead of nothing would advertise an axis of range
            // 0..0, which is worse than saying it does not exist.
            (CFG_ABS_INFO, _) => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Move as many pending events into the eventq as the driver has parked
    /// buffers for. Events that do not fit stay queued -- dropping them
    /// would break up key press/release pairs, and a lost release is a key
    /// stuck down in the guest.
    fn flush(&mut self, vring: &InVring) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let Some(mem) = self.mem.as_ref() else {
            return Ok(()); // kick before SET_MEM_TABLE; events keep.
        };
        let mem = mem.memory();
        let mut used_any = false;
        while !self.pending.is_empty() {
            let chain = vring
                .get_mut()
                .get_queue_mut()
                .pop_descriptor_chain(mem.clone());
            let Some(chain) = chain else { break };
            let head = chain.head_index();
            let mut writer = chain
                .writer(&*mem)
                .map_err(|e| std::io::Error::other(format!("writer: {e}")))?;

            if writer.available_bytes() < InputEvent::WIRE_LEN {
                // Too small to hold one event. Complete it with zero bytes
                // rather than stalling the queue on a buffer we cannot use.
                vring
                    .add_used(head, 0)
                    .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
                used_any = true;
                continue;
            }
            let ev = self.pending.pop_front().expect("checked non-empty");
            std::io::Write::write_all(&mut writer, &ev.to_bytes())?;
            vring
                .add_used(head, InputEvent::WIRE_LEN as u32)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            used_any = true;
        }
        if used_any {
            vring
                .signal_used_queue()
                .map_err(|e| std::io::Error::other(format!("signal: {e}")))?;
        }
        Ok(())
    }

    /// The status queue carries LED and force-feedback events from the
    /// guest. There is nothing here to actuate, so they are completed and
    /// discarded -- but they MUST be completed, or the guest's driver waits
    /// on a buffer that never returns.
    fn drain_status(&mut self, vring: &InVring) -> std::io::Result<()> {
        let Some(mem) = self.mem.as_ref() else {
            return Ok(());
        };
        let mem = mem.memory();
        let mut used_any = false;
        loop {
            let chain = vring
                .get_mut()
                .get_queue_mut()
                .pop_descriptor_chain(mem.clone());
            let Some(chain) = chain else { break };
            vring
                .add_used(chain.head_index(), 0)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            used_any = true;
        }
        if used_any {
            vring
                .signal_used_queue()
                .map_err(|e| std::io::Error::other(format!("signal: {e}")))?;
        }
        Ok(())
    }
}

impl VhostUserBackendMut for InputDevice {
    type Bitmap = ();
    type Vring = InVring;

    fn num_queues(&self) -> usize {
        2
    }

    fn max_queue_size(&self) -> usize {
        256
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
            | (1 << VIRTIO_RING_F_INDIRECT_DESC)
    }

    /// CONFIG is not optional here: virtio-input has no other way to say
    /// what it is. Without it cloud-hypervisor answers the guest's config
    /// reads with 0xFF and the driver registers a device with no name and
    /// no event types.
    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::REPLY_ACK
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let payload = self.config_payload();
        let mut cfg = [0u8; CFG_LEN];
        cfg[0] = self.select;
        cfg[1] = self.subsel;
        cfg[2] = payload.len().min(CFG_PAYLOAD) as u8;
        cfg[8..8 + payload.len().min(CFG_PAYLOAD)]
            .copy_from_slice(&payload[..payload.len().min(CFG_PAYLOAD)]);

        let start = (offset as usize).min(CFG_LEN);
        let end = (start + size as usize).min(CFG_LEN);
        cfg[start..end].to_vec()
    }

    fn set_config(&mut self, offset: u32, buf: &[u8]) -> std::io::Result<()> {
        // Only select and subsel are writable. Everything else the guest
        // sends is a read-back of what we gave it, and honouring it would
        // let the guest redefine its own device.
        for (i, b) in buf.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = *b,
                1 => self.subsel = *b,
                _ => {}
            }
        }
        Ok(())
    }

    /// EVENT_IDX is not negotiated (it is not in `features()`), so this only
    /// ever arrives as false. Recorded rather than ignored so that turning
    /// the feature on later cannot silently keep the old signalling.
    fn set_event_idx(&mut self, enabled: bool) {
        if enabled {
            eprintln!("vhost-user-input: EVENT_IDX acked but not implemented");
        }
    }

    fn update_memory(&mut self, mem: Mem) -> std::io::Result<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[Self::Vring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        if !evset.contains(EventSet::IN) {
            return Ok(());
        }
        match device_event {
            // The driver parked fresh buffers: anything held back fits now.
            QUEUE_EVENT => self.flush(&vrings[QUEUE_EVENT as usize]),
            QUEUE_STATUS => self.drain_status(&vrings[QUEUE_STATUS as usize]),
            EVENT_SOURCE => {
                let mut pending = std::mem::take(&mut self.pending);
                let r = self.source.drain(&mut pending);
                self.pending = pending;
                r?;
                self.flush(&vrings[QUEUE_EVENT as usize])
            }
            other => {
                eprintln!("vhost-user-input: unexpected event {other}");
                Ok(())
            }
        }
    }
}

/// Serve as a vhost-user device on `socket` until the VMM hangs up.
pub fn serve(socket: &str, spec: &SourceSpec, name: &str) -> anyhow::Result<()> {
    let source = Source::open(spec)?;
    let source_fd = source.as_raw_fd();
    let backend = Arc::new(RwLock::new(InputDevice::new(source, name)));

    let mut daemon = VhostUserDaemon::new(
        "vhost-user-input".into(),
        backend,
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("vhost-user daemon: {e:?}"))?;

    // The input source is not a virtqueue, so the daemon does not know
    // about it; EVENT_SOURCE says why 3.
    for h in daemon.get_epoll_handlers() {
        h.register_listener(source_fd, EventSet::IN, EVENT_SOURCE as u64)
            .map_err(|e| anyhow::anyhow!("register input source: {e}"))?;
    }

    eprintln!("vhost-user-input: virtio-input on {socket} ({name})");
    daemon
        .serve(socket)
        .map_err(|e| anyhow::anyhow!("vhost-user: {e:?}"))?;
    eprintln!("vhost-user-input: VMM hung up");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_wire_layout() {
        // le16 type, le16 code, le32 value -- the guest reads exactly this.
        let ev = InputEvent { kind: EV_KEY, code: 30, value: 1 };
        assert_eq!(ev.to_bytes(), [0x01, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x00, 0x00]);
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
        let ev = InputEvent::parse_line("0x01 0x1e 1  # KEY_A down").unwrap().unwrap();
        assert_eq!((ev.kind, ev.code, ev.value), (1, 30, 1));
    }

    #[test]
    fn malformed_lines_are_errors_not_silent_events() {
        // A line that is nearly right is the dangerous case: it must not
        // turn into some other key.
        assert!(InputEvent::parse_line("1 30").is_err());
        assert!(InputEvent::parse_line("1 30 1 1").is_err());
        assert!(InputEvent::parse_line("1 99999 1").is_err());
    }

    fn dev() -> InputDevice {
        InputDevice::new(
            Source::Fifo(
                BufReader::new(std::fs::File::open("/dev/null").unwrap()),
                std::path::PathBuf::from("/dev/null"),
            ),
            "leandro test input",
        )
    }

    #[test]
    fn config_reports_name_with_its_length() {
        let mut d = dev();
        d.select = CFG_ID_NAME;
        let cfg = d.get_config(0, CFG_LEN as u32);
        assert_eq!(cfg.len(), CFG_LEN);
        assert_eq!(cfg[0], CFG_ID_NAME);
        assert_eq!(cfg[2] as usize, "leandro test input".len());
        assert_eq!(&cfg[8..8 + 18], b"leandro test input");
    }

    #[test]
    fn ev_bits_names_syn_key_rel_and_nothing_else() {
        let mut d = dev();
        d.select = CFG_EV_BITS;
        d.subsel = 0;
        let cfg = d.get_config(0, CFG_LEN as u32);
        assert_eq!(cfg[2], 1); // one byte covers bits 0..2
        assert_eq!(cfg[8], (1 << EV_SYN) | (1 << EV_KEY) | (1 << EV_REL));
    }

    #[test]
    fn absent_capabilities_report_size_zero() {
        // The driver learns "no absolute axes" from size == 0. A zeroed
        // ABS_INFO would instead claim an axis with range 0..0.
        let mut d = dev();
        d.select = CFG_ABS_INFO;
        assert_eq!(d.get_config(0, CFG_LEN as u32)[2], 0);
        d.select = CFG_PROP_BITS;
        assert_eq!(d.get_config(0, CFG_LEN as u32)[2], 0);
    }

    #[test]
    fn only_select_and_subsel_are_writable() {
        let mut d = dev();
        d.set_config(0, &[CFG_EV_BITS, EV_KEY as u8]).unwrap();
        assert_eq!((d.select, d.subsel), (CFG_EV_BITS, EV_KEY as u8));
        // A write into the payload must not stick: the device describes
        // itself, the guest does not get to redefine it.
        let before = d.get_config(0, CFG_LEN as u32);
        d.set_config(8, &[0xff; 8]).unwrap();
        assert_eq!(d.get_config(0, CFG_LEN as u32), before);
    }

    #[test]
    fn partial_config_reads_are_windows_not_restarts() {
        // The guest reads `size` alone at offset 2 all the time.
        let mut d = dev();
        d.select = CFG_ID_NAME;
        let full = d.get_config(0, CFG_LEN as u32);
        assert_eq!(d.get_config(2, 1), vec![full[2]]);
        assert_eq!(d.get_config(8, 4), full[8..12].to_vec());
        // Reads past the end are clamped rather than panicking.
        assert!(d.get_config(CFG_LEN as u32, 8).is_empty());
        assert_eq!(d.get_config(CFG_LEN as u32 - 4, 64).len(), 4);
    }

    #[test]
    fn key_bitmap_covers_the_whole_claimed_range() {
        let mut d = dev();
        d.select = CFG_EV_BITS;
        d.subsel = EV_KEY as u8;
        let cfg = d.get_config(0, CFG_LEN as u32);
        // KEY_MAX_CLAIMED = 0x2ff needs 96 bytes, which fits the 128-byte
        // union. If that ever stops being true the payload would be
        // silently truncated here.
        assert_eq!(cfg[2] as usize, KEY_MAX_CLAIMED as usize / 8 + 1);
        assert!((cfg[2] as usize) <= CFG_PAYLOAD);
        // BTN_LEFT (0x110) is the one a mouse needs and the reason keys and
        // relative axes share one device.
        assert_ne!(cfg[8 + 0x110 / 8] & (1 << (0x110 % 8)), 0);
    }
}
