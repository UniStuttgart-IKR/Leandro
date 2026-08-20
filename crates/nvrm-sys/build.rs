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
    // NVKMS's user-facing ioctl header. Last for the same reason as the
    // line above: this directory has its own nvkms.h/nvtypes-adjacent
    // names, and the four directories above must keep winning on any
    // duplicate.
    "kernel-open/nvidia-modeset",
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
        // The allocation parameter blocks that live in nvos.h. The header
        // was already included for NVOS21/33/46/54/64; these types were
        // filtered out by the allowlist, which meant `xlate::alloc_param_size`
        // had to carry their sizes as arithmetic done by hand while reading
        // the header. One line each turns about forty of those entries into
        // numbers the compiler checks -- see the test in xlate.rs. The guest
        // module copies exactly that many bytes on every allocation, so a
        // wrong one truncates a request or reads out of bounds, identically
        // on both sides of the boundary and therefore invisibly to any sweep.
        .allowlist_type("NV_GR_ALLOCATION_PARAMETERS")        // graphics/compute
        // NV_BSP_/NV_MSENC_ are #defines onto these two, so bindgen only
        // ever sees the real names (nvos.h:2945, :2994).
        .allowlist_type("NV_NVDEC_ALLOCATION_PARAMETERS")     // NVDEC
        .allowlist_type("NV_NVENC_ALLOCATION_PARAMETERS")     // NVENC
        .allowlist_type("NV_OFA_ALLOCATION_PARAMETERS")       // optical flow
        .allowlist_type("NV_NVJPG_ALLOCATION_PARAMETERS")     // JPEG
        .allowlist_type("NV_CONTEXT_DMA_ALLOCATION_PARAMS")   // NV01_CONTEXT_DMA
        .allowlist_type("NV_HOPPER_USERMODE_A_PARAMS")
        .allowlist_type("NV_VIDMEM_ACCESS_BIT_ALLOCATION_PARAMS")
        // ... and the ones that live in their own class headers.
        .allowlist_type("NVB0B5_ALLOCATION_PARAMETERS")
        .allowlist_type("NV_MEMORY_VIRTUAL_ALLOCATION_PARAMS")
        .allowlist_type("NV9072_ALLOCATION_PARAMETERS")
        .allowlist_type("NV2081_ALLOC_PARAMETERS")
        .allowlist_type("NV_MEMORY_MAPPER_ALLOCATION_PARAMS")
        .allowlist_type("NV00DE_ALLOC_PARAMETERS")
        .allowlist_type("NV_CONFIDENTIAL_COMPUTE_ALLOC_PARAMS")
        .allowlist_type("NV83DE_ALLOC_PARAMETERS")
        .allowlist_type("NV_SEMAPHORE_SURFACE_ALLOC_PARAMETERS")
        .allowlist_type("NVC640_ALLOCATION_PARAMETERS")
        .allowlist_type("NVA0BC_ALLOC_PARAMETERS")
        .allowlist_type("NV_PHYSICAL_MEMORY_ALLOCATION_PARAMS")
        .allowlist_type("NV_MEMORY_SYNCPOINT_ALLOCATION_PARAMS")
        .allowlist_type("NV_VASPACE_ALLOCATION_PARAMETERS")
        .allowlist_type("NV_CHANNEL_ALLOC_PARAMS")
        .allowlist_type("NV_CHANNELGPFIFO_ALLOCATION_PARAMETERS")
        .allowlist_type("NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS")   // cla06c.h
        .allowlist_type("NV_CTXSHARE_ALLOCATION_PARAMETERS")        // cl9067.h
        .allowlist_type("NVA06C_CTRL_.*")
        .allowlist_type("NVA083_CTRL_.*")
        .allowlist_type("NVC36F_CTRL_.*")
        .allowlist_type("nv_ioctl_.*")
        // /dev/nvidia-modeset carries ONE ioctl number, and the command is a
        // field inside this 16-byte struct (nvkms-ioctl.h). The tracer reads
        // that field, so the offsets are a layout guard here rather than two
        // numbers written into the reader.
        .allowlist_type("NvKmsIoctlParams")
        .allowlist_var("NVKMS_IOCTL_.*")
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
