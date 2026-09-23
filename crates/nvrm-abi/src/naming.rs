// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The guest-visible identity of a Leandro virtual GPU, generated from one structured
//! specification (P17, docs/caraxes/briefing-naming.md).
//!
//! The fields are independent: the platform is always `Leandro`; the transport is how
//! the guest reaches the device (`VFIO` for the Caraxes vfio-user synthetic PCI GPU,
//! `VirtIO` for the Leandro v1 vhost-user path); the personality is the board the guest
//! is told it has (`RTX 2070`); the profile is the guest framebuffer size (`4G`,
//! `1333M`); the backend (`OpenRM`, `nova-core`) is an implementation detail and never
//! appears in the name.
//!
//! Two name formats exist. [`NameFormat::Legacy`], the default, is
//! `Leandro <Personality>-<Profile>`, the string both devices have always emitted and the
//! v1 gates compare (`Leandro RTX 2070-4G`). [`NameFormat::Transport`] is
//! `Leandro <Transport> <Personality>-<Profile>` and is opt-in (`LEA_GPU_NAME_FORMAT`)
//! until the gates accept it.
//!
//! The profile is always a size. A vGPU-style type such as `4Q` names what the VM costs
//! the card, not what the guest sees; [`crate::vgpu::Catalogue::resolve`] turns it into a
//! guest framebuffer and [`Profile::from_catalogue`] takes the size from there, so
//! `RTX2070-4Q` with 3072 MiB of guest FB is named `-3G`, as Leandro v1 always named it.

use std::fmt;
use std::str::FromStr;

use crate::vgpu;

/// The platform word every guest-visible name starts with.
pub const PLATFORM: &str = "Leandro";

/// The environment variable selecting the name format for either device.
pub const NAME_FORMAT_ENV: &str = "LEA_GPU_NAME_FORMAT";

/// What a name that fits nowhere else becomes.
const FALLBACK: &str = "Leandro GPU";

/// A value that is not one of the accepted spellings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// How the guest reaches the device.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Caraxes: the synthetic PCI GPU served over vfio-user (`vfio-user-nvgpu`).
    Vfio,
    /// Leandro v1: the virtio device served over vhost-user (`vhost-user-nvrm`).
    Virtio,
}

impl Transport {
    /// The word in the guest-visible name.
    pub fn word(self) -> &'static str {
        match self {
            Transport::Vfio => "VFIO",
            Transport::Virtio => "VirtIO",
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

impl FromStr for Transport {
    type Err = ParseError;
    /// `VFIO`, `vfio-user`, `caraxes` or `v2`; `VirtIO`, `vhost-user`, `virtio-nvrm` or
    /// `v1`; case is ignored.
    fn from_str(s: &str) -> Result<Self, ParseError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vfio" | "vfio-user" | "caraxes" | "v2" => Ok(Transport::Vfio),
            "virtio" | "vhost-user" | "virtio-nvrm" | "v1" => Ok(Transport::Virtio),
            _ => Err(ParseError(format!(
                "transport {s:?} is neither VFIO (vfio-user, Caraxes) nor VirtIO (vhost-user, Leandro v1)"
            ))),
        }
    }
}

/// The board the guest is told it has, as it appears in the name (`RTX 2070`, `A100`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Personality(String);

impl Personality {
    pub fn new(name: &str) -> Personality {
        Personality(name.trim().to_string())
    }

    /// The personality of a host card named by its driver (`NVIDIA GeForce RTX 2070`
    /// becomes `RTX 2070`): the vendor prefix gives way to the platform word, as in
    /// NVIDIA's own vGPU names.
    pub fn from_driver_name(real: &str) -> Personality {
        let real = real.trim();
        let real = real.strip_prefix("NVIDIA ").unwrap_or(real);
        Personality::new(real.strip_prefix("GeForce ").unwrap_or(real))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Personality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The guest framebuffer size the name carries, in whole MiB (never zero).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Profile {
    fb_mib: u64,
}

impl Profile {
    /// `None` for zero: no cap, no size in the name.
    pub fn from_mib(fb_mib: u64) -> Option<Profile> {
        (fb_mib > 0).then_some(Profile { fb_mib })
    }

    /// The whole MiB of `fb_bytes`; `None` below one MiB.
    pub fn from_fb_bytes(fb_bytes: u64) -> Option<Profile> {
        Profile::from_mib(fb_bytes >> 20)
    }

    /// The guest framebuffer of a catalogue type or size (`RTX2070-4Q`, `RTX2070-130M`).
    pub fn from_catalogue(p: &vgpu::Profile) -> Option<Profile> {
        Profile::from_fb_bytes(p.fb_length)
    }

    pub fn fb_mib(self) -> u64 {
        self.fb_mib
    }

    pub fn fb_bytes(self) -> u64 {
        self.fb_mib << 20
    }

    /// `<n>G` for whole GiB, `<n>M` otherwise.
    pub fn label(self) -> String {
        if self.fb_mib % 1024 == 0 {
            format!("{}G", self.fb_mib / 1024)
        } else {
            format!("{}M", self.fb_mib)
        }
    }

    /// Only the label form the name uses (`4G`, `1333M`), for reading names back.
    fn from_label(s: &str) -> Option<Profile> {
        let (n, unit) = s.split_at(s.len().checked_sub(1)?);
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let scale = match unit {
            "G" => 1024,
            "M" => 1,
            _ => return None,
        };
        Profile::from_mib(n.parse::<u64>().ok()?.checked_mul(scale)?)
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

impl FromStr for Profile {
    type Err = ParseError;
    /// A size: `4G`, `4GiB`, `1333M`, `1333MiB`, a bare MiB count, optionally with a
    /// board prefix (`RTX2070-130M`). A catalogue type (`4Q`) is refused: it names what
    /// the VM costs the card, and only the card's catalogue knows the guest size.
    fn from_str(s: &str) -> Result<Self, ParseError> {
        let t = s.trim();
        let size = match t.rsplit_once('-') {
            Some((board, size)) if !board.trim().is_empty() => size,
            Some(_) => {
                return Err(ParseError(format!(
                    "profile {s:?} names no board before '-'"
                )))
            }
            None => t,
        };
        if let Some(n) = size.strip_suffix(['Q', 'q']) {
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ParseError(format!(
                    "{s:?} is a vGPU-style type (what the VM costs the card), not a guest \
                     framebuffer size: resolve it with the card's catalogue \
                     (nvrm-client --bin vgpuprofile, vgpu::Catalogue::resolve) and name \
                     the size it gives"
                )));
            }
        }
        vgpu::parse_mib(size)
            .and_then(Profile::from_mib)
            .ok_or_else(|| {
                ParseError(format!(
                    "profile {s:?} is not a framebuffer size (4G, 4GiB, 1333M, 1333MiB or MiB)"
                ))
            })
    }
}

/// What executes the guest's work. Never part of the guest-visible name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Backend {
    /// The host's NVIDIA open kernel RM, as an unprivileged or privileged client.
    #[default]
    OpenRm,
    /// The nova-core driver.
    NovaCore,
    /// Anything else, by name.
    Other(String),
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::OpenRm => f.write_str("OpenRM"),
            Backend::NovaCore => f.write_str("nova-core"),
            Backend::Other(s) => f.write_str(s),
        }
    }
}

impl FromStr for Backend {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, ParseError> {
        let t = s.trim();
        match t.to_ascii_lowercase().as_str() {
            "" => Err(ParseError("empty backend name".to_string())),
            "openrm" | "open-rm" | "rm" => Ok(Backend::OpenRm),
            "nova" | "nova-core" | "novacore" => Ok(Backend::NovaCore),
            _ => Ok(Backend::Other(t.to_string())),
        }
    }
}

/// Which of the two name shapes a device emits.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum NameFormat {
    /// `Leandro <Personality>-<Profile>`: the names both devices have always emitted
    /// and the v1 gates compare. The default until the gates accept the transport word.
    #[default]
    Legacy,
    /// `Leandro <Transport> <Personality>-<Profile>`.
    Transport,
}

impl FromStr for NameFormat {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, ParseError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "legacy" | "short" => Ok(NameFormat::Legacy),
            "transport" | "full" => Ok(NameFormat::Transport),
            _ => Err(ParseError(format!(
                "name format {s:?} is neither legacy (Leandro RTX 2070-4G) nor transport \
                 (Leandro VFIO RTX 2070-4G)"
            ))),
        }
    }
}

impl NameFormat {
    /// [`NAME_FORMAT_ENV`], or the default when unset or empty.
    pub fn from_env() -> Result<NameFormat, ParseError> {
        std::env::var(NAME_FORMAT_ENV)
            .map_or(Ok(NameFormat::default()), |v| v.parse())
            .map_err(|e| ParseError(format!("{NAME_FORMAT_ENV}: {e}")))
    }
}

/// Everything the guest-visible identity is derived from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualGpuSpec {
    pub transport: Transport,
    pub personality: Personality,
    /// `None`: no size in the name (Leandro v1 without a cap).
    pub profile: Option<Profile>,
    pub backend: Backend,
}

impl VirtualGpuSpec {
    /// The guest-visible name, unbounded.
    pub fn guest_name(&self, format: NameFormat) -> String {
        let mut s = self.head(format);
        if let Some(p) = self.profile {
            s.push('-');
            s.push_str(&p.label());
        }
        s
    }

    /// The name for a NUL-terminated field of `max` bytes (`name_max`, 64 for
    /// `NV2080_GPU_MAX_NAME_STRING_LENGTH`): without the profile when the whole does
    /// not fit, since a truncated size would be a wrong size; [`FALLBACK`] when not
    /// even that fits.
    pub fn guest_name_within(&self, format: NameFormat, max: usize) -> String {
        let full = self.guest_name(format);
        if full.len() < max {
            return full;
        }
        let head = self.head(format);
        if head.len() < max {
            return head;
        }
        FALLBACK.to_string()
    }

    fn head(&self, format: NameFormat) -> String {
        match format {
            NameFormat::Legacy => format!("{PLATFORM} {}", self.personality),
            NameFormat::Transport => {
                format!("{PLATFORM} {} {}", self.transport, self.personality)
            }
        }
    }
}

/// The fields a guest-visible name carries (the backend is never among them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedName {
    /// `None` for a legacy-format name.
    pub transport: Option<Transport>,
    pub personality: Personality,
    pub profile: Option<Profile>,
}

/// Read a guest-visible name of either format back into its fields
/// (`Leandro RTX 2070-4G`, `Leandro VFIO A100-16G`, `Leandro RTX 2070`).
pub fn parse_name(name: &str) -> Result<ParsedName, ParseError> {
    let rest = name
        .trim()
        .strip_prefix(PLATFORM)
        .and_then(|r| r.strip_prefix(' '))
        .ok_or_else(|| ParseError(format!("{name:?} does not start with \"{PLATFORM} \"")))?;
    let (transport, rest) = match rest.split_once(' ') {
        Some((w, r)) if w == Transport::Vfio.word() => (Some(Transport::Vfio), r),
        Some((w, r)) if w == Transport::Virtio.word() => (Some(Transport::Virtio), r),
        _ => (None, rest),
    };
    let (personality, profile) = match rest.rsplit_once('-') {
        Some((p, size)) => match Profile::from_label(size) {
            Some(size) => (p, Some(size)),
            None => (rest, None),
        },
        None => (rest, None),
    };
    if personality.trim().is_empty() {
        return Err(ParseError(format!("{name:?} names no board")));
    }
    Ok(ParsedName {
        transport,
        personality: Personality::new(personality),
        profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(t: Transport, board: &str, mib: u64) -> VirtualGpuSpec {
        VirtualGpuSpec {
            transport: t,
            personality: Personality::new(board),
            profile: Profile::from_mib(mib),
            backend: Backend::OpenRm,
        }
    }

    /// The brief's three examples.
    #[test]
    fn the_transport_format_names_all_four_fields_but_the_backend() {
        use NameFormat::Transport as T;
        assert_eq!(
            spec(Transport::Vfio, "RTX 2070", 4096).guest_name(T),
            "Leandro VFIO RTX 2070-4G"
        );
        assert_eq!(
            spec(Transport::Virtio, "RTX 2070", 4096).guest_name(T),
            "Leandro VirtIO RTX 2070-4G"
        );
        let mut a100 = spec(Transport::Vfio, "A100", 16384);
        a100.backend = Backend::NovaCore;
        assert_eq!(a100.guest_name(T), "Leandro VFIO A100-16G");
    }

    /// The compatibility default: exactly the string the gates compare, on both
    /// transports and whatever the backend.
    #[test]
    fn the_default_format_is_the_legacy_name() {
        assert_eq!(NameFormat::default(), NameFormat::Legacy);
        for t in [Transport::Vfio, Transport::Virtio] {
            for b in [
                Backend::OpenRm,
                Backend::NovaCore,
                Backend::Other("x".into()),
            ] {
                let mut s = spec(t, "RTX 2070", 4096);
                s.backend = b;
                assert_eq!(s.guest_name(NameFormat::default()), "Leandro RTX 2070-4G");
            }
        }
        assert_eq!(
            spec(Transport::Virtio, "RTX 2070", 0).guest_name(NameFormat::Legacy),
            "Leandro RTX 2070"
        );
    }

    /// The v1 profiles: a `4Q` type is named by its guest framebuffer, a `1333MiB`
    /// size by itself.
    #[test]
    fn v1_profiles_keep_their_names() {
        let cat = vgpu::Catalogue::derive("RTX2070", 8192 << 20, 7773 << 20, 256 << 20, 256 << 20);
        let q = cat.resolve("4Q").expect("4Q");
        let p = Profile::from_catalogue(&q).unwrap();
        assert_eq!(p.fb_bytes(), q.fb_length);
        assert_eq!(p.label(), "3584M");
        assert_eq!("1333MiB".parse::<Profile>().unwrap().label(), "1333M");
        assert_eq!("1333".parse::<Profile>().unwrap().label(), "1333M");
        let e = "4Q".parse::<Profile>().unwrap_err();
        assert!(e.0.contains("catalogue"), "{e}");
        assert!("RTX2070-4Q".parse::<Profile>().is_err());
    }

    #[test]
    fn profiles_parse_every_existing_spelling() {
        for (s, mib) in [
            ("4G", 4096),
            ("4GiB", 4096),
            ("4g", 4096),
            ("4096", 4096),
            ("130M", 130),
            ("130MiB", 130),
            ("RTX2070-130M", 130),
            (" 2816 ", 2816),
        ] {
            assert_eq!(s.parse::<Profile>().unwrap().fb_mib(), mib, "{s}");
        }
        for bad in ["", "0", "4GB", "four", "-4G"] {
            assert!(bad.parse::<Profile>().is_err(), "{bad:?}");
        }
        assert_eq!(Profile::from_mib(1024).unwrap().label(), "1G");
        assert_eq!(Profile::from_mib(512).unwrap().label(), "512M");
        assert_eq!(
            Profile::from_fb_bytes((1536 << 20) + 5).unwrap().label(),
            "1536M"
        );
        assert_eq!(Profile::from_fb_bytes(1 << 19), None);
    }

    #[test]
    fn transports_backends_and_formats_parse() {
        for s in ["VFIO", "vfio-user", "Caraxes", "v2"] {
            assert_eq!(s.parse::<Transport>().unwrap(), Transport::Vfio);
        }
        for s in ["VirtIO", "vhost-user", "virtio-nvrm", "V1"] {
            assert_eq!(s.parse::<Transport>().unwrap(), Transport::Virtio);
        }
        assert!("pci".parse::<Transport>().is_err());
        assert_eq!("OpenRM".parse::<Backend>().unwrap(), Backend::OpenRm);
        assert_eq!("nova-core".parse::<Backend>().unwrap(), Backend::NovaCore);
        assert_eq!(
            "mock".parse::<Backend>().unwrap(),
            Backend::Other("mock".into())
        );
        assert_eq!("".parse::<NameFormat>().unwrap(), NameFormat::Legacy);
        assert_eq!(
            "Transport".parse::<NameFormat>().unwrap(),
            NameFormat::Transport
        );
        assert!("long".parse::<NameFormat>().is_err());
    }

    #[test]
    fn the_personality_drops_the_vendor_prefixes() {
        assert_eq!(
            Personality::from_driver_name("NVIDIA GeForce RTX 2070").as_str(),
            "RTX 2070"
        );
        assert_eq!(
            Personality::from_driver_name(" NVIDIA A100-SXM4-40GB ").as_str(),
            "A100-SXM4-40GB"
        );
        assert_eq!(
            Personality::from_driver_name("Tesla T4").as_str(),
            "Tesla T4"
        );
    }

    /// v1's fallbacks at the 64-byte field, in both formats.
    #[test]
    fn a_name_that_does_not_fit_loses_the_profile_whole() {
        let max = 64;
        let s = spec(Transport::Vfio, &"X".repeat(53), 2048);
        assert_eq!(
            s.guest_name_within(NameFormat::Legacy, max),
            format!("Leandro {}", "X".repeat(53))
        );
        let s = spec(Transport::Vfio, &"X".repeat(56), 2048);
        assert_eq!(s.guest_name_within(NameFormat::Legacy, max), "Leandro GPU");
        let s = spec(Transport::Vfio, &"X".repeat(48), 2048);
        assert_eq!(
            s.guest_name_within(NameFormat::Transport, max),
            format!("Leandro VFIO {}", "X".repeat(48))
        );
        let s = spec(Transport::Vfio, "RTX 2070", 4096);
        assert_eq!(
            s.guest_name_within(NameFormat::Transport, max),
            "Leandro VFIO RTX 2070-4G"
        );
    }

    #[test]
    fn names_read_back_into_their_fields() {
        for t in [Transport::Vfio, Transport::Virtio] {
            for (board, mib) in [
                ("RTX 2070", 4096),
                ("A100-SXM4-40GB", 10240),
                ("RTX 2070", 1333),
                ("RTX 2070", 0),
            ] {
                let s = spec(t, board, mib);
                for f in [NameFormat::Legacy, NameFormat::Transport] {
                    let p = parse_name(&s.guest_name(f)).unwrap();
                    assert_eq!(p.personality, s.personality);
                    assert_eq!(p.profile, s.profile);
                    assert_eq!(p.transport, (f == NameFormat::Transport).then_some(t));
                }
            }
        }
        assert!(parse_name("NVIDIA GeForce RTX 2070").is_err());
        assert!(parse_name("Leandro ").is_err());
        let p = parse_name("Leandro A100-SXM4-40GB").unwrap();
        assert_eq!(
            (p.personality.as_str(), p.profile),
            ("A100-SXM4-40GB", None)
        );
    }
}
