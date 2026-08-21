// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! What would NVIDIA's vGPU carve this card into, if we followed its own
//! arithmetic?
//!
//! This is the READING half of the vGPU-shaped VRAM profile
//! (docs/OPEN-QUESTIONS.md number 69). It asks the card the two numbers
//! that arithmetic needs and then prints the catalogue that follows from
//! them. Nothing here is a table anybody typed: the segment size comes out
//! of the card, the total comes out of the card, and the profile sizes are
//! derived.
//!
//! Two controls, both non-privileged:
//!
//!   * `NV2080_CTRL_CMD_GPU_GET_VMMU_SEGMENT_SIZE` (0x2080017e,
//!     ctrl2080gpu.h:3135). The VMMU is the second-level address translation
//!     unit that gives a vGPU its own view of framebuffer; its SEGMENT is
//!     the granule a guest framebuffer is cut in. `flags = 0x10448` in
//!     `g_subdevice_nvoc.c` = GSP_PLUGIN_FOR_VGPU_GSP | CACHEABLE |
//!     ROUTE_TO_PHYSICAL | NON_PRIVILEGED -- so an ordinary client may ask,
//!     and the answer comes from the GSP.
//!   * `NV2080_CTRL_CMD_FB_GET_INFO_V2` (0x20801303) for TOTAL_RAM_SIZE and
//!     HEAP_SIZE, which is what `Ram.fbTotalMemSizeMb` is in the vGPU
//!     arithmetic.
//!
//!   cargo run --release -p nvrm-client --bin vgpuprofile
//!   vgpuprofile --select 2Q      one row, as shell key=value pairs
//!
//! `--select` is what makes this the MANAGER's half of the split. vGPU's
//! host RM owns the catalogue and hands a per-VM plugin its slice; here
//! the backend holds no RM client (main.rs) and cannot read the card at
//! all, so the script that starts a VM resolves the type name through this
//! tool and passes the numbers on. Same division of labour, different
//! reason for it.

use nvrm_abi::vgpu;
use nvrm_abi::{sys, NvDevice};
use nvrm_client::RmClient;

/// `NV2080_CTRL_CMD_GPU_GET_VMMU_SEGMENT_SIZE` (ctrl2080gpu.h:3135).
const CMD_GPU_GET_VMMU_SEGMENT_SIZE: u32 = 0x2080_017e;
/// `NV2080_CTRL_CMD_FB_GET_INFO_V2` (ctrl2080fb.h:489).
const CMD_FB_GET_INFO_V2: u32 = 0x2080_1303;
/// `NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE` (ctrl2080fb.h), in kilobytes.
const FB_INFO_INDEX_HEAP_FREE: u32 = 0x16;

/// `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` (ctrl2080gpu.h:325).
const CMD_GPU_GET_NAME_STRING: u32 = 0x2080_0110;

/// `NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS`: flags @0, then the 64-byte
/// ASCII union (ctrl2080gpu.h:338).
#[repr(C)]
#[derive(Copy, Clone)]
struct NameParams {
    flags: u32,
    ascii: [u8; 64],
}

impl Default for NameParams {
    fn default() -> Self {
        // flags 0 = NV2080_CTRL_GPU_GET_NAME_STRING_FLAGS_TYPE_ASCII.
        Self { flags: 0, ascii: [0; 64] }
    }
}

/// `NV2080_CTRL_FB_INFO { NvU32 index; NvU32 data; }`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct FbInfo {
    index: u32,
    data: u32,
}

/// `NV2080_CTRL_FB_GET_INFO_V2_PARAMS`: count @0, then the list. The real
/// struct carries 128 entries; asking for fewer is legal (the count says
/// how many are read) but the BUFFER has to be the full one, because RM
/// reads `paramsSize` against the class's own size.
#[repr(C)]
#[derive(Copy, Clone)]
struct FbInfoParams {
    count: u32,
    list: [FbInfo; 128],
}

impl Default for FbInfoParams {
    fn default() -> Self {
        Self { count: 0, list: [FbInfo::default(); 128] }
    }
}

fn main() {
    sys::assert_driver_version();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let select = match args.as_slice() {
        [] => None,
        [flag, want] if flag == "--select" => Some(want.clone()),
        _ => {
            eprintln!("usage: vgpuprofile [--select TYPE]");
            std::process::exit(2);
        }
    };
    // Under --select the two lines above are noise on stdout, which a
    // shell is about to eval. Everything informational goes to stderr.
    let quiet = select.is_some();
    macro_rules! say {
        ($($a:tt)*) => { if quiet { eprintln!($($a)*) } else { println!($($a)*) } };
    }

    let mut rm = RmClient::new().expect("NV01_ROOT_CLIENT");
    let root = rm.root();
    // The per-GPU node must be open or the card is not attached to this
    // client and NV01_DEVICE_0 fails with INSUFFICIENT_PERMISSIONS.
    let _gpu = NvDevice::open_gpu(0).expect("/dev/nvidia0");

    let device = rm.next_handle();
    let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    rm.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp)).expect("NV01_DEVICE_0");

    let subdevice = rm.next_handle();
    let mut sp = sys::NV2080_ALLOC_PARAMETERS::default();
    sp.subDeviceId = 0;
    rm.alloc(device, subdevice, sys::NV20_SUBDEVICE_0, Some(&mut sp)).expect("NV20_SUBDEVICE_0");

    // ---- the card's VMMU segment size ----------------------------------
    let mut seg = sys::NV2080_CTRL_GPU_GET_VMMU_SEGMENT_SIZE_PARAMS::default();
    let segment = match rm.control(subdevice, CMD_GPU_GET_VMMU_SEGMENT_SIZE, &mut seg) {
        Ok(()) => {
            say!(
                "vmmu segment size: {} bytes = {} MiB (asked the card)",
                seg.vmmuSegmentSize,
                seg.vmmuSegmentSize >> 20
            );
            seg.vmmuSegmentSize
        }
        Err(e) => {
            // Zero is RM's own way of saying "no VMMU here" (gpu.c:940:
            // NOT_SUPPORTED leaves the field at zero). Report it and stop
            // rather than substituting a number from another card.
            eprintln!("vgpuprofile: vmmu segment size UNAVAILABLE -- {e}");
            eprintln!("  RM leaves this at zero when the chip has no VMMU (gpu.c:940).");
            eprintln!("  Without it there is no vGPU-shaped quantisation to derive.");
            std::process::exit(1);
        }
    };

    // ---- and the total it partitions -----------------------------------
    let mut fb = FbInfoParams::default();
    fb.count = 3;
    fb.list[0].index = vgpu::FB_INFO_INDEX_TOTAL_RAM_SIZE;
    fb.list[1].index = vgpu::FB_INFO_INDEX_HEAP_SIZE;
    fb.list[2].index = FB_INFO_INDEX_HEAP_FREE;
    rm.control(subdevice, CMD_FB_GET_INFO_V2, &mut fb).expect("FB_GET_INFO_V2");
    let total_kb = fb.list[0].data as u64;
    let heap_kb = fb.list[1].data as u64;
    let free_kb = fb.list[2].data as u64;
    say!(
        "fb total: {} MiB (TOTAL_RAM_SIZE), heap {} MiB (HEAP_SIZE), free now {} MiB",
        total_kb / 1024,
        heap_kb / 1024,
        free_kb / 1024
    );

    // THE HOST IS A TENANT TOO, and vGPU never has to think about it: a
    // card running vGPU profiles runs nothing else, while this one is
    // driving the machine's own desktop. Whatever is in use right now is
    // not available to guests, and a catalogue derived from the whole heap
    // would hand out memory that is already spoken for. Measured at this
    // instant rather than assumed, and overridable for a host that will be
    // idle later (or busier).
    let in_use_kb = heap_kb.saturating_sub(free_kb);
    let host_reserve = match std::env::var("LEA_VGPU_HOST_RESERVE_MIB") {
        Ok(v) if !v.trim().is_empty() => match v.trim().parse::<u64>() {
            Ok(m) => {
                say!("host reserve: {m} MiB (LEA_VGPU_HOST_RESERVE_MIB)");
                m << 20
            }
            Err(_) => {
                eprintln!("vgpuprofile: LEA_VGPU_HOST_RESERVE_MIB={v:?} unusable -- measuring instead");
                in_use_kb * 1024
            }
        },
        _ => {
            say!(
                "host reserve: {} MiB, which is what the HOST is holding right now \
                 (heap minus free). vGPU never has to allow for this -- its cards \
                 run nothing but guests.",
                in_use_kb / 1024
            );
            in_use_kb * 1024
        }
    };
    let usable = (heap_kb * 1024).saturating_sub(host_reserve);

    // ---- and the board's own name --------------------------------------
    let mut np = NameParams::default();
    rm.control(subdevice, CMD_GPU_GET_NAME_STRING, &mut np).expect("GPU_GET_NAME_STRING");
    let name = np.ascii.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect::<String>();
    say!("board: {name:?}");

    // ---- what vGPU's arithmetic makes of that --------------------------
    // The per-VM overhead this project measured (number 68), overridable
    // so a different workload's measurement can be tried against the same
    // card without editing anything.
    let overhead = std::env::var("LEA_VGPU_OVERHEAD_MIB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(256)
        << 20;
    let board = vgpu::board_name(&name);
    let cat = vgpu::Catalogue::derive(&board, total_kb * 1024, usable, segment, overhead);

    if let Some(want) = select {
        match cat.find(&want) {
            Some(p) => {
                // key=value, so a shell can `eval` it. The unit is in every
                // name, because a bare number in an env var is how a MiB
                // becomes a MB two scripts later.
                println!("vgpu_type={}", p.name);
                println!("vgpu_profile_mib={}", p.profile_size >> 20);
                println!("vgpu_fb_mib={}", p.fb_length >> 20);
                println!("vgpu_max_instance={}", p.max_instance);
                println!("vgpu_segments={}", p.segments);
                println!("vgpu_segment_mib={}", cat.segment >> 20);
                println!("vgpu_encoder_cap={}", p.encoder_capacity);
                return;
            }
            None => {
                eprintln!("vgpuprofile: no type {want:?} on this card. It offers:");
                eprintln!("{}", cat.table());
                std::process::exit(1);
            }
        }
    }
    println!();
    println!("{}", cat.table());
}
