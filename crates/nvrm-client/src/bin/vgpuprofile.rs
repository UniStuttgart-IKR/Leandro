// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Derive vGPU-style VRAM profiles from the GPU's VMMU segment size and RAM size.
//!
//! Uses non-privileged GET_VMMU_SEGMENT_SIZE and FB_GET_INFO_V2 controls.
//! `--select` emits shell assignments consumed by VM launch scripts.
//! See docs/OPEN-QUESTIONS.md, item 69.
//!
//! ```text
//! vgpuprofile                 # profile catalogue
//! vgpuprofile --select 2Q     # named profile
//! vgpuprofile --select 130M   # size using the same allocation rule
//! ```

use nvrm_abi::{mediate, vgpu};
use nvrm_abi::{sys, NvDevice};
use nvrm_client::RmClient;

/// `NV2080_CTRL_CMD_GPU_GET_VMMU_SEGMENT_SIZE` (ctrl2080gpu.h:3135).
const CMD_GPU_GET_VMMU_SEGMENT_SIZE: u32 = 0x2080_017e;

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
        Self {
            flags: 0,
            ascii: [0; 64],
        }
    }
}

const _: () = {
    assert!(size_of::<NameParams>() == 68);
    assert!(std::mem::offset_of!(NameParams, ascii) == 4);
};

/// Profile properties as shell assignments with explicit units.
fn select_block(cat: &vgpu::Catalogue, p: &vgpu::Profile) -> String {
    [
        format!("vgpu_type={}", p.name),
        format!("vgpu_profile_mib={}", p.profile_size >> 20),
        format!("vgpu_fb_mib={}", p.fb_length >> 20),
        format!("vgpu_max_instance={}", p.max_instance),
        format!("vgpu_segments={}", p.segments),
        format!("vgpu_segment_mib={}", cat.segment >> 20),
        format!("vgpu_encoder_cap={}", p.encoder_capacity),
        // What admission is measured against: the card, not the heap. See
        // `Catalogue::admits`.
        format!("vgpu_available_mib={}", cat.available() >> 20),
    ]
    .join("\n")
}

fn main() {
    sys::assert_driver_version();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let select = match args.as_slice() {
        [] => None,
        [flag, want] if flag == "--select" => Some(want.clone()),
        _ => {
            eprintln!("usage: vgpuprofile [--select TYPE|SIZE]");
            std::process::exit(2);
        }
    };
    // Reserve stdout for shell assignments when selecting one profile.
    let quiet = select.is_some();
    macro_rules! say {
        ($($a:tt)*) => { if quiet { eprintln!($($a)*) } else { println!($($a)*) } };
    }

    let mut rm = RmClient::new().expect("NV01_ROOT_CLIENT");
    let root = rm.root();
    // RM device allocation requires the GPU node to remain open.
    let _gpu = NvDevice::open_gpu(0).expect("/dev/nvidia0");

    let device = rm.next_handle();
    let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    // SAFETY: NV0080_ALLOC_PARAMETERS matches NV01_DEVICE_0; no embedded buffers.
    unsafe { rm.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp)) }.expect("NV01_DEVICE_0");

    let subdevice = rm.next_handle();
    let mut sp = sys::NV2080_ALLOC_PARAMETERS::default();
    sp.subDeviceId = 0;
    // SAFETY: NV2080_ALLOC_PARAMETERS matches NV20_SUBDEVICE_0; no embedded buffers.
    unsafe { rm.alloc(device, subdevice, sys::NV20_SUBDEVICE_0, Some(&mut sp)) }
        .expect("NV20_SUBDEVICE_0");

    // the card's VMMU segment size
    let mut seg = sys::NV2080_CTRL_GPU_GET_VMMU_SEGMENT_SIZE_PARAMS::default();
    // SAFETY: The generated parameter type matches this command and has no nested pointers.
    let segment = match unsafe { rm.control(subdevice, CMD_GPU_GET_VMMU_SEGMENT_SIZE, &mut seg) } {
        Ok(()) => {
            say!(
                "vmmu segment size: {} bytes = {} MiB (asked the card)",
                seg.vmmuSegmentSize,
                seg.vmmuSegmentSize >> 20
            );
            seg.vmmuSegmentSize
        }
        Err(e) => {
            // Unsupported VMMU queries leave size zero (gpu.c); no profile can be derived.
            eprintln!("vgpuprofile: vmmu segment size UNAVAILABLE -- {e}");
            eprintln!("  RM leaves this at zero when the chip has no VMMU (gpu.c:940).");
            eprintln!("  Without it there is no vGPU-shaped quantisation to derive.");
            std::process::exit(1);
        }
    };

    // and the total it partitions
    let mut fb = sys::NV2080_CTRL_FB_GET_INFO_V2_PARAMS::default();
    fb.fbInfoListSize = 3;
    fb.fbInfoList[0].index = vgpu::FB_INFO_INDEX_TOTAL_RAM_SIZE;
    fb.fbInfoList[1].index = vgpu::FB_INFO_INDEX_HEAP_SIZE;
    fb.fbInfoList[2].index = mediate::FB_INFO_INDEX_HEAP_FREE;
    // SAFETY: The generated parameter type matches this command and has no nested pointers.
    unsafe { rm.control(subdevice, mediate::CMD_FB_GET_INFO_V2, &mut fb) }.expect("FB_GET_INFO_V2");
    let total_kb = fb.fbInfoList[0].data as u64;
    let heap_kb = fb.fbInfoList[1].data as u64;
    let free_kb = fb.fbInfoList[2].data as u64;
    say!(
        "fb total: {} MiB (TOTAL_RAM_SIZE), heap {} MiB (HEAP_SIZE), free now {} MiB",
        total_kb / 1024,
        heap_kb / 1024,
        free_kb / 1024
    );

    // Reserve currently used host memory unless the operator supplies a value.
    let in_use_kb = heap_kb.saturating_sub(free_kb);
    let host_reserve = match std::env::var("LEA_VGPU_HOST_RESERVE_MIB") {
        Ok(v) if !v.trim().is_empty() => {
            match v.trim().parse::<u64>() {
                Ok(m) => {
                    say!("host reserve: {m} MiB (LEA_VGPU_HOST_RESERVE_MIB)");
                    m << 20
                }
                Err(_) => {
                    eprintln!("vgpuprofile: LEA_VGPU_HOST_RESERVE_MIB={v:?} unusable -- measuring instead");
                    in_use_kb * 1024
                }
            }
        }
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

    // and the board's own name
    let mut np = NameParams::default();
    // SAFETY: NameParams matches the ASCII command layout and owns the output buffer.
    unsafe { rm.control(subdevice, mediate::CMD_GPU_GET_NAME_STRING, &mut np) }
        .expect("GPU_GET_NAME_STRING");
    let name = np
        .ascii
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as char)
        .collect::<String>();
    say!("board: {name:?}");

    // Default overhead is the measured per-VM cost; allow workload-specific overrides.
    let overhead = std::env::var("LEA_VGPU_OVERHEAD_MIB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(256)
        << 20;
    let board = vgpu::board_name(&name);
    let cat = vgpu::Catalogue::derive(&board, total_kb * 1024, usable, segment, overhead);

    if let Some(want) = select {
        match cat.resolve(&want) {
            Some(p) => {
                println!("{}", select_block(&cat, &p));
                return;
            }
            None => {
                eprintln!("vgpuprofile: no type or size {want:?} on this card. It offers:");
                eprintln!("{}", cat.table());
                std::process::exit(1);
            }
        }
    }
    println!();
    println!("{}", cat.table());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_and_its_size_print_the_same_numbers() {
        let cat = vgpu::Catalogue::derive("RTX2070", 8192 << 20, 6871 << 20, 256 << 20, 256 << 20);
        let (q, g) = (cat.resolve("4Q").unwrap(), cat.resolve("3G").unwrap());
        let body = |p| {
            select_block(&cat, &p)
                .split_once('\n')
                .unwrap()
                .1
                .to_string()
        };
        assert_eq!(body(q), body(g));
        assert!(
            select_block(&cat, &cat.resolve("3G").unwrap()).starts_with("vgpu_type=RTX2070-3G\n")
        );
    }
}
