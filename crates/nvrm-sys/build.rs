// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
use std::{env, path::PathBuf};

/// Include paths inside the NVIDIA tree. Deliberately explicit instead of a
/// single -I on the repo root: header names are duplicated (nvtypes.h exists
/// twice, nv-ioctl.h as well) and the order decides which one wins.
const INCLUDE_DIRS: &[&str] = &[
    "kernel-open/common/inc",
    "src/common/sdk/nvidia/inc",
    "src/nvidia/arch/nvalloc/unix/include",
    // Last, so that the three above win on duplicate names. This directory
    // supplies uvm_linux_ioctl.h/uvm_ioctl.h/uvm_types.h; their dependencies
    // (nvCpuUuid.h, nv_uvm_user_types.h) resolve through
    // kernel-open/common/inc.
    "kernel-open/nvidia-uvm",
];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/open-gpu-kernel-modules")
        .canonicalize()
        .expect("vendor/open-gpu-kernel-modules missing -> scripts/build.sh vendor");

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=../../DRIVER_VERSION");

    let mut b = bindgen::Builder::default()
        .header("wrapper.h")
        // NVIDIA's header comments are prose, and bindgen turns them into
        // Rust doc comments -- where indented lines become ```-less code
        // blocks and `cargo test --doc` tries to COMPILE them:
        //
        //   bindings.rs - NV_VASPACE_ALLOCATION_PARAMETERS (line 3564)
        //   error: expected one of or `::`, found `GPU`
        //     |  Big GPU: With FERMI_VASPACE_A, see ...
        //
        // `doctest = false` in Cargo.toml was meant to cover this and does
        // not: `cargo test --doc` runs them anyway. Dropping the comments at
        // the source is the fix that actually holds. Nothing is lost that a
        // reader needs -- the vendor header is vendored, and every offset
        // this crate cares about is asserted by layout_tests below.
        .generate_comments(false)
        .use_core()
        .derive_default(true)
        .derive_debug(true)
        .layout_tests(true) // the offsets are the entire point
        .prepend_enum_name(false)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for d in INCLUDE_DIRS {
        b = b.clang_arg(format!("-I{}", root.join(d).display()));
    }

    // The headers are written for the kernel driver and would otherwise pull
    // in platform ifdefs that do not fit userspace.
    b = b
        .clang_arg("-DNV_LINUX")
        .clang_arg("-D__linux__")
        .clang_arg("-std=gnu11");

    // Allowlist: without it bindgen emits ~15k items and compile time
    // explodes. If a symbol is missing -> add a pattern here, do not widen
    // the existing ones into catch-alls.
    let b = b
        .allowlist_type("NVOS.*")
        .allowlist_type("NV0000_CTRL_.*")   // root-client controls: probed ids, PCI info
        .allowlist_type("NV0080_.*")
        .allowlist_type("NV2080_.*")
        .allowlist_type("NV_MEMORY_ALLOCATION_PARAMS")
        // OS_DESCRIPTOR uses its OWN, smaller params struct (40 bytes instead
        // of 128). Without this entry the size would have to be maintained by
        // hand, and treating it as the 128-byte struct is an 88-byte
        // over-read.
        .allowlist_type("NV_OS_DESC_MEMORY_ALLOCATION_PARAMS")
        .allowlist_type("NV_VASPACE_ALLOCATION_PARAMETERS")
        .allowlist_type("NV_CHANNEL_ALLOC_PARAMS")
        .allowlist_type("NV_CHANNELGPFIFO_ALLOCATION_PARAMETERS")
        .allowlist_type("NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS")   // cla06c.h
        .allowlist_type("NV_CTXSHARE_ALLOCATION_PARAMETERS")        // cl9067.h
        .allowlist_type("NVA06C_CTRL_.*")
        .allowlist_type("NVA083_CTRL_.*")
        .allowlist_type("NVC36F_CTRL_.*")
        .allowlist_type("nv_ioctl_.*")
        .allowlist_type("nv_pci_info_t")
        .allowlist_type("nv_ioctl_card_info_t")     // NV_ESC_CARD_INFO: the BDF as an ioctl, not a control
        .allowlist_var("NV_ESC_.*")
        .allowlist_var("NV_IOCTL_.*")
        .allowlist_var("NV01_.*")
        .allowlist_var("NV04_.*")   // op codes carried by the in-kernel RM API
        .allowlist_var("NV0000_CTRL_.*")
        .allowlist_var("NV0000_CTRL_GPU_INVALID_ID")
        .allowlist_var("NV_MAX_DEVICES")
        .allowlist_var("NV20_.*")
        .allowlist_var("NV2080_CTRL_CMD_OS_UNIX_.*")
        .allowlist_var("NV0080_CTRL_CMD_OS_UNIX_.*")
        // The virtual display: the class NVKMS takes when a GPU has no
        // connectors, and the six controls the class defines (NVKMS asks
        // three of them).
        .allowlist_var("NVA083_.*")
        .allowlist_var("NV9010_.*")     // the vblank callback class + its one control
        // The semaphore-surface waiter controls (ctrl00da.h): the guest
        // module and the backend both key on cmd + notificationHandle.
        .allowlist_type("NV_SEMAPHORE_SURFACE_CTRL_.*")
        .allowlist_var("NV_SEMAPHORE_SURFACE_CTRL_CMD_.*")
        .allowlist_type("NvUnixEvent")              // what GET_EVENT_DATA writes (nvos.h)
        .allowlist_type("NV0005_ALLOC_PARAMETERS")  // event alloc params (cl0005.h)
        .allowlist_var("NV2080_NOTIFIERS_.*")       // notifier indices the event path filters
        .allowlist_var("NV0080_CTRL_CMD_GPU_GET_CLASSLIST")
        .allowlist_var("NV_MEMORY_.*")
        .allowlist_var("NV_CTXSHARE_.*")                            // context-share flags
        .allowlist_var("NV2080_ENGINE_TYPE_.*")                     // engine ids for TSG/channel allocs
        .allowlist_var("NV2080_CTRL_CMD_BUS_.*")                    // BUS_GET_INFO_V2: where NVML reads the BDF
        .allowlist_var("NV2080_CTRL_BUS_INFO_.*")                   // its index list
        .allowlist_var("NVOS.*_FLAGS_.*")
        .allowlist_var("NVOS32_.*")
        .allowlist_var("FERMI_.*")        // FERMI_VASPACE_A and
                                          // FERMI_CONTEXT_SHARE_A
        .allowlist_var("KEPLER_.*")       // KEPLER_CHANNEL_GROUP_A
        .allowlist_var("VOLTA_.*")
        .allowlist_var("TURING_.*")       // TURING_CHANNEL_GPFIFO_A and
                                          // TURING_USERMODE_A; without the
                                          // latter there is no doorbell page
        .allowlist_var("AMPERE_CHANNEL_GPFIFO_A")
        .allowlist_var("NVC361_.*")
        .allowlist_var("NVC36F_.*")
        .allowlist_var("NVC46F_.*")
        .allowlist_var("NVA06C_.*")
        .allowlist_var("NV_RM_API_VERSION_.*")
        .allowlist_var("NV_OK")
        .allowlist_var("NV_ERR_.*")
        .allowlist_var("NV_CHANNELGPFIFO_.*")
        .allowlist_var("NV50_.*")
        // UVM: param structs including their layout tests, command numbers,
        // init flags and the mapping-attribute enums.
        .allowlist_type("UVM_.*_PARAMS")
        .allowlist_type("Uvm.*")
        .allowlist_var("UVM_.*")
        .allowlist_var("UvmGpu.*")
        // GET_GID_INFO: the params struct. The DRF positions of its flags are
        // colon macros in the header and do not survive bindgen -
        // FORMAT_BINARY/TYPE_SHA1 are spelled out in the calling code.
        .allowlist_type("NV2080_CTRL_GPU_GET_GID_INFO_PARAMS")
        .allowlist_var("NV2080_CTRL_CMD_GPU_GET_GID_INFO")
        .allowlist_var("NV2080_GPU_.*GID.*");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    b.generate()
        .expect("bindgen failed")
        .write_to_file(&out)
        .expect("writing bindings.rs failed");

    // Pass the version through for the runtime assert.
    let ver = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../DRIVER_VERSION"),
    )
    .expect("DRIVER_VERSION missing");
    println!("cargo:rustc-env=LEA_DRIVER_VERSION={}", ver.trim());
}
