// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The two answers that make the guest's card a vGPU rather than a smaller
//! RTX 2070 (docs/OPEN-QUESTIONS.md number 69).
//!
//! `vram.rs` mediates SIZES. These two mediate what the card IS: whether
//! it is virtualised at all, and what share of the encoder it carries.
//! Both are flat parameter buffers rewritten on the way back, the same
//! shape as the FB sizes and the process list, and both are in
//! `nvrm_abi::mediate`'s manifest so that a guest run compared against a
//! native one masks them instead of reporting them as defects.
//!
//! THE ENCODER SHARE FOLLOWS THE GUEST FRAMEBUFFER under every cap, because
//! a share is a size and two VMs told the same size must be told the same
//! share. THE UUID is every VM's own, capped or not. The MODE is the
//! vGPU-shaped policy's alone: saying "vGPU" is a claim about the guest
//! driver, not about a size (see [`mediate_mode`]).

use nvrm_abi::mediate;

pub use nvrm_abi::mediate::{
    CMD_GPU_GET_ENCODER_CAPACITY, CMD_GPU_GET_GID_INFO, CMD_GPU_GET_VIRTUALIZATION_MODE,
};

/// Answer `NV0080_CTRL_CMD_GPU_GET_VIRTUALIZATION_MODE` the way a vGPU
/// guest's RM answers it.
///
/// WHAT THIS CHANGES, and it is worth being precise, because the field is
/// read by everything. Measured (matrix/catalog-610.57.04.json): 34 calls
/// from 20 library classes -- every CUDA probe, all four EGL platforms, GL,
/// GLES, NVDEC, NVENC, NVML, OpenCL and all three Vulkan probes -- and the
/// guest gets the host's `NONE` today. `VGX` (2) is what a vGPU GUEST
/// reports; `HOST` (3) is what the machine running the plugin reports, so
/// `VGX` is the only honest value on this side of the boundary.
///
/// `isGridBuild` is set with it. The two are one statement: a mode that
/// says VGX beside a boolean that says "not a GRID build" is a card
/// contradicting itself, which is the same rule the FB sizes follow.
///
/// WHAT IT DOES NOT CHANGE: nothing inside the guest's kernel driver.
/// `IS_VIRTUAL(pGpu)` there is set by how the driver came up, not by this
/// answer, and no RPC channel to a vGPU plugin exists or is claimed. This
/// rewrites what a client is TOLD, which is exactly as far as this
/// boundary reaches.
///
/// Returns the mode written, or `None` if the buffer is not this structure.
pub fn rewrite_virtualization_mode(aux: &mut [u8]) -> Option<u32> {
    if aux.len() < mediate::VIRTMODE_LEN {
        return None;
    }
    let o = mediate::VIRTMODE_OFF;
    aux[o..o + 4].copy_from_slice(&mediate::VIRTUALIZATION_MODE_VGX.to_le_bytes());
    // NvBool is one byte; NV_TRUE is 1.
    aux[mediate::VIRTMODE_GRIDBUILD_OFF] = 1;
    Some(mediate::VIRTUALIZATION_MODE_VGX)
}

/// Answer `NV2080_CTRL_CMD_GPU_GET_ENCODER_CAPACITY` with the profile's
/// share instead of the whole card's.
///
/// A bare-metal card answers `NV_ENC_CAPACITY_MAX_VALUE` = 100
/// (subdevice_ctrl_gpu_kernel.c:1000); a vGPU guest gets its type's
/// `encoderCapacity` over RPC from the host (rpc.c:10349). The percentage
/// here comes from the catalogue, which divides it the way it divides the
/// framebuffer.
///
/// `queryType` @0 -- H264, HEVC or AV1 -- is the QUESTION and is left
/// alone: the same share applies to whichever codec was asked about, and
/// rewriting the question would hide a client asking for a codec this card
/// does not have.
///
/// WARNING, and it is a measurement nobody has taken yet: whether the
/// guest's NVENC actually LIMITS itself to what it is told here is not
/// established. vGPU enforces the share in the host plugin, and this
/// boundary has no such enforcement -- it reports. Until a run shows
/// otherwise, this is a number the guest is told, not a limit it is held
/// to.
pub fn rewrite_encoder_capacity(aux: &mut [u8], percent: u32) -> Option<u32> {
    if aux.len() < mediate::ENCCAP_LEN || percent == 0 || percent > 100 {
        return None;
    }
    let o = mediate::ENCCAP_OFF;
    aux[o..o + 4].copy_from_slice(&percent.to_le_bytes());
    Some(percent)
}


// ===========================================================================
// The VM's own identity
// ===========================================================================
// MEASURED 2026-08-21, four guests of one fleet, all four asked at once:
//
//   vm0: GPU-41f54c36-8418-25f3-8ab0-801d98eddb4d, Leandro RTX 2070, 8192 MiB
//   vm1: GPU-41f54c36-8418-25f3-8ab0-801d98eddb4d, ...
//   vm2: GPU-41f54c36-8418-25f3-8ab0-801d98eddb4d, ...
//   vm3: GPU-41f54c36-8418-25f3-8ab0-801d98eddb4d, ...
//
// One UUID for four machines, and it is the HOST card's. `nvidia-smi
// --query-gpu=uuid` is what a scheduler keys a GPU on -- Kubernetes'
// device plugin, Slurm's generic resources, `docker --gpus` -- so today
// four of these VMs are one GPU as far as any of them can tell. It is the
// same class of leak as the host PID table (vram.rs) and the host BDF
// (the guest module's bdf_mediation), one namespace further out.
//
// A vGPU guest does not have this problem: its UUID is the mdev device's,
// not the board's, and two vGPUs on one card differ. Here every VM gets one
// of its own, whatever its policy -- and the host's goes back in wherever
// the guest hands its own to the driver (`Card`).

/// WHICH of these answers this backend gives, `LEA_VGPU_MEDIATE`.
///
/// A comma list of `mode`, `uuid` and `enc`; the default is
/// `uuid,enc` and NOT `mode`, and that default is a measurement rather
/// than a preference -- see [`mediate_mode`].
///
/// Read once: these are checked per answered control, and `std::env::var`
/// takes a lock and scans `environ`.
fn switches() -> &'static (bool, bool, bool) {
    static SW: std::sync::OnceLock<(bool, bool, bool)> = std::sync::OnceLock::new();
    SW.get_or_init(|| {
        let raw = std::env::var("LEA_VGPU_MEDIATE").unwrap_or_default();
        let raw = raw.trim();
        if raw.is_empty() {
            // The mode is off because it is MEASURED to cost CUDA; see
            // `mediate_mode`.
            return (false, true, true);
        }
        if raw.eq_ignore_ascii_case("none") {
            return (false, false, false);
        }
        let has = |w: &str| raw.split(',').any(|p| p.trim().eq_ignore_ascii_case(w));
        let all = has("all");
        (all || has("mode"), all || has("uuid"), all || has("enc"))
    })
}

/// Is the VIRTUALIZATION MODE answer on? **Off by default, and this is the
/// most expensive thing measured on this branch.**
///
/// Measured 2026-08-21, four guests under `RTX2070-2Q` with the mode
/// answered as `VGX` and `isGridBuild` set: `nvidia-smi` was entirely
/// happy -- it printed `Leandro RTX2070-2Q`, 1280 MiB, `Virtualization
/// Mode: VGPU` -- and **libcuda would not start at all**:
/// `vrampress: cuInit 100`, which is `CUDA_ERROR_NO_DEVICE`. Every guest, every row of the
/// benchmark that had it on. The card is visible, named, sized, and has no
/// CUDA device on it.
///
/// The reading, and it is the lesson of this whole branch: a vGPU guest's
/// userspace reaches the GPU through a path that exists BECAUSE the guest
/// driver is a vGPU guest driver -- an RPC channel to a plugin in the
/// host. Telling an ordinary driver's userspace that it is on a vGPU makes
/// it look for that path, and there is none here. **Saying it is a vGPU
/// and being one are different things, and libcuda knows the difference
/// even though nvidia-smi does not.**
///
/// So it is opt-in, for a rig that wants to see it, and off wherever CUDA
/// matters.
pub fn mediate_mode() -> bool {
    switches().0
}

/// Is this VM's own UUID answered, and the card's put back where the guest
/// hands it to the driver? On by default.
///
/// Measured 2026-08-21 with the answer alone and nothing put back:
/// `nvidia-smi` printed the VM's UUID and libcuda stopped at `cuInit 3`
/// (NOT_INITIALIZED). The trace of that same libcuda
/// (docs/measurements/vram-69b/libcuda/mode-off.jsonl:148-150, 307-313)
/// shows why: it reads the UUID through GID_INFO and hands exactly those 16
/// bytes to `UVM_REGISTER_GPU` and `UVM_PAGEABLE_MEM_ACCESS_ON_GPU`, and the
/// host's UVM knows no GPU by the VM's UUID. [`Card::to_host`] is that half.
/// Until a guest has run CUDA with both halves, `LEA_VGPU_MEDIATE=enc`
/// turns this off.
pub fn mediate_uuid() -> bool {
    switches().1
}

/// Is the encoder-capacity answer on? On by default: it is a number a
/// client reads and nothing acts on inside the guest.
pub fn mediate_enc() -> bool {
    switches().2
}

/// The VM's name inside a socket path: the directory AND the socket's
/// stem, so both layouts that start backends name a VM uniquely --
/// `vm/<name>/nvrm.sock` (rig.sh) and `<run_dir>/nvrm/<device-id>.sock`
/// (a manager that keeps every socket in one directory, where the directory
/// alone gave every VM the same UUID). Stable across a backend restart,
/// because the path is.
pub fn name_from_socket(socket: &str) -> String {
    let p = std::path::Path::new(socket);
    let part = |o: Option<&std::ffi::OsStr>| o.map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    format!("{}/{}", part(p.parent().and_then(|d| d.file_name())), part(p.file_stem()))
}

/// The card this backend serves, asked once at start-up
/// ([`crate::host_pool::card`]): its UUID and this VM's, in both spellings
/// RM uses, and `TOTAL_RAM_SIZE` in bytes, which is what an encoder share
/// is a share of.
#[derive(Debug)]
pub struct Card {
    pub host: [u8; 16],
    pub guest: [u8; 16],
    pub total: u64,
    ascii: (String, String),
}

impl Card {
    pub fn new(socket: &str, host: [u8; 16], total: u64) -> Card {
        let guest = vm_uuid(&name_from_socket(socket));
        Card { host, guest, total, ascii: (format_uuid(&host), format_uuid(&guest)) }
    }

    /// The card's UUID where the VM's stands: before the driver sees a
    /// call that names the GPU by UUID.
    pub fn to_host(&self, buf: &mut [u8]) {
        swap(buf, &self.guest, &self.host);
        swap(buf, self.ascii.1.as_bytes(), self.ascii.0.as_bytes());
    }

    /// The VM's UUID where the card's stands: in every answer that could
    /// carry it. The lengths are equal, so `length` fields stay true.
    pub fn to_guest(&self, buf: &mut [u8]) {
        swap(buf, &self.host, &self.guest);
        swap(buf, self.ascii.0.as_bytes(), self.ascii.1.as_bytes());
    }
}

/// Every occurrence of `from` at a 4-byte boundary -- where every UUID field
/// of RM's and UVM's parameter blocks sits -- becomes `to`. Found by value
/// rather than by offset, because the UUID travels in a dozen UVM blocks
/// and three controls; 128 bits of it do not occur by accident.
fn swap(buf: &mut [u8], from: &[u8], to: &[u8]) {
    let mut o = 0;
    while o + from.len() <= buf.len() {
        if buf[o..o + from.len()] == *from {
            buf[o..o + from.len()].copy_from_slice(to);
        }
        o += 4;
    }
}

/// The controls whose params carry the card's UUID: GID_INFO and
/// GET_UUID_FROM_GPU_ID answer it, GET_UUID_INFO is asked with it.
pub const UUID_CONTROLS: [u32; 3] = [
    CMD_GPU_GET_GID_INFO,
    nvrm_abi::sys::NV0000_CTRL_CMD_GPU_GET_UUID_INFO,
    nvrm_abi::sys::NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID,
];

static CARD: std::sync::OnceLock<Card> = std::sync::OnceLock::new();

/// Keep what the card answered. Called once, from `serve`, before the
/// ledger reads its profile. A card that did not answer costs the encoder
/// share and the VM's own UUID, not the VM: the backend still serves.
pub fn set_card(socket: &str, asked: anyhow::Result<([u8; 16], u64)>) {
    match asked {
        Ok((host, total)) => { let _ = CARD.set(Card::new(socket, host, total)); }
        Err(e) => eprintln!(
            "vhost-user-nvrm: the card did not answer at start-up ({e:#}) -- no encoder \
             share and no UUID of this VM's own"
        ),
    }
}

/// What [`set_card`] kept, if the card answered.
pub fn card() -> Option<&'static Card> {
    CARD.get()
}

/// FNV-1a, 64 bit, over the bytes given.
///
/// NOT a cryptographic hash and it does not need to be: what is wanted is
/// a value that is STABLE for a VM across restarts and DIFFERENT between
/// VMs on one card. A UUID's job here is to be a key, not a secret, and a
/// SHA-1 would mean a dependency for a property nothing rests on. The
/// constants are FNV's own (offset basis and prime).
fn fnv1a(seed: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in seed {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Sixteen bytes derived from the VM's name, shaped as a version-4 UUID.
///
/// EVERY byte comes from the name and NONE from the host's own UUID. The
/// host's would have been the easier mix -- keep the card's prefix, change
/// the tail -- and it would have handed the guest a piece of the host's
/// identity to reconstruct. The card is already named in the product
/// string; it does not need to be in the UUID as well.
///
/// The version and variant nibbles are set because a reader (and NVML's
/// own formatting) expects a UUID and not sixteen arbitrary bytes.
pub fn vm_uuid(name: &str) -> [u8; 16] {
    let a = fnv1a(name.as_bytes());
    // A second, differently seeded pass for the other half: FNV over the
    // same bytes twice would give the same eight.
    let b = fnv1a(&[name.as_bytes(), b"/leandro-vgpu"].concat());
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_be_bytes());
    out[8..].copy_from_slice(&b.to_be_bytes());
    out[6] = (out[6] & 0x0f) | 0x40; // version 4
    out[8] = (out[8] & 0x3f) | 0x80; // variant 1
    out
}

/// The ASCII form RM answers with: `GPU-` and then 8-4-4-4-12 hex.
pub fn format_uuid(u: &[u8; 16]) -> String {
    let hex = |r: &[u8]| r.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "GPU-{}-{}-{}-{}-{}",
        hex(&u[0..4]),
        hex(&u[4..6]),
        hex(&u[6..8]),
        hex(&u[8..10]),
        hex(&u[10..16])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_and_the_boolean_are_one_statement() {
        let mut v = vec![0u8; mediate::VIRTMODE_LEN];
        assert_eq!(rewrite_virtualization_mode(&mut v), Some(2));
        assert_eq!(u32::from_le_bytes(v[0..4].try_into().unwrap()), 2, "VGX");
        assert_eq!(v[mediate::VIRTMODE_GRIDBUILD_OFF], 1, "and a GRID build");
    }

    #[test]
    fn a_buffer_that_is_not_the_structure_is_left_alone() {
        let mut short = vec![0u8; mediate::VIRTMODE_LEN - 1];
        assert_eq!(rewrite_virtualization_mode(&mut short), None);
        assert!(short.iter().all(|&b| b == 0), "nothing written");

        let mut small = vec![0u8; mediate::ENCCAP_LEN - 1];
        assert_eq!(rewrite_encoder_capacity(&mut small, 25), None);
    }

    #[test]
    fn the_encoder_share_replaces_the_answer_and_not_the_question() {
        let mut v = vec![0u8; mediate::ENCCAP_LEN];
        // queryType = 1 (HEVC), the question the client asked.
        v[mediate::ENCCAP_QUERY_OFF..mediate::ENCCAP_QUERY_OFF + 4]
            .copy_from_slice(&1u32.to_le_bytes());
        v[mediate::ENCCAP_OFF..mediate::ENCCAP_OFF + 4].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(rewrite_encoder_capacity(&mut v, 25), Some(25));
        assert_eq!(
            u32::from_le_bytes(
                v[mediate::ENCCAP_QUERY_OFF..mediate::ENCCAP_QUERY_OFF + 4].try_into().unwrap()
            ),
            1,
            "the question is carried unchanged"
        );
        assert_eq!(
            u32::from_le_bytes(v[mediate::ENCCAP_OFF..mediate::ENCCAP_OFF + 4].try_into().unwrap()),
            25
        );
    }

    /// A share of zero is "no policy", not "no encoder", and 100 is what
    /// the card says anyway. Neither is worth a rewrite.
    #[test]
    fn a_nonsense_share_is_not_written() {
        let mut v = vec![0u8; mediate::ENCCAP_LEN];
        assert_eq!(rewrite_encoder_capacity(&mut v, 0), None);
        assert_eq!(rewrite_encoder_capacity(&mut v, 101), None);
    }

    #[test]
    fn two_vms_on_one_card_get_two_uuids() {
        let a = vm_uuid("vm0");
        let b = vm_uuid("vm1");
        assert_ne!(a, b, "four guests sharing one UUID is the bug this fixes");
        assert_eq!(a, vm_uuid("vm0"), "and it is the same one after a restart");
    }

    #[test]
    fn it_is_shaped_like_a_uuid() {
        let s = format_uuid(&vm_uuid("desktop"));
        assert!(s.starts_with("GPU-"), "{s}");
        let hex: Vec<&str> = s.trim_start_matches("GPU-").split('-').collect();
        assert_eq!(hex.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(hex.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit())), "{s}");
        // Version 4, variant 1, where NVML's readers expect them.
        let u = vm_uuid("desktop");
        assert_eq!(u[6] >> 4, 4);
        assert_eq!(u[8] >> 6, 0b10);
    }

    /// The card's UUID leaves every answer in both spellings, and comes
    /// back in every question -- and a UUID that is neither passes as it is.
    #[test]
    fn the_uuid_swaps_both_ways_in_both_spellings() {
        let card = Card::new("vm/desktop2/nvrm.sock", [0x41; 16], 8192 << 20);
        let (host, guest) = (format_uuid(&card.host), format_uuid(&card.guest));
        let mut answer = vec![0u8; 12];
        answer.extend_from_slice(&card.host);
        answer.extend_from_slice(&[0u8; 4]);
        answer.extend_from_slice(host.as_bytes());
        answer.extend_from_slice(&[0u8; 4]);
        let asked = answer.clone();
        card.to_guest(&mut answer);
        assert_eq!(&answer[12..28], &card.guest);
        assert_eq!(&answer[32..72], guest.as_bytes());
        card.to_host(&mut answer);
        assert_eq!(answer, asked, "and back");
        let mut foreign = [0x5au8; 64];
        card.to_host(&mut foreign);
        card.to_guest(&mut foreign);
        assert_eq!(foreign, [0x5au8; 64]);
    }

    #[test]
    fn every_vm_is_named_apart_in_both_layouts() {
        assert_eq!(name_from_socket("/mnt/vmstore/leandro/vm/desktop2/nvrm.sock"), "desktop2/nvrm");
        let (a, b) = (name_from_socket("/run/ms/nvrm/0f3a.sock"), name_from_socket("/run/ms/nvrm/77c1.sock"));
        assert_ne!(vm_uuid(&a), vm_uuid(&b), "one directory, two VMs, two UUIDs");
        assert_eq!(vm_uuid(&a), vm_uuid(&name_from_socket("/run/ms/nvrm/0f3a.sock")), "a restart keeps it");
        assert_eq!(name_from_socket("nvrm.sock"), "/nvrm");
    }

}
