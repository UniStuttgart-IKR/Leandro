// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! What the GPU says it can do, asked directly.
//!
//! `NV0080_CTRL_CMD_GPU_GET_CLASSLIST` (0x800201, `ctrl0080gpu.h:70`) is a
//! small control with an outsized role: it is how **NVKMS (nvidia-modeset.ko)
//! picks its display HAL** (hardware abstraction layer: `nvkms-hal.c:130-190`
//! walks a table of display classes and takes the first one this list
//! reports, with `NVA083_GRID_DISPLAYLESS` last), and it is where the
//! guest's NVENC (the hardware video encoder) used to stop, until the
//! nested-pointer annotation for it landed in `xlate.rs`.
//!
//! It is also the shape that breaks naive forwarding: the params struct
//! carries a **second-level embedded pointer** (`classList`), so it is
//! called twice -- once with `classList = NULL` to learn `numClasses`, then
//! again with a buffer. The first call needs no translation and the second
//! does, which is exactly why a guest without the annotation saw the first
//! succeed and the second return `NV_ERR_INVALID_ADDRESS`.
//!
//! Runs on the host and, once the guest carries it, in the guest -- the
//! same binary, so the two answers are comparable without a second
//! implementation to disagree with the first.
//!
//! ```text
//! classlist            # every class, grouped
//! classlist --raw      # one hex class per line, for diffing host vs guest
//! ```

use nvrm_abi::{sys, NvDevice};
use nvrm_client::RmClient;

/// `NV0080_CTRL_CMD_GPU_GET_CLASSLIST` (ctrl0080gpu.h:70).
const CMD_GPU_GET_CLASSLIST: u32 = 0x0080_0201;

/// `NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS` (ctrl0080gpu.h:76), 16 bytes:
/// numClasses @0, classList (NvP64, 8-aligned) @8.
#[repr(C)]
#[derive(Default)]
struct GetClassListParams {
    num_classes: u32,
    _pad: u32,
    class_list: u64,
}

const _: () = {
    assert!(core::mem::size_of::<GetClassListParams>() == 16);
    assert!(core::mem::offset_of!(GetClassListParams, class_list) == 8);
};

/// The display classes NVKMS looks for, **in the order it looks**
/// (`nvkms-hal.c:145-172`). Whichever appears FIRST decides the HAL, so the
/// ordering is the interesting part, not mere membership.
///
/// WARNING: these are the display CORE CHANNEL classes, `NV<prefix>7D`.
/// `nvkms-hal.c` writes them through the `ENTRY_NVD(C5, C5, ...)` macro,
/// whose first argument is only the *prefix*; the class is
/// `NVC57D_CORE_CHANNEL_DMA` (`clc57d.h:32`). Reading the prefix as the
/// class gives `0xc5b7`, which is a real class -- it is Turing's NVENC --
/// so the mistake produces a plausible, wrong answer rather than an error.
/// It cost one run of this probe reporting "no display class at all" on a
/// card that was driving a desktop at the time.
const DISPLAY_CLASSES: &[(u32, &str)] = &[
    (0xcc7d, "NVCC7D core channel (Blackwell CC)"),
    (0xcb7d, "NVCB7D core channel (Blackwell CB)"),
    (0xca7d, "NVCA7D core channel (Blackwell GB20X)"),
    (0xc97d, "NVC97D core channel (Blackwell)"),
    (0xc87d, "NVC87D core channel (T239)"),
    (0xc77d, "NVC77D core channel (Ada)"),
    (0xc67d, "NVC67D core channel (Ampere)"),
    (0xc57d, "NVC57D core channel (Turing)"),
    (0xa083, "NVA083_GRID_DISPLAYLESS  <- the null display HAL"),
];

/// Engine classes worth calling out by name.
const NOTABLE: &[(u32, &str)] = &[
    (0xc4b7, "NVC4B7_VIDEO_ENCODER (Turing NVENC)"),
    (0xc4b0, "NVC4B0_VIDEO_DECODER (Turing NVDEC)"),
    (0xc5b0, "NVC5B0_VIDEO_DECODER"),
    (0xc4d1, "NVC4D1_VIDEO_NVJPG"),
    (0xc597, "NVC597_TURING_A (3D)"),
    (0xc5c0, "NVC5C0_TURING_COMPUTE_A"),
    (0xc56f, "NVC56F_TURING_CHANNEL_GPFIFO_A"),
];

fn main() {
    // The version lockstep applies to the HOST; in the guest
    // /proc/driver/nvidia/version does not exist and a hard check would
    // panic there. Same trap as smipids.rs and mmapping.rs.
    match nvrm_sys::running_driver_version() {
        Ok(v) if v == nvrm_sys::DRIVER_VERSION => {}
        Ok(v) => {
            eprintln!(
                "classlist: driver mismatch: running {v}, built for {}",
                nvrm_sys::DRIVER_VERSION
            );
            std::process::exit(1);
        }
        Err(_) => eprintln!(
            "classlist: /proc/driver/nvidia/version unreadable -- guest run, \
             version not checkable (the host holds the lockstep)"
        ),
    }
    let raw = std::env::args().any(|a| a == "--raw");

    let mut rm = RmClient::open_without_version_check().expect("NV01_ROOT_CLIENT");
    let root = rm.root();

    // The per-GPU node has to be OPEN or NV01_DEVICE_0 fails with
    // NV_ERR_INSUFFICIENT_PERMISSIONS. Held, never used.
    let _gpu = NvDevice::open_gpu(0).expect("/dev/nvidia0");

    let device = rm.next_handle();
    let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    rm.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp)).expect("NV01_DEVICE_0");

    // ---- call 1: classList = NULL, just to learn the count --------------
    let mut p = GetClassListParams::default();
    if let Err(e) = rm.control(device, CMD_GPU_GET_CLASSLIST, &mut p) {
        eprintln!("classlist: GET_CLASSLIST (count pass): {e}");
        std::process::exit(1);
    }
    let n = p.num_classes as usize;
    if !raw {
        println!("classlist: numClasses = {n}");
    }
    if n == 0 || n > 4096 {
        eprintln!("classlist: implausible numClasses {n} -- stopping");
        std::process::exit(1);
    }

    // ---- call 2: with a real buffer -------------------------------------
    // THIS is the call that needs the nested-pointer annotation. In a guest
    // without one it returns NV_ERR_INVALID_ADDRESS (0x1e).
    let mut classes = vec![0u32; n];
    let mut q = GetClassListParams {
        num_classes: n as u32,
        _pad: 0,
        class_list: classes.as_mut_ptr() as usize as u64,
    };
    if let Err(e) = rm.control(device, CMD_GPU_GET_CLASSLIST, &mut q) {
        eprintln!("classlist: GET_CLASSLIST (fill pass): {e}");
        eprintln!(
            "  If this is a guest and the count pass above succeeded, this is the\n\
             \x20 missing nested-pointer annotation for 0x800201 (xlate::nested_ptrs)."
        );
        std::process::exit(1);
    }
    classes.sort_unstable();

    if raw {
        for c in &classes {
            println!("{c:#06x}");
        }
        return;
    }

    println!("\n== display classes NVKMS looks for, in its own search order ==");
    let mut first_hit = None;
    for (c, name) in DISPLAY_CLASSES {
        let have = classes.contains(c);
        if have && first_hit.is_none() {
            first_hit = Some((*c, *name));
        }
        println!("  {} {c:#06x}  {name}", if have { "YES" } else { " no" });
    }
    match first_hit {
        Some((c, name)) => println!("  -> NVKMS would select {c:#06x} ({name})"),
        None => println!("  -> no display class at all: NVKMS cannot allocate a device here"),
    }

    println!("\n== notable engine classes ==");
    for (c, name) in NOTABLE {
        println!("  {} {c:#06x}  {name}", if classes.contains(c) { "YES" } else { " no" });
    }

    println!("\n== all {n} classes ==");
    for (i, c) in classes.iter().enumerate() {
        print!("{c:#06x} ");
        if i % 10 == 9 {
            println!();
        }
    }
    println!();
}
