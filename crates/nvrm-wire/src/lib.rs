// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Wire protocol between the guest kernel module (`virtio_nvrm.ko`) and the
//! host daemon (`vhost-user-nvrm`): a fixed 160-byte request header
//! ([`Req`]), a 32-byte reply header ([`Rsp`]), and the payload rules that
//! say what follows each. Little-endian, `#[repr(C)]`, and the C side is
//! GENERATED from these types (`nvrm-genhdr`), so the layout tests at the
//! bottom of this file are the contract.
//!
//! What crosses the wire is an NVIDIA RM ioctl (RM = the Resource Manager,
//! the driver's `/dev/nvidia*` interface -- docs/ARCHITECTURE.md §3): the
//! inline parameter block, an optional second buffer the block points at
//! ("aux"), and the offsets of the fields the host has to rewrite before
//! the call reaches the real driver.
//!
//! Designed transport-independent; since 2026-08-04 there is exactly one
//! carrier (virtio-nvrm). The earlier ones (Unix SEQPACKET,
//! DRM_IOCTL_VIRTGPU_EXECBUFFER) spoke the same schema.
//!
//! Routing by TOKEN, not by fd number: the host issues a token at open
//! time and keeps token -> host_fd internally; the guest creates a
//! placeholder FD of its own and routes over the token.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod tables;

pub const PROTO_VERSION: u32 = 6; // v6: fd_field_proc -- WHOSE fd the inline token names

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
    /// Place a mapping into the host-visible window.
    ///
    /// The guest cannot `mmap` the device itself -- it holds no real device
    /// FD. Instead it names the token whose preceding `RM_MAP_MEMORY` made
    /// the mapping ready (`target_token`), the length (`map_len`) and the
    /// window offset it chose (`addr`; the guest manages the window). The
    /// host checks the slot, hands the FD to the VMM via `SHMEM_MAP` at that
    /// offset, and answers with the cacheability in `Rsp.token` (1 = cached,
    /// 2 = uncached -- the encoding virtio-gpu uses). The guest then maps
    /// the window pages into the process. `KIND_MAP_RELEASE` undoes it.
    MapPrepare = 4,
    /// Back the semaphore pool with guest pages.
    ///
    /// The guest names the base (`addr`) and the length (`map_len`) of the
    /// pool it placed over the UVM mmap; `target_token` is the uvm FD. The
    /// host assembles the guest pages in its own address space, registers
    /// them with RM as an OS descriptor and attaches them with
    /// CREATE_EXTERNAL_RANGE + MAP_EXTERNAL_ALLOCATION at GPU VA == `addr`.
    /// There is NO mmap on the uvm FD -- and that is exactly what removes
    /// the address coupling (uvm.c:793-796).
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

// ---------------------------------------------------------------------------
// Kinds that ONLY the virtio-nvrm carrier knows
// ---------------------------------------------------------------------------
// Deliberately NOT `Kind` variants: `Session` matches `Kind` exhaustively.
// As constants they stay additive in the number space and
// `Kind::from_u32` answers `None` for them -- a session that did get to
// see one would reply cleanly with EPROTO instead of quietly
// misinterpreting it. The device intercepts them before that.
//
// PROTO_VERSION stayed at 4 when they were added: compatibility by
// construction rather than by verification (docs/OPEN-QUESTIONS.md, item 1).

/// Fetch the descriptor tables (`tables`).
///
/// `addr` = byte offset into the table stream, `map_len` = maximum length
/// of the wanted piece. Reply: `token` = total length of the stream,
/// `inline_len` = length of the piece sent along.
pub const KIND_GET_TABLES: u32 = 6;

/// Take a mapping back out of the host-visible window -- the counterpart
/// to `MapPrepare` on this carrier. `addr` = window offset, `map_len` =
/// length.
pub const KIND_MAP_RELEASE: u32 = 7;

/// The guest process in `guest_proc` has ended: its session may fall.
///
/// The module sends it when the LAST device FD of that process closes --
/// including on SIGKILL, because `release` arrives there just the same.
/// Without this message every session would live until the VM ends, and
/// the dense ID could never be reused.
pub const KIND_PROC_GONE: u32 = 8;

/// Host -> guest: an RM event fired. Travels ONLY on the event virtqueue
/// (queue 1); the guest pre-posts `Req`-sized inbufs there and the device
/// writes one `Req` per firing. Never answered, never sent by the guest.
///
/// WHY a `Req` and not a struct of its own: the guest already has the
/// layout, the static asserts and the parser for it, and the fields below
/// hold everything a firing needs. Every field NOT named here carries
/// `Req::default()` and MUST NOT be interpreted.
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
///                          (the one NV_ESC_ALLOC_OS_EVENT rode on -- RM
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
///                          with its id -- for 0x7e the
///                          NVOS10_EVENT_KERNEL_CALLBACK_EX* in guest kernel
///                          VA; 0 for 0x79
///   guest_proc       @156  owner session of target_token
///
/// Data/Status are LOST on the substituted path: RM's OS-event post
/// carries info32=0/info16=0 (os.c:1509-1515, osObjectEventNotification
/// os.c:1677-1685) and NV_ESC_RM_GET_EVENT_DATA hands back only
/// hObject/NotifyIndex/info32/info16 (osapi.c:504-535). Every NVKMS
/// callback checked ignores both (nvkms-kapi-sync.c:48-53,
/// nvkms-rm.c:1686-1694, 4127-4149, nvkms-evo.c:5001-5008).
///
/// Additive: PROTO_VERSION stayed 5 when this landed (OPEN-QUESTIONS nr 1); a host that does
/// not know it never sends it, a guest that does not know it never posts
/// buffers on queue 1.
pub const KIND_EVENT_FIRED: u32 = 9;

/// Who the guest process is. Travels as the inline payload of an `Open` --
/// additive: a caller that sends nothing gets `guest_proc == 0` and one
/// session per carrier, as before.
///
/// `pid` is the number as the GUEST sees it (`pid_vnr`), i.e. the one `ps`
/// shows inside the guest -- good for display, not as a key: the guest
/// kernel hands it out again once the process ends. The key is the dense
/// ID in `Req.guest_proc`.
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
            core::ptr::copy_nonoverlapping(b.as_ptr(), &mut r as *mut Self as *mut u8, Self::WIRE_LEN);
        }
        Some(r)
    }

    /// The name as text, cut at the first NUL. Non-ASCII is discarded
    /// rather than transliterated -- the name goes into an RM buffer, and
    /// whatever lands there should be printable.
    #[cfg(feature = "std")]
    pub fn comm_str(&self) -> String {
        self.comm
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' })
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

/// RMAPI_PARAM_COPY_MAX_PARAMS_SIZE from param_copy.h -- the upper bound RM
/// itself enforces for embedded params buffers. The aux buffer must be able
/// to grow that large.
pub const MAX_AUX: usize = 1024 * 1024;
pub const MAX_MSG: usize = MAX_PAYLOAD + MAX_AUX + 4096;

/// Request. What the fields mean depends on the Kind:
///
/// - Hello:  ioctl_nr = PROTO_VERSION. The rest 0.
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
    /// Which GUEST PROCESS owns the fd named by `fd_field_token`.
    ///
    /// The exact counterpart of [`Req::aux_fd_field_proc`], for the fd that
    /// sits in the INLINE struct rather than in the aux buffer, and it exists
    /// for the same reason: tokens are minted per session (`Mirror::next`
    /// counts from 1 in each), so a token means nothing without the session
    /// that issued it.
    ///
    /// Measured 2026-08-17: an EGLImage import names an fd exported by
    /// ANOTHER guest process -- Xwayland imports what a client exported --
    /// and the host, looking only in the caller's own mirror, answered EBADF
    /// 139936 times in one session. NVIDIA's GL stack reports that failed
    /// import as `GL_OUT_OF_MEMORY`, so the symptom was invisible Steam and
    /// CS2 windows and it looked like a memory problem. The reader that
    /// settled it printed `token 0x1 asked by proc 5 lives in proc 1`:
    /// 21 cross-session misses, 0 stale.
    ///
    /// NONE_U32 = not stated, and then the host falls back to the caller's
    /// own mirror -- which is what every guest built before this field did.
    /// It is NOT 0: `guest_proc == 0` is a live session key.
    ///
    /// It sits in the padding hole after `fd_field_off`, so no offset moved
    /// and `Req` is still 160 bytes. `PROTO_VERSION` goes to 6 anyway, for
    /// the reason `aux_fd_field_proc` gives below: what changes is the
    /// MEANING of a request the host already accepts, and that is a break a
    /// size check cannot catch.
    pub fd_field_proc: u32,
    pub fd_field_token: u64,
    pub embedded_ptr_off: u32,
    pub nested_count: u32,
    pub nested: [NestedDesc; MAX_NESTED],
    /// FD field INSIDE the aux buffer (e.g. NV0005_ALLOC_PARAMETERS.data for
    /// NV01_EVENT_OS_EVENT): byte offset of an 8-byte NvP64 holding a
    /// process-local fd number. NONE_U32 = none.
    pub aux_fd_field_off: u32,
    /// Which GUEST PROCESS owns the fd named by `aux_fd_field_token`.
    ///
    /// Not the same as `guest_proc`, and that is the whole point: tokens are
    /// minted per session (`Mirror::next` counts from 1 in each), so a token
    /// only means something together with the session that issued it.
    /// NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD arrives under NVKMS's
    /// `guest_proc` while naming an fd that belongs to the X server -- the
    /// host would otherwise look it up in the wrong `Mirror` and answer
    /// EBADF.
    ///
    /// NONE_U32 = not stated. It is NOT 0: `guest_proc == 0` is a live
    /// session key (the "not stated" caller, which Hello itself uses), so 0
    /// could not tell a legitimate owner from a forgotten assignment.
    ///
    /// Read ONLY when `aux_fd_field_token != NONE_U64`. The host must
    /// resolve strictly against this owner and refuse otherwise -- falling
    /// back to its own mirror would silently restore the bug this field
    /// exists to fix.
    ///
    /// It sits in the padding hole after `aux_fd_field_off`, so no offset
    /// moved and `Req` is still 160 bytes. `PROTO_VERSION` goes to 5 anyway:
    /// what changed is the MEANING of a request the host already accepts,
    /// and a host that ignored the new word would resolve tokens in the
    /// wrong session exactly as before. That is a semantic break, and it is
    /// the kind a size check cannot catch.
    pub aux_fd_field_proc: u32,
    /// Token of the fd referenced by aux_fd_field_off. NONE_U64 = leave the
    /// value untouched (fd was negative / not translatable).
    pub aux_fd_field_token: u64,
    /// For MapPrepare and UvmPoolBack: length of the wanted mapping.
    pub map_len: u64,
    /// UvmPoolBack only: the base (guest VA == GPU VA) of the pool.
    pub addr: u64,
    /// Ioctl on NV_ESC_RM_ALLOC_MEMORY / class 0x71 only: number of GPA
    /// runs in the aux buffer, 16 bytes each (gpa u64, len u64). The aux
    /// buffer then carries EXCLUSIVELY those runs -- the call has no
    /// embedded params buffer. 0 = none.
    pub gpa_run_count: u32,
    /// Which GUEST PROCESS is speaking (the guest module's dense ID, from
    /// 1 up).
    ///
    /// 0 means "not stated"; such callers share one session per carrier.
    /// The field sits on the former `_pad` -- no offset moved, which is why
    /// `PROTO_VERSION` stayed at 4 at the time. (It is 6 now: 5 for
    /// `aux_fd_field_proc` and 6 for `fd_field_proc`, each of which changed
    /// a MEANING rather than a layout.)
    ///
    /// WARNING: it is a word of the GUEST's. A compromised guest kernel can
    /// write whatever it likes here; the attribution is good for
    /// bookkeeping and for RM's USERD separation INSIDE the VM. The only
    /// boundary the host enforces remains the VM.
    pub guest_proc: u32,
}

impl Default for Req {
    fn default() -> Self {
        Req {
            seq: 0, kind: 0, dev_tag: 0, ioctl_nr: 0,
            target_token: 0, inline_len: 0, aux_len: 0,
            fd_field_off: NONE_U32, fd_field_proc: NONE_U32, fd_field_token: NONE_U64,
            embedded_ptr_off: NONE_U32, nested_count: 0, nested: [NestedDesc::default(); MAX_NESTED],
            aux_fd_field_off: NONE_U32, aux_fd_field_proc: NONE_U32,
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
    /// Always 0 today. It counted the FDs passed back with SCM_RIGHTS on the
    /// retired Unix-socket transport; across a VM boundary there is no FD to
    /// pass. The word stays because the layout is the wire contract.
    pub scm_fd_count: u32,
    pub _pad: u32,
}

macro_rules! pod_bytes {
    ($t:ty) => {
        impl $t {
            pub const WIRE_LEN: usize = core::mem::size_of::<$t>();
            pub fn as_bytes(&self) -> &[u8] {
                unsafe {
                    core::slice::from_raw_parts(
                        self as *const Self as *const u8, Self::WIRE_LEN)
                }
            }
            pub fn from_bytes(b: &[u8]) -> Option<Self> {
                if b.len() < Self::WIRE_LEN { return None; }
                let mut r = <$t>::default();
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        b.as_ptr(), &mut r as *mut Self as *mut u8, Self::WIRE_LEN);
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
        let r = Req { seq: 7, kind: Kind::Ioctl as u32, ioctl_nr: 0x4e,
                      target_token: 42, ..Req::default() };
        let r2 = Req::from_bytes(r.as_bytes()).unwrap();
        assert_eq!((r2.seq, r2.ioctl_nr, r2.target_token), (7, 0x4e, 42));
        assert_eq!(r2.fd_field_off, NONE_U32);
    }

    /// The wire invariants the C header is generated against. If one of
    /// these asserts breaks, `nvrm_wire.h` has to follow (`cargo run
    /// --release --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h`)
    /// -- and PROTO_VERSION has to be checked: a moved offset is a protocol
    /// break, not a refactoring detail.
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
        // In the padding hole after aux_fd_field_off -- if this moves to
        // 128 the field displaced the token instead of filling the gap.
        assert_eq!(offset_of!(Req, aux_fd_field_proc), 124);
        assert_eq!(offset_of!(Req, aux_fd_field_token), 128);
        assert_eq!(offset_of!(Req, map_len), 136);
        assert_eq!(offset_of!(Req, addr), 144);
        assert_eq!(offset_of!(Req, gpa_run_count), 152);
        // guest_proc sits on the former _pad, and aux_fd_field_proc in the
        // hole at 124 -- which is why neither of them moved an offset. The
        // version still went 4 -> 5 for the second one: what changed was the
        // MEANING of a field the host already read, and no size check sees
        // that. If a field MOVES, that is a new protocol either way.
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

    /// Give every field its own value once and check that the trip through
    /// bytes is lossless -- not just for three fields.
    #[test]
    fn req_roundtrip_every_field() {
        let mut r = Req::default();
        r.seq = 0x0101_0101;
        r.kind = Kind::UvmPoolBack as u32;
        r.dev_tag = DevTag::UvmTools as u32;
        r.ioctl_nr = 0x0404_0404;
        r.target_token = 0x0505_0505_0505_0505;
        r.inline_len = 0x0606_0606;
        r.aux_len = 0x0707_0707;
        r.fd_field_off = 0x0808_0808;
        r.fd_field_proc = 0x0809_0809;
        r.fd_field_token = 0x0909_0909_0909_0909;
        r.embedded_ptr_off = 0x0a0a_0a0a;
        r.nested_count = 3;
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

        // And the endianness is little, not host coincidence: seq sits as
        // an LE u32 at offset 0, guest_proc at 156.
        assert_eq!(&b[0..4], &0x0101_0101u32.to_le_bytes());
        assert_eq!(&b[156..160], &0x1111_1111u32.to_le_bytes());
        // fd_field_proc lives in the padding hole at 36, which is the one
        // place a value can survive the round trip and still reach the
        // guest in the wrong word: the hole is only a hole to Rust, and
        // the C header names it. Assert the byte position, not just the
        // value.
        assert_eq!(&b[36..40], &0x0809_0809u32.to_le_bytes());
    }

    #[test]
    fn rsp_and_procinfo_roundtrip() {
        let s = Rsp { seq: 9, ret: -22, token: 0xfeed_beef_cafe_f00d,
                      inline_len: 48, aux_len: 4096, scm_fd_count: 1, _pad: 0 };
        let s2 = Rsp::from_bytes(s.as_bytes()).unwrap();
        assert_eq!(s2.as_bytes(), s.as_bytes());
        assert_eq!((s2.ret, s2.token), (-22, 0xfeed_beef_cafe_f00d));

        let mut p = ProcInfo { pid: 4711, _pad: 0, comm: [0; 16] };
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

    /// The default sentinels are part of the contract: NONE means "no
    /// offset / no token", and 0 would be a VALID offset.
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

    /// The carrier-only kinds (GET_TABLES/MAP_RELEASE/PROC_GONE/EVENT_FIRED)
    /// are deliberately NOT Kind variants: a session that saw one should answer
    /// EPROTO, not misread it. from_u32 must return None for them.
    #[test]
    fn kind_and_devtag_from_u32() {
        for v in 0..=5u32 {
            assert_eq!(Kind::from_u32(v).map(|k| k as u32), Some(v));
        }
        for v in [KIND_GET_TABLES, KIND_MAP_RELEASE, KIND_PROC_GONE, KIND_EVENT_FIRED, 10, u32::MAX] {
            assert!(Kind::from_u32(v).is_none(), "Kind {v} must never reach a Session");
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

    /// comm goes into an RM buffer: cutting at NUL and replacing non-ASCII
    /// are safety behaviour, not convenience. (std feature: runs in the
    /// workspace run, not with `-p` alone.)
    #[cfg(feature = "std")]
    #[test]
    fn comm_str_truncates_and_sanitises() {
        let mut p = ProcInfo::default();
        p.comm = *b"abc\0def\0\0\0\0\0\0\0\0\0";
        assert_eq!(p.comm_str(), "abc");
        p.comm = [0xff; 16]; // no NUL, not printable
        assert_eq!(p.comm_str(), "????????????????");
        p.comm = [b'x'; 16]; // no NUL: exactly 16 characters, no overrun
        assert_eq!(p.comm_str(), "xxxxxxxxxxxxxxxx");
    }
}
