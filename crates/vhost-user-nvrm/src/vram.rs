// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! VRAM cap per VM.
//!
//! Terms, once: RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN, and its ioctls are called escapes;
//! NVOS64 and NVOS32 are the parameter blocks of its two allocation
//! escapes, RM_ALLOC and RM_VID_HEAP_CONTROL (nvos.h); FB is the card's
//! own memory (the framebuffer); USERD is a channel's user-space doorbell
//! page.
//!
//! One backend serves exactly one VM, so the ledger is a process-wide
//! quantity: every session of this backend charges the same counter, and
//! the cap is reached for the VM as a whole, not per guest process. That
//! is deliberate and it is the only boundary the host can enforce -- a
//! finer split would have to trust labels the guest kernel hands out
//! (docs/FUTURE.md, "the boundary no technique moves").
//!
//! WARNING: this counts only what the guest asks for EXPLICITLY through a
//! memory class. RM's own device memory -- channel instance memory, USERD,
//! context buffers, the share of the GSP (the on-card system processor
//! running half the driver) -- never crosses the boundary as an
//! allocation request and is therefore invisible here. The cap bounds the
//! part a workload can grow without limit, not the card's full occupancy.
//!
//! Measured (probe/suites/test_vram_churn.py, native, LD_PRELOAD tracer):
//! the classes that carry an `NV_MEMORY_ALLOCATION_PARAMS` are the only
//! ones with a guest-chosen size, and among those exactly
//! `attr.LOCATION == VIDMEM` without `ALLOC_FLAGS_VIRTUAL` lands in FB.
//! Evidence and the counter-examples are at [`request_bytes`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nvrm_abi::nvgpu::nvos32_attr;
use nvrm_abi::sys;

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

// The four offsets and the flag above are typed out by hand, from a
// comment. These tie them to the generated struct, so a driver bump that
// moves a field breaks the BUILD rather than charging the ledger from the
// wrong four bytes -- which would be silent, and wrong in the direction
// that lets a guest allocate past its cap.
const _: () = {
    assert!(P_FLAGS == core::mem::offset_of!(sys::NV_MEMORY_ALLOCATION_PARAMS, flags));
    assert!(P_ATTR == core::mem::offset_of!(sys::NV_MEMORY_ALLOCATION_PARAMS, attr));
    assert!(P_SIZE == core::mem::offset_of!(sys::NV_MEMORY_ALLOCATION_PARAMS, size));
    assert!(P_LEN == core::mem::size_of::<sys::NV_MEMORY_ALLOCATION_PARAMS>());
    assert!(ALLOC_FLAGS_VIRTUAL == sys::NVOS32_ALLOC_FLAGS_VIRTUAL);
};

/// The cap, in bytes. `LEA_VRAM_LIMIT_MIB`, default 0 = off.
///
/// Read ONCE. `std::env::var_os` scans `environ` linearly and takes a
/// lock; at ~12 us per forwarded ioctl a per-message read would distort
/// exactly the number this rig measures (docs/TESTING.md §2).
///
/// The name is deliberately unlike both pin limits: `max_pin_mib` (guest
/// module, cumulative over all pins) and `LEA_MAX_PIN_MIB` (host, one
/// arena) already collide enough that only the log line tells them apart.
fn limit_bytes() -> u64 {
    use std::sync::OnceLock;
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let mib = match std::env::var("LEA_VRAM_LIMIT_MIB") {
            Err(_) => 0,
            Ok(s) => match s.trim().parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    // Off, not "0 MiB": a cap of zero would fail every
                    // allocation, and a typo must not look like a policy.
                    eprintln!(
                        "vhost-user-nvrm: LEA_VRAM_LIMIT_MIB={s:?} unusable -- cap stays off"
                    );
                    0
                }
            },
        };
        if mib > 0 {
            eprintln!("vhost-user-nvrm: VRAM cap {mib} MiB for this VM (LEA_VRAM_LIMIT_MIB)");
        }
        mib << 20
    })
}

/// How many bytes of FB this `NV_ESC_RM_ALLOC` would occupy, or `None` if
/// it occupies none.
///
/// Measured under the tracer, `test_vram_churn.py` native (610.43.03,
/// torch 2.13.0+cu130) -- every distinct request in that run:
///
/// | hClass | flags     | attr        | LOCATION | occupies FB |
/// |--------|-----------|-------------|----------|-------------|
/// | 0x40   | 0x1c101   | 0x18000000  | VIDMEM 0 | yes         |
/// | 0x3e   | 0xc001    | 0x3a000000  | PCI 1    | no, sysmem  |
/// | 0x50a0 | 0x8c415   | 0x0 (in)    | -        | no, VIRTUAL |
///
/// So neither the class alone nor the flags alone decide it: 0x50a0
/// (NV50_MEMORY_VIRTUAL) asked for 0xfb000000 bytes = 4.2 GB in that run
/// and occupied nothing, because `ALLOC_FLAGS_VIRTUAL` was set. Reading
/// the class as "0x40 means VRAM" would have been right for this workload
/// and wrong in principle; reading LOCATION is what RM itself acts on.
///
/// `LOCATION_ANY` counts as a candidate on the way IN. RM may resolve it
/// either way, and a request that is not charged before the ioctl is a
/// request that walks past the cap. What it really became is settled
/// afterwards from the written-back attr -- see [`Books::settle`].
pub fn request_bytes(hclass: u32, aux: &[u8]) -> Option<u64> {
    // Only the three classes whose params ARE NV_MEMORY_ALLOCATION_PARAMS
    // (xlate.rs:402). 0x71 has its own, much smaller struct -- decoding it
    // with this layout would read ~88 bytes past the buffer.
    if !matches!(hclass, 0x003e | 0x0040 | 0x50a0) || aux.len() < P_LEN {
        return None;
    }
    let flags = u32::from_le_bytes(aux[P_FLAGS..P_FLAGS + 4].try_into().unwrap());
    if flags & ALLOC_FLAGS_VIRTUAL != 0 {
        return None;
    }
    let attr = u32::from_le_bytes(aux[P_ATTR..P_ATTR + 4].try_into().unwrap());
    if !may_be_vidmem(attr) {
        return None;
    }
    let size = u64::from_le_bytes(aux[P_SIZE..P_SIZE + 8].try_into().unwrap());
    // A zero-size allocation is RM's problem, not the cap's.
    (size > 0).then_some(size)
}

// ===========================================================================
// The other allocation door: NV_ESC_RM_VID_HEAP_CONTROL (0x4a)
// ===========================================================================
// Measured 2026-08-15 (docs/OPEN-QUESTIONS.md nr 12): under a 4096 MiB
// cap the ledger stood at 101 MiB while CS2 grew the card to 4783 MiB, and
// tracked a 512 MiB CUDA tensor exactly. The cap was a COMPUTE cap, and
// the reason is this door -- [`request_bytes`] above is reached only under
// `NV_ESC_RM_ALLOC`, and the graphics stack allocates through NVOS32
// instead: 1120 calls in a CS2 trace, 110 in vulkaninfo's.
//
// The two doors ask the same question in different structs. Everything the
// ledger decides -- VIDMEM or not, virtual or not, reserve then settle on
// what RM wrote back -- is IDENTICAL, and stays identical by calling the
// same helpers. Only the offsets differ.
//
// Offsets from the layout guard in `nvrm-abi/src/nvgpu.rs`
// (`NVOS32_PARAMETERS` 184 bytes: function @8, status @20, data @40; the
// `AllocSize` member 120 bytes: hMemory @4, flags @12, attr @16, size @48).

/// `NVOS32_PARAMETERS::function`.
const V_FUNCTION: usize = 8;
/// `NVOS32_PARAMETERS::status` -- plain `NV_STATUS`, the same space the
/// NVOS64 path uses (`nvos.h:74`, `#define NVOS_STATUS NV_STATUS`), so a
/// refusal here can read exactly like a refusal there.
pub const V_STATUS_OFF: usize = 20;
/// Where the union starts.
const V_DATA: usize = 40;
/// `AllocSize::hMemory` -- IN/OUT, RM generates it unless the guest
/// provided one. Read AFTER the call, which is the only time it is final.
const VA_HMEMORY: usize = V_DATA + 4;
/// `AllocSize::flags`.
const VA_FLAGS: usize = V_DATA + 12;
/// `AllocSize::attr` -- IN/OUT, exactly like NVOS32_ATTR in the other door.
const VA_ATTR: usize = V_DATA + 16;
/// `AllocSize::size` -- IN/OUT: the guest asks with it, RM writes back what
/// it really allocated.
///
/// The ledger charges what was ASKED, exactly as the NVOS64 path does. RM
/// rounds up to its page granularity, so the books under-count by that
/// rounding. Stated rather than corrected: the two doors agreeing matters
/// more than either being exact, and a cap whose two halves drift is worse
/// than one that is uniformly a little generous.
const VA_SIZE: usize = V_DATA + 48;
/// The whole struct. Anything shorter is not it.
const V_LEN: usize = 184;

// The same tie to the generated structs as for the NVOS64 door above.
//
// `data` is a UNION and `AllocSize` is one of its members; a union member
// always begins at offset 0 of the union, so an offset inside the whole
// `NVOS32_PARAMETERS` is `V_DATA` plus the offset inside the member --
// which bindgen emits as its own struct, `NVOS32_PARAMETERS`'s
// `__bindgen_ty_1__bindgen_ty_1`.
const _: () = {
    assert!(V_FUNCTION == core::mem::offset_of!(sys::NVOS32_PARAMETERS, function));
    assert!(V_STATUS_OFF == core::mem::offset_of!(sys::NVOS32_PARAMETERS, status));
    assert!(V_DATA == core::mem::offset_of!(sys::NVOS32_PARAMETERS, data));
    assert!(V_LEN == core::mem::size_of::<sys::NVOS32_PARAMETERS>());

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

/// Bytes of FB this `ALLOC_SIZE` is asking for, or `None` if it is asking
/// for something the cap does not count.
///
/// The three tests are the SAME three as [`request_bytes`], in the same
/// order and for the same reasons: not a virtual reservation, plausibly
/// VIDMEM, non-zero.
pub fn vidheap_request_bytes(buf: &[u8]) -> Option<u64> {
    if vidheap_function(buf)? != sys::NVOS32_FUNCTION_ALLOC_SIZE {
        return None;
    }
    let flags = u32::from_le_bytes(buf[VA_FLAGS..VA_FLAGS + 4].try_into().unwrap());
    if flags & ALLOC_FLAGS_VIRTUAL != 0 {
        return None;
    }
    let attr = u32::from_le_bytes(buf[VA_ATTR..VA_ATTR + 4].try_into().unwrap());
    if !may_be_vidmem(attr) {
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

/// On the way in: VIDMEM, or ANY (which RM may turn into VIDMEM).
fn may_be_vidmem(attr: u32) -> bool {
    let loc = nvos32_attr::LOCATION.get(attr);
    loc == sys::NVOS32_ATTR_LOCATION_VIDMEM || loc == sys::NVOS32_ATTR_LOCATION_ANY
}

/// On the way out: what RM wrote back. Only VIDMEM stays charged.
fn is_vidmem(attr: u32) -> bool {
    nvos32_attr::LOCATION.get(attr) == sys::NVOS32_ATTR_LOCATION_VIDMEM
}

/// One guest process, as the VM's own `nvidia-smi` should see it.
///
/// `guest_pid` is the number the GUEST knows (`pid_vnr`), because that is
/// what `nvidia-smi` resolves in its own `/proc`. The dense `sub_id` is the
/// key, never the display value: the guest kernel reuses PIDs.
#[derive(Clone, Debug)]
pub struct ProcRow {
    pub guest_pid: u32,
    pub bytes: u64,
}

/// The VM's counter. Shared by every session of this backend.
///
/// Two jobs, and they are deliberately not the same one:
///   - `used`/`limit` ENFORCE, and only when a limit is set.
///   - `roster` REPORTS, and always -- the guest's process list needs the
///     numbers whether or not anyone capped the VM.
#[derive(Debug)]
pub struct Ledger {
    limit: u64,
    used: AtomicU64,
    /// sub_id -> what that guest process is and holds. A Mutex, not an
    /// atomic: it is touched on alloc/free and on the two list controls,
    /// never per forwarded ioctl.
    roster: Mutex<BTreeMap<u32, ProcRow>>,
}

impl Ledger {
    /// The backend's one ledger. The limit is read here, once per process.
    pub fn new() -> Arc<Self> {
        Arc::new(Ledger { limit: limit_bytes(), used: AtomicU64::new(0), roster: Mutex::default() })
    }

    /// A ledger that never refuses -- for tests and for the fuzz target,
    /// which must reach the same code without an environment.
    pub fn off() -> Arc<Self> {
        Arc::new(Ledger { limit: 0, used: AtomicU64::new(0), roster: Mutex::default() })
    }

    /// A ledger with a limit set from the test rather than the
    /// environment: a `OnceLock` read once per process cannot be varied
    /// per test case.
    #[cfg(test)]
    pub fn for_test(limit: u64) -> Arc<Self> {
        Arc::new(Ledger { limit, used: AtomicU64::new(0), roster: Mutex::default() })
    }

    /// Is the cap on at all? The whole path hangs off this, and it is a
    /// plain field read: with the cap off the hot path pays one `bool`.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.limit != 0
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Reserve `bytes`, or refuse. CAS rather than fetch_add-then-check:
    /// a temporary overshoot is visible to a concurrent session and would
    /// make it refuse for a reason that no longer exists.
    ///
    /// WARNING: with no limit set this ACCEPTS and still counts. Counting
    /// and enforcing were one thing while the cap was the only consumer;
    /// the guest's process list needs the numbers regardless, and a
    /// counter that only runs when someone caps the VM would report zero
    /// in the default configuration.
    fn charge(&self, bytes: u64) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            // saturating_add, not `+`: `bytes` is a guest word. In release
            // it would wrap and land BELOW the limit -- the one arithmetic
            // slip that turns a cap into an open door (docs/TESTING.md §1).
            let next = cur.saturating_add(bytes);
            if self.limit != 0 && next > self.limit {
                return false;
            }
            match self.used.compare_exchange_weak(
                cur, next, Ordering::Relaxed, Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    /// A guest process announced itself (its first `Open` carried the
    /// identity). Idempotent: a later `Open` of the same process repeats
    /// the same data.
    pub fn register(&self, sub_id: u32, guest_pid: u32) {
        let mut r = self.roster.lock().unwrap();
        r.entry(sub_id).or_insert(ProcRow { guest_pid, bytes: 0 }).guest_pid = guest_pid;
    }

    /// The guest process is gone -- its session fell.
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

    /// Every guest process of this VM that has an identity, in a stable
    /// order (the dense ID ascending, i.e. by age).
    ///
    /// Processes with `guest_pid == 0` are left out: a caller that stated
    /// nothing has no PID the guest could resolve, and an invented one
    /// would be worse than an absent line.
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
            match self.used.compare_exchange_weak(
                cur, next, Ordering::Relaxed, Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => cur = now,
            }
        }
    }
}

/// What one memory object costs and what its release hangs off.
///
/// Also the argument of [`Books::settle`]: the five fields belong
/// together, and passing them one by one was a seven-argument call that
/// nobody could read at the call site.
#[derive(Copy, Clone, Debug)]
pub struct Charge {
    /// The FD the alloc rode on. Closing it frees the client behind it,
    /// and with the client every object under it -- without a single
    /// RM_FREE crossing the boundary.
    pub token: u64,
    /// NVOS64.hRoot -- the client. Freeing it takes the whole subtree.
    pub root: u32,
    /// NVOS64.hObjectParent -- the device. Freeing it takes the memory
    /// objects hanging off it. Measured: every 0x40 in the churn run had
    /// hParent 0x5c000002, the device, and hRoot the client.
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

/// One session's share of the ledger.
///
/// Two books, not one: the ledger is the VM's total, this is what THIS
/// session still owes it. Drop settles the difference -- which is the
/// path that catches everything the guest never announced, from a killed
/// process to a `KIND_PROC_GONE`.
pub struct Books {
    ledger: Arc<Ledger>,
    /// Which guest process these books belong to -- the key into the
    /// ledger's roster, so the reporting side never has to walk `open`.
    sub_id: u32,
    /// (hRoot, hObjectNew) -> charge, packed into one u64.
    ///
    /// The client MUST be part of the key: RM handles are unique within a
    /// client, not within a session, and libcuda creates more than one
    /// client. Keyed on the handle alone, a second client reusing a
    /// handle number would silently release the first client's charge and
    /// the books would drift below the truth.
    open: HashMap<u64, Charge>,
    /// Sum of `open`. Kept alongside so Drop needs no walk and cannot
    /// drift from what was charged.
    owed: u64,
    /// How often this session was refused. Only for the log line.
    refusals: u64,
}

impl Books {
    pub fn new(sub_id: u32, ledger: Arc<Ledger>) -> Self {
        Books { ledger, sub_id, open: HashMap::new(), owed: 0, refusals: 0 }
    }

    /// The guest process stated who it is. Only from here on can it appear
    /// in the VM's process list -- without a guest PID there is nothing
    /// `nvidia-smi` could resolve.
    pub fn announce(&self, guest_pid: u32) {
        self.ledger.register(self.sub_id, guest_pid);
        self.ledger.set_bytes(self.sub_id, self.owed);
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

    /// Count a refusal and say whether it is worth a log line. The first
    /// eight, then every hundredth: libcuda retries after an OOM, and a
    /// guest that simply keeps asking must not be able to fill the host's
    /// disk through the backend log.
    pub fn count_refusal(&mut self) -> Option<u64> {
        self.refusals += 1;
        (self.refusals <= 8 || self.refusals % 100 == 0).then_some(self.refusals)
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

    /// Bytes this session currently owes the ledger.
    pub fn owed(&self) -> u64 {
        self.owed
    }

    /// Reserve before the ioctl. `false` means: refuse the allocation.
    pub fn reserve(&mut self, bytes: u64) -> bool {
        self.ledger.charge(bytes)
    }

    /// After a reserved allocation came back: keep the charge, or give it
    /// back.
    ///
    /// `ok` is the RM verdict, `attr_out` what RM wrote into the params.
    /// A failed alloc occupies nothing, and an allocation RM placed in
    /// sysmem after all (`LOCATION_ANY`) is not this cap's business.
    pub fn settle(&mut self, ok: bool, attr_out: u32, c: Charge) {
        if !ok || !is_vidmem(attr_out) {
            self.ledger.release(c.bytes);
            return;
        }
        // A (client, handle) pair RM just handed out cannot already be
        // live. If it is, the old entry is stale bookkeeping and would be
        // leaked forever -- give it back rather than orphan it.
        if let Some(old) = self.open.insert(key_of(c.root, c.handle), c) {
            self.ledger.release(old.bytes);
            self.owed = self.owed.saturating_sub(old.bytes);
        }
        self.owed = self.owed.saturating_add(c.bytes);
        self.publish();
    }

    /// The guest freed `handle` under client `root`. That releases the
    /// object itself, and everything below it: freeing a device takes its
    /// memory objects, freeing a client takes the lot.
    ///
    /// This is the completeness question, and it is the whole risk of the
    /// feature -- a charge that is never released is a cap that strangles
    /// the VM after an hour, not a cap that protects it.
    ///
    /// Measured: memory objects hang off the device, the device off the
    /// client (churn run: every 0x40 with hRoot 0xc1d83a38, hParent
    /// 0x5c000002). Deeper trees do not occur for memory classes -- if
    /// they ever did, this would under-release, which is the direction a
    /// cap must not fail in silently. It is named here rather than
    /// guarded against, because a guard would be untested code.
    pub fn free_object(&mut self, root: u32, handle: u32) {
        let mut freed = 0u64;
        self.open.retain(|_, c| {
            // Same client only: two clients may well use the same handle
            // number, and freeing one must not release the other's.
            let dies = c.root == root && (c.handle == handle || c.parent == handle)
                || c.root == handle;
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

// ===========================================================================
// The VM's own process list
// ===========================================================================
// `nvidia-smi` builds it from two controls, and both are forwarded today.
// Measured 2026-08-06, guest against host: the guest receives the HOST's
// table verbatim -- eight host PIDs with their per-process FB usage. It
// prints "No running processes found" only because it cannot resolve those
// PIDs in its own /proc. The numbers cross the boundary regardless, which
// makes replacing the table a fix for an information leak and not only a
// cosmetic feature.
//
// Both structures are flat -- no second-level pointer, `paramsSize` is
// self-describing -- so they are rewritten in place on the way back. No
// protocol change, no table entry, PROTO_VERSION untouched (4 when this
// was written, 6 today -- no bump ever came from here).

// THE OFFSETS COME FROM `nvrm_abi::mediate`, and so does the manifest that
// `verify` masks with. That is the point of having moved them: a field this
// code rewrites and the manifest does not describe would be reported as a
// defect on every guest run, and a manifest field this code does not touch
// would quietly widen the mask. Neither can happen while there is one
// definition, and these are it -- every one an `offset_of!` on the bindgen
// struct rather than a number anybody typed.
pub use nvrm_abi::mediate::{
    CMD_GPU_GET_PIDS, CMD_GPU_GET_PID_INFO, PIDINFO_COUNT_OFF, PIDINFO_ENTRY,
    PIDINFO_INDEX_VIDEO_MEMORY_USAGE, PIDINFO_LEN, PIDINFO_LIST_OFF, PIDINFO_MAX,
    PIDINFO_MEM_PRIVATE, PIDS_COUNT_OFF, PIDS_LEN, PIDS_MAX, PIDS_TBL_OFF,
};

/// Replace the PID table with this VM's guest processes.
///
/// Host PIDs disappear from the guest's view. That is the point: they are
/// not resolvable there, they are not the guest's business, and today they
/// leak.
///
/// Returns how many rows were written, or `None` if the buffer is not this
/// structure -- in which case the caller forwards RM's answer untouched
/// rather than inventing one.
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

/// Answer the per-PID query from this VM's own books.
///
/// The guest asks about GUEST PIDs -- RM has never heard of them, so its
/// answer is meaningless here and gets overwritten rather than trusted.
/// A PID that is not one of ours gets zero bytes and NV_OK: it exists as
/// far as the guest is concerned (it just asked about it), it simply holds
/// nothing of ours.
///
/// WARNING: `count` comes from the guest. It is clamped against both the
/// header maximum and the buffer the guest actually sent -- believing it
/// would write past the end of the aux buffer, which is the same class of
/// bug as the three `guest_words.rs` exists for.
pub fn rewrite_get_pid_info(aux: &mut [u8], roster: &[ProcRow]) -> Option<usize> {
    if aux.len() < PIDINFO_LIST_OFF + PIDINFO_ENTRY {
        return None;
    }
    let asked = u32::from_le_bytes(
        aux[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4].try_into().unwrap(),
    ) as usize;
    let fits = (aux.len() - PIDINFO_LIST_OFF) / PIDINFO_ENTRY;
    let n = asked.min(PIDINFO_MAX).min(fits);
    for i in 0..n {
        let base = PIDINFO_LIST_OFF + PIDINFO_ENTRY * i;
        let pid = u32::from_le_bytes(aux[base..base + 4].try_into().unwrap());
        let index = u32::from_le_bytes(aux[base + 4..base + 8].try_into().unwrap());
        let bytes = if index == PIDINFO_INDEX_VIDEO_MEMORY_USAGE {
            roster.iter().find(|r| r.guest_pid == pid).map_or(0, |r| r.bytes)
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

// ... and the header arithmetic itself against the generated structs, so
// the two numbers above cannot both be consistently wrong. Every offset
// here is a write target: these functions REPLACE what RM wrote, and an
// offset that has moved would scatter guest PIDs and byte counts across
// neighbouring fields of a buffer that goes straight back to the guest.
// The offsets themselves are `offset_of!` expressions in `nvrm_abi::mediate`
// now, so asserting them against `offset_of!` here would be asserting a
// thing against itself. What is left is the statement that is NOT implied by
// the struct layout: this loop's own bound.
const _: () = {
    // The zeroing loop writes 6 u64 to clear the whole union; it must stay
    // inside the entry. `data` is the union whose first member is
    // `vidMemUsage`, and `memPrivate` is that member's first field, so the
    // union's own offset inside the entry is where the loop starts.
    assert!(PIDINFO_MEM_PRIVATE + 6 * 8 <= PIDINFO_ENTRY);
};

// ===========================================================================
// The card the guest sees
// ===========================================================================
// Two more controls, the same shape as the process list: flat parameter
// buffers, rewritten on the way back.
//
// Measured 2026-08-06 with a hook in this very path, guest `nvidia-smi`
// plus `torch`: of the twelve FB_GET_INFO_V2 indices that cross the
// boundary, exactly THREE carry a memory size --
//   0x08 TOTAL_RAM_SIZE  0x800000 KB = 8 GiB   (the physical card)
//   0x09 HEAP_SIZE       0x797240 KB = 7773 MiB (what torch reports as
//                                                total_memory)
//   0x16 HEAP_FREE       ~0x69f000 KB, moves between calls
// The rest are ECC status, LTC count and friends and are left alone.
//
// WARNING: it is not one number, it is a coherent set. Capping only
// TOTAL_RAM_SIZE hands the guest "free 6.4 GB of total 1 GB", which is
// worse than not capping it at all. total, heap and free therefore all
// come from one source -- this ledger.

/// `NV2080_CTRL_CMD_FB_GET_INFO_V2` (ctrl2080fb.h:489).
pub use nvrm_abi::mediate::CMD_FB_GET_INFO_V2;
/// `NV2080_CTRL_CMD_FB_GET_INFO` (ctrl2080fb.h:480), the V1 form -- the
/// SAME index list, but the array hangs off an `NvP64` instead of sitting
/// in the params buffer (`xlate::nested_ptrs`, ptr_off 8).
///
/// This one is not an afterthought, it is the one the GRAPHICS stack
/// asks. Measured 2026-08-15 under a 4096 MiB cap: `nvidia-smi` in the
/// guest said 4096 MiB (it asks V2, which was already capped) while
/// `vulkaninfo` reported `memoryHeaps[0].size = 8.00 GiB` -- the whole
/// card. vulkaninfo's own trace names the caller: four calls to
/// 0x20801301, none to 0x20801303. A client sizes its texture budget from
/// that heap, so an uncapped answer here is not a cosmetic leak: it is the
/// VM being invited to overcommit the card.
pub use nvrm_abi::mediate::CMD_FB_GET_INFO;
/// `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` (ctrl2080gpu.h:325).
pub use nvrm_abi::mediate::CMD_GPU_GET_NAME_STRING;

/// `NV2080_CTRL_FB_GET_INFO_V2_PARAMS`: `fbInfoListSize` @0, then
/// `NV2080_CTRL_FB_INFO { u32 index; u32 data; }` -- 1028 bytes for the
/// 128-entry maximum.
pub use nvrm_abi::mediate::{FBINFO_COUNT_OFF, FBINFO_ENTRY, FBINFO_LIST_OFF, FBINFO_MAX};

/// The size indices, all in KILOBYTES (ctrl2080fb.h:76-112, :254-260).
/// Measured: the guest asks for 0x08, 0x09 and 0x16. The other two are
/// rewritten as well because a card that answers one of them honestly and
/// the others capped is a card that contradicts itself.
const FB_INFO_INDEX_RAM_SIZE: u32 = 0x07;
const FB_INFO_INDEX_TOTAL_RAM_SIZE: u32 = 0x08;
const FB_INFO_INDEX_HEAP_SIZE: u32 = 0x09;
const FB_INFO_INDEX_HEAP_FREE: u32 = 0x16;
const FB_INFO_INDEX_USABLE_RAM_SIZE: u32 = 0x20;

/// Cap the memory sizes the guest is told, and keep them consistent with
/// each other.
///
/// `used` is what this VM holds by our own books. It is smaller than the
/// true footprint (RM's own device memory behind a channel never crosses
/// the boundary -- measured ~106 MiB per CUDA context), so `free` is
/// correspondingly generous. That is stated rather than papered over with
/// an invented surcharge: a number that is wrong by a known amount beats
/// one that is wrong by a guessed one.
///
/// Returns how many entries were rewritten, or `None` if the buffer is not
/// this structure.
pub fn rewrite_fb_info(aux: &mut [u8], limit: u64, used: u64) -> Option<usize> {
    if limit == 0 || aux.len() < FBINFO_LIST_OFF + FBINFO_ENTRY {
        return None;
    }
    let asked = u32::from_le_bytes(
        aux[FBINFO_COUNT_OFF..FBINFO_COUNT_OFF + 4].try_into().unwrap(),
    ) as usize;
    cap_fb_entries(&mut aux[FBINFO_LIST_OFF..], asked, limit, used)
}

/// The same rewrite for the V1 form, where the array is a SEPARATE buffer.
///
/// `list` is the nested block on its own (the backend has already brought
/// it across and pointed the params buffer at it); `asked` is
/// `fbInfoListSize` out of that params buffer. Everything after that is the
/// V2 path verbatim -- deliberately, because two index tables that could
/// drift apart is the bug this function exists to prevent.
pub fn rewrite_fb_info_list(
    list: &mut [u8], asked: usize, limit: u64, used: u64,
) -> Option<usize> {
    if limit == 0 || list.len() < FBINFO_ENTRY {
        return None;
    }
    cap_fb_entries(list, asked, limit, used)
}

/// Cap one `NV2080_CTRL_FB_INFO[]`, wherever it happens to live.
///
/// ONE table of indices, ONE arithmetic, two callers (V1 and V2). The
/// warning above -- that the numbers are a coherent set, not five
/// independent ones -- only holds as long as this stays a single function.
fn cap_fb_entries(list: &mut [u8], asked: usize, limit: u64, used: u64) -> Option<usize> {
    let fits = list.len() / FBINFO_ENTRY;
    let n = asked.min(FBINFO_MAX).min(fits);

    // KB, and saturating into u32: `data` is 32 bit, so a limit past 4 TiB
    // would wrap. Clamping is the honest failure -- a wrapped size would
    // read as a tiny card.
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

/// `NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS`: `gpuNameStringFlags` @0,
/// `ascii[64]` @4 (ctrl2080gpu.h:338, `NV2080_GPU_MAX_NAME_STRING_LENGTH` = 64).
pub use nvrm_abi::mediate::{NAME_MAX, NAME_OFF};

/// The name the guest's `nvidia-smi` prints for the card.
///
/// Modelled on what NVIDIA's own vGPU does: an `A100` becomes a
/// `GRID A100-10C`, where the vendor prefix gives way to the mediation
/// layer and the profile size joins the name. Here:
///
/// ```text
///   NVIDIA GeForce RTX 2070   ->  Leandro RTX 2070        (no cap)
///   NVIDIA GeForce RTX 2070   ->  Leandro RTX 2070-1G     (cap 1024 MiB)
///   NVIDIA GeForce RTX 2070   ->  Leandro RTX 2070-1536M  (cap 1536 MiB)
/// ```
///
/// `Leandro` is the project name spelled out -- the DISPLAY name only. The
/// env prefix stays `LEA_*` (docs/NAMING.md rule 1); renaming that would
/// break every script, every doc line and every past measurement. This is
/// the only place the umbrella name is allowed to surface, and the point is
/// that nobody can be in a mediated VM and not notice: the card says so.
///
/// The marketing prefixes go because they are the vendor's, and the string
/// is 64 bytes including the NUL -- a suffix that did not fit would be
/// truncated into a lie about the profile size, so it is dropped whole
/// instead. Spelling the prefix out cost four of those 64 bytes, so the
/// budget is: 8 for `"Leandro "`, 55 for the base plus suffix, 1 for the NUL.
pub fn guest_card_name(real: &str, limit: u64) -> String {
    let base = real
        .trim()
        .strip_prefix("NVIDIA ")
        .unwrap_or(real.trim())
        .strip_prefix("GeForce ")
        .unwrap_or_else(|| real.trim().strip_prefix("NVIDIA ").unwrap_or(real.trim()));

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
    if full.len() < NAME_MAX {
        return full;
    }
    let short = format!("Leandro {base}");
    if short.len() < NAME_MAX {
        return short;
    }
    // Nothing sensible fits. Say the one thing that matters and stop.
    "Leandro GPU".to_string()
}

/// Write the name into the answer, NUL-terminated and NUL-padded.
///
/// Returns `None` if the buffer is not this structure -- the caller then
/// forwards RM's own name rather than inventing one.
pub fn rewrite_gpu_name(aux: &mut [u8], limit: u64) -> Option<String> {
    if aux.len() < NAME_OFF + NAME_MAX {
        return None;
    }
    let raw = &aux[NAME_OFF..NAME_OFF + NAME_MAX];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(NAME_MAX);
    let real = String::from_utf8_lossy(&raw[..end]).to_string();

    let name = guest_card_name(&real, limit);
    let b = name.as_bytes();
    let n = b.len().min(NAME_MAX - 1);
    aux[NAME_OFF..NAME_OFF + n].copy_from_slice(&b[..n]);
    for byte in aux[NAME_OFF + n..NAME_OFF + NAME_MAX].iter_mut() {
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
        assert_eq!(request_bytes(0x3e, &params(0xc001, 0x3a000000, 0x1000)), None);
        // 4.2 GB of VIRTUAL address space (hClass 0x50a0) -- occupies nothing.
        assert_eq!(request_bytes(0x50a0, &params(0x8c415, 0x16000000, 0xfb00_0000)), None);
        // 0x71 carries a 40-byte struct; it must never be decoded here.
        assert_eq!(request_bytes(0x71, &params(0, 0, 0x2000_0000)), None);
        // Short aux is not this struct.
        assert_eq!(request_bytes(0x40, &params(0x1c101, 0x18000000, 1)[..64]), None);
    }

    #[test]
    fn location_any_is_charged_on_the_way_in_and_settled_on_the_way_out() {
        let any = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_ANY);
        assert_eq!(request_bytes(0x40, &params(0, any, 4096)), Some(4096));

        let led = Arc::new(Ledger { limit: 1 << 20, used: AtomicU64::new(0), roster: Mutex::default() });
        let mut b = Books::new(7, led.clone());
        assert!(b.reserve(4096));
        assert_eq!(led.used(), 4096);
        // RM resolved it to sysmem -> the charge goes back.
        let pci = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI);
        b.settle(true, pci, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0x5c0000ab, bytes: 4096 });
        assert_eq!(led.used(), 0);
    }

    #[test]
    fn a_refused_alloc_gives_its_reservation_back() {
        let led = Arc::new(Ledger { limit: 1 << 20, used: AtomicU64::new(0), roster: Mutex::default() });
        let mut b = Books::new(7, led.clone());
        assert!(b.reserve(4096));
        b.settle(false, 0, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0x5c0000ab, bytes: 4096 });
        assert_eq!(led.used(), 0);
        assert_eq!(b.owed(), 0);
    }

    #[test]
    fn the_cap_refuses_and_lets_go_again() {
        let led = Arc::new(Ledger { limit: 8192, used: AtomicU64::new(0), roster: Mutex::default() });
        let mut b = Books::new(7, led.clone());
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);

        assert!(b.reserve(4096));
        b.settle(true, vid, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0xaa, bytes: 4096 });
        assert!(b.reserve(4096));
        b.settle(true, vid, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0xbb, bytes: 4096 });
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
            let led = Arc::new(Ledger { limit: 1 << 30, used: AtomicU64::new(0), roster: Mutex::default() });
            let mut b = Books::new(7, led.clone());
            for h in [0xaau32, 0xbb, 0xcc] {
                assert!(b.reserve(4096));
                b.settle(true, vid, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: h, bytes: 4096 });
            }
            assert_eq!(led.used(), 12288);
            b.free_object(0xc1d8, victim);
            assert_eq!(led.used(), 0, "freeing {victim:#x} must take the memory objects");
        }
    }

    /// RM handles are unique within a client, not within a session, and
    /// libcuda makes several clients. Keyed on the handle alone, the
    /// second client's allocation would release the first's charge and
    /// the books would drift below the truth -- a cap that quietly grows.
    #[test]
    fn two_clients_may_use_the_same_handle_number() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Arc::new(Ledger { limit: 1 << 30, used: AtomicU64::new(0), roster: Mutex::default() });
        let mut b = Books::new(7, led.clone());

        assert!(b.reserve(4096));
        b.settle(true, vid, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0xaa, bytes: 4096 });
        assert!(b.reserve(4096));
        b.settle(true, vid, Charge { token: 1, root: 0xdddd, parent: 0x5c000002, handle: 0xaa, bytes: 4096 });
        assert_eq!(led.used(), 8192, "two clients, two charges");

        b.free_object(0xc1d8, 0xaa);
        assert_eq!(led.used(), 4096, "only the first client's object went");
        b.free_object(0xdddd, 0xaa);
        assert_eq!(led.used(), 0);
    }

    #[test]
    fn closing_the_fd_takes_what_rode_on_it() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Arc::new(Ledger { limit: 1 << 30, used: AtomicU64::new(0), roster: Mutex::default() });
        let mut b = Books::new(7, led.clone());
        b.reserve(4096);
        b.settle(true, vid, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0xaa, bytes: 4096 });
        b.reserve(4096);
        b.settle(true, vid, Charge { token: 2, root: 0xdddd, parent: 0x5c000002, handle: 0xbb, bytes: 4096 });

        b.close_token(1);
        assert_eq!(led.used(), 4096, "only the charge on token 1 goes");
        b.close_token(2);
        assert_eq!(led.used(), 0);
    }

    #[test]
    fn a_dying_session_pays_its_debt() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Arc::new(Ledger { limit: 1 << 30, used: AtomicU64::new(0), roster: Mutex::default() });
        {
            let mut b = Books::new(7, led.clone());
            for h in 0..10u32 {
                assert!(b.reserve(1 << 20));
                b.settle(true, vid, Charge { token: 1, root: 0xc1d8, parent: 0x5c000002, handle: h, bytes: 1 << 20 });
            }
            assert_eq!(led.used(), 10 << 20);
        }
        assert_eq!(led.used(), 0, "Drop settles what no message ever announced");
    }

    #[test]
    fn a_guest_word_cannot_wrap_the_counter() {
        let led = Arc::new(Ledger { limit: 1 << 20, used: AtomicU64::new(0), roster: Mutex::default() });
        let mut b = Books::new(7, led.clone());
        assert!(b.reserve(4096));
        // `size` is a guest word. 4096 + (u64::MAX - 4095) wraps to exactly
        // 0, which is under any limit: with `+` this request would be
        // waved through in release (and panic in debug -- two different
        // programs, docs/TESTING.md §1). It must be refused in both.
        assert!(!b.reserve(u64::MAX - 4095));
        assert!(!b.reserve(u64::MAX));
        assert_eq!(led.used(), 4096);
    }

    /// With no limit set, the books still COUNT -- they only stop
    /// REFUSING. The VM's process list needs the numbers in exactly the
    /// default configuration, where nobody capped anything; a counter that
    /// ran only under a cap would report zero to every guest that never
    /// set one.
    #[test]
    fn with_no_limit_the_books_count_but_never_refuse() {
        let vid = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_VIDMEM);
        let led = Ledger::off();
        assert!(!led.enabled(), "no limit is set");
        let mut b = Books::new(7, led.clone());

        assert!(b.reserve(8 << 30), "without a limit nothing is refused");
        b.settle(true, vid, Charge {
            token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0xaa, bytes: 8 << 30,
        });
        assert_eq!(led.used(), 8 << 30, "and it is still counted");
        assert_eq!(b.owed(), 8 << 30);

        // Even u64::MAX must not wrap the counter into acceptance-by-
        // accident; saturating arithmetic holds with and without a limit.
        assert!(b.reserve(u64::MAX));
        assert_eq!(led.used(), u64::MAX);
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

            b.announce(4711);
            assert_eq!(led.roster().len(), 1);
            assert_eq!(led.roster()[0].guest_pid, 4711);
            assert_eq!(led.roster()[0].bytes, 0);

            b.reserve(4 << 20);
            b.settle(true, vid, Charge {
                token: 1, root: 0xc1d8, parent: 0x5c000002, handle: 0xaa, bytes: 4 << 20,
            });
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
            ProcRow { guest_pid: 4674, bytes: 380 << 20 },
            ProcRow { guest_pid: 4676, bytes: 648 << 20 },
        ];
        assert_eq!(rewrite_get_pids(&mut v, &roster), Some(2));
        assert_eq!(
            u32::from_le_bytes(v[PIDS_COUNT_OFF..PIDS_COUNT_OFF + 4].try_into().unwrap()),
            2
        );
        assert_eq!(pid_at(&v, 0), 4674);
        assert_eq!(pid_at(&v, 1), 4676);
        assert_eq!(pid_at(&v, 2), 0, "the third host PID was overwritten, not left behind");
        for i in 2..PIDS_MAX {
            assert_eq!(pid_at(&v, i), 0, "no host PID survives anywhere in the table");
        }
    }

    /// A buffer that is not the documented struct must not be treated as
    /// one -- and the caller turns `None` into a refusal rather than
    /// forwarding RM's host table.
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
            ProcRow { guest_pid: 4674, bytes: 380 << 20 },
            ProcRow { guest_pid: 4676, bytes: 648 << 20 },
        ];
        assert_eq!(rewrite_get_pid_info(&mut v, &roster), Some(3));
        assert_eq!(priv_at(&v, 0), 380 << 20);
        assert_eq!(priv_at(&v, 1), 648 << 20);
        assert_eq!(priv_at(&v, 2), 0, "a PID that is not ours holds nothing of ours");
        for i in 0..3 {
            let o = PIDINFO_LIST_OFF + PIDINFO_ENTRY * i + 8;
            assert_eq!(u32::from_le_bytes(v[o..o + 4].try_into().unwrap()), sys::NV_OK);
        }
    }

    /// `count` is a guest word. Believing it against a short buffer writes
    /// past the end -- the same class of bug `guest_words.rs` exists for.
    #[test]
    fn a_lying_count_cannot_write_past_the_buffer() {
        // Room for two entries, but the guest claims the header maximum.
        let len = PIDINFO_LIST_OFF + 2 * PIDINFO_ENTRY;
        let mut v = vec![0u8; len];
        v[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4]
            .copy_from_slice(&(PIDINFO_MAX as u32).to_le_bytes());
        assert_eq!(rewrite_get_pid_info(&mut v, &[]), Some(2), "clamped to what fits");
        assert_eq!(
            u32::from_le_bytes(v[PIDINFO_COUNT_OFF..PIDINFO_COUNT_OFF + 4].try_into().unwrap()),
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

    /// The measured request: HEAP_FREE, TOTAL_RAM_SIZE, HEAP_SIZE, with the
    /// values a real 8 GiB card returns. Under a 2 GiB cap all three have
    /// to agree with each other -- a capped total beside an honest free is
    /// worse than no cap at all.
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
        assert_eq!(fb_at(&v, 0), ((limit - used) / 1024) as u32, "free = limit - used");
        assert_eq!(fb_at(&v, 1), (limit / 1024) as u32);
        assert_eq!(fb_at(&v, 2), (limit / 1024) as u32);
        assert!(fb_at(&v, 0) < fb_at(&v, 1), "free below total, in every case");
    }

    /// Indices that are not sizes are RM's business and stay untouched.
    #[test]
    fn non_size_indices_are_left_alone() {
        let mut v = fb_buf(&[(0x1a, 0xf), (FB_INFO_INDEX_HEAP_SIZE, 0x797240), (0x23, 0x20)]);
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
        assert_eq!(rewrite_fb_info_list(&mut v1, entries.len(), limit, used), Some(3));

        for i in 0..entries.len() {
            assert_eq!(list_at(&v1, i), fb_at(&v2, i),
                "entry {i}: the two doors disagree about the same card");
        }
        assert_eq!(list_at(&v1, 1), (limit / 1024) as u32, "8 GiB card capped to 2 GiB");
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

        assert_eq!(request_bytes(0x003e, &nvos64), Some(size));
        assert_eq!(vidheap_request_bytes(&nvos32), Some(size), "the other door must agree");
    }

    /// The three refusals are the same three, in the same order.
    #[test]
    fn the_vid_heap_door_ignores_what_the_alloc_door_ignores() {
        let size = 64u64 << 20;
        assert_eq!(
            vidheap_request_bytes(&nvos32(
                sys::NVOS32_FUNCTION_ALLOC_SIZE, ALLOC_FLAGS_VIRTUAL, vidmem_attr(), size)),
            None, "a virtual reservation holds no memory"
        );
        let sysmem = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI);
        assert_eq!(
            vidheap_request_bytes(&nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, 0, sysmem, size)),
            None, "sysmem is not this cap's business"
        );
        assert_eq!(
            vidheap_request_bytes(&nvos32(sys::NVOS32_FUNCTION_ALLOC_SIZE, 0, vidmem_attr(), 0)),
            None, "a zero-size allocation is RM's problem"
        );
    }

    /// Every other NVOS32 function shares the struct and must not be read
    /// as an allocation -- FREE above all, whose union member holds a
    /// handle and flags exactly where AllocSize holds a size.
    #[test]
    fn only_alloc_size_is_an_allocation() {
        for f in [sys::NVOS32_FUNCTION_FREE, sys::NVOS32_FUNCTION_INFO,
                  sys::NVOS32_FUNCTION_ALLOC_SIZE_RANGE, sys::NVOS32_FUNCTION_HW_FREE] {
            let mut v = nvos32(f, 0, vidmem_attr(), 4 << 20);
            // Whatever those bytes mean for THIS function, they are not a
            // size, and nothing may be charged for them.
            v[VA_SIZE..VA_SIZE + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            assert_eq!(vidheap_request_bytes(&v), None, "function {f} is not an allocation");
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
        }
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
        assert_eq!(guest_card_name(real, 0), "Leandro RTX 2070");
        assert_eq!(guest_card_name(real, 2048 << 20), "Leandro RTX 2070-2G");
        assert_eq!(guest_card_name(real, 1024 << 20), "Leandro RTX 2070-1G");
        assert_eq!(guest_card_name(real, 1536 << 20), "Leandro RTX 2070-1536M");
        assert_eq!(
            guest_card_name("NVIDIA A100-SXM4-40GB", 10240 << 20),
            "Leandro A100-SXM4-40GB-10G"
        );
    }

    /// 64 bytes including the NUL, of which `"Leandro "` spends 8. Both
    /// fallback rungs are pinned BY NUMBER: the previous version of this
    /// assertion was `len() < 64` on a name that came out 57 long, so it
    /// passed without ever reaching either rung. Spelling the prefix out
    /// made the budget four bytes tighter, which is exactly the kind of
    /// change a test that never bites will not catch.
    #[test]
    fn a_name_that_does_not_fit_loses_the_suffix_whole() {
        // 8 + 53 + 3 = 64 -> does not fit, so the suffix goes as a unit
        // rather than being truncated into a wrong profile size.
        let b53 = "X".repeat(53);
        assert_eq!(
            guest_card_name(&format!("NVIDIA GeForce {b53}"), 2048 << 20),
            format!("Leandro {b53}")
        );
        // 8 + 56 = 64 -> even the bare name does not fit. Say the one thing
        // that matters and stop.
        let b56 = "X".repeat(56);
        assert_eq!(guest_card_name(&format!("NVIDIA GeForce {b56}"), 2048 << 20), "Leandro GPU");
        // and the longest name that DOES fit still fits, to the last byte
        let b55 = "X".repeat(55);
        assert_eq!(guest_card_name(&format!("NVIDIA GeForce {b55}"), 0).len(), 63);
    }

    #[test]
    fn the_name_is_written_nul_terminated() {
        let mut v = vec![0xffu8; NAME_OFF + NAME_MAX];
        let real = b"NVIDIA GeForce RTX 2070";
        v[NAME_OFF..NAME_OFF + real.len()].copy_from_slice(real);
        v[NAME_OFF + real.len()] = 0;
        assert_eq!(rewrite_gpu_name(&mut v, 2048 << 20).as_deref(), Some("Leandro RTX 2070-2G"));
        let end = v[NAME_OFF..].iter().position(|&c| c == 0).unwrap();
        assert_eq!(&v[NAME_OFF..NAME_OFF + end], b"Leandro RTX 2070-2G");
        assert!(v[NAME_OFF + end..].iter().all(|&c| c == 0), "the tail is padded, not left over");
    }

    /// A caller that never stated who it is has no guest PID -- and an
    /// invented one would be worse than an absent line, because
    /// `nvidia-smi` resolves it in the guest's /proc.
    #[test]
    fn a_process_without_an_identity_is_not_listed() {
        let led = Ledger::off();
        let b = Books::new(7, led.clone());
        b.announce(0);
        assert!(led.roster().is_empty());
    }
}
