// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Per-VM accounting and limits for explicit guest VRAM allocations.
//!
//! One backend serves one VM; its sessions share a ledger. Guest process IDs
//! are bookkeeping labels supplied by the guest, not isolation boundaries.
//!
//! RM's internal allocations (channel state, USERD, contexts and GSP memory)
//! are not visible here. A profile reservation allows for measured overhead
//! but does not enforce total card occupancy. See [`request_bytes`] for the
//! allocation classes and placement rules that are charged.

use nvrm_sys::RmAbi;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use nvrm_abi::nvgpu::nvos32_attr;
use nvrm_abi::{sys, vgpu};

/// `NV_MEMORY_ALLOCATION_PARAMS` field offsets, from the layout guard in
/// `nvrm-abi/src/nvgpu.rs` (`flags @ 8`, `attr @ 24`, `size @ 64`).
const P_FLAGS: usize = 8;
const P_ATTR: usize = 24;
const P_SIZE: usize = 64;
/// The struct is 128 bytes; anything shorter is not this struct.
const P_LEN: usize = 128;

/// `NVOS32_ALLOC_FLAGS_VIRTUAL`, nvos.h:1457. A virtual allocation
/// reserves address space and no memory at all.
const ALLOC_FLAGS_VIRTUAL: u32 = 0x0008_0000;

// ABI changes must fail compilation before the ledger reads the wrong fields.
const _: () = {
    assert!(P_FLAGS == core::mem::offset_of!(sys::NV_MEMORY_ALLOCATION_PARAMS, flags));
    assert!(P_ATTR == core::mem::offset_of!(sys::NV_MEMORY_ALLOCATION_PARAMS, attr));
    assert!(P_SIZE == core::mem::offset_of!(sys::NV_MEMORY_ALLOCATION_PARAMS, size));
    assert!(P_LEN == core::mem::size_of::<sys::NV_MEMORY_ALLOCATION_PARAMS>());
    assert!(ALLOC_FLAGS_VIRTUAL == sys::NVOS32_ALLOC_FLAGS_VIRTUAL);
};

// Accounting exposes the full cap; Reserved subtracts configured overhead;
// Grid uses the card-derived profile and framebuffer sizes. A reservation
// reduces the guest-visible limit without allocating or holding card memory.
// Backends do not coordinate admission, so operators can overprovision a card.

/// Default overhead allowance in MiB. Measured overhead was about 25 MiB for
/// CUDA/desktop and 175 MiB for game/NVENC workloads (2026-08-21, issue 68).
/// This allowance is workload-dependent, not a bound on RM allocations.
pub const DEFAULT_RESERVATION_MIB: u64 = 256;

/// Memory policy selected at backend startup.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    /// No cap at all. The default, and what every run before 2026-08-21
    /// measured.
    Off,
    /// `LEA_VRAM_LIMIT_MIB`: a per-VM counter charged at allocation time.
    Accounting,
    /// `LEA_VRAM_PROFILE_MIB`: the profile is what the VM may cost the
    /// CARD, and the guest gets what is left after the reservation.
    Reserved,
    /// `LEA_VGPU_TYPE`: the same, except that neither number is the
    /// operator's: both are the card's rule ([`nvrm_abi::vgpu`]), applied
    /// by the launcher to a type or a size. Number 69.
    Grid,
}

impl Policy {
    /// The variable that set this policy, for a message that has to tell
    /// an operator which knob to turn.
    pub fn knob(self) -> &'static str {
        match self {
            Policy::Off => "no cap",
            Policy::Accounting => "LEA_VRAM_LIMIT_MIB",
            Policy::Reserved => "LEA_VRAM_PROFILE_MIB",
            Policy::Grid => "LEA_VGPU_TYPE",
        }
    }
}

/// Profile budget, overhead allowance and guest framebuffer, in bytes.
/// Field names follow VGPU_TYPE (common_vgpu_mgr.h:95).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub policy: Policy,
    /// Planned card budget; equal to fb_length under Accounting.
    pub size: u64,
    /// Allowance for untracked RM allocations; zero under Accounting.
    pub reservation: u64,
    /// Enforced guest allocation limit and reported framebuffer capacity.
    pub fb_length: u64,
    /// What the launcher called it under [`Policy::Grid`] (`RTX2070-2Q`,
    /// `RTX2070-130M`), empty otherwise. For the log; the guest's card is
    /// named after `fb_length` under every policy.
    pub vgpu_type: &'static str,
    /// vGPU's `encoderCapacity`, a percentage: `fb_length`'s share of the
    /// card ([`nvrm_abi::vgpu::encoder_share`]) under every cap. 0 without
    /// a cap or without the card's answer, which is how
    /// `grid::rewrite_encoder_capacity` knows to leave RM's own alone.
    pub encoder_capacity: u32,
}

impl Profile {
    /// No policy: the default configuration.
    pub const OFF: Profile = Profile {
        policy: Policy::Off,
        size: 0,
        reservation: 0,
        fb_length: 0,
        vgpu_type: "",
        encoder_capacity: 0,
    };

    /// Accounting policy with the given byte limit.
    pub fn accounting(bytes: u64) -> Profile {
        if bytes == 0 {
            return Profile::OFF;
        }
        Profile {
            policy: Policy::Accounting,
            size: bytes,
            reservation: 0,
            fb_length: bytes,
            vgpu_type: "",
            encoder_capacity: 0,
        }
    }

    /// The line this backend prints once, at startup. It is the only place
    /// the three numbers appear together, so it spells the arithmetic out
    /// rather than leaving it to be reconstructed from two of them.
    pub fn announce(&self) -> Option<String> {
        let mib = |b: u64| b >> 20;
        match self.policy {
            Policy::Off => None,
            // Kept WORD FOR WORD: this line is grepped by rig scripts and
            // appears in every measurement taken before 2026-08-21.
            Policy::Accounting => Some(format!(
                "VRAM cap {} MiB for this VM (LEA_VRAM_LIMIT_MIB)",
                mib(self.size)
            )),
            Policy::Reserved | Policy::Grid => Some(format!(
                "VRAM profile {} MiB for this VM = {} MiB guest FB + {} MiB reserved \
                 for RM's own device memory ({} {}). The guest is told {} MiB and is \
                 refused at the same number; the reservation is not allocated, it is \
                 FB the guest is never offered. Nothing here checks the card or a \
                 sibling VM, so profiles that sum past the card are accepted.",
                mib(self.size),
                mib(self.fb_length),
                mib(self.reservation),
                self.policy.knob(),
                if self.policy == Policy::Grid {
                    self.vgpu_type
                } else {
                    "/ LEA_VRAM_RESERVE_MIB"
                },
                mib(self.fb_length),
            )),
        }
    }
}

/// Raw startup settings for pure policy validation.
/// Empty values disable a setting; invalid sizes, conflicting policies and
/// reservations without a usable framebuffer must prevent startup.
#[derive(Default, Copy, Clone)]
struct RawEnv<'a> {
    limit: Option<&'a str>,
    profile: Option<&'a str>,
    reserve: Option<&'a str>,
    /// The launcher resolves a type through vgpuprofile and supplies its sizes.
    vgpu_type: Option<&'a str>,
    vgpu_profile: Option<&'a str>,
    vgpu_fb: Option<&'a str>,
    /// Only used when the card did not answer: the backend prices the
    /// encoder itself from `card_total`, the same way for every policy.
    vgpu_encoder: Option<&'a str>,
    /// `TOTAL_RAM_SIZE` in bytes, asked at start-up (`grid::card`); 0 if
    /// the card did not answer.
    card_total: u64,
}

fn decide(env: RawEnv) -> Result<(Profile, Vec<String>), String> {
    let mut notes = Vec::new();
    let mut unusable = Vec::new();
    let mut mib = |name: &str, raw: Option<&str>| -> Option<u64> {
        let s = raw?;
        if s.trim().is_empty() {
            // The launcher uses an empty string for an unset setting.
            return None;
        }
        match s.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(v) if v <= u64::MAX >> 20 => Some(v),
            Ok(_) => {
                unusable.push(format!("{name}={s:?} exceeds the byte counter range"));
                None
            }
            Err(_) => {
                let hint = vgpu::parse_mib(s)
                    .map(|m| format!(" -- write {m}"))
                    .unwrap_or_default();
                unusable.push(format!("{name}={s:?} is not a number of MiB{hint}"));
                None
            }
        }
    };
    // The closure borrows `unusable`, so it has to be finished with before
    // any of the branches below can look at it.
    let (limit, size, reserve, vgpu_profile, vgpu_fb) = (
        mib("LEA_VRAM_LIMIT_MIB", env.limit),
        mib("LEA_VRAM_PROFILE_MIB", env.profile),
        mib("LEA_VRAM_RESERVE_MIB", env.reserve),
        mib("LEA_VGPU_PROFILE_MIB", env.vgpu_profile),
        mib("LEA_VGPU_FB_MIB", env.vgpu_fb),
    );
    if !unusable.is_empty() {
        return Err(format!(
            "{}. The unit is in the name; the backend will not start without the \
             limit it was given.",
            unusable.join("; ")
        ));
    }

    // The vGPU-shaped policy, and it is all-or-nothing: a type name
    // without its numbers is a name for something nobody computed.
    let vgpu_type = env.vgpu_type.map(str::trim).filter(|t| !t.is_empty());
    let (policy, size, fb, label) = if let Some(t) = vgpu_type {
        if limit.is_some() || size.is_some() {
            return Err(format!(
                "LEA_VGPU_TYPE={t} is set together with LEA_VRAM_LIMIT_MIB or \
                 LEA_VRAM_PROFILE_MIB. Those are three policies for one number. \
                 Set exactly one."
            ));
        }
        let (Some(p), Some(f)) = (vgpu_profile, vgpu_fb) else {
            return Err(format!(
                "LEA_VGPU_TYPE={t} needs LEA_VGPU_PROFILE_MIB and LEA_VGPU_FB_MIB \
                 beside it -- the type is a name in the card's catalogue. \
                 `nvrm-client --bin vgpuprofile` prints it; lea_backend_start \
                 resolves the name."
            ));
        };
        if f >= p {
            return Err(format!(
                "LEA_VGPU_TYPE={t}: guest FB {f} MiB is not smaller than the \
                 profile {p} MiB, so nothing is reserved. A vGPU profile always \
                 keeps something back."
            ));
        }
        (Policy::Grid, p, f, t)
    } else {
        if vgpu_profile.is_some() || vgpu_fb.is_some() {
            notes.push(
                "LEA_VGPU_PROFILE_MIB / LEA_VGPU_FB_MIB are set without LEA_VGPU_TYPE \
                 -- there is no type to give them to, so they do nothing"
                    .to_string(),
            );
        }
        match (limit, size) {
            (Some(l), Some(p)) => {
                return Err(format!(
                    "LEA_VRAM_LIMIT_MIB={l} and LEA_VRAM_PROFILE_MIB={p} are both set. \
                     They are two policies for the same number and this backend will \
                     not pick one for you: the cap is what the GUEST may allocate, the \
                     profile is what the VM may cost the CARD. Set exactly one."
                ));
            }
            (None, Some(size)) => {
                let reservation = reserve.unwrap_or(DEFAULT_RESERVATION_MIB);
                if reservation >= size {
                    return Err(format!(
                        "LEA_VRAM_RESERVE_MIB={reservation} leaves nothing of \
                         LEA_VRAM_PROFILE_MIB={size}: the guest would be told it has a \
                         card with no memory. Raise the profile or lower the reservation."
                    ));
                }
                (Policy::Reserved, size, size - reservation, "")
            }
            (limit, None) => {
                if reserve.is_some() {
                    notes.push(
                        "LEA_VRAM_RESERVE_MIB is set without LEA_VRAM_PROFILE_MIB -- \
                         there is no profile to reserve from, so it does nothing"
                            .to_string(),
                    );
                }
                let Some(l) = limit else {
                    return Ok((Profile::OFF, notes));
                };
                (Policy::Accounting, l, l, "")
            }
        }
    };
    // ONE encoder share for one framebuffer, whichever policy set it. A
    // launcher's LEA_VGPU_ENCODER_CAP is the same number by the same rule;
    // it is only needed when this process could not ask the card.
    let encoder_capacity = if env.card_total != 0 {
        vgpu::encoder_share(fb << 20, env.card_total)
    } else {
        env.vgpu_encoder
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|p| (1..=100).contains(p))
            .unwrap_or(0)
    };
    Ok((
        Profile {
            policy,
            size: size << 20,
            reservation: (size - fb) << 20,
            fb_length: fb << 20,
            // Read once, at startup, and kept for the life of the process:
            // the name goes into a Copy struct and into every log line.
            vgpu_type: Box::leak(label.to_string().into_boxed_str()),
            encoder_capacity,
        },
        notes,
    ))
}

/// Read the profile once at startup, outside the ioctl path.
/// VRAM settings are separate from the guest's cumulative max_pin_mib and
/// the host's per-arena LEA_MAX_PIN_MIB limits.
fn profile_from_env(card_total: u64) -> Result<Profile, String> {
    let get = |n: &str| std::env::var(n).ok();
    let (limit, profile, reserve) = (
        get("LEA_VRAM_LIMIT_MIB"),
        get("LEA_VRAM_PROFILE_MIB"),
        get("LEA_VRAM_RESERVE_MIB"),
    );
    let (vtype, vprofile, vfb, venc) = (
        get("LEA_VGPU_TYPE"),
        get("LEA_VGPU_PROFILE_MIB"),
        get("LEA_VGPU_FB_MIB"),
        get("LEA_VGPU_ENCODER_CAP"),
    );
    let (p, notes) = decide(RawEnv {
        limit: limit.as_deref(),
        profile: profile.as_deref(),
        reserve: reserve.as_deref(),
        vgpu_type: vtype.as_deref(),
        vgpu_profile: vprofile.as_deref(),
        vgpu_fb: vfb.as_deref(),
        vgpu_encoder: venc.as_deref(),
        card_total,
    })?;
    for n in notes {
        eprintln!("vhost-user-nvrm: {n}");
    }
    if let Some(line) = p.announce() {
        eprintln!("vhost-user-nvrm: {line}");
    }
    Ok(p)
}

/// Requested bytes for nonvirtual class 0x40 allocations in VIDMEM.
/// On the tested GSP-client dGPU, 0x40 rejects ANY/PCI (video_mem.c:616-619).
/// Class 0x3e uses system RAM; PROTECTED may still return attr=VIDMEM
/// (system_mem.c:192-195, mem_mgr.c:1586-1614). Never charge it by attr alone.
/// Class 0x50a0 requires VIRTUAL and reserves only GPU VA
/// (virtual_mem.c:357-358, mem_utils.c:1542-1546).
/// Native 610.43.03 churn traces confirmed these three allocation categories.
pub fn request_bytes(hclass: u32, aux: &[u8]) -> Option<u64> {
    // These classes use NV_MEMORY_ALLOCATION_PARAMS. Class 0x71 has a
    // smaller layout and must not be decoded with these offsets.
    if !matches!(hclass, 0x003e | 0x0040 | 0x50a0) || aux.len() < P_LEN {
        return None;
    }
    // ... and of those three, only NV01_MEMORY_LOCAL_USER is VideoMemory
    // (resource_list.h:537-547). RM's own dmem accounting charges that
    // class and no other (video_mem.c:626).
    if hclass != 0x0040 {
        return None;
    }
    let flags = u32::from_le_bytes(aux[P_FLAGS..P_FLAGS + 4].try_into().unwrap());
    if flags & ALLOC_FLAGS_VIRTUAL != 0 {
        return None;
    }
    let attr = u32::from_le_bytes(aux[P_ATTR..P_ATTR + 4].try_into().unwrap());
    if !asks_for_vidmem(attr) {
        return None;
    }
    let size = u64::from_le_bytes(aux[P_SIZE..P_SIZE + 8].try_into().unwrap());
    // A zero-size allocation is RM's problem, not the cap's.
    (size > 0).then_some(size)
}

// NV_ESC_RM_VID_HEAP_CONTROL (0x4a) is the graphics allocation path.
// It uses the same VIDMEM/virtual classification as RM_ALLOC.
// NVOS32_PARAMETERS: function @8, status @20, data @40, size 184.
// AllocSize: hMemory @4, flags @12, attr @16, size @48.
// Layout assertions below bind these offsets to the generated types.

/// `NVOS32_PARAMETERS::function`.
const V_FUNCTION: usize = 8;
/// NVOS32 status uses NV_STATUS, like NVOS64 (nvos.h:74).
pub const V_STATUS_OFF: usize = 20;
/// Where the union starts.
const V_DATA: usize = 40;
/// RM may assign hMemory; read the returned handle after the ioctl.
const VA_HMEMORY: usize = V_DATA + 4;
/// `AllocSize::flags`.
const VA_FLAGS: usize = V_DATA + 12;
/// Allocation attributes, updated by RM.
const VA_ATTR: usize = V_DATA + 16;
/// Requested size on input, allocated size on output.
/// Both allocation paths charge requested bytes, excluding RM page rounding.
const VA_SIZE: usize = V_DATA + 48;
/// The whole struct. Anything shorter is not it.
const V_LEN: usize = 184;
/// Total/free output fields for NVOS32_FUNCTION_INFO.
const V_TOTAL: usize = 24;
const V_FREE: usize = 32;

// AllocSize starts at offset zero within the data union. Add V_DATA to
// its field offsets and assert the result against the generated layout.
const _: () = {
    assert!(V_FUNCTION == core::mem::offset_of!(sys::NVOS32_PARAMETERS, function));
    assert!(V_STATUS_OFF == core::mem::offset_of!(sys::NVOS32_PARAMETERS, status));
    assert!(V_DATA == core::mem::offset_of!(sys::NVOS32_PARAMETERS, data));
    assert!(V_LEN == core::mem::size_of::<sys::NVOS32_PARAMETERS>());
    assert!(V_TOTAL == core::mem::offset_of!(sys::NVOS32_PARAMETERS, total));
    assert!(V_FREE == core::mem::offset_of!(sys::NVOS32_PARAMETERS, free));

    type AllocSize = sys::NVOS32_PARAMETERS__bindgen_ty_1__bindgen_ty_1;
    assert!(VA_HMEMORY == V_DATA + core::mem::offset_of!(AllocSize, hMemory));
    assert!(VA_FLAGS == V_DATA + core::mem::offset_of!(AllocSize, flags));
    assert!(VA_ATTR == V_DATA + core::mem::offset_of!(AllocSize, attr));
    assert!(VA_SIZE == V_DATA + core::mem::offset_of!(AllocSize, size));
};

/// What this NVOS32 call is: `NVOS32_FUNCTION_*`, or `None` if the buffer
/// is not an `NVOS32_PARAMETERS`.
pub fn vidheap_function(buf: &[u8]) -> Option<u32> {
    (buf.len() >= V_LEN)
        .then(|| u32::from_le_bytes(buf[V_FUNCTION..V_FUNCTION + 4].try_into().unwrap()))
}

/// Requested nonvirtual VIDMEM bytes, using the same rules as request_bytes.
/// RM selects class 0x50a0 for VIRTUAL, 0x40 for VIDMEM and 0x3e for ANY/PCI
/// (rmapi_deprecated_vidheapctrl.c:137-142). ANY therefore uses system RAM.
pub fn vidheap_request_bytes(buf: &[u8]) -> Option<u64> {
    if vidheap_function(buf)? != sys::NVOS32_FUNCTION_ALLOC_SIZE {
        return None;
    }
    let flags = u32::from_le_bytes(buf[VA_FLAGS..VA_FLAGS + 4].try_into().unwrap());
    if flags & ALLOC_FLAGS_VIRTUAL != 0 {
        return None;
    }
    let attr = u32::from_le_bytes(buf[VA_ATTR..VA_ATTR + 4].try_into().unwrap());
    if !asks_for_vidmem(attr) {
        return None;
    }
    let size = u64::from_le_bytes(buf[VA_SIZE..VA_SIZE + 8].try_into().unwrap());
    (size > 0).then_some(size)
}

/// What RM wrote back into `attr`, for [`Books::settle`]. An allocation RM
/// placed in sysmem after all is not this cap's business.
pub fn vidheap_attr_out(buf: &[u8]) -> u32 {
    if buf.len() < V_LEN {
        return 0;
    }
    u32::from_le_bytes(buf[VA_ATTR..VA_ATTR + 4].try_into().unwrap())
}

/// The memory handle RM settled on, for the charge and for the later free.
pub fn vidheap_handle(buf: &[u8]) -> u32 {
    if buf.len() < V_LEN {
        return 0;
    }
    u32::from_le_bytes(buf[VA_HMEMORY..VA_HMEMORY + 4].try_into().unwrap())
}

/// Only VIDMEM requests reach framebuffer on the tested card; see request_bytes.
fn asks_for_vidmem(attr: u32) -> bool {
    nvos32_attr::LOCATION.get(attr) == sys::NVOS32_ATTR_LOCATION_VIDMEM
}

/// On the way out: what RM wrote back. Only VIDMEM stays charged. With
/// only VIDMEM charged on the way in this is a second look rather than the
/// decision, and it stays: a written-back attr that disagrees with the
/// request is exactly the case a cap should not keep paying for.
fn is_vidmem(attr: u32) -> bool {
    nvos32_attr::LOCATION.get(attr) == sys::NVOS32_ATTR_LOCATION_VIDMEM
}

// Refusal logs include the process, allocation path, class and LOCATION so
// an incorrect classification can be distinguished from an exhausted cap.

/// Which of the two allocation escapes a request came through, and in
/// which form. The NVOS21 short form is the same escape as NVOS64 with the
/// same params, only `status` sits elsewhere.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Door {
    Nvos64,
    Nvos21,
    Nvos32,
}

impl Door {
    /// The escape named the way nvos.h names it, so a log line can be
    /// grepped against a trace.
    pub fn name(self) -> &'static str {
        match self {
            Door::Nvos64 => "NVOS64 RM_ALLOC",
            Door::Nvos21 => "NVOS21 RM_ALLOC",
            Door::Nvos32 => "NVOS32 VID_HEAP",
        }
    }
}

/// `NVOS32_ATTR_LOCATION` as a word (nvos.h:1069-1072). 2 has no name in
/// this driver.
pub fn location_name(attr: u32) -> &'static str {
    match nvos32_attr::LOCATION.get(attr) {
        sys::NVOS32_ATTR_LOCATION_VIDMEM => "VIDMEM",
        sys::NVOS32_ATTR_LOCATION_PCI => "PCI",
        sys::NVOS32_ATTR_LOCATION_ANY => "ANY",
        _ => "LOCATION_2",
    }
}

/// Original allocation request, captured before RM overwrites attr.
/// NVOS32 has no explicit class, represented here as hclass=0.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub door: Door,
    pub hclass: u32,
    pub flags: u32,
    pub attr: u32,
    pub size: u64,
}

impl Ask {
    /// An `NV_MEMORY_ALLOCATION_PARAMS` behind RM_ALLOC, for the three
    /// classes [`request_bytes`] reads. `None` for anything else.
    pub fn of_alloc(door: Door, hclass: u32, aux: &[u8]) -> Option<Ask> {
        if !matches!(hclass, 0x003e | 0x0040 | 0x50a0) || aux.len() < P_LEN {
            return None;
        }
        Some(Ask {
            door,
            hclass,
            flags: u32::from_le_bytes(aux[P_FLAGS..P_FLAGS + 4].try_into().unwrap()),
            attr: u32::from_le_bytes(aux[P_ATTR..P_ATTR + 4].try_into().unwrap()),
            size: u64::from_le_bytes(aux[P_SIZE..P_SIZE + 8].try_into().unwrap()),
        })
    }

    /// An `NVOS32_FUNCTION_ALLOC_SIZE`. `None` for every other function.
    pub fn of_vidheap(buf: &[u8]) -> Option<Ask> {
        if vidheap_function(buf)? != sys::NVOS32_FUNCTION_ALLOC_SIZE {
            return None;
        }
        Some(Ask {
            door: Door::Nvos32,
            hclass: 0,
            flags: u32::from_le_bytes(buf[VA_FLAGS..VA_FLAGS + 4].try_into().unwrap()),
            attr: u32::from_le_bytes(buf[VA_ATTR..VA_ATTR + 4].try_into().unwrap()),
            size: u64::from_le_bytes(buf[VA_SIZE..VA_SIZE + 8].try_into().unwrap()),
        })
    }

    /// Throttle key: allocation path, class and requested LOCATION.
    /// Different sizes share a key so size variation cannot bypass throttling.
    pub fn kind(&self) -> (Door, u32, u32) {
        (self.door, self.hclass, nvos32_attr::LOCATION.get(self.attr))
    }
}

impl fmt::Display for Ask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.door.name())?;
        if self.door == Door::Nvos32 {
            write!(f, "ALLOC_SIZE")?;
        } else {
            write!(f, "class {:#x}", self.hclass)?;
        }
        write!(
            f,
            " flags {:#x} attr {:#010x} ({}) size {:#x} ({:.1} MiB)",
            self.flags,
            self.attr,
            location_name(self.attr),
            self.size,
            self.size as f64 / (1u64 << 20) as f64,
        )
    }
}

/// Guest process row keyed internally by sub_id. Display guest_pid, which
/// guest tools resolve through their own /proc; numeric PIDs may be reused.
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub guest_pid: u32,
    pub bytes: u64,
    /// The guest's `comm`, for the ledger's census in a refusal line. Not
    /// part of anything the guest is told.
    pub name: String,
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Placement {
    door: Door,
    class: u32,
    requested_location: u32,
    /// Returned location on success, RM status on failure.
    outcome: Result<u32, u32>,
}

/// Shared per-VM allocation counter and process roster.
/// Counting remains active without a cap; enforcement requires a limit.
#[derive(Debug)]
pub struct Ledger {
    /// Enforce fb_length; reservation is an allowance, not a second counter.
    profile: Profile,
    used: AtomicU64,
    /// sub_id -> what that guest process is and holds. A Mutex, not an
    /// atomic: it is touched on alloc/free and on the two list controls,
    /// never per forwarded ioctl.
    roster: Mutex<BTreeMap<u32, ProcRow>>,
    /// Observed placement outcomes, logged once per request kind and result.
    placements: Mutex<HashSet<Placement>>,
}

impl Ledger {
    /// Read startup policy once. Invalid configuration prevents serving requests.
    pub fn new() -> Result<Arc<Self>, String> {
        Ok(Self::with_profile(profile_from_env(
            crate::grid::card().map_or(0, |c| c.total),
        )?))
    }

    fn with_profile(profile: Profile) -> Arc<Self> {
        Arc::new(Ledger {
            profile,
            used: AtomicU64::new(0),
            roster: Mutex::default(),
            placements: Mutex::default(),
        })
    }

    /// A ledger without a VRAM cap, for tests and the fuzz target.
    pub fn off() -> Arc<Self> {
        Self::with_profile(Profile::OFF)
    }

    /// A ledger with a limit set from the test rather than the
    /// environment: a process reads the environment once and cannot vary
    /// it per test case.
    #[cfg(test)]
    pub fn for_test(limit: u64) -> Arc<Self> {
        Self::with_profile(Profile::accounting(limit))
    }

    /// The same, for the reserved policy, where the enforced number and
    /// the paid-for number are deliberately not equal.
    #[cfg(test)]
    pub fn for_test_profile(profile: Profile) -> Arc<Self> {
        Self::with_profile(profile)
    }

    /// Is the cap on at all? The whole path hangs off this, and it is a
    /// plain field read: with the cap off the hot path pays one `bool`.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.profile.fb_length != 0
    }

    /// Enforced and reported framebuffer capacity, excluding the reservation.
    pub fn limit(&self) -> u64 {
        self.profile.fb_length
    }

    /// The whole profile, for the log lines that have to name the policy
    /// rather than only its enforced half.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Reserve bytes atomically without exposing a temporary overshoot.
    /// Counting remains active without a cap for guest process reporting.
    fn charge(&self, bytes: u64) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            // Saturation would accept uncounted bytes at u64::MAX.
            let Some(next) = cur.checked_add(bytes) else {
                return false;
            };
            if self.profile.fb_length != 0 && next > self.profile.fb_length {
                return false;
            }
            match self
                .used
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    /// A guest process announced itself (its first `Open` carried the
    /// identity). Idempotent: a later `Open` of the same process repeats
    /// the same data.
    pub fn register(&self, sub_id: u32, guest_pid: u32, name: &str) {
        let mut r = self.roster.lock().unwrap();
        let row = r.entry(sub_id).or_insert(ProcRow {
            guest_pid,
            bytes: 0,
            name: String::new(),
        });
        row.guest_pid = guest_pid;
        row.name = name.to_string();
    }

    /// The books in one line, for a refusal: what is used against what
    /// limit, and who holds it. Every guest process that holds anything,
    /// largest first; what no named process holds is the kernel's clients
    /// and processes that never stated who they are.
    pub fn census(&self) -> String {
        let used = self.used();
        let mut rows: Vec<(u32, ProcRow)> = self
            .roster
            .lock()
            .unwrap()
            .iter()
            .map(|(&s, r)| (s, r.clone()))
            .collect();
        rows.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(&b.0)));
        let named: u64 = rows.iter().map(|(_, r)| r.bytes).sum();
        let mib = |b: u64| b as f64 / (1u64 << 20) as f64;
        let mut s = format!("ledger {:.1} of {:.1} MiB:", mib(used), mib(self.limit()));
        let mut empty = 0;
        for (sub, r) in &rows {
            if r.bytes == 0 {
                empty += 1;
                continue;
            }
            s.push_str(&format!(
                " {sub}={}[{}] {:.1}",
                r.name,
                r.guest_pid,
                mib(r.bytes)
            ));
        }
        s.push_str(&format!(
            "; unnamed {:.1}; {empty} more processes hold nothing",
            mib(used.saturating_sub(named))
        ));
        s
    }

    /// True the FIRST time this VM sees this request kind end this way.
    /// `outcome` is the LOCATION RM wrote back, or the status it failed
    /// with.
    fn first_placement(&self, ask: &Ask, outcome: Result<u32, u32>) -> bool {
        let (door, class, requested_location) = ask.kind();
        self.placements.lock().unwrap().insert(Placement {
            door,
            class,
            requested_location,
            outcome,
        })
    }

    /// Remove a terminated session from the process roster.
    pub fn forget(&self, sub_id: u32) {
        self.roster.lock().unwrap().remove(&sub_id);
    }

    /// What this guest process now holds. Written through from `Books` so
    /// that the roster never has to be recomputed from the charge map.
    pub fn set_bytes(&self, sub_id: u32, bytes: u64) {
        if let Some(row) = self.roster.lock().unwrap().get_mut(&sub_id) {
            row.bytes = bytes;
        }
    }

    /// Named guest processes ordered by sub_id. Omit unspecified PID zero.
    pub fn roster(&self) -> Vec<ProcRow> {
        self.roster
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.guest_pid != 0)
            .cloned()
            .collect()
    }

    fn release(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        // saturating: an underflow would panic in debug and wrap to ~2^64
        // in release, which is a permanent refusal for the whole VM. If
        // the books are ever wrong, they are to be wrong quietly downwards.
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_sub(bytes);
            match self
                .used
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }
}

/// Allocation size and ownership needed to release its ledger charge.
#[derive(Copy, Clone, Debug)]
pub struct Charge {
    /// Allocation token, used to release charges when its FD closes.
    pub token: u64,
    /// RM client. Freeing it releases all its charges.
    pub root: u32,
    /// Parent object; observed memory allocations are direct device children.
    pub parent: u32,
    /// NVOS64.hObjectNew. Also in the key; kept here so the free walk
    /// reads one place instead of unpacking.
    pub handle: u32,
    pub bytes: u64,
}

/// (client, handle) as one key. Neither half identifies an object alone.
fn key_of(root: u32, handle: u32) -> u64 {
    (root as u64) << 32 | handle as u64
}

/// Session-owned charges against the shared VM ledger.
/// Drop returns remaining charges after process teardown.
pub struct Books {
    ledger: Arc<Ledger>,
    /// Session key in the shared process roster.
    sub_id: u32,
    /// Charges keyed by (client, handle): handles can repeat across clients.
    open: HashMap<u64, Charge>,
    /// Sum of `open`. Kept alongside so Drop needs no walk and cannot
    /// drift from what was charged.
    owed: u64,
    /// How often this session was refused, per request kind
    /// ([`Ask::kind`]). Only for the log line.
    refusals: HashMap<(Door, u32, u32), u64>,
}

impl Books {
    pub fn new(sub_id: u32, ledger: Arc<Ledger>) -> Self {
        Books {
            ledger,
            sub_id,
            open: HashMap::new(),
            owed: 0,
            refusals: HashMap::new(),
        }
    }

    /// Publish the guest PID/name and current usage for this session.
    pub fn announce(&self, guest_pid: u32, name: &str) {
        self.ledger.register(self.sub_id, guest_pid, name);
        self.ledger.set_bytes(self.sub_id, self.owed);
    }

    /// Shared usage summary from Ledger::census.
    pub fn census(&self) -> String {
        self.ledger.census()
    }

    /// Push `owed` into the roster. Called wherever `owed` changes, so the
    /// reported number and the enforced number can never disagree.
    fn publish(&self) {
        self.ledger.set_bytes(self.sub_id, self.owed);
    }

    /// The VM's process list, for the two controls that build it.
    pub fn roster(&self) -> Vec<ProcRow> {
        self.ledger.roster()
    }

    /// Log the first eight refusals of each request kind, then every hundredth.
    /// Retries of one kind must not suppress the first refusal of another.
    pub fn count_refusal(&mut self, ask: &Ask) -> Option<u64> {
        let n = self.refusals.entry(ask.kind()).or_insert(0);
        *n += 1;
        (*n <= 8 || *n % 100 == 0).then_some(*n)
    }

    /// Where RM put a request the ledger reserved for, as a log line --
    /// once per VM per (door, class, LOCATION asked, LOCATION written
    /// back or failure status), `None` every other time.
    pub fn placement(&self, ask: &Ask, ok: bool, status: u32, attr_out: u32) -> Option<String> {
        let outcome = if ok {
            Ok(nvos32_attr::LOCATION.get(attr_out))
        } else {
            Err(status)
        };
        if !self.ledger.first_placement(ask, outcome) {
            return None;
        }
        let verdict = if ok {
            format!(
                "RM placed it in {} (attr out {attr_out:#010x}), {}",
                location_name(attr_out),
                if is_vidmem(attr_out) {
                    "charged"
                } else {
                    "charge given back"
                }
            )
        } else {
            format!("RM refused it with status {status:#x}, charge given back")
        };
        Some(format!("first of its kind: {ask} -- {verdict}"))
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.ledger.enabled()
    }

    pub fn used(&self) -> u64 {
        self.ledger.used()
    }

    pub fn limit(&self) -> u64 {
        self.ledger.limit()
    }

    /// The whole policy, for the name the guest's card carries.
    pub fn profile(&self) -> Profile {
        self.ledger.profile()
    }

    /// Which variable set the policy that just refused. A refusal names
    /// the knob that produced it or the operator has to guess which of the
    /// two is on.
    pub fn knob(&self) -> &'static str {
        self.ledger.profile().policy.knob()
    }

    /// Bytes this session currently owes the ledger.
    pub fn owed(&self) -> u64 {
        self.owed
    }

    /// Reserve before the ioctl. `false` means: refuse the allocation.
    pub fn reserve(&mut self, bytes: u64) -> bool {
        self.ledger.charge(bytes)
    }

    /// Keep the reservation only after successful VIDMEM allocation.
    /// Release it on RM failure or a non-VIDMEM result.
    pub fn settle(&mut self, ok: bool, attr_out: u32, c: Charge) {
        if !ok || !is_vidmem(attr_out) {
            self.ledger.release(c.bytes);
            return;
        }
        // RM returned this handle as newly allocated; replace any stale charge.
        if let Some(old) = self.open.insert(key_of(c.root, c.handle), c) {
            self.ledger.release(old.bytes);
            self.owed = self.owed.saturating_sub(old.bytes);
        }
        self.owed = self.owed.saturating_add(c.bytes);
        self.publish();
    }

    /// Release an object, its direct children, or all charges of a client.
    /// Memory objects were observed directly under their device. Deeper
    /// hierarchies would need explicit parent tracking to release descendants.
    pub fn free_object(&mut self, root: u32, handle: u32) {
        let mut freed = 0u64;
        self.open.retain(|_, c| {
            // Same client only: two clients may well use the same handle
            // number, and freeing one must not release the other's.
            let dies =
                c.root == root && (c.handle == handle || c.parent == handle || handle == root);
            if dies {
                freed += c.bytes;
            }
            !dies
        });
        self.give_back(freed);
    }

    /// The guest closed an FD. Every RM client that was created on it is
    /// gone with it, so every charge that rode on that token is gone too.
    pub fn close_token(&mut self, token: u64) {
        let mut freed = 0u64;
        self.open.retain(|_, c| {
            if c.token == token {
                freed += c.bytes;
                return false;
            }
            true
        });
        self.give_back(freed);
    }

    fn give_back(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.ledger.release(bytes);
        self.owed = self.owed.saturating_sub(bytes);
        self.publish();
    }
}

impl Drop for Books {
    /// The last line of defence, and the one that has to hold: the guest
    /// process died, the session falls, and the host FDs close. RM frees
    /// everything behind them without a single message arriving here.
    fn drop(&mut self) {
        if self.owed != 0 {
            self.ledger.release(self.owed);
        }
        // ... and the process leaves the list. A row that outlived its
        // session would show the guest a PID its own /proc no longer has.
        self.ledger.forget(self.sub_id);
    }
}

// Replace host process-query results with this VM's ledger.
// Both controls use flat buffers and are rewritten in place. Leaving host
// PIDs in the response leaks them even when guest tools cannot resolve them.

// Share offsets with the comparison manifest so rewritten fields and
// measurement masks use the same ABI definitions.
pub use nvrm_abi::mediate::{
    CMD_GPU_GET_PIDS, CMD_GPU_GET_PID_INFO, PIDINFO_COUNT_OFF, PIDINFO_ENTRY,
    PIDINFO_INDEX_VIDEO_MEMORY_USAGE, PIDINFO_LEN, PIDINFO_LIST_OFF, PIDINFO_MAX,
    PIDINFO_MEM_PRIVATE, PIDS_COUNT_OFF, PIDS_LEN, PIDS_MAX, PIDS_TBL_OFF,
};

/// Replace the PID table with this VM's guest processes.
/// Return the row count, or None for a short buffer that remains unchanged.
pub fn rewrite_get_pids(aux: &mut [u8], roster: &[ProcRow]) -> Option<usize> {
    if aux.len() < PIDS_LEN {
        return None;
    }
    let n = roster.len().min(PIDS_MAX);
    for (i, row) in roster.iter().take(n).enumerate() {
        let o = PIDS_TBL_OFF + 4 * i;
        aux[o..o + 4].copy_from_slice(&row.guest_pid.to_le_bytes());
    }
    // Zero the tail: RM left the host's PIDs there, and a stale entry past
    // the new count is exactly the kind of leftover a reader trusts.
    for i in n..PIDS_MAX {
        let o = PIDS_TBL_OFF + 4 * i;
        aux[o..o + 4].copy_from_slice(&0u32.to_le_bytes());
    }
    aux[PIDS_COUNT_OFF..PIDS_COUNT_OFF + 4].copy_from_slice(&(n as u32).to_le_bytes());
    Some(n)
}

/// Answer guest PID queries from this VM's ledger; unknown PIDs report zero.
/// Clamp the guest count to both the ABI maximum and the received buffer.
pub fn rewrite_get_pid_info(aux: &mut [u8], roster: &[ProcRow]) -> Option<usize> {
    if aux.len() < PIDINFO_LIST_OFF + PIDINFO_ENTRY {
        return None;
    }
    let asked = u32::from_le_bytes(
        aux[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let fits = (aux.len() - PIDINFO_LIST_OFF) / PIDINFO_ENTRY;
    let n = asked.min(PIDINFO_MAX).min(fits);
    for i in 0..n {
        let base = PIDINFO_LIST_OFF + PIDINFO_ENTRY * i;
        let pid = u32::from_le_bytes(aux[base..base + 4].try_into().unwrap());
        let index = u32::from_le_bytes(aux[base + 4..base + 8].try_into().unwrap());
        let bytes = if index == PIDINFO_INDEX_VIDEO_MEMORY_USAGE {
            roster
                .iter()
                .find(|r| r.guest_pid == pid)
                .map_or(0, |r| r.bytes)
        } else {
            // An index we do not serve. Zero the payload rather than let
            // RM's answer about a foreign PID through.
            0
        };
        aux[base + 8..base + 12].copy_from_slice(&nvrm_abi::sys::NV_OK.to_le_bytes());
        // The whole union, not just memPrivate: shared/duped and the
        // protected variants are RM's numbers about a host process.
        for k in 0..6 {
            let o = base + PIDINFO_MEM_PRIVATE + 8 * k;
            aux[o..o + 8].copy_from_slice(&0u64.to_le_bytes());
        }
        let o = base + PIDINFO_MEM_PRIVATE;
        aux[o..o + 8].copy_from_slice(&bytes.to_le_bytes());
    }
    aux[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4].copy_from_slice(&(n as u32).to_le_bytes());
    Some(n)
}

// The two lengths are the whole reason these functions can be trusted, so
// they are checked against the header arithmetic rather than assumed.
const _: () = {
    assert!(PIDS_LEN == 3812);
    assert!(PIDINFO_LEN == 14408);
};

// Offsets come from generated layouts through nvrm_abi::mediate.
// Also check the zeroing loop's bound, which the layout alone cannot establish.
const _: () = {
    // The zeroing loop writes 6 u64 to clear the whole union; it must stay
    // inside the entry. `data` is the union whose first member is
    // `vidMemUsage`, and `memPrivate` is that member's first field, so the
    // union's own offset inside the entry is where the loop starts.
    assert!(PIDINFO_MEM_PRIVATE + 6 * 8 <= PIDINFO_ENTRY);
};

// Keep reported total, heap and free memory consistent with the same ledger.
// Native/guest traces identified TOTAL_RAM_SIZE, HEAP_SIZE and HEAP_FREE as
// queried size indices (2026-08-06); unrelated status indices remain unchanged.

/// V1 FB_GET_INFO uses the same index list through an NvP64 at offset 8
/// (ctrl2080fb.h:480, xlate::nested_ptrs). Graphics clients query this form;
/// capping V2 alone left vulkaninfo reporting the full card (2026-08-15).
pub use nvrm_abi::mediate::CMD_FB_GET_INFO;
/// `NV2080_CTRL_CMD_FB_GET_INFO_V2` (ctrl2080fb.h:489).
pub use nvrm_abi::mediate::CMD_FB_GET_INFO_V2;
/// `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` (ctrl2080gpu.h:325).
pub use nvrm_abi::mediate::CMD_GPU_GET_NAME_STRING;

/// V2 header: list count @0, followed by {u32 index, u32 data} entries.
/// The 128-entry maximum gives a 1028-byte structure.
pub use nvrm_abi::mediate::{FBINFO_COUNT_OFF, FBINFO_ENTRY, FBINFO_LIST_OFF, FBINFO_MAX};

/// Memory-size indices use KiB (ctrl2080fb.h:76-112, :254-260).
/// Keep all five consistent, including those absent from recorded traces.
/// The catalogue and mediation paths share these definitions.
use nvrm_abi::mediate::{
    FB_INFO_INDEX_HEAP_FREE, FB_INFO_INDEX_HEAP_SIZE, FB_INFO_INDEX_RAM_SIZE,
    FB_INFO_INDEX_TOTAL_RAM_SIZE, FB_INFO_INDEX_USABLE_RAM_SIZE,
};

/// Report consistent capped total, heap and free sizes.
/// Usage excludes RM's internal allocations, so reported free is an estimate.
/// Return the rewritten count, or None when uncapped or the buffer is short.
pub fn rewrite_fb_info(aux: &mut [u8], limit: u64, used: u64) -> Option<usize> {
    if limit == 0 || aux.len() < FBINFO_LIST_OFF + FBINFO_ENTRY {
        return None;
    }
    let asked = u32::from_le_bytes(
        aux[FBINFO_COUNT_OFF..FBINFO_COUNT_OFF + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    cap_fb_entries(&mut aux[FBINFO_LIST_OFF..], asked, limit, used)
}

/// Apply the shared FB size rewrite to the separate V1 list buffer.
/// asked is the list count from the parent parameter block.
pub fn rewrite_fb_info_list(list: &mut [u8], asked: usize, limit: u64, used: u64) -> Option<usize> {
    if limit == 0 || list.len() < FBINFO_ENTRY {
        return None;
    }
    cap_fb_entries(list, asked, limit, used)
}

/// Shared size-index selection and arithmetic for both FB_GET_INFO forms.
fn cap_fb_entries(list: &mut [u8], asked: usize, limit: u64, used: u64) -> Option<usize> {
    let fits = list.len() / FBINFO_ENTRY;
    let n = asked.min(FBINFO_MAX).min(fits);

    // Saturate KiB values to the u32 data field instead of wrapping.
    let kb = |b: u64| -> u32 { (b / 1024).min(u32::MAX as u64) as u32 };
    let free = limit.saturating_sub(used);

    let mut touched = 0;
    for i in 0..n {
        let o = FBINFO_ENTRY * i;
        let index = u32::from_le_bytes(list[o..o + 4].try_into().unwrap());
        let v = match index {
            FB_INFO_INDEX_RAM_SIZE
            | FB_INFO_INDEX_TOTAL_RAM_SIZE
            | FB_INFO_INDEX_HEAP_SIZE
            | FB_INFO_INDEX_USABLE_RAM_SIZE => kb(limit),
            FB_INFO_INDEX_HEAP_FREE => kb(free),
            _ => continue,
        };
        list[o + 4..o + 8].copy_from_slice(&v.to_le_bytes());
        touched += 1;
    }
    Some(touched)
}

/// Cap NVOS32_FUNCTION_INFO total/free bytes like the FB_GET_INFO controls.
/// RM queries FB internally, bypassing their return-path rewrite
/// (rmapi_deprecated_vidheapctrl.c:340-383). No guest INFO call was observed
/// in the 2026-09-17 census; this path is covered by source review and tests.
/// Leave data.Info addresses/block details unchanged. Return false when
/// uncapped, short or not an INFO response.
pub fn rewrite_vidheap_info(buf: &mut [u8], limit: u64, used: u64) -> bool {
    if limit == 0 || vidheap_function(buf) != Some(sys::NVOS32_FUNCTION_INFO) {
        return false;
    }
    buf[V_TOTAL..V_TOTAL + 8].copy_from_slice(&limit.to_le_bytes());
    buf[V_FREE..V_FREE + 8].copy_from_slice(&limit.saturating_sub(used).to_le_bytes());
    true
}

/// `NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS`: `gpuNameStringFlags` @0,
/// `ascii[64]` @4 (ctrl2080gpu.h:338, `NV2080_GPU_MAX_NAME_STRING_LENGTH` = 64).
pub use nvrm_abi::mediate::{name_max, name_off};

/// Replace vendor prefixes with Leandro and append the guest framebuffer size.
/// Examples: Leandro RTX 2070, Leandro RTX 2070-3G, Leandro RTX 2070-1536M.
/// Drop a suffix that would exceed the name buffer; never truncate its size.
pub fn guest_card_name<A: RmAbi>(real: &str, profile: Profile) -> String {
    let limit = profile.fb_length;
    let real = real.trim();
    let real = real.strip_prefix("NVIDIA ").unwrap_or(real);
    let base = real.strip_prefix("GeForce ").unwrap_or(real);

    let suffix = if limit == 0 {
        String::new()
    } else {
        let mib = limit / (1 << 20);
        if mib >= 1024 && mib % 1024 == 0 {
            format!("-{}G", mib / 1024)
        } else {
            format!("-{mib}M")
        }
    };

    let full = format!("Leandro {base}{suffix}");
    if full.len() < name_max::<nvrm_sys::DefaultAbi>() {
        return full;
    }
    let short = format!("Leandro {base}");
    if short.len() < name_max::<nvrm_sys::DefaultAbi>() {
        return short;
    }
    // Nothing sensible fits. Say the one thing that matters and stop.
    "Leandro GPU".to_string()
}

/// Write a NUL-terminated, padded name; leave short buffers unchanged.
pub fn rewrite_gpu_name<A: RmAbi>(aux: &mut [u8], profile: Profile) -> Option<String> {
    if aux.len() < name_off::<nvrm_sys::DefaultAbi>() + name_max::<nvrm_sys::DefaultAbi>() {
        return None;
    }
    let raw = &aux[name_off::<nvrm_sys::DefaultAbi>()
        ..name_off::<nvrm_sys::DefaultAbi>() + name_max::<nvrm_sys::DefaultAbi>()];
    let end = raw
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(name_max::<nvrm_sys::DefaultAbi>());
    let real = String::from_utf8_lossy(&raw[..end]).to_string();

    let name = guest_card_name::<nvrm_sys::DefaultAbi>(&real, profile);
    let b = name.as_bytes();
    let n = b.len().min(name_max::<nvrm_sys::DefaultAbi>() - 1);
    aux[name_off::<nvrm_sys::DefaultAbi>()..name_off::<nvrm_sys::DefaultAbi>() + n]
        .copy_from_slice(&b[..n]);
    for byte in aux[name_off::<nvrm_sys::DefaultAbi>() + n
        ..name_off::<nvrm_sys::DefaultAbi>() + name_max::<nvrm_sys::DefaultAbi>()]
        .iter_mut()
    {
        *byte = 0;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact `NV_MEMORY_ALLOCATION_PARAMS` bytes of a measured request.
    fn params(flags: u32, attr: u32, size: u64) -> Vec<u8> {
        let mut v = vec![0u8; P_LEN];
        v[P_FLAGS..P_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        v[P_ATTR..P_ATTR + 4].copy_from_slice(&attr.to_le_bytes());
        v[P_SIZE..P_SIZE + 8].copy_from_slice(&size.to_le_bytes());
        v
    }

    #[test]
    fn the_measured_requests_are_classified_as_measured() {
        // 512 MiB VRAM block, torch (hClass 0x40).
        assert_eq!(
            request_bytes(0x40, &params(0x1c101, 0x18000000, 0x2000_0000)),
            Some(0x2000_0000)
        );
        // Sysmem staging buffer (hClass 0x3e, LOCATION_PCI).
        assert_eq!(
            request_bytes(0x3e, &params(0xc001, 0x3a000000, 0x1000)),
            None
        );
        // A 4.2 GB virtual reservation consumes no physical framebuffer.
        assert_eq!(
            request_bytes(0x50a0, &params(0x8c415, 0x16000000, 0xfb00_0000)),
            None
        );
        // 0x71 carries a 40-byte struct; it must never be decoded here.
        assert_eq!(request_bytes(0x71, &params(0, 0, 0x2000_0000)), None);
        // Short aux is not this struct.
        assert_eq!(
            request_bytes(0x40, &params(0x1c101, 0x18000000, 1)[..64]),
            None
        );
    }

    /// The table at [`request_bytes`], row by row: of every class and
    /// LOCATION, only 0x40 with VIDMEM can occupy FB. Before 2026-09-17 the
    /// ANY rows were charged, and at a full ledger refused.
    #[test]
    fn only_vidmem_on_the_video_memory_class_is_charged() {
        let (vid, pci, any) = (
            nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM),
            nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI),
            nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_ANY),
        );
        assert_eq!(request_bytes(0x40, &params(0, vid, 4096)), Some(4096));
        assert_eq!(
            request_bytes(0x40, &params(0, any, 4096)),
            None,
            "RM refuses ANY on 0x40"
        );
        assert_eq!(request_bytes(0x40, &params(0, pci, 4096)), None, "and PCI");
        for attr in [vid, pci, any] {
            assert_eq!(
                request_bytes(0x3e, &params(0, attr, 4096)),
                None,
                "0x3e is system memory"
            );
            assert_eq!(
                request_bytes(0x50a0, &params(0, attr, 4096)),
                None,
                "0x50a0 is address space"
            );
        }
        // PROTECTED 0x3e comes BACK as VIDMEM and is still sysmem: it must
        // not be charged on the way in, because settle would keep it.
        const ALLOC_FLAGS_PROTECTED: u32 = 0x0100_0000;
        assert_eq!(
            request_bytes(0x3e, &params(ALLOC_FLAGS_PROTECTED, any, 4096)),
            None
        );

        // The other door: RM picks the class from the same two fields.
        let ask = |flags, attr| {
            vidheap_request_bytes(&nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, flags, attr, 4096))
        };
        assert_eq!(ask(0, vid), Some(4096), "VIDMEM -> 0x40");
        assert_eq!(ask(0, any), None, "ANY -> 0x3e, system memory");
        assert_eq!(ask(0, pci), None, "PCI -> 0x3e");
        assert_eq!(ask(ALLOC_FLAGS_VIRTUAL, vid), None, "VIRTUAL -> 0x50a0");
    }

    /// Only VIDMEM is charged, and a VIDMEM request whose attr comes back
    /// as anything else still gives its charge back: the written-back attr
    /// has the last word.
    #[test]
    fn a_charge_whose_attr_comes_back_elsewhere_is_given_back() {
        let led = Ledger::for_test(1 << 20);
        let mut b = Books::new(7, led.clone());
        assert!(b.reserve(4096));
        assert_eq!(led.used(), 4096);
        // RM wrote something other than VIDMEM back -> the charge goes back.
        let pci = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI);
        b.settle(
            true,
            pci,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0x5c0000ab,
                bytes: 4096,
            },
        );
        assert_eq!(led.used(), 0);
    }

    #[test]
    fn a_refused_alloc_gives_its_reservation_back() {
        let led = Ledger::for_test(1 << 20);
        let mut b = Books::new(7, led.clone());
        assert!(b.reserve(4096));
        b.settle(
            false,
            0,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0x5c0000ab,
                bytes: 4096,
            },
        );
        assert_eq!(led.used(), 0);
        assert_eq!(b.owed(), 0);
    }

    #[test]
    fn the_cap_refuses_and_lets_go_again() {
        let led = Ledger::for_test(8192);
        let mut b = Books::new(7, led.clone());
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);

        assert!(b.reserve(4096));
        b.settle(
            true,
            vid,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0xaa,
                bytes: 4096,
            },
        );
        assert!(b.reserve(4096));
        b.settle(
            true,
            vid,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0xbb,
                bytes: 4096,
            },
        );
        assert_eq!(led.used(), 8192);

        // Full.
        assert!(!b.reserve(1));

        // One handle freed -> room again.
        b.free_object(0xc1d8, 0xaa);
        assert_eq!(led.used(), 4096);
        assert!(b.reserve(4096));
    }

    #[test]
    fn freeing_a_parent_or_a_client_takes_the_children() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        for victim in [0x5c000002u32, 0xc1d8u32] {
            let led = Ledger::for_test(1 << 30);
            let mut b = Books::new(7, led.clone());
            for h in [0xaau32, 0xbb, 0xcc] {
                assert!(b.reserve(4096));
                b.settle(
                    true,
                    vid,
                    Charge {
                        token: 1,
                        root: 0xc1d8,
                        parent: 0x5c000002,
                        handle: h,
                        bytes: 4096,
                    },
                );
            }
            assert_eq!(led.used(), 12288);
            b.free_object(0xc1d8, victim);
            assert_eq!(
                led.used(),
                0,
                "freeing {victim:#x} must take the memory objects"
            );
        }
    }

    /// Equal handles in separate clients must retain independent charges.
    #[test]
    fn two_clients_may_use_the_same_handle_number() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Ledger::for_test(1 << 30);
        let mut b = Books::new(7, led.clone());

        assert!(b.reserve(4096));
        b.settle(
            true,
            vid,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0xaa,
                bytes: 4096,
            },
        );
        assert!(b.reserve(4096));
        b.settle(
            true,
            vid,
            Charge {
                token: 1,
                root: 0xdddd,
                parent: 0x5c000002,
                handle: 0xaa,
                bytes: 4096,
            },
        );
        assert_eq!(led.used(), 8192, "two clients, two charges");

        b.free_object(0xc1d8, 0xaa);
        assert_eq!(led.used(), 4096, "only the first client's object went");
        b.free_object(0xdddd, 0xaa);
        assert_eq!(led.used(), 0);
    }

    #[test]
    fn an_object_handle_matching_another_client_does_not_release_that_client() {
        let ledger = Ledger::for_test(8192);
        let mut books = Books::new(7, ledger.clone());
        for (root, handle) in [(1, 2), (2, 3)] {
            assert!(books.reserve(4096));
            books.settle(
                true,
                vidmem_attr(),
                Charge {
                    token: 1,
                    root,
                    parent: 4,
                    handle,
                    bytes: 4096,
                },
            );
        }
        books.free_object(1, 2);
        assert_eq!(ledger.used(), 4096);
        assert_eq!(books.owed(), 4096);
        assert!(books.open.contains_key(&key_of(2, 3)));

        books.free_object(2, 2);
        assert_eq!(ledger.used(), 0);
    }

    #[test]
    fn closing_the_fd_takes_what_rode_on_it() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Ledger::for_test(1 << 30);
        let mut b = Books::new(7, led.clone());
        b.reserve(4096);
        b.settle(
            true,
            vid,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0xaa,
                bytes: 4096,
            },
        );
        b.reserve(4096);
        b.settle(
            true,
            vid,
            Charge {
                token: 2,
                root: 0xdddd,
                parent: 0x5c000002,
                handle: 0xbb,
                bytes: 4096,
            },
        );

        b.close_token(1);
        assert_eq!(led.used(), 4096, "only the charge on token 1 goes");
        b.close_token(2);
        assert_eq!(led.used(), 0);
    }

    #[test]
    fn a_dying_session_pays_its_debt() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Ledger::for_test(1 << 30);
        {
            let mut b = Books::new(7, led.clone());
            for h in 0..10u32 {
                assert!(b.reserve(1 << 20));
                b.settle(
                    true,
                    vid,
                    Charge {
                        token: 1,
                        root: 0xc1d8,
                        parent: 0x5c000002,
                        handle: h,
                        bytes: 1 << 20,
                    },
                );
            }
            assert_eq!(led.used(), 10 << 20);
        }
        assert_eq!(led.used(), 0, "Drop settles what no message ever announced");
    }

    #[test]
    fn a_guest_word_cannot_wrap_the_counter() {
        let led = Ledger::for_test(1 << 20);
        let mut b = Books::new(7, led.clone());
        assert!(b.reserve(4096));
        // 4096 + (u64::MAX - 4095) must be refused, not wrap to zero.
        assert!(!b.reserve(u64::MAX - 4095));
        assert!(!b.reserve(u64::MAX));
        assert_eq!(led.used(), 4096);
    }

    #[test]
    fn counter_overflow_is_refused_without_losing_existing_charges() {
        for ledger in [Ledger::off(), Ledger::for_test(u64::MAX)] {
            assert!(ledger.charge(u64::MAX - 1));
            assert!(!ledger.charge(2));
            assert_eq!(ledger.used(), u64::MAX - 1);
            assert!(ledger.charge(1));
            assert_eq!(ledger.used(), u64::MAX);
            assert!(!ledger.charge(1));
            ledger.release(u64::MAX);
            assert_eq!(ledger.used(), 0);
        }
    }

    /// Guest process usage is counted even when no VRAM cap is configured.
    #[test]
    fn with_no_limit_the_books_still_count() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Ledger::off();
        assert!(!led.enabled(), "no limit is set");
        let mut b = Books::new(7, led.clone());

        assert!(b.reserve(8 << 30), "no configured VRAM cap");
        b.settle(
            true,
            vid,
            Charge {
                token: 1,
                root: 0xc1d8,
                parent: 0x5c000002,
                handle: 0xaa,
                bytes: 8 << 30,
            },
        );
        assert_eq!(led.used(), 8 << 30, "and it is still counted");
        assert_eq!(b.owed(), 8 << 30);

        // Overflow must not accept bytes that the ledger cannot count.
        assert!(!b.reserve(u64::MAX));
        assert_eq!(led.used(), 8 << 30);
    }

    /// The roster is what the process list is built from: it appears with
    /// the identity, tracks the bytes, and leaves with the session.
    #[test]
    fn the_roster_follows_the_session() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Ledger::off();
        {
            let mut b = Books::new(7, led.clone());
            assert!(led.roster().is_empty(), "no identity yet, no row");

            b.announce(4711, "python3");
            assert_eq!(led.roster().len(), 1);
            assert_eq!(led.roster()[0].guest_pid, 4711);
            assert_eq!(led.roster()[0].bytes, 0);

            b.reserve(4 << 20);
            b.settle(
                true,
                vid,
                Charge {
                    token: 1,
                    root: 0xc1d8,
                    parent: 0x5c000002,
                    handle: 0xaa,
                    bytes: 4 << 20,
                },
            );
            assert_eq!(led.roster()[0].bytes, 4 << 20, "the row tracks the bytes");

            b.free_object(0xc1d8, 0xaa);
            assert_eq!(led.roster()[0].bytes, 0, "and follows them back down");
        }
        assert!(led.roster().is_empty(), "the row dies with the session");
    }

    // ---- the card the guest sees ---------------------------------------

    fn pids_buf() -> Vec<u8> {
        // As RM leaves it: three HOST pids, count 3.
        let mut v = vec![0u8; PIDS_LEN];
        v[PIDS_COUNT_OFF..PIDS_COUNT_OFF + 4].copy_from_slice(&3u32.to_le_bytes());
        for (i, pid) in [1036u32, 276821, 3085239].iter().enumerate() {
            let o = PIDS_TBL_OFF + 4 * i;
            v[o..o + 4].copy_from_slice(&pid.to_le_bytes());
        }
        v
    }

    fn pid_at(v: &[u8], i: usize) -> u32 {
        let o = PIDS_TBL_OFF + 4 * i;
        u32::from_le_bytes(v[o..o + 4].try_into().unwrap())
    }

    /// The whole point: the guest's list is the guest's, and the host PIDs
    /// RM put there are gone. Measured 2026-08-06: without this the guest
    /// receives eight host PIDs together with their per-process FB usage.
    #[test]
    fn the_host_pids_do_not_survive_the_rewrite() {
        let mut v = pids_buf();
        assert_eq!(pid_at(&v, 0), 1036, "RM really did put a host PID there");
        let roster = vec![
            ProcRow {
                guest_pid: 4674,
                bytes: 380 << 20,
                name: "python3".into(),
            },
            ProcRow {
                guest_pid: 4676,
                bytes: 648 << 20,
                name: "python3".into(),
            },
        ];
        assert_eq!(rewrite_get_pids(&mut v, &roster), Some(2));
        assert_eq!(
            u32::from_le_bytes(v[PIDS_COUNT_OFF..PIDS_COUNT_OFF + 4].try_into().unwrap()),
            2
        );
        assert_eq!(pid_at(&v, 0), 4674);
        assert_eq!(pid_at(&v, 1), 4676);
        assert_eq!(
            pid_at(&v, 2),
            0,
            "the third host PID was overwritten, not left behind"
        );
        for i in 2..PIDS_MAX {
            assert_eq!(
                pid_at(&v, i),
                0,
                "no host PID survives anywhere in the table"
            );
        }
    }

    /// A short PID buffer must not be interpreted or partially rewritten.
    #[test]
    fn a_short_pids_buffer_is_refused_not_guessed() {
        let mut v = vec![0u8; PIDS_LEN - 1];
        assert_eq!(rewrite_get_pids(&mut v, &[]), None);
    }

    fn info_buf(entries: &[(u32, u32)], len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4]
            .copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (i, (pid, index)) in entries.iter().enumerate() {
            let b = PIDINFO_LIST_OFF + PIDINFO_ENTRY * i;
            v[b..b + 4].copy_from_slice(&pid.to_le_bytes());
            v[b + 4..b + 8].copy_from_slice(&index.to_le_bytes());
            // RM's answer about a host process, which must not survive.
            let o = b + PIDINFO_MEM_PRIVATE;
            v[o..o + 8].copy_from_slice(&(999u64 << 20).to_le_bytes());
        }
        v
    }

    fn priv_at(v: &[u8], i: usize) -> u64 {
        let o = PIDINFO_LIST_OFF + PIDINFO_ENTRY * i + PIDINFO_MEM_PRIVATE;
        u64::from_le_bytes(v[o..o + 8].try_into().unwrap())
    }

    #[test]
    fn pid_info_is_answered_from_our_own_books() {
        let mut v = info_buf(&[(4674, 0), (4676, 0), (9999, 0)], PIDINFO_LEN);
        let roster = vec![
            ProcRow {
                guest_pid: 4674,
                bytes: 380 << 20,
                name: "python3".into(),
            },
            ProcRow {
                guest_pid: 4676,
                bytes: 648 << 20,
                name: "python3".into(),
            },
        ];
        assert_eq!(rewrite_get_pid_info(&mut v, &roster), Some(3));
        assert_eq!(priv_at(&v, 0), 380 << 20);
        assert_eq!(priv_at(&v, 1), 648 << 20);
        assert_eq!(
            priv_at(&v, 2),
            0,
            "a PID that is not ours holds nothing of ours"
        );
        for i in 0..3 {
            let o = PIDINFO_LIST_OFF + PIDINFO_ENTRY * i + 8;
            assert_eq!(
                u32::from_le_bytes(v[o..o + 4].try_into().unwrap()),
                sys::NV_OK
            );
        }
    }

    /// A claimed entry count cannot permit writes beyond the received buffer.
    #[test]
    fn a_lying_count_cannot_write_past_the_buffer() {
        // Room for two entries, but the guest claims the header maximum.
        let len = PIDINFO_LIST_OFF + 2 * PIDINFO_ENTRY;
        let mut v = vec![0u8; len];
        v[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4]
            .copy_from_slice(&(PIDINFO_MAX as u32).to_le_bytes());
        assert_eq!(
            rewrite_get_pid_info(&mut v, &[]),
            Some(2),
            "clamped to what fits"
        );
        assert_eq!(
            u32::from_le_bytes(
                v[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4]
                    .try_into()
                    .unwrap()
            ),
            2,
            "and the guest is told the truth about how many it got"
        );
    }

    fn fb_buf(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut v = vec![0u8; FBINFO_LIST_OFF + FBINFO_ENTRY * FBINFO_MAX];
        v[0..4].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (i, (index, data)) in entries.iter().enumerate() {
            let o = FBINFO_LIST_OFF + FBINFO_ENTRY * i;
            v[o..o + 4].copy_from_slice(&index.to_le_bytes());
            v[o + 4..o + 8].copy_from_slice(&data.to_le_bytes());
        }
        v
    }

    fn fb_at(v: &[u8], i: usize) -> u32 {
        let o = FBINFO_LIST_OFF + FBINFO_ENTRY * i + 4;
        u32::from_le_bytes(v[o..o + 4].try_into().unwrap())
    }

    /// A capped response must report consistent total, heap and free sizes.
    #[test]
    fn the_capped_card_does_not_contradict_itself() {
        let mut v = fb_buf(&[
            (FB_INFO_INDEX_HEAP_FREE, 0x69f000),
            (FB_INFO_INDEX_TOTAL_RAM_SIZE, 0x800000),
            (FB_INFO_INDEX_HEAP_SIZE, 0x797240),
        ]);
        let limit = 2048u64 << 20;
        let used = 982u64 << 20;
        assert_eq!(rewrite_fb_info(&mut v, limit, used), Some(3));
        assert_eq!(
            fb_at(&v, 0),
            ((limit - used) / 1024) as u32,
            "free = limit - used"
        );
        assert_eq!(fb_at(&v, 1), (limit / 1024) as u32);
        assert_eq!(fb_at(&v, 2), (limit / 1024) as u32);
        assert!(
            fb_at(&v, 0) < fb_at(&v, 1),
            "free below total, in every case"
        );
    }

    /// Indices that are not sizes are RM's business and stay untouched.
    #[test]
    fn non_size_indices_are_left_alone() {
        let mut v = fb_buf(&[
            (0x1a, 0xf),
            (FB_INFO_INDEX_HEAP_SIZE, 0x797240),
            (0x23, 0x20),
        ]);
        assert_eq!(rewrite_fb_info(&mut v, 1 << 30, 0), Some(1));
        assert_eq!(fb_at(&v, 0), 0xf);
        assert_eq!(fb_at(&v, 2), 0x20);
    }

    /// Without a cap the VM has the whole card, and saying anything else
    /// would be the lie.
    #[test]
    fn without_a_cap_the_card_keeps_its_size() {
        let mut v = fb_buf(&[(FB_INFO_INDEX_HEAP_SIZE, 0x797240)]);
        assert_eq!(rewrite_fb_info(&mut v, 0, 0), None);
        assert_eq!(fb_at(&v, 0), 0x797240);
    }

    /// A bare `NV2080_CTRL_FB_INFO[]`, the way the V1 form delivers it:
    /// no count word in front, because that one stays in the params buffer.
    fn fb_list(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut v = vec![0u8; FBINFO_ENTRY * entries.len()];
        for (i, &(index, data)) in entries.iter().enumerate() {
            let o = FBINFO_ENTRY * i;
            v[o..o + 4].copy_from_slice(&index.to_le_bytes());
            v[o + 4..o + 8].copy_from_slice(&data.to_le_bytes());
        }
        v
    }

    fn list_at(v: &[u8], i: usize) -> u32 {
        let o = FBINFO_ENTRY * i + 4;
        u32::from_le_bytes(v[o..o + 4].try_into().unwrap())
    }

    /// The regression this function was written for. Measured
    /// 2026-08-15 under a 4096 MiB cap: guest `nvidia-smi` said 4096 MiB
    /// (V2, capped) while `vulkaninfo` reported an 8 GiB heap (V1,
    /// uncapped). Both doors must now answer the same card.
    #[test]
    fn the_v1_door_answers_the_same_card_as_the_v2_door() {
        let entries = [
            (FB_INFO_INDEX_HEAP_FREE, 0x69f000),
            (FB_INFO_INDEX_TOTAL_RAM_SIZE, 0x800000),
            (FB_INFO_INDEX_HEAP_SIZE, 0x797240),
        ];
        let (limit, used) = (2048u64 << 20, 982u64 << 20);

        let mut v2 = fb_buf(&entries);
        let mut v1 = fb_list(&entries);
        assert_eq!(rewrite_fb_info(&mut v2, limit, used), Some(3));
        assert_eq!(
            rewrite_fb_info_list(&mut v1, entries.len(), limit, used),
            Some(3)
        );

        for i in 0..entries.len() {
            assert_eq!(
                list_at(&v1, i),
                fb_at(&v2, i),
                "entry {i}: the two doors disagree about the same card"
            );
        }
        assert_eq!(
            list_at(&v1, 1),
            (limit / 1024) as u32,
            "8 GiB card capped to 2 GiB"
        );
    }

    /// The count comes from the params buffer, the room from the nested
    /// block, and a mismatch must clamp rather than read past the end.
    #[test]
    fn the_v1_list_never_reads_past_its_block() {
        let mut v = fb_list(&[(FB_INFO_INDEX_HEAP_SIZE, 0x797240)]);
        // The guest claims 128 entries and sent room for one.
        assert_eq!(rewrite_fb_info_list(&mut v, 128, 1 << 30, 0), Some(1));
        assert_eq!(list_at(&v, 0), (1u64 << 30) as u32 / 1024);
    }

    /// An NVOS32_PARAMETERS with an ALLOC_SIZE member, built field by
    /// field at the offsets the layout guard fixes.
    fn nvos32(function: u32, flags: u32, attr: u32, size: u64) -> Vec<u8> {
        let mut v = vec![0u8; V_LEN];
        v[V_FUNCTION..V_FUNCTION + 4].copy_from_slice(&function.to_le_bytes());
        v[VA_FLAGS..VA_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        v[VA_ATTR..VA_ATTR + 4].copy_from_slice(&attr.to_le_bytes());
        v[VA_SIZE..VA_SIZE + 8].copy_from_slice(&size.to_le_bytes());
        v
    }

    fn vidmem_attr() -> u32 {
        nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM)
    }

    /// The regression this door was opened for: CS2 allocates through
    /// NVOS32 (1120 calls in its trace) and the ledger, hooked only on
    /// NVOS64, counted none of it. Both doors must now answer the same
    /// question the same way.
    #[test]
    fn the_vid_heap_door_charges_what_the_alloc_door_charges() {
        let size = 256u64 << 20;
        let attr = vidmem_attr();

        let mut nvos64 = vec![0u8; P_LEN];
        nvos64[P_ATTR..P_ATTR + 4].copy_from_slice(&attr.to_le_bytes());
        nvos64[P_SIZE..P_SIZE + 8].copy_from_slice(&size.to_le_bytes());

        let nvos32 = nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, 0, attr, size);

        assert_eq!(request_bytes(0x0040, &nvos64), Some(size));
        assert_eq!(
            vidheap_request_bytes(&nvos32),
            Some(size),
            "the other door must agree"
        );
    }

    /// The three refusals are the same three, in the same order.
    #[test]
    fn the_vid_heap_door_ignores_what_the_alloc_door_ignores() {
        let size = 64u64 << 20;
        assert_eq!(
            vidheap_request_bytes(&nvos32(
                sys::NVOS32_FUNCTION_ALLOC_SIZE,
                ALLOC_FLAGS_VIRTUAL,
                vidmem_attr(),
                size
            )),
            None,
            "a virtual reservation holds no memory"
        );
        let sysmem = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI);
        assert_eq!(
            vidheap_request_bytes(&nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, 0, sysmem, size)),
            None,
            "sysmem is not this cap's business"
        );
        let any = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_ANY);
        assert_eq!(
            vidheap_request_bytes(&nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, 0, any, size)),
            None,
            "neither is ANY, which RM allocates as 0x3e"
        );
        assert_eq!(
            vidheap_request_bytes(&nvos32(
                sys::NVOS32_FUNCTION_ALLOC_SIZE,
                0,
                vidmem_attr(),
                0
            )),
            None,
            "a zero-size allocation is RM's problem"
        );
    }

    /// Other NVOS32 union variants must not be interpreted as AllocSize.
    #[test]
    fn only_alloc_size_is_an_allocation() {
        for f in [
            sys::NVOS32_FUNCTION_FREE,
            sys::NVOS32_FUNCTION_INFO,
            sys::NVOS32_FUNCTION_ALLOC_SIZE_RANGE,
            sys::NVOS32_FUNCTION_HW_FREE,
        ] {
            let mut v = nvos32(f, 0, vidmem_attr(), 4 << 20);
            // Whatever those bytes mean for THIS function, they are not a
            // size, and nothing may be charged for them.
            v[VA_SIZE..VA_SIZE + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert_eq!(
                vidheap_request_bytes(&v),
                None,
                "function {f} is not an allocation"
            );
        }
    }

    /// A buffer too short to be this struct is not this struct, and every
    /// reader has to say so rather than index into it.
    #[test]
    fn a_short_buffer_is_never_an_nvos32() {
        for n in [0usize, 8, 40, V_LEN - 1] {
            let v = vec![0xffu8; n];
            assert_eq!(vidheap_function(&v), None, "len {n}");
            assert_eq!(vidheap_request_bytes(&v), None, "len {n}");
            assert_eq!(vidheap_attr_out(&v), 0, "len {n}");
            assert_eq!(vidheap_handle(&v), 0, "len {n}");
            let mut w = v.clone();
            assert!(!rewrite_vidheap_info(&mut w, 1 << 30, 0), "len {n}");
            assert_eq!(w, v, "len {n}: a short buffer is never written");
        }
    }

    /// An NVOS32_FUNCTION_INFO answer as the host RM gives it: its own heap
    /// in `total`, its own free memory in `free`, both in bytes, and the
    /// largest free block in `data.Info`.
    fn nvos32_info(total: u64, free: u64) -> Vec<u8> {
        let mut v = nvos32(sys::NVOS32_FUNCTION_INFO, 0, 0, 0);
        v[V_TOTAL..V_TOTAL + 8].copy_from_slice(&total.to_le_bytes());
        v[V_FREE..V_FREE + 8].copy_from_slice(&free.to_le_bytes());
        v[V_DATA + 16..V_DATA + 24].copy_from_slice(&0x1234_5000u64.to_le_bytes());
        v
    }

    fn u64_at(v: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(v[o..o + 8].try_into().unwrap())
    }

    /// NVOS32 INFO must report the same cap as both FB_GET_INFO forms.
    #[test]
    fn the_nvos32_info_door_answers_the_same_card_as_fb_get_info() {
        let (limit, used) = (2816u64 << 20, 982u64 << 20);
        let mut info = nvos32_info(7771 << 20, 6100 << 20);
        assert!(rewrite_vidheap_info(&mut info, limit, used));

        let mut v2 = fb_buf(&[
            (FB_INFO_INDEX_HEAP_FREE, 0x5d4000),
            (FB_INFO_INDEX_HEAP_SIZE, 0x797240),
        ]);
        assert_eq!(rewrite_fb_info(&mut v2, limit, used), Some(2));
        assert_eq!(
            u64_at(&info, V_FREE),
            fb_at(&v2, 0) as u64 * 1024,
            "free agrees"
        );
        assert_eq!(
            u64_at(&info, V_TOTAL),
            fb_at(&v2, 1) as u64 * 1024,
            "total agrees with the heap"
        );
        assert_eq!(
            u64_at(&info, V_DATA + 16),
            0x1234_5000,
            "data.Info is RM's and stays"
        );
    }

    /// Past the limit there is nothing free, not a wrapped 16 EiB.
    #[test]
    fn nvos32_info_free_saturates_at_zero() {
        let mut info = nvos32_info(7771 << 20, 6100 << 20);
        assert!(rewrite_vidheap_info(&mut info, 1 << 30, 2 << 30));
        assert_eq!(u64_at(&info, V_FREE), 0);
        assert_eq!(u64_at(&info, V_TOTAL), 1 << 30);
    }

    /// Only INFO fills `total`/`free`; every other function's bytes there are
    /// RM's (zeroed) and must not be turned into a size. And without a cap
    /// the VM has the whole card, on this door as on the others.
    #[test]
    fn nvos32_info_is_rewritten_only_for_info_and_only_under_a_cap() {
        for f in [
            sys::NVOS32_FUNCTION_ALLOC_SIZE,
            sys::NVOS32_FUNCTION_FREE,
            sys::NVOS32_FUNCTION_ALLOC_SIZE_RANGE,
            sys::NVOS32_FUNCTION_HW_FREE,
        ] {
            let v = nvos32(f, 0, vidmem_attr(), 4 << 20);
            let mut w = v.clone();
            assert!(!rewrite_vidheap_info(&mut w, 1 << 30, 0), "function {f}");
            assert_eq!(w, v, "function {f}: untouched");
        }
        let v = nvos32_info(7771 << 20, 6100 << 20);
        let mut w = v.clone();
        assert!(!rewrite_vidheap_info(&mut w, 0, 0));
        assert_eq!(w, v, "no cap: the host's answer stands");
    }

    /// Without a cap the V1 door stays honest too.
    #[test]
    fn without_a_cap_the_v1_door_keeps_the_card_size() {
        let mut v = fb_list(&[(FB_INFO_INDEX_HEAP_SIZE, 0x797240)]);
        assert_eq!(rewrite_fb_info_list(&mut v, 1, 0, 0), None);
        assert_eq!(list_at(&v, 0), 0x797240);
    }

    /// The name follows NVIDIA's own vGPU convention: the vendor prefix
    /// gives way to the mediation layer, the profile size joins the name.
    #[test]
    fn the_card_says_what_it_is() {
        let real = "NVIDIA GeForce RTX 2070";
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(real, Profile::OFF),
            "Leandro RTX 2070"
        );
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(real, Profile::accounting(2048 << 20)),
            "Leandro RTX 2070-2G"
        );
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(real, Profile::accounting(1024 << 20)),
            "Leandro RTX 2070-1G"
        );
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(real, Profile::accounting(1536 << 20)),
            "Leandro RTX 2070-1536M"
        );
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(
                "NVIDIA A100-SXM4-40GB",
                Profile::accounting(10240 << 20)
            ),
            "Leandro A100-SXM4-40GB-10G"
        );
    }

    /// Exercise both fallback branches at the default ABI's name-size boundary.
    #[test]
    fn a_name_that_does_not_fit_loses_the_suffix_whole() {
        // 8 + 53 + 3 = 64 -> does not fit, so the suffix goes as a unit
        // rather than being truncated into a wrong profile size.
        let b53 = "X".repeat(53);
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(
                &format!("NVIDIA GeForce {b53}"),
                Profile::accounting(2048 << 20)
            ),
            format!("Leandro {b53}")
        );
        // 8 + 56 = 64 -> even the bare name does not fit. Say the one thing
        // that matters and stop.
        let b56 = "X".repeat(56);
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(
                &format!("NVIDIA GeForce {b56}"),
                Profile::accounting(2048 << 20)
            ),
            "Leandro GPU"
        );
        // and the longest name that DOES fit still fits, to the last byte
        let b55 = "X".repeat(55);
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(&format!("NVIDIA GeForce {b55}"), Profile::OFF)
                .len(),
            63
        );
    }

    #[test]
    fn the_name_is_written_nul_terminated() {
        let mut v =
            vec![0xffu8; name_off::<nvrm_sys::DefaultAbi>() + name_max::<nvrm_sys::DefaultAbi>()];
        let real = b"NVIDIA GeForce RTX 2070";
        v[name_off::<nvrm_sys::DefaultAbi>()..name_off::<nvrm_sys::DefaultAbi>() + real.len()]
            .copy_from_slice(real);
        v[name_off::<nvrm_sys::DefaultAbi>() + real.len()] = 0;
        assert_eq!(
            rewrite_gpu_name::<nvrm_sys::DefaultAbi>(&mut v, Profile::accounting(2048 << 20))
                .as_deref(),
            Some("Leandro RTX 2070-2G")
        );
        let end = v[name_off::<nvrm_sys::DefaultAbi>()..]
            .iter()
            .position(|&c| c == 0)
            .unwrap();
        assert_eq!(
            &v[name_off::<nvrm_sys::DefaultAbi>()..name_off::<nvrm_sys::DefaultAbi>() + end],
            b"Leandro RTX 2070-2G"
        );
        assert!(
            v[name_off::<nvrm_sys::DefaultAbi>() + end..]
                .iter()
                .all(|&c| c == 0),
            "the tail is padded, not left over"
        );
    }

    /// Do not invent a display PID for a session without guest identity.
    #[test]
    fn a_process_without_an_identity_is_not_listed() {
        let led = Ledger::off();
        let b = Books::new(7, led.clone());
        b.announce(0, "");
        assert!(led.roster().is_empty());
    }

    // Test policy parsing without changing the process environment.
    // Empty launcher variables must behave like unset variables.

    const MIB: u64 = 1 << 20;

    fn three<'a>(
        limit: Option<&'a str>,
        profile: Option<&'a str>,
        reserve: Option<&'a str>,
    ) -> RawEnv<'a> {
        RawEnv {
            limit,
            profile,
            reserve,
            ..RawEnv::default()
        }
    }

    fn ok(limit: Option<&str>, profile: Option<&str>, reserve: Option<&str>) -> Profile {
        decide(three(limit, profile, reserve))
            .expect("configuration refused")
            .0
    }

    #[test]
    fn the_default_configuration_has_no_policy() {
        assert_eq!(ok(None, None, None), Profile::OFF);
        // ... and neither has the shape the rig actually passes.
        let (p, notes) = decide(three(Some(""), Some(""), Some(""))).unwrap();
        assert_eq!(p, Profile::OFF);
        assert!(
            notes.is_empty(),
            "an unset knob is not worth a warning: {notes:?}"
        );
    }

    #[test]
    fn the_old_cap_is_the_guests_number_and_reserves_nothing() {
        let p = ok(Some("3072"), None, None);
        assert_eq!(p.policy, Policy::Accounting);
        assert_eq!(p.size, 3072 * MIB);
        assert_eq!(p.reservation, 0, "accounting reserves nothing, and says so");
        assert_eq!(p.fb_length, 3072 * MIB);
        // The startup line is grepped and appears in every measurement
        // taken before this policy existed. It does not move.
        assert_eq!(
            p.announce().unwrap(),
            "VRAM cap 3072 MiB for this VM (LEA_VRAM_LIMIT_MIB)"
        );
    }

    #[test]
    fn a_profile_is_the_cards_number_and_the_reservation_comes_off_it() {
        let p = ok(None, Some("3072"), None);
        assert_eq!(p.policy, Policy::Reserved);
        assert_eq!(p.size, 3072 * MIB, "what the VM may cost the card");
        assert_eq!(p.reservation, DEFAULT_RESERVATION_MIB * MIB);
        assert_eq!(p.fb_length, (3072 - DEFAULT_RESERVATION_MIB) * MIB);
        assert_eq!(
            p.size,
            p.fb_length + p.reservation,
            "the three numbers are one arithmetic, not three settings"
        );
        // The measured overhead this default is sized against: ~175 MiB per
        // backend on 2026-08-21 (number 68). The reservation has to be
        // larger, or it does not cover what it exists to cover.
        assert!(p.reservation > 175 * MIB);
    }

    #[test]
    fn the_reservation_is_a_knob_because_the_measurement_is_a_measurement() {
        let p = ok(None, Some("3072"), Some("300"));
        assert_eq!(p.reservation, 300 * MIB);
        assert_eq!(p.fb_length, 2772 * MIB);
    }

    #[test]
    fn two_policies_for_one_number_are_refused_rather_than_ranked() {
        let e = decide(three(Some("3072"), Some("3072"), None)).unwrap_err();
        assert!(e.contains("both set"), "{e}");
        // Neither wins by being first, last or larger.
        assert!(decide(three(Some("1024"), Some("8192"), None)).is_err());
    }

    #[test]
    fn a_reservation_that_eats_the_profile_is_refused() {
        assert!(decide(three(None, Some("256"), Some("256"))).is_err());
        assert!(
            decide(three(None, Some("128"), None)).is_err(),
            "the default eats a small profile"
        );
        // One MiB of framebuffer is a policy, not a contradiction.
        assert_eq!(ok(None, Some("257"), Some("256")).fb_length, MIB);
    }

    /// Unit-suffixed values must fail startup and suggest a numeric MiB value.
    #[test]
    fn a_value_that_is_not_mib_refuses_to_start() {
        let e = decide(three(Some("3 GiB"), None, None)).unwrap_err();
        assert!(
            e.contains("LEA_VRAM_LIMIT_MIB=\"3 GiB\"") && e.contains("write 3072"),
            "{e}"
        );
        let e = decide(grid("RTX2070-4Q", "4G", "3072")).unwrap_err();
        assert!(
            e.contains("LEA_VGPU_PROFILE_MIB") && e.contains("write 4096"),
            "{e}"
        );
        assert!(
            decide(three(None, Some("-1"), None)).is_err(),
            "a negative number is no size either"
        );
        // A reservation without a profile reserves from nothing. That is
        // worth a line, because the operator plainly meant something.
        let (p, notes) = decide(three(None, None, Some("256"))).unwrap();
        assert_eq!(p, Profile::OFF);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("does nothing"), "{:?}", notes[0]);
    }

    #[test]
    fn mib_values_must_fit_the_byte_counter() {
        let overflow = "17592186044416"; // 2^44 MiB would wrap to zero bytes.
        let cases = [
            ("LEA_VRAM_LIMIT_MIB", three(Some(overflow), None, None)),
            ("LEA_VRAM_PROFILE_MIB", three(None, Some(overflow), None)),
            (
                "LEA_VRAM_RESERVE_MIB",
                three(None, Some("3072"), Some(overflow)),
            ),
            ("LEA_VGPU_PROFILE_MIB", grid("test", overflow, "1536")),
            ("LEA_VGPU_FB_MIB", grid("test", "2048", overflow)),
        ];
        for (name, env) in cases {
            let error = decide(env).unwrap_err();
            assert!(
                error.contains(name) && error.contains("byte counter range"),
                "{error}"
            );
        }

        let largest = (u64::MAX >> 20).to_string();
        let (profile, _) = decide(three(Some(&largest), None, None)).unwrap();
        assert_eq!(profile.fb_length, u64::MAX & !((1 << 20) - 1));
        assert_eq!(profile.policy, Policy::Accounting);
    }

    /// Report and enforce fbLength; never offer the reserved profile portion.
    #[test]
    fn the_guest_is_told_exactly_what_it_may_allocate() {
        let p = ok(None, Some("3072"), None);
        let led = Ledger::for_test_profile(p);
        assert_eq!(led.limit(), p.fb_length);
        assert!(
            led.limit() < p.size,
            "the reservation is real, or it is nothing"
        );

        // What the guest's nvidia-smi and every Vulkan client are told.
        let mut v = fb_buf(&[
            (FB_INFO_INDEX_TOTAL_RAM_SIZE, 0x800000),
            (FB_INFO_INDEX_HEAP_SIZE, 0x797240),
            (FB_INFO_INDEX_HEAP_FREE, 0x69f000),
        ]);
        assert_eq!(rewrite_fb_info(&mut v, led.limit(), led.used()), Some(3));
        assert_eq!(
            fb_at(&v, 0),
            (p.fb_length / 1024) as u32,
            "total is fbLength"
        );
        assert_eq!(fb_at(&v, 1), (p.fb_length / 1024) as u32);
        assert_eq!(
            fb_at(&v, 2),
            (p.fb_length / 1024) as u32,
            "and so is free, empty"
        );

        // ... and the card's name says the same number, not the profile.
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>("NVIDIA GeForce RTX 2070", p),
            "Leandro RTX 2070-2816M"
        );

        // The enforced number is fbLength: the last byte of it goes in, the
        // next one does not, and the reservation is never available.
        let mut b = Books::new(1, led.clone());
        assert!(b.reserve(p.fb_length));
        assert!(!b.reserve(1), "the reservation is not spare change");
        assert_eq!(led.used(), p.fb_length);
    }

    /// And the counter-check: under the OLD policy the guest is told the
    /// whole cap, because nothing was held back from it. The two policies
    /// differ in what the guest sees, which is the only place the
    /// difference is observable from inside the VM.
    #[test]
    fn the_two_policies_show_the_guest_different_cards() {
        let acct = Ledger::for_test(3072 * MIB);
        let resv = Ledger::for_test_profile(ok(None, Some("3072"), None));
        assert_eq!(acct.limit(), 3072 * MIB);
        assert_eq!(resv.limit(), 2816 * MIB);
        assert_eq!(
            acct.profile().size,
            resv.profile().size,
            "same bill to the card"
        );
        assert!(
            resv.limit() < acct.limit(),
            "and a different one to the guest"
        );
    }

    // Policy integration tests; nvrm_abi::vgpu tests the catalogue arithmetic.

    fn grid<'a>(t: &'a str, profile: &'a str, fb: &'a str) -> RawEnv<'a> {
        RawEnv {
            vgpu_type: Some(t),
            vgpu_profile: Some(profile),
            vgpu_fb: Some(fb),
            ..RawEnv::default()
        }
    }

    #[test]
    fn a_vgpu_type_carries_its_catalogue_numbers_or_is_refused() {
        // The 2Q row this card really answered on 2026-08-21: 2048 MiB
        // profile, 1536 MiB guest FB, 6 segments of 256 MiB.
        let p = decide(grid("RTX2070-2Q", "2048", "1536")).unwrap().0;
        assert_eq!(p.policy, Policy::Grid);
        assert_eq!(p.size, 2048 * MIB);
        assert_eq!(p.fb_length, 1536 * MIB);
        assert_eq!(p.reservation, 512 * MIB);
        assert_eq!(p.vgpu_type, "RTX2070-2Q");

        // A name on its own is a name for something nobody computed.
        let e = decide(RawEnv {
            vgpu_type: Some("RTX2070-2Q"),
            ..RawEnv::default()
        })
        .unwrap_err();
        assert!(e.contains("vgpuprofile"), "{e}");
        // ... and a profile that reserves nothing is not a vGPU profile.
        assert!(decide(grid("RTX2070-2Q", "2048", "2048")).is_err());
    }

    #[test]
    fn three_policies_for_one_number_are_refused_too() {
        let e = decide(RawEnv {
            limit: Some("3072"),
            vgpu_type: Some("RTX2070-2Q"),
            vgpu_profile: Some("2048"),
            vgpu_fb: Some("1536"),
            ..RawEnv::default()
        })
        .unwrap_err();
        assert!(e.contains("three policies"), "{e}");
    }

    /// Card names use the guest framebuffer size for every policy.
    #[test]
    fn the_grid_card_is_named_after_its_framebuffer() {
        let p = decide(grid("RTX2070-2Q", "2048", "1536")).unwrap().0;
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>("NVIDIA GeForce RTX 2070", p),
            "Leandro RTX 2070-1536M"
        );

        // ... and the sizes it is told are the type's, not the card's.
        let led = Ledger::for_test_profile(p);
        let mut v = fb_buf(&[
            (FB_INFO_INDEX_TOTAL_RAM_SIZE, 0x800000),
            (FB_INFO_INDEX_HEAP_FREE, 0x69f000),
        ]);
        assert_eq!(rewrite_fb_info(&mut v, led.limit(), led.used()), Some(2));
        assert_eq!(fb_at(&v, 0), (1536 * MIB / 1024) as u32);
        assert_eq!(led.limit(), 1536 * MIB, "and refused at the same number");
    }

    /// Equal framebuffer sizes produce equal names and encoder shares.
    /// Without a card query, use the launcher's encoder share.
    #[test]
    fn the_same_framebuffer_is_the_same_card_under_every_policy() {
        let card = 8192 * MIB;
        let with_card = |env: RawEnv| {
            decide(RawEnv {
                card_total: card,
                ..env
            })
            .unwrap()
            .0
        };
        let profiles = [
            with_card(three(Some("3072"), None, None)),
            with_card(three(None, Some("3328"), None)),
            with_card(RawEnv {
                vgpu_encoder: Some("50"),
                ..grid("RTX2070-4Q", "3968", "3072")
            }),
            with_card(grid("RTX2070-3G", "3968", "3072")),
        ];
        for p in profiles {
            assert_eq!(p.fb_length, 3072 * MIB);
            assert_eq!(p.encoder_capacity, 37, "{:?}", p.policy);
            assert_eq!(
                guest_card_name::<nvrm_sys::DefaultAbi>("NVIDIA GeForce RTX 2070", p),
                "Leandro RTX 2070-3G"
            );
        }
        let launcher = RawEnv {
            vgpu_encoder: Some("37"),
            ..grid("RTX2070-3G", "3968", "3072")
        };
        assert_eq!(
            decide(launcher).unwrap().0.encoder_capacity,
            37,
            "no card: the launcher's share"
        );
        assert_eq!(
            with_card(RawEnv::default()).encoder_capacity,
            0,
            "no cap: RM's own answer"
        );
    }

    /// The other two policies keep the name they had, and this is the
    /// counter-test for it: nothing about them moved.
    #[test]
    fn the_older_policies_keep_their_names() {
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(
                "NVIDIA GeForce RTX 2070",
                Profile::accounting(3072 * MIB)
            ),
            "Leandro RTX 2070-3G"
        );
        assert_eq!(
            guest_card_name::<nvrm_sys::DefaultAbi>(
                "NVIDIA GeForce RTX 2070",
                ok(None, Some("3072"), None)
            ),
            "Leandro RTX 2070-2816M"
        );
    }

    // =======================================================================
    // The refusal line (2026-09-17)
    // =======================================================================

    /// A refusal has to say what was asked, in the words a trace uses:
    /// the door, the class, the flags, the attr with its LOCATION spelled
    /// out, and the size.
    #[test]
    fn a_refusal_names_the_request() {
        let any = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_ANY);
        let ask = Ask::of_alloc(Door::Nvos64, 0x40, &params(0x1c101, any, 512 << 20)).unwrap();
        let line = ask.to_string();
        assert!(
            line.starts_with("NVOS64 RM_ALLOC class 0x40 flags 0x1c101"),
            "{line}"
        );
        assert!(
            line.contains("(ANY)") && line.contains("(512.0 MiB)"),
            "{line}"
        );

        let v = nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, 0, vidmem_attr(), 4 << 20);
        let line = Ask::of_vidheap(&v).unwrap().to_string();
        assert!(
            line.starts_with("NVOS32 VID_HEAP ALLOC_SIZE flags 0x0"),
            "{line}"
        );
        assert!(line.contains("(VIDMEM)"), "{line}");

        // FREE shares the struct and is not a request.
        assert_eq!(
            Ask::of_vidheap(&nvos32(sys::NVOS32_FUNCTION_FREE, 0, 0, 0)),
            None
        );
    }

    /// One process retrying one kind a thousand times must not use up the
    /// lines the first refusal of ANOTHER kind needs.
    #[test]
    fn the_first_refusal_of_each_kind_is_never_throttled() {
        let led = Ledger::for_test(1 << 20);
        let mut b = Books::new(7, led);
        let vid = Ask::of_alloc(Door::Nvos64, 0x40, &params(0, vidmem_attr(), 4096)).unwrap();
        let logged = (0..1000)
            .filter(|_| b.count_refusal(&vid).is_some())
            .count();
        assert_eq!(logged, 8 + 10, "the first eight, then every hundredth");

        let pci = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI);
        let other = Ask::of_alloc(Door::Nvos64, 0x3e, &params(0, pci, 4096)).unwrap();
        assert_eq!(b.count_refusal(&other), Some(1));
        let door = Ask::of_vidheap(&nvos32(
            sys::NVOS32_FUNCTION_ALLOC_SIZE,
            0,
            vidmem_attr(),
            4096,
        ))
        .unwrap();
        assert_eq!(
            b.count_refusal(&door),
            Some(1),
            "the other door is another kind"
        );
    }

    /// Where RM put a LOCATION_ANY is said once per VM per outcome, not
    /// once per allocation.
    #[test]
    fn a_placement_is_named_once_per_kind_and_outcome() {
        let led = Ledger::for_test(1 << 30);
        let b1 = Books::new(1, led.clone());
        let b2 = Books::new(2, led.clone());
        let any = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_ANY);
        let ask = Ask::of_alloc(Door::Nvos64, 0x40, &params(0, any, 4096)).unwrap();

        let line = b1
            .placement(&ask, true, 0, vidmem_attr())
            .expect("the first is named");
        assert!(
            line.contains("RM placed it in VIDMEM") && line.contains("charged"),
            "{line}"
        );
        assert_eq!(
            b2.placement(&ask, true, 0, vidmem_attr()),
            None,
            "per VM, not per process"
        );

        let pci = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI);
        let line = b2
            .placement(&ask, true, 0, pci)
            .expect("a new outcome is named");
        assert!(line.contains("charge given back"), "{line}");
        let line = b2
            .placement(&ask, false, sys::NV_ERR_NO_MEMORY, any)
            .expect("and a failure");
        assert!(line.contains("status 0x51"), "{line}");
    }

    /// The census names who holds the ledger, largest first, and what no
    /// named process holds.
    #[test]
    fn the_census_says_who_holds_the_ledger() {
        let led = Ledger::for_test(2816 * MIB);
        let mut game = Books::new(9, led.clone());
        let mut shell = Books::new(3, led.clone());
        let mut kernel = Books::new(1, led.clone());
        game.announce(4711, "ShadowOfTheTomb");
        shell.announce(1201, "gnome-shell");
        for (b, bytes, h) in [
            (&mut game, 2048 * MIB, 1u32),
            (&mut shell, 256 * MIB, 2),
            (&mut kernel, 64 * MIB, 3),
        ] {
            assert!(b.reserve(bytes));
            b.settle(
                true,
                vidmem_attr(),
                Charge {
                    token: 1,
                    root: 0xc1d8,
                    parent: 0x5c000002,
                    handle: h,
                    bytes,
                },
            );
        }
        let c = led.census();
        assert!(c.starts_with("ledger 2368.0 of 2816.0 MiB:"), "{c}");
        let (g, s) = (
            c.find("9=ShadowOfTheTomb[4711] 2048.0").unwrap(),
            c.find("3=gnome-shell[1201] 256.0").unwrap(),
        );
        assert!(g < s, "largest first: {c}");
        assert!(c.contains("unnamed 64.0"), "{c}");
    }
}
