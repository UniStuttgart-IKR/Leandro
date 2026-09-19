// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! virtio-input backend with evdev and newline-delimited FIFO sources.
//! Queue 0 delivers input events; queue 1 acknowledges ignored status events.

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_queue::QueueT;
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;

mod source;

use source::Source;
pub use source::{InputEvent, SourceSpec};

type Mem = GuestMemoryAtomic<GuestMemoryMmap<()>>;
type InVring = VringRwLock<Mem>;

const VIRTIO_F_VERSION_1: u64 = 32;
const VIRTIO_RING_F_INDIRECT_DESC: u64 = 28;

// Event IDs 0/1 are queues; 2 is the daemon exit event.
const QUEUE_EVENT: u16 = 0;
const QUEUE_STATUS: u16 = 1;
const EVENT_SOURCE: u16 = 3;

// Config selectors: virtio 1.4, section 5.8.4.
const CFG_UNSET: u8 = 0x00;
const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

// Config layout: select, subsel, size, five reserved bytes, 128-byte payload.
const CFG_LEN: usize = 136;
const CFG_PAYLOAD: usize = 128;

// Linux input event types we announce (input-event-codes.h).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;

// Keyboard and mouse button codes through KEY_MAX.
const KEY_MAX_CLAIMED: u16 = 0x2ff;

struct InputDevice {
    mem: Option<Mem>,
    source: Source,
    pending: VecDeque<InputEvent>,
    // Guest-selected config page.
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

    /// Selected capability; an empty payload means unsupported.
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
            // BUS_VIRTUAL, vendor, product, version.
            (CFG_ID_DEVIDS, _) => {
                let mut v = Vec::with_capacity(8);
                for x in [0x06u16, 0x0000, 0x0000, 0x0001] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            // No device properties.
            (CFG_PROP_BITS, _) => Vec::new(),
            (CFG_EV_BITS, 0) => bitmap(&[EV_SYN, EV_KEY, EV_REL]),
            (CFG_EV_BITS, s) if s as u16 == EV_KEY => {
                bitmap(&(0..=KEY_MAX_CLAIMED).collect::<Vec<_>>())
            }
            (CFG_EV_BITS, s) if s as u16 == EV_REL => bitmap(&[REL_X, REL_Y, REL_WHEEL]),
            // Empty ABS_INFO avoids advertising a zero-range axis.
            (CFG_ABS_INFO, _) => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Fill available buffers; retain pending events to preserve key releases.
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
                // Complete unusable buffers without consuming an event.
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

    /// Acknowledge ignored LED and force-feedback events.
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

    // CONFIG supplies the device identity and supported event types.
    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::REPLY_ACK
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let payload = self.config_payload();
        let mut cfg = [0u8; CFG_LEN];
        cfg[0] = self.select;
        cfg[1] = self.subsel;
        let len = payload.len().min(CFG_PAYLOAD);
        cfg[2] = len as u8;
        cfg[8..8 + len].copy_from_slice(&payload[..len]);

        let start = (offset as usize).min(CFG_LEN);
        let end = (start + size as usize).min(CFG_LEN);
        cfg[start..end].to_vec()
    }

    fn set_config(&mut self, offset: u32, buf: &[u8]) -> std::io::Result<()> {
        // Only the capability selectors are writable.
        for (i, b) in buf.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = *b,
                1 => self.subsel = *b,
                _ => {}
            }
        }
        Ok(())
    }

    // EVENT_IDX is not advertised.
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
            QUEUE_EVENT => self.flush(&vrings[QUEUE_EVENT as usize]),
            QUEUE_STATUS => self.drain_status(&vrings[QUEUE_STATUS as usize]),
            EVENT_SOURCE => {
                self.source.drain(&mut self.pending)?;
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

    // Register the external source alongside the virtqueues.
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

    fn dev() -> InputDevice {
        InputDevice::new(
            Source::Evdev(std::fs::File::open("/dev/null").unwrap()),
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
        // The complete key bitmap must fit the config payload.
        assert_eq!(cfg[2] as usize, KEY_MAX_CLAIMED as usize / 8 + 1);
        assert!((cfg[2] as usize) <= CFG_PAYLOAD);
        // BTN_LEFT.
        assert_ne!(cfg[8 + 0x110 / 8] & (1 << (0x110 % 8)), 0);
    }
}
