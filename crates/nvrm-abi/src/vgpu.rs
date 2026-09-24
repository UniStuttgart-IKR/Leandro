// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Framebuffer profiles derived from the card's memory and measured host overhead.
//!
//! VMMU is the second-level framebuffer translation unit; its segment is
//! the allocation granule. FB denotes framebuffer memory.
//!
//! [`Catalogue::profile_for`] charges a VM for its framebuffer, host overhead,
//! and proportional share of unavailable memory. Named types (`RTX2070-4Q`)
//! and explicit sizes (`3G`, `130M`) use the same accounting rule.
//!
//! The catalogue follows the GSP-client arithmetic in
//! `kvgpumgrSetSupportedPlacementIds`, kernel_vgpu_mgr.c:3795-3805:
//!
//! ```text
//!   totalAvailableFb = ALIGN_UP(fbTotal, 8 * vmmuSegmentSize)
//!   guestFbLength    = totalAvailableFb / maxInstance
//!                      - fbReservation - gspHeapSize
//!   guestFbLength    = MIN(guestFbLength, fbLength)
//!   guestFbLength    = ALIGN_DOWN(guestFbLength, vmmuSegmentSize)
//!   guestVmmuCount   = guestFbLength / vmmuSegmentSize
//! ```
//!
//! The non-GSP branch (:3757-3783) divides reserved memory among instances:
//!
//! ```text
//!   vgpuReservedFb = ALIGN_UP(totalReservedFb / maxInstance, segment)
//! ```
//!
//! This project estimates reservations from measurements. NVIDIA's per-profile
//! `fbReservation` and `gspHeapSize` are supplied by the closed host driver and
//! firmware (`memmgrGetVgpuHostRmReservedFb_KERNEL`, mem_mgr.c:3984).
//! There is no per-VM GSP plugin here, so `gspHeapSize` is zero.
//!
//! Names follow `<board>-<gigabytes><class>`, with `Q` for graphics and CUDA;
//! the letter implies no NVIDIA licence. The guest card uses the `Leandro`
//! brand. Only catalogue types require whole segments: this backend does not
//! assign VMMU placements. See docs/OPEN-QUESTIONS.md, questions 68 and 69.

/// Valid values of `NV2080_CTRL_GPU_VMMU_SEGMENT_SIZE_*` (ctrl2080gpu.h:3143-3148).
/// `nvrm-client --bin vgpuprofile` validates the card's answer against these.
pub const VMMU_SEGMENT_SIZES: [u64; 6] = [
    0x0200_0000, // 32 MiB
    0x0400_0000, // 64 MiB
    0x0800_0000, // 128 MiB
    0x1000_0000, // 256 MiB
    0x2000_0000, // 512 MiB
    0x4000_0000, // 1024 MiB
];

pub use crate::mediate::{FB_INFO_INDEX_HEAP_SIZE, FB_INFO_INDEX_TOTAL_RAM_SIZE};

/// One VM's memory cost and guest limits, computed by [`Catalogue::profile_for`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Type or size label, such as `RTX2070-2Q` or `RTX2070-130M`.
    /// The guest card name follows `fb_length` ([`crate::naming::Profile::from_catalogue`]).
    pub name: String,
    /// vGPU's `maxInstance`: how many VMs of this size fit on the card.
    pub max_instance: u32,
    /// vGPU's `profileSize`: what one VM costs the card.
    pub profile_size: u64,
    /// vGPU's `fbReservation`, `profile_size - fb_length`.
    pub reservation: u64,
    /// vGPU's `fbLength`: the guest framebuffer limit, in bytes.
    pub fb_length: u64,
    /// vGPU's `guestVmmuCount`, displayed to the operator.
    pub segments: u64,
    /// vGPU's `encoderCapacity`: [`encoder_share`].
    pub encoder_capacity: u32,
}

/// Named profiles and memory accounting for one card.
#[derive(Clone, Debug)]
pub struct Catalogue {
    /// Board name without vendor prefixes or spaces, such as `RTX2070`.
    pub board: String,
    /// `NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE`, bytes. This is what
    /// vGPU's arithmetic calls `fbTotalMemSizeMb`.
    pub total: u64,
    /// `..._HEAP_SIZE` minus host allocations, in bytes. Each VM pays a
    /// proportional share of the difference from `total`.
    pub usable: u64,
    /// What the card answered for its VMMU segment size.
    pub segment: u64,
    /// Measured RM memory overhead per VM, in bytes. Question 68 records
    /// about 175 MiB for gaming/streaming and 25 MiB for CUDA/desktop use.
    pub overhead: u64,
    pub profiles: Vec<Profile>,
}

/// Remove vendor prefixes and spaces: `NVIDIA GeForce RTX 2070` becomes `RTX2070`.
pub fn board_name(real: &str) -> String {
    let s = real.trim();
    let s = s.strip_prefix("NVIDIA ").unwrap_or(s);
    let s = s.strip_prefix("GeForce ").unwrap_or(s);
    let s = s.strip_prefix("Quadro ").unwrap_or(s);
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Parse positive whole binary sizes as MiB: `3G`, `3GiB`, `130M`, `130MiB`,
/// or a bare MiB count. Decimal units such as `GB` are rejected.
pub fn parse_mib(s: &str) -> Option<u64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let scale = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "M" | "MIB" => 1,
        "G" | "GIB" => 1024,
        _ => return None,
    };
    n.parse::<u64>().ok()?.checked_mul(scale).filter(|&m| m > 0)
}

/// Percentage returned by `NV2080_CTRL_CMD_GPU_GET_ENCODER_CAPACITY`.
/// Bare metal returns 100 (subdevice_ctrl_gpu_kernel.c:1000).
///
/// The share uses total card memory, independent of host heap use (question 71).
/// Clamp nonzero-card shares to at least 1: the rewrite treats 0 as no limit.
pub fn encoder_share(fb_length: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    (100 * fb_length as u128 / total as u128).clamp(1, 100) as u32
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 {
        v
    } else {
        v.div_ceil(a) * a
    }
}
fn align_down(v: u64, a: u64) -> u64 {
    if a == 0 {
        v
    } else {
        (v / a) * a
    }
}

impl Catalogue {
    /// Derive profiles from card memory, segment size, and per-VM overhead.
    /// All sizes are bytes; `usable` excludes memory held by the host.
    ///
    /// Power-of-two instance counts follow vGPU's recursive placement division
    /// (`_kvgpumgrSetHeterogeneousResources`, kernel_vgpu_mgr.c:3020 ff.).
    /// Only whole-GiB shares receive a named profile.
    ///
    /// Round the carve-out share up and the guest framebuffer down to whole
    /// segments, so [`Catalogue::profile_for`] never charges more than the share.
    pub fn derive(board: &str, total: u64, usable: u64, segment: u64, overhead: u64) -> Catalogue {
        let mut cat = Catalogue {
            board: board.to_string(),
            total,
            usable,
            segment,
            overhead,
            profiles: Vec::new(),
        };
        if segment == 0 || total == 0 {
            return cat;
        }
        let available = cat.available();
        let carve_out = total.saturating_sub(usable);
        for count in [1u64, 2, 4, 8, 16] {
            let share = available / count;
            if share % GIB != 0 || share == 0 {
                continue;
            }
            let reserved = (carve_out as u128 * share as u128).div_ceil(available as u128) as u64;
            let fb = align_down(
                share.saturating_sub(align_up(reserved + overhead, segment)),
                segment,
            );
            // Measured overhead must still leave enough heap for every instance.
            if fb == 0 || fb * count > usable {
                continue;
            }
            let row = cat.profile_for(format!("{board}-{}Q", share / GIB), fb);
            cat.profiles.extend(row);
        }
        cat
    }

    /// vGPU's `totalAvailableFb`: total card memory aligned up to eight
    /// VMMU segments (kernel_vgpu_mgr.c:3797).
    pub fn available(&self) -> u64 {
        if self.segment == 0 {
            self.total
        } else {
            align_up(self.total, 8 * self.segment)
        }
    }

    /// Compute the card memory charged for `fb_length` bytes of guest framebuffer.
    ///
    /// ```text
    ///   profileSize   = (fb_length + overhead) * available / (available - carve_out), up to a MiB
    ///   fbReservation = profileSize - fb_length
    ///   maxInstance   = available / profileSize
    ///   encoder       = encoder_share(fb_length, total)
    /// ```
    ///
    /// Each VM pays its framebuffer, measured overhead, and proportional share
    /// of unavailable memory, rounded up. Returns `None` if it cannot fit.
    pub fn profile_for(&self, name: String, fb_length: u64) -> Option<Profile> {
        let available = self.available();
        let usable = available.checked_sub(self.total.saturating_sub(self.usable))?;
        let need = fb_length.checked_add(self.overhead)?;
        if fb_length == 0 || need > usable {
            return None;
        }
        let cost = align_up(
            (need as u128 * available as u128).div_ceil(usable as u128) as u64,
            MIB,
        );
        Some(Profile {
            name,
            max_instance: (available / cost) as u32,
            profile_size: cost,
            reservation: cost - fb_length,
            fb_length,
            segments: fb_length.checked_div(self.segment).unwrap_or(0),
            encoder_capacity: encoder_share(fb_length, self.total),
        })
    }

    /// Check whether `want` fits beside the live profile sizes.
    ///
    /// Compare their sum with `available`: profile costs already include their
    /// shares of the carve-out. Comparing with `usable` would charge it twice.
    /// Unlike vGPU (`_kvgpumgrSetHeterogeneousResources`, `_kvgpumgrIsPlacementValid`,
    /// kernel_vgpu_mgr.c), this backend has no fixed VMMU placements to check.
    pub fn admits(&self, live: &[u64], want: u64) -> bool {
        live.iter().copied().sum::<u64>() + want <= self.available()
    }

    /// Resolve a type (`2Q`, `RTX2070-2Q`) or framebuffer size
    /// (`3G`, `130MiB`, `RTX2070-130M`), ignoring case.
    /// Both use [`Catalogue::profile_for`] for accounting.
    pub fn resolve(&self, want: &str) -> Option<Profile> {
        let board = format!("{}-", self.board.to_ascii_uppercase());
        let w = want.trim().to_ascii_uppercase();
        let short = w.strip_prefix(&board).unwrap_or(&w);
        if let Some(p) = self
            .profiles
            .iter()
            .find(|p| p.name.to_ascii_uppercase() == board.clone() + short)
        {
            return Some(p.clone());
        }
        let mib = parse_mib(short)?;
        let fb_length = mib.checked_mul(MIB)?;
        let size = if mib % 1024 == 0 {
            format!("{}G", mib / 1024)
        } else {
            format!("{mib}M")
        };
        self.profile_for(format!("{}-{size}", self.board), fb_length)
    }

    /// Format the card's memory, reservations, and profiles as a table.
    pub fn table(&self) -> String {
        let mib = |b: u64| b / MIB;
        let mut s = String::new();
        s.push_str(&format!(
            "card {}: {} MiB total, {} MiB usable heap, {} MiB carved out by the card, \
             {} MiB VMMU segment\nper-VM host overhead assumed: {} MiB\n\n",
            self.board,
            mib(self.total),
            mib(self.usable),
            mib(self.total - self.usable),
            mib(self.segment),
            mib(self.overhead),
        ));
        s.push_str(
            "type          max  profile   reserved   guest FB   segments   encoder%   all instances\n",
        );
        for p in &self.profiles {
            s.push_str(&format!(
                "{:<13} {:>3} {:>8} {:>10} {:>10} {:>10} {:>9} {:>13}\n",
                p.name,
                p.max_instance,
                mib(p.profile_size),
                mib(p.reservation),
                mib(p.fb_length),
                p.segments,
                p.encoder_capacity,
                mib(p.fb_length * p.max_instance as u64),
            ));
        }
        if self.profiles.is_empty() {
            s.push_str("(none -- the card's own carve-out leaves no whole-gigabyte profile)\n");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FB_GET_INFO_V2 measured 8192 MiB total and 7773 MiB heap on 2026-08-21,
    /// both in the guest and natively.
    fn rtx2070(segment: u64) -> Catalogue {
        Catalogue::derive("RTX2070", 8192 * MIB, 7773 * MIB, segment, 256 * MIB)
    }

    /// Launcher input after subtracting host desktop use; this yields 2Q = 1280 MiB.
    fn rtx2070_with_a_busy_host() -> Catalogue {
        Catalogue::derive(
            "RTX2070",
            8192 * MIB,
            (7771 - 900) * MIB,
            256 * MIB,
            256 * MIB,
        )
    }

    fn cards() -> impl Iterator<Item = Catalogue> {
        VMMU_SEGMENT_SIZES.into_iter().flat_map(|seg| {
            (0..=2400).step_by(37).map(move |host| {
                Catalogue::derive("RTX2070", 8192 * MIB, (7773 - host) * MIB, seg, 256 * MIB)
            })
        })
    }

    /// Profile accounting with measured host desktop use.
    #[test]
    fn the_rule_prices_a_framebuffer() {
        let cat = rtx2070_with_a_busy_host();
        let p = cat.resolve("3G").expect("3 GiB fits");
        assert_eq!(p.name, "RTX2070-3G");
        assert_eq!(
            (
                p.fb_length,
                p.profile_size,
                p.reservation,
                p.max_instance,
                p.encoder_capacity
            ),
            (3072 * MIB, 3968 * MIB, 896 * MIB, 2, 37)
        );
        let p = cat.resolve("130MiB").expect("130 MiB fits");
        assert_eq!(p.name, "RTX2070-130M");
        assert_eq!(
            (
                p.fb_length,
                p.profile_size,
                p.reservation,
                p.max_instance,
                p.encoder_capacity
            ),
            (130 * MIB, 461 * MIB, 331 * MIB, 17, 1)
        );
        // The framebuffer plus overhead must fit the usable heap.
        assert!(cat.resolve("6615M").is_some());
        assert!(cat.resolve("6616M").is_none());
    }

    /// A type and its framebuffer size differ only in their labels.
    #[test]
    fn a_type_and_its_size_are_the_same_profile() {
        for cat in cards() {
            for row in &cat.profiles {
                let size = cat
                    .resolve(&format!("{}M", row.fb_length / MIB))
                    .expect("the row's size");
                assert_eq!(
                    Profile {
                        name: row.name.clone(),
                        ..size
                    },
                    *row
                );
            }
        }
    }

    /// Each type costs at most its share and admits its promised instance count.
    /// The pinned host states catch a 1-2 MiB overcharge from rounding down.
    #[test]
    fn a_type_never_costs_more_than_its_share() {
        let pinned = [6139, 6655, 7165]
            .map(|u| Catalogue::derive("RTX2070", 8192 * MIB, u * MIB, 256 * MIB, 256 * MIB));
        for cat in cards().chain(pinned) {
            for p in &cat.profiles {
                let gib: u64 = p
                    .name
                    .trim_start_matches("RTX2070-")
                    .trim_end_matches('Q')
                    .parse()
                    .unwrap();
                let count = cat.available() / (gib * GIB);
                assert!(
                    p.profile_size <= gib * GIB,
                    "{} costs {} MiB",
                    p.name,
                    p.profile_size / MIB
                );
                assert!(p.max_instance as u64 >= count);
                let live = vec![p.profile_size; count as usize - 1];
                assert!(
                    cat.admits(&live, p.profile_size),
                    "{}: the last instance was refused",
                    p.name
                );
            }
        }
    }

    /// Accounting invariants also hold for explicit framebuffer sizes.
    #[test]
    fn any_size_follows_the_rule() {
        let cat = rtx2070_with_a_busy_host();
        for mib in (1..=6615).step_by(7) {
            let p = cat.resolve(&format!("{mib}")).expect("fits");
            assert_eq!(p.fb_length, mib * MIB);
            assert!(p.profile_size >= p.fb_length + cat.overhead);
            assert_eq!(p.reservation, p.profile_size - p.fb_length);
            assert!(p.max_instance as u64 * p.profile_size <= cat.available());
            assert!(!cat.admits(
                &vec![p.profile_size; p.max_instance as usize],
                p.profile_size
            ));
            assert_eq!(p.encoder_capacity, encoder_share(p.fb_length, cat.total));
        }
    }

    /// The mixed case: one 4Q beside two 2Q fits the card, and a further
    /// 1Q does not.
    #[test]
    fn one_4q_admits_two_2q_and_nothing_more() {
        let cat = rtx2070_with_a_busy_host();
        let p4 = cat.resolve("4Q").expect("4Q");
        let p2 = cat.resolve("2Q").expect("2Q");
        let p1 = cat.resolve("1Q").expect("1Q");
        assert!(cat.admits(&[p4.profile_size], p2.profile_size));
        assert!(cat.admits(&[p4.profile_size, p2.profile_size], p2.profile_size));
        assert!(!cat.admits(
            &[p4.profile_size, p2.profile_size, p2.profile_size],
            p1.profile_size
        ));
        // The admitted VMs' framebuffers and overhead must fit the actual heap.
        let asked = p4.fb_length + 2 * p2.fb_length + 3 * cat.overhead;
        assert!(
            asked <= cat.usable,
            "{} MiB asked of {} MiB",
            asked / MIB,
            cat.usable / MIB
        );
    }

    #[test]
    fn the_board_name_loses_the_vendor_and_the_spaces() {
        assert_eq!(board_name("NVIDIA GeForce RTX 2070"), "RTX2070");
        assert_eq!(board_name("NVIDIA RTX A6000"), "RTXA6000");
        assert_eq!(board_name("Tesla V100-SXM2-16GB"), "TeslaV100-SXM2-16GB");
    }

    #[test]
    fn every_profile_fits_the_card_it_was_derived_from() {
        for seg in VMMU_SEGMENT_SIZES {
            let c = rtx2070(seg);
            for p in &c.profiles {
                assert_eq!(p.fb_length % seg, 0, "{} not segment-aligned", p.name);
                assert_eq!(p.fb_length, p.segments * seg);
                assert!(
                    p.fb_length * p.max_instance as u64 <= c.usable,
                    "{} x{} does not fit the usable heap",
                    p.name,
                    p.max_instance
                );
            }
        }
    }

    /// The card's own carve-out is 419 MiB here: the whole-card VM carries
    /// all of it, a 1 GiB one an eighth.
    #[test]
    fn the_reservation_is_divided_among_the_instances() {
        let c = rtx2070(32 * MIB);
        let one = c.resolve("8Q").expect("one instance of the whole card");
        let many = c.resolve("1Q").expect("eight instances of one gigabyte");
        assert_eq!(one.max_instance, 1);
        assert_eq!(many.max_instance, 8);
        assert!(
            one.reservation > many.reservation,
            "one instance carries the whole carve-out ({} MiB) and eight share it ({} MiB)",
            one.reservation / MIB,
            many.reservation / MIB
        );
        // Reservation includes at least the measured per-VM overhead.
        assert!(many.reservation >= 256 * MIB);
    }

    #[test]
    fn a_profile_is_named_the_way_vgpu_names_one() {
        let c = rtx2070(32 * MIB);
        let p = c.resolve("2Q").expect("2Q");
        assert_eq!(p.name, "RTX2070-2Q");
        assert!(p.profile_size <= 2 * GIB);
        assert_eq!(p.max_instance, 4);
        // Short and fully qualified names resolve identically.
        assert_eq!(c.resolve("RTX2070-2Q"), c.resolve("2q"));
        assert_eq!(c.resolve("rtx2070-130m"), c.resolve("130 MiB"));
    }

    /// Encoder shares scale with framebuffer size and clamp to at least 1%.
    #[test]
    fn the_encoder_share_follows_the_framebuffer() {
        let total = 8192 * MIB;
        assert_eq!(encoder_share(total, total), 100);
        assert_eq!(encoder_share(4096 * MIB, total), 50);
        assert_eq!(encoder_share(3072 * MIB, total), 37);
        assert_eq!(encoder_share(130 * MIB, total), 1);
        assert_eq!(encoder_share(2 * MIB, total), 1, "clamped, never 0");
        assert_eq!(encoder_share(1, 0), 0, "no card, no share");
    }

    #[test]
    fn sizes_are_binary_and_whole() {
        for (s, mib) in [
            ("4G", 4096),
            ("4GiB", 4096),
            ("130MiB", 130),
            ("130M", 130),
            ("3072", 3072),
            (" 3 gib ", 3072),
        ] {
            assert_eq!(parse_mib(s), Some(mib), "{s:?}");
        }
        for s in ["3GB", "130MB", "1.5G", "", "0", "-1", "4T", "G"] {
            assert_eq!(parse_mib(s), None, "{s:?}");
        }
    }

    #[test]
    fn oversized_framebuffer_sizes_are_rejected() {
        let cat = rtx2070_with_a_busy_host();
        // Unchecked byte conversion wraps these to 1 MiB or 1 GiB.
        let mib = u64::MAX / MIB + 2;
        let gib = u64::MAX / GIB + 2;
        for want in [
            mib.to_string(),
            format!("{mib}M"),
            format!("{mib}MiB"),
            format!("RTX2070-{mib}M"),
            format!("{gib}G"),
            format!("{gib}GiB"),
        ] {
            assert!(
                cat.resolve(&want).is_none(),
                "accepted oversized size {want}"
            );
        }
    }

    #[test]
    fn a_card_with_no_vmmu_yields_no_catalogue() {
        // RM returns a zero segment size for chips without a VMMU (gpu.c:940).
        let c = Catalogue::derive("RTX2070", 8192 * MIB, 7773 * MIB, 0, 256 * MIB);
        assert!(c.profiles.is_empty());
        assert!(c.table().contains("none"));
    }

    /// A 1 GiB type cannot survive a large segment: the reservation rounds
    /// up to the segment, and a 1 GiB row would have nothing left.
    #[test]
    fn a_segment_larger_than_the_slice_removes_the_row() {
        let big = rtx2070(1024 * MIB);
        assert!(big.resolve("1Q").is_none());
        assert!(big.resolve("8Q").is_some());
    }
}
