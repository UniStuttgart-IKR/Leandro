// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The vGPU-shaped profile catalogue: what NVIDIA's own arithmetic would
//! make of THIS card.
//!
//! Terms, once: the VMMU is the second-level translation unit that gives a
//! vGPU its own view of framebuffer, and its SEGMENT is the granule a guest
//! framebuffer is cut in. FB is the card's own memory.
//!
//! WHY THIS EXISTS, and how it differs from `vram.rs`'s profile
//! (docs/OPEN-QUESTIONS.md 68 and 69). There, the operator names a number
//! and a reservation comes off it; the sizes are arbitrary and per VM,
//! which is what that entry's scope asked for. Here the CARD names the
//! numbers: a catalogue is derived from the card's own total, its usable
//! heap and its VMMU segment size, every profile on the card is the same
//! size, the guest framebuffer is quantised to whole segments, and the
//! instance count is bounded. That is vGPU's model, constraint for
//! constraint -- including the one entry 68 deliberately rejected.
//!
//! WHAT IS COPIED, and it is the arithmetic rather than the mechanism:
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
//! -- `kvgpumgrSetSupportedPlacementIds`, kernel_vgpu_mgr.c:3795-3805, the
//! GSP-client branch, which is the branch this card takes. The non-GSP
//! branch above it (:3757-3783) is where the OTHER rule comes from, and it
//! is the one this file needs most:
//!
//! ```text
//!   vgpuReservedFb = ALIGN_UP(totalReservedFb / maxInstance, segment)
//! ```
//!
//! **The reserve is DIVIDED among the instances, not charged to each.**
//! That is the difference between a catalogue and a per-VM knob, and it is
//! why this file computes a different reservation for every row.
//!
//! WHAT CANNOT BE COPIED, and is therefore ours and marked as ours:
//!
//!   * `fbReservation` and `gspHeapSize` are per-profile fields of a
//!     catalogue that lives in the closed vGPU host driver;
//!     `memmgrGetVgpuHostRmReservedFb_KERNEL` (mem_mgr.c:3984) asks the GSP
//!     for the number and the GSP is firmware. Ours is built from two
//!     measurements instead -- see [`Catalogue::derive`].
//!   * The profile NAMES and their class letters (`Q`, `C`, `B`, `A`) come
//!     from that same closed catalogue. The SHAPE is public --
//!     `<board>-<gigabytes><class>` -- and this file follows it, with
//!     `Leandro` where NVIDIA writes `GRID` or `NVIDIA`, because a mediated
//!     card must never be mistakable for a vendor one.
//!   * There is no GSP plugin per VM here and no per-VM GSP heap, so
//!     `gspHeapSize` is zero and says so.

/// `NV2080_CTRL_GPU_VMMU_SEGMENT_SIZE_*` (ctrl2080gpu.h:3143-3148). The
/// card is asked rather than looked up (`nvrm-client --bin vgpuprofile`);
/// these are here so a nonsense answer can be recognised as one.
pub const VMMU_SEGMENT_SIZES: [u64; 6] = [
    0x0200_0000, // 32 MiB
    0x0400_0000, // 64 MiB
    0x0800_0000, // 128 MiB
    0x1000_0000, // 256 MiB
    0x2000_0000, // 512 MiB
    0x4000_0000, // 1024 MiB
];

pub use crate::mediate::{FB_INFO_INDEX_HEAP_SIZE, FB_INFO_INDEX_TOTAL_RAM_SIZE};

/// The class letter of a profile, in vGPU's own vocabulary.
///
/// vGPU's letters mean licence classes: `Q` is Quadro vDWS (full graphics
/// AND CUDA), `C` is vCS (compute only, no display), `B` is vPC and `A` is
/// vApps. Ours means the same shape and NO licence -- there is nothing to
/// license here, and a letter that promised one would be a lie. `Q` is the
/// default because these guests do graphics and CUDA at once, which is
/// exactly the combination `Q` names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Class {
    Q,
    C,
}

impl Class {
    pub fn letter(self) -> char {
        match self {
            Class::Q => 'Q',
            Class::C => 'C',
        }
    }
    pub fn parse(c: char) -> Option<Class> {
        match c.to_ascii_uppercase() {
            'Q' => Some(Class::Q),
            'C' => Some(Class::C),
            _ => None,
        }
    }
}

/// One row of the catalogue: what a VM of this type costs and gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// vGPU's `vgpuName` without the vendor prefix: `RTX2070-2Q`.
    pub name: String,
    pub class: Class,
    /// vGPU's `maxInstance`: how many of THIS type fit on the card.
    pub max_instance: u32,
    /// vGPU's `profileSize`: what one instance costs the card.
    pub profile_size: u64,
    /// vGPU's `fbReservation`: this instance's share of what the card and
    /// the host keep for themselves.
    pub reservation: u64,
    /// vGPU's `fbLength`: what the guest sees, a whole number of segments.
    pub fb_length: u64,
    /// vGPU's `guestVmmuCount`.
    pub segments: u64,
}

impl Profile {
    /// The name the guest's `nvidia-smi` prints, with the umbrella name
    /// where NVIDIA puts `GRID`.
    pub fn guest_name(&self) -> String {
        format!("Leandro {}", self.name)
    }

    /// What all instances of this type together cost the card.
    pub fn total_cost(&self) -> u64 {
        (self.profile_size) * self.max_instance as u64
    }
}

/// Every profile this card supports, derived from the card.
#[derive(Clone, Debug)]
pub struct Catalogue {
    /// The board name with the vendor's marketing words removed and its
    /// spaces closed up, the way vGPU writes it: `RTX2070`.
    pub board: String,
    /// `NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE`, bytes. This is what
    /// vGPU's arithmetic calls `fbTotalMemSizeMb`.
    pub total: u64,
    /// `..._HEAP_SIZE`, bytes: what is actually allocatable. The gap
    /// between this and `total` is the card's own carve-out (ECC, page
    /// tables, RM's static reserve) and is the FIRST thing a reservation
    /// has to cover -- it is not optional and it is not ours to spend.
    pub usable: u64,
    /// What the card answered for its VMMU segment size.
    pub segment: u64,
    /// The per-VM host-side overhead this project has measured: RM's own
    /// device memory behind a channel, which never crosses the boundary as
    /// a request (number 68: ~175 MiB with a game and a stream, ~25 MiB
    /// with a CUDA allocator and a desktop).
    pub overhead: u64,
    pub profiles: Vec<Profile>,
}

/// The board string vGPU would use: no vendor, no marketing, no spaces.
///
/// `NVIDIA GeForce RTX 2070` becomes `RTX2070`, the way `NVIDIA RTX A6000`
/// becomes `RTXA6000` in NVIDIA's own profile names.
pub fn board_name(real: &str) -> String {
    let s = real.trim();
    let s = s.strip_prefix("NVIDIA ").unwrap_or(s);
    let s = s.strip_prefix("GeForce ").unwrap_or(s);
    let s = s.strip_prefix("Quadro ").unwrap_or(s);
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 { v } else { v.div_ceil(a) * a }
}
fn align_down(v: u64, a: u64) -> u64 {
    if a == 0 { v } else { (v / a) * a }
}

impl Catalogue {
    /// Derive the catalogue from what the card answered.
    ///
    /// `total` and `usable` are `TOTAL_RAM_SIZE` and `HEAP_SIZE` in bytes,
    /// `segment` is the card's VMMU segment size, `overhead` is the
    /// per-VM host-side cost this project measured.
    ///
    /// THE INSTANCE COUNTS are powers of two, which is not an arbitrary
    /// simplification: vGPU's placement arithmetic recursively halves the
    /// placement region (`_kvgpumgrSetHeterogeneousResources`,
    /// kernel_vgpu_mgr.c:3020 ff.), and the profile sizes NVIDIA publishes
    /// for a board are its FB divided by 1, 2, 4, 8 ... A count whose
    /// profile is not a whole gigabyte is dropped, because vGPU names its
    /// profiles in gigabytes and a `RTX2070-0Q` would be a nonsense name.
    ///
    /// THE RESERVATION per instance is
    /// `ALIGN_UP((total - usable) / maxInstance + overhead, segment)`:
    /// the card's own carve-out DIVIDED among the instances (vGPU's
    /// `totalReservedFb / maxInstance`, kernel_vgpu_mgr.c:3762) plus this
    /// project's own per-VM overhead, which is per VM and therefore is not
    /// divided.
    pub fn derive(board: &str, total: u64, usable: u64, segment: u64, overhead: u64) -> Catalogue {
        let mut profiles = Vec::new();
        if segment == 0 || total == 0 {
            return Catalogue {
                board: board.to_string(),
                total,
                usable,
                segment,
                overhead,
                profiles,
            };
        }
        // vGPU aligns the total up to EIGHT segments before dividing
        // (kernel_vgpu_mgr.c:3797). Eight, because the placement region is
        // halved at most three times before the segment becomes the unit.
        let available = align_up(total, 8 * segment);
        let carve_out = total.saturating_sub(usable);

        for max_instance in [1u32, 2, 4, 8, 16] {
            let profile_size = available / max_instance as u64;
            if profile_size % GIB != 0 || profile_size == 0 {
                continue;
            }
            let reservation =
                align_up(carve_out / max_instance as u64 + overhead, segment);
            if reservation >= profile_size {
                continue;
            }
            let fb_length = align_down(profile_size - reservation, segment);
            if fb_length == 0 {
                continue;
            }
            // The card has to be able to hand out what the catalogue
            // promises. This is the check vGPU does not need, because its
            // reserve accounting is exact and ours is a measurement.
            if fb_length * max_instance as u64 > usable {
                continue;
            }
            let gib = profile_size / GIB;
            profiles.push(Profile {
                name: format!("{board}-{gib}Q"),
                class: Class::Q,
                max_instance,
                profile_size,
                reservation,
                fb_length,
                segments: fb_length / segment,
            });
        }
        Catalogue { board: board.to_string(), total, usable, segment, overhead, profiles }
    }

    /// Find a profile by the part after the board name (`2Q`) or by its
    /// whole name (`RTX2070-2Q`), case-insensitively.
    pub fn find(&self, want: &str) -> Option<&Profile> {
        let w = want.trim().to_ascii_uppercase();
        self.profiles.iter().find(|p| {
            p.name.to_ascii_uppercase() == w
                || p.name.to_ascii_uppercase() == format!("{}-{}", self.board.to_ascii_uppercase(), w)
        })
    }

    /// The catalogue as a person would read it, with the arithmetic spelled
    /// out rather than left to be reconstructed.
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
        s.push_str("type          max  profile   reserved   guest FB   segments   all instances\n");
        for p in &self.profiles {
            s.push_str(&format!(
                "{:<13} {:>3} {:>8} {:>10} {:>10} {:>10} {:>13}\n",
                p.name,
                p.max_instance,
                mib(p.profile_size),
                mib(p.reservation),
                mib(p.fb_length),
                p.segments,
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

    /// The card this was written on, with the numbers it answered:
    /// 8192 MiB total, 7773 MiB heap (measured 2026-08-21 through
    /// FB_GET_INFO_V2 in a guest, and the same two indices natively).
    fn rtx2070(segment: u64) -> Catalogue {
        Catalogue::derive("RTX2070", 8192 * MIB, 7773 * MIB, segment, 256 * MIB)
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
                    p.fb_length + p.reservation <= p.profile_size,
                    "{} promises more than its profile",
                    p.name
                );
                assert!(
                    p.fb_length * p.max_instance as u64 <= c.usable,
                    "{} x{} does not fit the usable heap",
                    p.name,
                    p.max_instance
                );
            }
        }
    }

    /// The card's own carve-out is 419 MiB here, and it is the thing a
    /// per-VM knob cannot see: with one instance the reservation has to
    /// cover all of it, with eight it covers an eighth each.
    #[test]
    fn the_reservation_is_divided_among_the_instances() {
        let c = rtx2070(32 * MIB);
        let one = c.find("8Q").expect("one instance of the whole card");
        let many = c.find("1Q").expect("eight instances of one gigabyte");
        assert_eq!(one.max_instance, 1);
        assert_eq!(many.max_instance, 8);
        assert!(
            one.reservation > many.reservation,
            "one instance carries the whole carve-out ({} MiB) and eight share it ({} MiB)",
            one.reservation / MIB,
            many.reservation / MIB
        );
        // Neither is below this project's own measured per-VM overhead.
        assert!(many.reservation >= 256 * MIB);
    }

    #[test]
    fn a_profile_is_named_the_way_vgpu_names_one() {
        let c = rtx2070(32 * MIB);
        let p = c.find("2Q").expect("2Q");
        assert_eq!(p.name, "RTX2070-2Q");
        assert_eq!(p.guest_name(), "Leandro RTX2070-2Q");
        assert_eq!(p.profile_size, 2 * GIB);
        assert_eq!(p.max_instance, 4);
        // ... and it is findable by either spelling.
        assert_eq!(c.find("RTX2070-2Q"), c.find("2q"));
    }

    #[test]
    fn a_card_with_no_vmmu_yields_no_catalogue() {
        // RM leaves the segment size at zero when the chip has no VMMU
        // (gpu.c:940). That is not an error and it is not a catalogue.
        let c = Catalogue::derive("RTX2070", 8192 * MIB, 7773 * MIB, 0, 256 * MIB);
        assert!(c.profiles.is_empty());
        assert!(c.table().contains("none"));
    }

    /// A 1 GiB profile cannot survive a large segment: the reservation
    /// rounds up to the segment, and a 1 GiB row would have nothing left.
    #[test]
    fn a_segment_larger_than_the_slice_removes_the_row() {
        let big = rtx2070(1024 * MIB);
        assert!(big.find("1Q").is_none());
        assert!(big.find("8Q").is_some());
    }
}
