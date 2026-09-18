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
//! WHY THIS EXISTS (docs/OPEN-QUESTIONS.md 68 and 69): a VM's cost to the
//! card is more than the framebuffer its guest is told, and the difference
//! belongs to the CARD -- its own carve-out, what the host holds, the
//! per-VM overhead this project measured. ONE RULE turns a guest
//! framebuffer into what the VM costs, [`Catalogue::profile_for`], and
//! every way of naming a VM's size goes through it: a catalogue type
//! (`RTX2070-4Q`) only picks the framebuffer, a size (`3G`, `130M`) is
//! the framebuffer. Two VMs told the same size cost the same, whatever
//! named them.
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
//! branch above it (:3757-3783) is where the OTHER rule comes from:
//!
//! ```text
//!   vgpuReservedFb = ALIGN_UP(totalReservedFb / maxInstance, segment)
//! ```
//!
//! **The reserve is DIVIDED among the instances, not charged to each** --
//! here, carried in proportion to what each VM uses (number 69(b)).
//!
//! WHAT CANNOT BE COPIED, and is therefore ours and marked as ours:
//!
//!   * `fbReservation` and `gspHeapSize` are per-profile fields of a
//!     catalogue that lives in the closed vGPU host driver;
//!     `memmgrGetVgpuHostRmReservedFb_KERNEL` (mem_mgr.c:3984) asks the GSP
//!     for the number and the GSP is firmware. Ours is built from two
//!     measurements instead -- see [`Catalogue::profile_for`].
//!   * The profile NAMES and their class letters (`Q`, `C`, `B`, `A`) come
//!     from that same closed catalogue. The SHAPE is public --
//!     `<board>-<gigabytes><class>` -- and this file follows it, with `Q`
//!     for guests that do graphics and CUDA at once and no licence behind
//!     the letter. `Leandro` stands where NVIDIA writes `GRID` or `NVIDIA`,
//!     because a mediated card must never be mistakable for a vendor one.
//!   * There is no GSP plugin per VM here and no per-VM GSP heap, so
//!     `gspHeapSize` is zero and says so.
//!   * Nothing is PLACED here, so a size named by a person need not be a
//!     whole number of segments; only the catalogue's own types are.

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

/// One VM's profile: what it costs the card and what its guest is told.
/// Made by [`Catalogue::profile_for`] and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// What named it: vGPU's `vgpuName` for a type (`RTX2070-2Q`), the size
    /// for a size (`RTX2070-130M`). A label for logs and admission; the
    /// guest's card name follows `fb_length` (vram.rs, `guest_card_name`).
    pub name: String,
    /// vGPU's `maxInstance`: how many VMs of this size fit on the card.
    pub max_instance: u32,
    /// vGPU's `profileSize`: what one VM costs the card.
    pub profile_size: u64,
    /// vGPU's `fbReservation`, `profile_size - fb_length`.
    pub reservation: u64,
    /// vGPU's `fbLength`: what the guest is told and refused at.
    pub fb_length: u64,
    /// vGPU's `guestVmmuCount`, for the operator's eye.
    pub segments: u64,
    /// vGPU's `encoderCapacity`: [`encoder_share`].
    pub encoder_capacity: u32,
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
    /// `..._HEAP_SIZE`, bytes, minus what the host holds: what guests can
    /// actually be given. The gap to `total` is the carve-out every VM
    /// carries a share of -- it is not optional and it is not ours to spend.
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

/// A size as a person writes one, in MiB: `3G`, `3GiB`, `130M`, `130MiB`,
/// or a bare number of MiB. Binary units and whole MiB only: `3GB` is
/// refused rather than guessed at, because MiB-or-MB is exactly the
/// confusion a unit in a name exists to prevent.
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

/// A VM's share of NVENC, in the percent
/// `NV2080_CTRL_CMD_GPU_GET_ENCODER_CAPACITY` answers in (a bare-metal card
/// says 100, subdevice_ctrl_gpu_kernel.c:1000).
///
/// PROPORTIONAL TO THE GUEST FRAMEBUFFER against the card's total, and to
/// nothing that moves: the backend computes this itself under every
/// policy, and the card's total is a constant while the usable heap
/// follows the host's desktop (number 71). The shares of VMs that fit the
/// card sum to at most 100. At least 1, because 0 is "no policy" to the
/// rewrite and would hand a 130 MiB VM the whole encoder.
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
    /// Derive the catalogue from what the card answered.
    ///
    /// `total` is `TOTAL_RAM_SIZE`, `usable` the heap minus what the host
    /// holds, both in bytes; `segment` is the card's VMMU segment size,
    /// `overhead` the per-VM host-side cost this project measured.
    ///
    /// THE INSTANCE COUNTS are powers of two, which is not an arbitrary
    /// simplification: vGPU's placement arithmetic recursively halves the
    /// placement region (`_kvgpumgrSetHeterogeneousResources`,
    /// kernel_vgpu_mgr.c:3020 ff.), and the profile sizes NVIDIA publishes
    /// for a board are its FB divided by 1, 2, 4, 8 ... A count whose
    /// share is not a whole gigabyte is dropped, because vGPU names its
    /// profiles in gigabytes and a `RTX2070-0Q` would be a nonsense name.
    ///
    /// A TYPE ONLY PICKS THE FRAMEBUFFER: its share of the card, less that
    /// share of the carve-out and the per-VM overhead, rounded to whole
    /// segments the way vGPU rounds. What the VM then costs is
    /// [`Catalogue::profile_for`]'s, like any other size. The carve-out
    /// share is rounded UP, so the rule never costs a type more than its
    /// share of the card: every set of types that fitted still fits.
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
            // The card has to be able to hand out what the catalogue
            // promises. This is the check vGPU does not need, because its
            // reserve accounting is exact and ours is a measurement.
            if fb == 0 || fb * count > usable {
                continue;
            }
            let row = cat.profile_for(format!("{board}-{}Q", share / GIB), fb);
            cat.profiles.extend(row);
        }
        cat
    }

    /// What the catalogue partitions: the card's total aligned up to eight
    /// VMMU segments, which is vGPU's `totalAvailableFb`
    /// (kernel_vgpu_mgr.c:3797). Profile sizes are measured against this.
    pub fn available(&self) -> u64 {
        if self.segment == 0 {
            self.total
        } else {
            align_up(self.total, 8 * self.segment)
        }
    }

    /// THE RULE: what a VM whose guest is told `fb_length` costs this card.
    ///
    /// ```text
    ///   profileSize  = (fb_length + overhead) * available / (available - carve_out), up to a MiB
    ///   fbReservation = profileSize - fb_length
    ///   maxInstance  = available / profileSize
    ///   encoder      = encoder_share(fb_length, total)
    /// ```
    ///
    /// The VM pays its framebuffer and the measured overhead, and carries
    /// the carve-out -- the card's own and what the host holds -- in
    /// proportion to them. So [`Catalogue::admits`]' sum of profile sizes
    /// against the card says: the framebuffers and overheads of the VMs fit
    /// what the card leaves usable. Rounded up, never down.
    ///
    /// `None` if the VM does not fit the card at all.
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

    /// May a VM of `want` start beside these already-live profile sizes?
    ///
    /// **The rule is `sum(profile_size) <= available`,** and the tempting
    /// wrong one is `<= usable`: a profile size already CONTAINS that VM's
    /// share of the carve-out, so charging the carve-out again by measuring
    /// against the heap refuses configurations that were measured running
    /// (number 69: eight 1Q guests, 3 GiB still free).
    ///
    /// vGPU needs more than arithmetic here because its framebuffers are
    /// PLACED -- fixed placement ids in the VMMU region, a recursive
    /// halving to find room, and a deny-list of combinations that overlap
    /// (`_kvgpumgrSetHeterogeneousResources`, `_kvgpumgrIsPlacementValid`,
    /// kernel_vgpu_mgr.c). Nothing is placed on this side; the sum is the
    /// only constraint there is.
    pub fn admits(&self, live: &[u64], want: u64) -> bool {
        live.iter().copied().sum::<u64>() + want <= self.available()
    }

    /// A profile by what a person calls it: a type (`2Q`, `RTX2070-2Q`) or a
    /// guest framebuffer size (`3G`, `130MiB`, `RTX2070-130M`),
    /// case-insensitively. A type only supplies the size; both go through
    /// [`Catalogue::profile_for`].
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
        let size = if mib % 1024 == 0 {
            format!("{}G", mib / 1024)
        } else {
            format!("{mib}M")
        };
        self.profile_for(format!("{}-{size}", self.board), mib * MIB)
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

    /// The card this was written on, with the numbers it answered:
    /// 8192 MiB total, 7773 MiB heap (measured 2026-08-21 through
    /// FB_GET_INFO_V2 in a guest, and the same two indices natively).
    fn rtx2070(segment: u64) -> Catalogue {
        Catalogue::derive("RTX2070", 8192 * MIB, 7773 * MIB, segment, 256 * MIB)
    }

    /// The card as the launcher actually sees it: the heap MINUS what the
    /// host desktop is holding, which is what `vgpuprofile` passes in and
    /// is where the published 2Q = 1280 MiB comes from.
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

    /// The rule, with the numbers of the card as the launcher sees it.
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
        // More than the card leaves usable, overhead included, is no profile.
        assert!(cat.resolve("6615M").is_some());
        assert!(cat.resolve("6616M").is_none());
    }

    /// **A type is one way to name a framebuffer, and nothing else.** For
    /// every row of every catalogue, naming the row's framebuffer as a size
    /// gives the same profile -- cost, reservation, instances, encoder --
    /// with only the label telling them apart.
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

    /// **Every set of types that fitted still fits.** A type's cost is at
    /// most its share of the card, so its instance count is at least what
    /// the share promises, and that many are admitted. The three host
    /// states are ones where rounding the carve-out share DOWN made the
    /// rule cost a type 1-2 MiB more than its share.
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

    /// The invariants of the rule for any size a person may name.
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
        // what the three of them actually ask the heap for, which is the
        // number the run has to survive
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
        // Neither is below this project's own measured per-VM overhead.
        assert!(many.reservation >= 256 * MIB);
    }

    #[test]
    fn a_profile_is_named_the_way_vgpu_names_one() {
        let c = rtx2070(32 * MIB);
        let p = c.resolve("2Q").expect("2Q");
        assert_eq!(p.name, "RTX2070-2Q");
        assert!(p.profile_size <= 2 * GIB);
        assert_eq!(p.max_instance, 4);
        // ... and it is findable by either spelling.
        assert_eq!(c.resolve("RTX2070-2Q"), c.resolve("2q"));
        assert_eq!(c.resolve("rtx2070-130m"), c.resolve("130 MiB"));
    }

    /// Proportional to the framebuffer, a whole card is a whole encoder,
    /// and no VM is told it has none.
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
    fn a_card_with_no_vmmu_yields_no_catalogue() {
        // RM leaves the segment size at zero when the chip has no VMMU
        // (gpu.c:940). That is not an error and it is not a catalogue.
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
