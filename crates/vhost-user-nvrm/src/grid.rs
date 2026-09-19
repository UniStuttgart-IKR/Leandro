// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Guest-visible GPU identity and encoder-capacity reporting.
//!
//! UUID translation is bidirectional. Encoder capacity is reported, not
//! enforced. Virtualization-mode reporting is opt-in because VGX mode
//! prevented CUDA initialization with the tested guest userspace.

use nvrm_abi::mediate;

pub use nvrm_abi::mediate::{
    CMD_GPU_GET_ENCODER_CAPACITY, CMD_GPU_GET_GID_INFO, CMD_GPU_GET_VIRTUALIZATION_MODE,
};

/// Report VGX mode and isGridBuild without changing driver initialization.
/// Returns None if the reply is too short.
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

/// Report the profile's encoder percentage without changing queryType.
/// This reports capacity; it does not enforce an encoder quota.
pub fn rewrite_encoder_capacity(aux: &mut [u8], percent: u32) -> Option<u32> {
    if aux.len() < mediate::ENCCAP_LEN || percent == 0 || percent > 100 {
        return None;
    }
    let o = mediate::ENCCAP_OFF;
    aux[o..o + 4].copy_from_slice(&percent.to_le_bytes());
    Some(percent)
}

// VM identity and UUID translation.

/// Cache LEA_VGPU_MEDIATE: mode,uuid,enc or all/none.
/// Default: uuid and enc enabled; mode disabled.
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

/// Whether to report VGX mode. Disabled by default: the measured guest
/// returned CUDA_ERROR_NO_DEVICE when this override was enabled.
pub fn mediate_mode() -> bool {
    switches().0
}

/// Enable guest UUID reporting and reverse translation on driver requests.
/// Both directions are required: UVM rejects the synthetic UUID.
pub fn mediate_uuid() -> bool {
    switches().1
}

/// Is the encoder-capacity answer on? On by default: it is a number a
/// client reads and nothing acts on inside the guest.
pub fn mediate_enc() -> bool {
    switches().2
}

/// Stable identity seed: parent directory basename and socket stem.
pub fn name_from_socket(socket: &str) -> String {
    let p = std::path::Path::new(socket);
    let part = |o: Option<&std::ffi::OsStr>| {
        o.map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    format!(
        "{}/{}",
        part(p.parent().and_then(|d| d.file_name())),
        part(p.file_stem())
    )
}

/// Physical and guest UUIDs plus physical VRAM bytes, queried at startup.
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
        Card {
            host,
            guest,
            total,
            ascii: (format_uuid(&host), format_uuid(&guest)),
        }
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

/// Replace matching values at 4-byte boundaries. This scans by value,
/// not ABI field offsets; callers must limit it to UUID-bearing buffers.
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
        Ok((host, total)) => {
            let _ = CARD.set(Card::new(socket, host, total));
        }
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

/// Non-cryptographic FNV-1a hash used for stable VM identity.
fn fnv1a(seed: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in seed {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Derive a stable identifier from the VM name, with UUID version/variant bits.
/// Contains no host UUID bytes; it is neither random nor an isolation token.
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
                v[mediate::ENCCAP_QUERY_OFF..mediate::ENCCAP_QUERY_OFF + 4]
                    .try_into()
                    .unwrap()
            ),
            1,
            "the question is carried unchanged"
        );
        assert_eq!(
            u32::from_le_bytes(
                v[mediate::ENCCAP_OFF..mediate::ENCCAP_OFF + 4]
                    .try_into()
                    .unwrap()
            ),
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
        assert_eq!(
            hex.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(
            hex.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit())),
            "{s}"
        );
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
        assert_eq!(
            name_from_socket("/mnt/vmstore/leandro/vm/desktop2/nvrm.sock"),
            "desktop2/nvrm"
        );
        let (a, b) = (
            name_from_socket("/run/ms/nvrm/0f3a.sock"),
            name_from_socket("/run/ms/nvrm/77c1.sock"),
        );
        assert_ne!(
            vm_uuid(&a),
            vm_uuid(&b),
            "one directory, two VMs, two UUIDs"
        );
        assert_eq!(
            vm_uuid(&a),
            vm_uuid(&name_from_socket("/run/ms/nvrm/0f3a.sock")),
            "a restart keeps it"
        );
        assert_eq!(name_from_socket("nvrm.sock"), "/nvrm");
    }
}
