// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Guest/host wire schema: a 160-byte request, a 32-byte reply, then payloads.
//! Inline ioctl data is followed by optional auxiliary and nested buffers.
//! Host-issued tokens identify descriptors within a guest-process session.
//!
//! `nvrm-genhdr` generates the C layout. Byte views use native representation;
//! supported hosts and guests are little-endian. Layout tests pin field offsets.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod tables;

pub const PROTO_VERSION: u32 = 6; // v6 adds the inline FD token owner.

/// Sentinel for "no offset / no token".
pub const NONE_U32: u32 = u32::MAX;
pub const NONE_U64: u64 = u64::MAX;

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Hello = 0,
    Open = 1,
    Close = 2,
    Ioctl = 3,
    /// Map a prepared device FD into the shared window.
    /// `target_token` selects the FD, `addr` the window offset, `map_len` its length.
    /// The host sends SHMEM_MAP to the VMM; Rsp.token returns cacheability
    /// (1 cached, 2 uncached). KIND_MAP_RELEASE removes the mapping.
    MapPrepare = 4,
    /// Back a UVM semaphore pool with guest GPA runs from aux.
    /// `target_token` selects UVM; `addr` is guest/GPU VA and `map_len` its length.
    /// The host registers an RM OS descriptor, creates an external UVM range and
    /// maps that allocation at the GPU VA without an mmap on the host UVM FD.
    UvmPoolBack = 5,
}

impl Kind {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Kind::Hello,
            1 => Kind::Open,
            2 => Kind::Close,
            3 => Kind::Ioctl,
            4 => Kind::MapPrepare,
            5 => Kind::UvmPoolBack,
            _ => return None,
        })
    }
}

// Carrier-only kinds are intercepted before Session dispatch. Keeping them out
// of Kind makes accidental session dispatch return EPROTO. Their addition did
// not bump the historical protocol version; unknown-kind handling still matters.

/// Fetch descriptor tables: `addr` is the stream offset, `map_len` the byte limit.
/// Reply token is the total stream length; inline_len is the returned piece size.
pub const KIND_GET_TABLES: u32 = 6;

/// Remove a shared-window mapping: `addr` is its offset, `map_len` its length.
pub const KIND_MAP_RELEASE: u32 = 7;

/// Retire the session in guest_proc after its last device FD closes.
/// The guest uses monotonic session IDs; closing FDs does not imply process exit.
pub const KIND_PROC_GONE: u32 = 8;

/// Host-to-guest notification on event virtqueue 1; no reply. The guest posts
/// Req-sized buffers. Unlisted fields retain Req::default() and are ignored.
///
/// Field roles (offsets per `req_layout_is_the_wire_contract`):
///   seq              @0    host running counter of fired events (log only)
///   kind             @4    KIND_EVENT_FIRED
///   ioctl_nr         @12   hClass the GUEST allocated: NV01_EVENT_OS_EVENT
///                          (0x79) = "make the fd readable";
///                          NV01_EVENT_KERNEL_CALLBACK_EX (0x7e) /
///                          NV01_EVENT_KERNEL_CALLBACK (0x78) = "call the
///                          guest kernel callback"
///   target_token     @16   token of the guest fd: for 0x79 the fd to wake
///                          (the one NV_ESC_ALLOC_OS_EVENT rode on; RM
///                          wakes THAT nvfp, nv.c:4036-4086); for 0x7e/0x78
///                          the fd the alloc rode on (log only)
///   inline_len       @24   Data   (NvUnixEvent.info32; 0 on this path)
///   aux_len          @28   Status (NV_OK on this path)
///   fd_field_off     @32   hClient (NVOS64.hRoot of the alloc)
///   fd_field_token   @40   host OS-event id (log only; 0 for 0x79)
///   embedded_ptr_off @48   hEvent (NVOS64.hObjectNew of the alloc)
///   nested_count     @52   notifyIndex exactly as the guest sent it
///                          (NV0005 @12, unstripped)
///   addr             @144  guest_data: the 8 bytes the guest put in
///                          NV0005.data (@16) BEFORE the host overwrote them
///                          with its id; for 0x7e the
///                          NVOS10_EVENT_KERNEL_CALLBACK_EX* in guest kernel
///                          VA; 0 for 0x79
///   guest_proc       @156  owner session of target_token
///
/// OS-event substitution loses Data/Status: RM posts info32/info16 as zero.
/// The inspected NVKMS callbacks ignore them (nvkms-kapi-sync.c, nvkms-rm.c,
/// nvkms-evo.c). The event queue was added without a protocol-version bump.
pub const KIND_EVENT_FIRED: u32 = 9;

/// Optional Open payload for diagnostics. Session routing always uses Req.guest_proc.
/// `pid` is a reusable guest PID; the guest kernel assigns a separate session ID.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub _pad: u32,
    /// `TASK_COMM_LEN` bytes, NUL-padded.
    pub comm: [u8; 16],
}

impl ProcInfo {
    pub const WIRE_LEN: usize = core::mem::size_of::<ProcInfo>();

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: repr(C), POD, no holes.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, Self::WIRE_LEN) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        let mut r = ProcInfo::default();
        // SAFETY: length checked, the target is POD.
        unsafe {
            core::ptr::copy_nonoverlapping(
                b.as_ptr(),
                &mut r as *mut Self as *mut u8,
                Self::WIRE_LEN,
            );
        }
        Some(r)
    }

    /// Stop at the first NUL and replace non-printable/non-ASCII bytes with ?.
    #[cfg(feature = "std")]
    pub fn comm_str(&self) -> String {
        self.comm
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| {
                if c.is_ascii_graphic() || c == b' ' {
                    c as char
                } else {
                    '?'
                }
            })
            .collect()
    }
}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DevTag {
    Ctl = 0,
    Gpu = 1,
    Uvm = 2,
    UvmTools = 3,
}

impl DevTag {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => DevTag::Ctl,
            1 => DevTag::Gpu,
            2 => DevTag::Uvm,
            3 => DevTag::UvmTools,
            _ => return None,
        })
    }
}

pub const MAX_PAYLOAD: usize = 16384; // NV_ABSOLUTE_MAX_IOCTL_SIZE

/// RMAPI_PARAM_COPY_MAX_PARAMS_SIZE (param_copy.h): maximum embedded buffer size.
pub const MAX_AUX: usize = 1024 * 1024;
pub const MAX_MSG: usize = MAX_PAYLOAD + MAX_AUX + 4096;

/// Request. What the fields mean depends on the Kind:
///
/// - Hello: ioctl_nr = PROTO_VERSION; remaining fields use Req::default().
/// - Open:   dev_tag = which node; ioctl_nr = GPU index (Gpu only).
/// - Close:  target_token = the token issued at open time.
/// - Ioctl:  target_token routes to the host FD; dev_tag/ioctl_nr say
///   what; inline_len/aux_len = payload; fd_field_off/fd_field_token =
///   FD translation (NONE_* if none); embedded_ptr_off = offset of the
///   aux pointer inside the inline struct (NONE_U32 if none).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct Req {
    pub seq: u32,
    pub kind: u32,
    pub dev_tag: u32,
    pub ioctl_nr: u32,
    pub target_token: u64,
    pub inline_len: u32,
    pub aux_len: u32,
    pub fd_field_off: u32,
    /// Owner session of fd_field_token; tokens are only unique within a session.
    /// NONE_U32 selects the caller session for legacy requests. An explicit owner
    /// must resolve there; session 0 is valid and differs from NONE_U32.
    /// Added in protocol v6 using padding, without changing Req size.
    pub fd_field_proc: u32,
    pub fd_field_token: u64,
    pub embedded_ptr_off: u32,
    pub nested_count: u32,
    pub nested: [NestedDesc; MAX_NESTED],
    /// Byte offset of an FD in aux, or NONE_U32. Allocation fields use NvP64;
    /// control fields use NvS32. The descriptor determines the write width.
    pub aux_fd_field_off: u32,
    /// Owner session of aux_fd_field_token, which may differ from guest_proc.
    /// When a token is present this owner must be supplied and resolved strictly.
    /// NONE_U32 means absent; 0 is a valid session. Added in protocol v5 using padding.
    pub aux_fd_field_proc: u32,
    /// Token for aux_fd_field_off. NONE_U64 denotes a negative FD sentinel.
    pub aux_fd_field_token: u64,
    /// Mapping length, or maximum table-chunk length for KIND_GET_TABLES.
    pub map_len: u64,
    /// UvmPoolBack: guest/GPU VA. MapPrepare/MapRelease: window offset.
    /// KIND_GET_TABLES: byte offset into the descriptor stream.
    pub addr: u64,
    /// Number of (gpa u64, len u64) runs for UvmPoolBack or OS-descriptor allocation.
    /// Userspace NVOS02 carries only runs; kernel NVOS64 puts its params before them.
    pub gpa_run_count: u32,
    /// Guest-assigned session ID (monotonic from 1); 0 selects the default session.
    /// This is untrusted VM-local attribution, not a host security principal.
    pub guest_proc: u32,
}

impl Default for Req {
    fn default() -> Self {
        Req {
            seq: 0,
            kind: 0,
            dev_tag: 0,
            ioctl_nr: 0,
            target_token: 0,
            inline_len: 0,
            aux_len: 0,
            fd_field_off: NONE_U32,
            fd_field_proc: NONE_U32,
            fd_field_token: NONE_U64,
            embedded_ptr_off: NONE_U32,
            nested_count: 0,
            nested: [NestedDesc::default(); MAX_NESTED],
            aux_fd_field_off: NONE_U32,
            aux_fd_field_proc: NONE_U32,
            aux_fd_field_token: NONE_U64,
            map_len: 0,
            addr: 0,
            gpa_run_count: 0,
            guest_proc: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct Rsp {
    pub seq: u32,
    /// Return of ioctl(2) itself (<0 => -errno). For Open: 0 is ok.
    pub ret: i32,
    /// For Open: the issued token. For MapPrepare: the cacheability. For
    /// KIND_GET_TABLES: the total length of the table stream. Otherwise 0.
    pub token: u64,
    pub inline_len: u32,
    pub aux_len: u32,
    /// Reserved for retired SCM_RIGHTS transport; always 0 on virtio-nvrm.
    pub scm_fd_count: u32,
    pub _pad: u32,
}

macro_rules! pod_bytes {
    ($t:ty) => {
        impl $t {
            pub const WIRE_LEN: usize = core::mem::size_of::<$t>();
            pub fn as_bytes(&self) -> &[u8] {
                unsafe {
                    core::slice::from_raw_parts(self as *const Self as *const u8, Self::WIRE_LEN)
                }
            }
            pub fn from_bytes(b: &[u8]) -> Option<Self> {
                if b.len() < Self::WIRE_LEN {
                    return None;
                }
                let mut r = <$t>::default();
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        b.as_ptr(),
                        &mut r as *mut Self as *mut u8,
                        Self::WIRE_LEN,
                    );
                }
                Some(r)
            }
        }
    };
}
pod_bytes!(Req);
pod_bytes!(Rsp);

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct NestedDesc {
    pub ptr_off: u32,
    pub aux_off: u32,
    pub len: u32,
    /// Padding to 16 bytes only, so the array inside Req sits cleanly.
    pub _pad: u32,
}

pub const MAX_NESTED: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn roundtrip() {
        let r = Req {
            seq: 7,
            kind: Kind::Ioctl as u32,
            ioctl_nr: 0x4e,
            target_token: 42,
            ..Req::default()
        };
        let r2 = Req::from_bytes(r.as_bytes()).unwrap();
        assert_eq!((r2.seq, r2.ioctl_nr, r2.target_token), (7, 0x4e, 42));
        assert_eq!(r2.fd_field_off, NONE_U32);
    }

    /// Generated C offsets must match these assertions. Review PROTO_VERSION
    /// when changing either a field layout or its meaning.
    #[test]
    fn req_layout_is_the_wire_contract() {
        assert_eq!(size_of::<Req>(), 160, "Req must stay 160 bytes");
        assert_eq!(align_of::<Req>(), 8);
        assert_eq!(offset_of!(Req, seq), 0);
        assert_eq!(offset_of!(Req, kind), 4);
        assert_eq!(offset_of!(Req, dev_tag), 8);
        assert_eq!(offset_of!(Req, ioctl_nr), 12);
        assert_eq!(offset_of!(Req, target_token), 16);
        assert_eq!(offset_of!(Req, inline_len), 24);
        // fd_field_proc sits in the padding hole after fd_field_off, which
        // is why Req is still 160 bytes.
        assert_eq!(offset_of!(Req, fd_field_proc), 36);
        assert_eq!(offset_of!(Req, aux_len), 28);
        assert_eq!(offset_of!(Req, fd_field_off), 32);
        assert_eq!(offset_of!(Req, fd_field_token), 40);
        assert_eq!(offset_of!(Req, embedded_ptr_off), 48);
        assert_eq!(offset_of!(Req, nested_count), 52);
        assert_eq!(offset_of!(Req, nested), 56);
        assert_eq!(offset_of!(Req, aux_fd_field_off), 120);
        // Owner fills the old padding slot without displacing the token.
        assert_eq!(offset_of!(Req, aux_fd_field_proc), 124);
        assert_eq!(offset_of!(Req, aux_fd_field_token), 128);
        assert_eq!(offset_of!(Req, map_len), 136);
        assert_eq!(offset_of!(Req, addr), 144);
        assert_eq!(offset_of!(Req, gpa_run_count), 152);
        assert_eq!(offset_of!(Req, guest_proc), 156);
    }

    #[test]
    fn rsp_and_procinfo_layout() {
        assert_eq!(size_of::<Rsp>(), 32);
        assert_eq!(align_of::<Rsp>(), 8);
        assert_eq!(offset_of!(Rsp, seq), 0);
        assert_eq!(offset_of!(Rsp, ret), 4);
        assert_eq!(offset_of!(Rsp, token), 8);
        assert_eq!(offset_of!(Rsp, inline_len), 16);
        assert_eq!(offset_of!(Rsp, aux_len), 20);
        assert_eq!(offset_of!(Rsp, scm_fd_count), 24);

        assert_eq!(size_of::<ProcInfo>(), 24);
        assert_eq!(offset_of!(ProcInfo, pid), 0);
        assert_eq!(offset_of!(ProcInfo, comm), 8);

        assert_eq!(size_of::<NestedDesc>(), 16);
        assert_eq!(offset_of!(NestedDesc, ptr_off), 0);
        assert_eq!(offset_of!(NestedDesc, aux_off), 4);
        assert_eq!(offset_of!(NestedDesc, len), 8);
    }

    /// Every request field survives the byte representation.
    #[test]
    fn req_roundtrip_every_field() {
        let mut r = Req {
            seq: 0x0101_0101,
            kind: Kind::UvmPoolBack as u32,
            dev_tag: DevTag::UvmTools as u32,
            ioctl_nr: 0x0404_0404,
            target_token: 0x0505_0505_0505_0505,
            inline_len: 0x0606_0606,
            aux_len: 0x0707_0707,
            fd_field_off: 0x0808_0808,
            fd_field_proc: 0x0809_0809,
            fd_field_token: 0x0909_0909_0909_0909,
            embedded_ptr_off: 0x0a0a_0a0a,
            nested_count: 3,
            ..Req::default()
        };
        for (i, n) in r.nested.iter_mut().enumerate() {
            n.ptr_off = 0x10 + i as u32;
            n.aux_off = 0x20 + i as u32;
            n.len = 0x30 + i as u32;
        }
        r.aux_fd_field_off = 0x0b0b_0b0b;
        r.aux_fd_field_proc = 0x0b0c_0b0c;
        r.aux_fd_field_token = 0x0c0c_0c0c_0c0c_0c0c;
        r.map_len = 0x0d0d_0d0d_0d0d_0d0d;
        r.addr = 0x0e0e_0e0e_0e0e_0e0e;
        r.gpa_run_count = 0x0f0f_0f0f;
        r.guest_proc = 0x1111_1111;

        let b = r.as_bytes().to_vec();
        assert_eq!(b.len(), Req::WIRE_LEN);
        let r2 = Req::from_bytes(&b).unwrap();
        assert_eq!(r2.as_bytes(), &b[..], "the round trip changes bytes");
        assert_eq!(r2.guest_proc, 0x1111_1111);
        assert_eq!(r2.fd_field_proc, 0x0809_0809);
        assert_eq!(r2.nested[2].len, 0x32);

        // Byte views require a little-endian target.
        assert_eq!(&b[0..4], &0x0101_0101u32.to_le_bytes());
        assert_eq!(&b[156..160], &0x1111_1111u32.to_le_bytes());
        assert_eq!(&b[36..40], &0x0809_0809u32.to_le_bytes());
    }

    #[test]
    fn rsp_and_procinfo_roundtrip() {
        let s = Rsp {
            seq: 9,
            ret: -22,
            token: 0xfeed_beef_cafe_f00d,
            inline_len: 48,
            aux_len: 4096,
            scm_fd_count: 1,
            _pad: 0,
        };
        let s2 = Rsp::from_bytes(s.as_bytes()).unwrap();
        assert_eq!(s2.as_bytes(), s.as_bytes());
        assert_eq!((s2.ret, s2.token), (-22, 0xfeed_beef_cafe_f00d));

        let mut p = ProcInfo {
            pid: 4711,
            _pad: 0,
            comm: [0; 16],
        };
        p.comm[..7].copy_from_slice(b"python3");
        let p2 = ProcInfo::from_bytes(p.as_bytes()).unwrap();
        assert_eq!(p2.as_bytes(), p.as_bytes());
        #[cfg(feature = "std")]
        assert_eq!(p2.comm_str(), "python3");
    }

    /// from_bytes rejects buffers that are too short; longer ones are
    /// allowed (the rest is payload, not an error).
    #[test]
    fn from_bytes_rejects_short_input() {
        let r = Req::default();
        let b = r.as_bytes();
        assert!(Req::from_bytes(&b[..Req::WIRE_LEN - 1]).is_none());
        let mut long = b.to_vec();
        long.push(0xff);
        assert!(Req::from_bytes(&long).is_some());
        assert!(Rsp::from_bytes(&[0u8; Rsp::WIRE_LEN - 1]).is_none());
        assert!(ProcInfo::from_bytes(&[0u8; ProcInfo::WIRE_LEN - 1]).is_none());
    }

    /// NONE means absent; zero is a valid offset.
    #[test]
    fn default_sentinels() {
        let r = Req::default();
        assert_eq!(r.fd_field_off, NONE_U32);
        assert_eq!(r.fd_field_token, NONE_U64);
        assert_eq!(r.embedded_ptr_off, NONE_U32);
        assert_eq!(r.aux_fd_field_off, NONE_U32);
        assert_eq!(r.aux_fd_field_proc, NONE_U32);
        assert_eq!(r.aux_fd_field_token, NONE_U64);
        assert_eq!((r.nested_count, r.gpa_run_count, r.guest_proc), (0, 0, 0));
    }

    /// Carrier-only kinds must not decode as session requests.
    #[test]
    fn kind_and_devtag_from_u32() {
        for v in 0..=5u32 {
            assert_eq!(Kind::from_u32(v).map(|k| k as u32), Some(v));
        }
        for v in [
            KIND_GET_TABLES,
            KIND_MAP_RELEASE,
            KIND_PROC_GONE,
            KIND_EVENT_FIRED,
            10,
            u32::MAX,
        ] {
            assert!(
                Kind::from_u32(v).is_none(),
                "Kind {v} must never reach a Session"
            );
        }
        // The event kind is a device-initiated message: additive, numbered
        // right after PROC_GONE, and NEVER a `Kind` variant.
        assert_eq!(KIND_EVENT_FIRED, 9);
        assert_eq!(KIND_EVENT_FIRED, KIND_PROC_GONE + 1);
        assert!(Kind::from_u32(KIND_EVENT_FIRED).is_none());
        for v in 0..=3u32 {
            assert_eq!(DevTag::from_u32(v).map(|d| d as u32), Some(v));
        }
        assert!(DevTag::from_u32(4).is_none());
    }

    /// Process names stop at NUL and replace non-printable bytes.
    #[cfg(feature = "std")]
    #[test]
    fn comm_str_truncates_and_sanitises() {
        let mut p = ProcInfo {
            comm: *b"abc\0def\0\0\0\0\0\0\0\0\0",
            ..ProcInfo::default()
        };
        assert_eq!(p.comm_str(), "abc");
        p.comm = [0xff; 16]; // no NUL, not printable
        assert_eq!(p.comm_str(), "????????????????");
        p.comm = [b'x'; 16]; // no NUL: exactly 16 characters, no overrun
        assert_eq!(p.comm_str(), "xxxxxxxxxxxxxxxx");
    }
}
