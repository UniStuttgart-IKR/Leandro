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

pub use nvrm_abi::mediate::{CMD_GPU_GET_ENCODER_CAPACITY, CMD_GPU_GET_VIRTUALIZATION_MODE};

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
}
