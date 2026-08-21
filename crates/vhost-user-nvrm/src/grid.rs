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
//! NEITHER IS ON UNLESS THE vGPU-SHAPED POLICY IS. Under the default, under
//! `LEA_VRAM_LIMIT_MIB` and under `LEA_VRAM_PROFILE_MIB`, RM's own answers
//! are forwarded untouched -- a VM that was given a smaller framebuffer is
//! not a vGPU and must not claim to be one.

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
// not the board's, and two vGPUs on one card differ.

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
            // MEASURED, not chosen: of the three, only the encoder share
            // leaves CUDA working. See `mediate_mode` and `mediate_uuid`.
            return (false, false, true);
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

/// Is the per-VM UUID answer on? **Off by default, because it breaks CUDA
/// -- differently from the mode answer, and less obviously.**
///
/// Measured 2026-08-21, one guest under `RTX2070-2Q`, this answer alone:
/// `nvidia-smi` printed the new UUID (`GPU-68f65e19-4ec5-46d4-8b4f-...`,
/// derived from the VM's name, and DIFFERENT per VM as intended), and
/// `vrampress: cuInit 3`, which is
/// `CUDA_ERROR_NOT_INITIALIZED` -- not "no device" but "this
/// device did not come up". So libcuda does more with the UUID than print
/// it, and something it cross-checks no longer agrees.
///
/// TWO CANDIDATES WERE EXCLUDED before this was left open, both measured
/// on the same guest:
///
///   * **The flags are not the problem.** libcuda asks exactly twice and
///     both times with `flags = 0x2` = `FORMAT_BINARY`, and this rewrite
///     answers 16 bytes with `length` set to 16 -- the shape RM itself
///     returns for a binary SHA-1 GID. Nothing is truncated and no
///     SHA-256 or uGPU form is asked for.
///   * **Nothing else in the guest disagrees.**
///     `/proc/driver/nvidia/gpus/` is EMPTY there -- `nvrm_nodes` runs
///     with `create_nodes=0` and `virtio_nvrm` touches no `/proc` at all
///     (number 2) -- so there is no second copy to contradict. And NVML is
///     content: `nvidia-smi -L` prints
///     `GPU 0: Leandro RTX2070-2Q (UUID: GPU-68f65e19-...)`.
///
/// So the objection is inside libcuda, and finding it means tracing
/// libcuda rather than reasoning about it. That is the next step and it is
/// not this branch's.
///
/// The leak it was written for is real and is recorded (four guests, one
/// UUID, all the card's). This is not a reason to keep the leak; it is a
/// reason not to ship the fix before it is understood.
pub fn mediate_uuid() -> bool {
    switches().1
}

/// Is the encoder-capacity answer on? On by default: it is a number a
/// client reads and nothing acts on inside the guest.
pub fn mediate_enc() -> bool {
    switches().2
}

/// The name of the VM this process serves, for the UUID below.
///
/// Set once at startup from the SOCKET PATH -- `vm/<name>/nvrm.sock` --
/// because that is the only per-VM name this process is given. It needs no
/// new knob and it cannot drift from the instance the rig thinks it
/// started: it IS the instance directory.
static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The instance name inside a socket path, as a pure function -- a
/// `OnceLock` can be set once per PROCESS and tests share one.
pub fn name_from_socket(socket: &str) -> String {
    std::path::Path::new(socket)
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| socket.to_string())
}

/// Remember which VM this is. Called once, from `serve`.
pub fn set_identity(socket: &str) {
    let _ = IDENTITY.set(name_from_socket(socket));
}

/// What this VM is called, or `"lea"` if nobody said.
pub fn identity() -> &'static str {
    IDENTITY.get().map(String::as_str).unwrap_or("lea")
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

/// Replace the card's UUID with this VM's, in whichever format was asked
/// for.
///
/// `flags` bit 1 picks the format (`..._FORMAT_BINARY`, ctrl2080gpu.h:1768);
/// ASCII is the zero value, which is why this is a mask test. `length` is
/// an OUTPUT and is rewritten with it -- a length that still describes RM's
/// answer beside data that does not is how a reader ends up parsing past
/// the end.
///
/// Returns what was written, for the log line.
pub fn rewrite_gid_info(aux: &mut [u8], name: &str) -> Option<String> {
    if aux.len() < mediate::GID_LEN {
        return None;
    }
    let flags = u32::from_le_bytes(
        aux[mediate::GID_FLAGS_OFF..mediate::GID_FLAGS_OFF + 4].try_into().unwrap(),
    );
    let u = vm_uuid(name);
    let d = mediate::GID_DATA_OFF;
    let written;
    if flags & mediate::GID_FLAGS_FORMAT_BINARY != 0 {
        aux[d..d + mediate::GID_SHA1_BINARY_LEN].copy_from_slice(&u);
        // Zero the rest of the buffer: RM's answer is still in there, and
        // a caller that reads past `length` would read the host's.
        for b in aux[d + mediate::GID_SHA1_BINARY_LEN..d + mediate::GID_DATA_MAX].iter_mut() {
            *b = 0;
        }
        written = mediate::GID_SHA1_BINARY_LEN;
    } else {
        let text = format_uuid(&u);
        let bytes = text.as_bytes();
        let n = bytes.len().min(mediate::GID_DATA_MAX - 1);
        aux[d..d + n].copy_from_slice(&bytes[..n]);
        for b in aux[d + n..d + mediate::GID_DATA_MAX].iter_mut() {
            *b = 0;
        }
        // RM counts the NUL in the ASCII length.
        written = n + 1;
    }
    aux[mediate::GID_LENGTH_OFF..mediate::GID_LENGTH_OFF + 4]
        .copy_from_slice(&(written as u32).to_le_bytes());
    Some(format_uuid(&u))
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

    /// The host's UUID must not survive in the answer -- neither in the
    /// bytes past the new one nor in a length that still describes it.
    #[test]
    fn the_hosts_uuid_does_not_survive_either_format() {
        for (flags, want_len) in [(0u32, 41usize), (mediate::GID_FLAGS_FORMAT_BINARY, 16)] {
            let mut v = vec![0xAAu8; mediate::GID_LEN];
            v[mediate::GID_FLAGS_OFF..mediate::GID_FLAGS_OFF + 4]
                .copy_from_slice(&flags.to_le_bytes());
            // RM's answer: the host card's UUID, filling the buffer.
            for b in v[mediate::GID_DATA_OFF..].iter_mut() {
                *b = 0x5A;
            }
            let written = rewrite_gid_info(&mut v, "vm2").expect("rewritten");
            assert!(written.starts_with("GPU-"));
            let len = u32::from_le_bytes(
                v[mediate::GID_LENGTH_OFF..mediate::GID_LENGTH_OFF + 4].try_into().unwrap(),
            ) as usize;
            assert_eq!(len, want_len);
            let tail = &v[mediate::GID_DATA_OFF + len - usize::from(flags == 0)..];
            assert!(tail.iter().all(|&b| b == 0), "the host's bytes are still there");
        }
    }

    #[test]
    fn the_identity_is_the_instance_directory() {
        // Not the socket file, and not the whole path: the directory is
        // what the rig calls the instance.
        assert_eq!(name_from_socket("/mnt/vmstore/leandro/vm/desktop2/nvrm.sock"), "desktop2");
        assert_eq!(name_from_socket("vm/vm7/nvrm.sock"), "vm7");
        // A path with no directory at all still yields something stable.
        assert_eq!(name_from_socket("nvrm.sock"), "nvrm.sock");
    }

}
