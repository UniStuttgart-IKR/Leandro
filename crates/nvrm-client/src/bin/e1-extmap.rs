// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Host-local OS-descriptor and UVM external-mapping diagnostic.
//!
//! Registers memfd pages as NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, creates an
//! external UVM range and maps the allocation without mmap on the UVM FD.
//! Tests the libcuda pool VA and a second VA.
//!
//! NV_ERR_INVALID_DEVICE can mean the GPU is absent from the UVM VA space
//! (uvm_map_external.c). dupMemory also checks address-space compatibility
//! (nv_gpu_ops.c). UVM command numbers below come from uvm_ioctl.h.

use nvrm_abi::{sys, NvDevice};
use nvrm_client::RmClient;
use std::os::fd::AsRawFd;

/// uvm_ioctl.h:288
const UVM_REGISTER_GPU_VASPACE: u64 = 25;
/// uvm_ioctl.h:365
const UVM_MAP_EXTERNAL_ALLOCATION: u64 = 33;
/// uvm_ioctl.h:405
const UVM_REGISTER_GPU: u64 = 37;
/// uvm_ioctl.h:842
const UVM_CREATE_EXTERNAL_RANGE: u64 = 73;

/// uvm_types.h:67 - ((NvU64)0x2); cast macro, not in the bindings.
const UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE: u64 = 0x2;

/// nvos.h:192-279. DRF (NVIDIA's hi:lo bit-field notation) as shifts of
/// the *_ values:
/// PHYSICALITY 7:4 (NONCONTIGUOUS=1), LOCATION 11:8 (PCI=0),
/// COHERENCY 15:12 (CACHED=1), MAPPING 31:30 (NO_MAP=1).
/// RmAllocOsDescriptor (escape.c:206-225) demands exactly LOCATION_PCI +
/// MAPPING_NO_MAP + a valid coherency.
#[allow(clippy::identity_op)] // (0 << 8) documents the DRF field LOCATION=PCI
const OSDESC_FLAGS: u32 = (1 << 4) | (0 << 8) | (1 << 12) | (1 << 30);

/// nvos.h:3162/3164 - BIT(3) and BIT(6); bindgen does not expand BIT().
/// The dup path of the registration demands vaspaceIsExternallyOwned
/// (nv_gpu_ops.c:2784-2788), otherwise NV_ERR_INVALID_FLAGS.
const VASPACE_IS_EXTERNALLY_OWNED: u32 = 1 << 3;
const VASPACE_ENABLE_PAGE_FAULTING: u32 = 1 << 6;
const VASPACE_FLAGS: u32 = VASPACE_IS_EXTERNALLY_OWNED | VASPACE_ENABLE_PAGE_FAULTING;

const POOL_BYTES: usize = 2 << 20;

/// Test the observed libcuda pool VA and a separate valid GPU VA.
const BASE_LIBCUDA: u64 = 0x2_04a0_0000;
const BASE_ARBITRARY: u64 = 0x5_1120_0000;

// SAFETY: callers must pair cmd with its complete initialized UVM payload.
unsafe fn uvm_ioctl<T>(fd: &std::fs::File, cmd: u64, p: &mut T) -> (i32, i32) {
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), cmd as libc::Ioctl, p as *mut T) };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    (r, if r == 0 { 0 } else { errno })
}

fn step(name: &str, ret: i32, errno: i32, rm_status: u32) -> bool {
    let ok = ret == 0 && rm_status == sys::NV_OK;
    println!(
        "{} {name}: ioctl={ret} errno={errno} rmStatus={rm_status:#x}",
        if ok { "ok  " } else { "FAIL" },
    );
    ok
}

fn main() {
    sys::assert_driver_version();

    // RM side: client, device, subdevice, external vaspace
    let mut rm = RmClient::new().expect("NV01_ROOT_CLIENT");
    let root = rm.root();
    let gpu = NvDevice::open_gpu(0).expect("/dev/nvidia0");

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

    // GET_GID_INFO FORMAT_BINARY|TYPE_SHA1 returns the 16-byte UVM UUID.
    let mut gid = sys::NV2080_CTRL_GPU_GET_GID_INFO_PARAMS::default();
    gid.index = 0;
    gid.flags = 2;
    // SAFETY: The generated parameter type matches this command and has no nested pointers.
    unsafe { rm.control(subdevice, sys::NV2080_CTRL_CMD_GPU_GET_GID_INFO, &mut gid) }
        .expect("GET_GID_INFO");
    assert_eq!(gid.length, 16, "SHA1-binary GID must be 16 bytes");
    let mut uuid = sys::NvProcessorUuid::default();
    uuid.uuid.copy_from_slice(&gid.data[..16]);
    println!("gpu uuid = {:02x?}", &uuid.uuid);

    let vaspace = rm.next_handle();
    let mut vp = sys::NV_VASPACE_ALLOCATION_PARAMETERS::default();
    vp.index = 0;
    vp.flags = VASPACE_FLAGS;
    // SAFETY: NV_VASPACE_ALLOCATION_PARAMETERS matches FERMI_VASPACE_A.
    unsafe { rm.alloc(device, vaspace, sys::FERMI_VASPACE_A, Some(&mut vp)) }
        .expect("FERMI_VASPACE_A (external)");

    // The memory "the guest" provides: a memfd in our own process
    let memfd = unsafe { libc::memfd_create(c"e1-extmap".as_ptr(), 0) };
    assert!(memfd >= 0, "memfd_create");
    assert_eq!(unsafe { libc::ftruncate(memfd, POOL_BYTES as i64) }, 0);
    let host_va = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            POOL_BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd,
            0,
        )
    };
    assert_ne!(host_va, libc::MAP_FAILED, "mmap memfd");
    unsafe { std::ptr::write_bytes(host_va as *mut u8, 0xa5, POOL_BYTES) };
    println!("host va  = {host_va:p} ({} MiB memfd)", POOL_BYTES >> 20);

    // (2) OS descriptor: RM gets the host VA *described*
    let osdesc = rm.next_handle();
    let mut wfd = nvrm_abi::nvgpu::Nvos02WithFd::default();
    wfd.params.hRoot = root;
    wfd.params.hObjectParent = device;
    wfd.params.hObjectNew = osdesc;
    wfd.params.hClass = sys::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
    wfd.params.flags = OSDESC_FLAGS;
    wfd.params.pMemory = host_va as usize as sys::NvP64;
    wfd.params.limit = (POOL_BYTES - 1) as u64;
    wfd.fd = -1; // VIRTUAL_ADDRESS descriptor, not a dma-buf
                 // NV_ACTUAL_DEVICE_ONLY requires a GPU FD registered against this client
                 // (escape.c); an unregistered FD fails client validation.
    let gpu_reg = gpu.open_for_mapping(rm.ctl()).expect("REGISTER_FD");
    unsafe {
        gpu_reg
            .ioctl_raw(sys::NV_ESC_RM_ALLOC_MEMORY, &mut wfd)
            .expect("NV_ESC_RM_ALLOC_MEMORY ioctl");
    }
    if !step("ALLOC_MEMORY 0x71", 0, 0, wfd.params.status as u32) {
        std::process::exit(1);
    }

    // (3) UVM: initialize (sharing mode), register gpu + vaspace
    let uvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/nvidia-uvm")
        .expect("/dev/nvidia-uvm");

    let mut ip = sys::UVM_INITIALIZE_PARAMS::default();
    ip.flags = UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE;
    // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
    let (r, e) = unsafe { uvm_ioctl(&uvm, sys::UVM_INITIALIZE as u64, &mut ip) };
    if !step("UVM_INITIALIZE(0x2)", r, e, ip.rmStatus) {
        std::process::exit(1);
    }

    let ctl_fd = rm.ctl().as_raw_fd();

    let mut rg: sys::UVM_REGISTER_GPU_PARAMS = unsafe { std::mem::zeroed() };
    rg.gpu_uuid = uuid;
    rg.rmCtrlFd = ctl_fd;
    rg.hClient = root;
    rg.hSmcPartRef = 0;
    // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
    let (r, e) = unsafe { uvm_ioctl(&uvm, UVM_REGISTER_GPU, &mut rg) };
    if !step("UVM_REGISTER_GPU", r, e, rg.rmStatus) {
        std::process::exit(1);
    }

    let mut rv: sys::UVM_REGISTER_GPU_VASPACE_PARAMS = unsafe { std::mem::zeroed() };
    rv.gpuUuid = uuid;
    rv.rmCtrlFd = ctl_fd;
    rv.hClient = root;
    rv.hVaSpace = vaspace;
    // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
    let (r, e) = unsafe { uvm_ioctl(&uvm, UVM_REGISTER_GPU_VASPACE, &mut rv) };
    if !step("UVM_REGISTER_GPU_VASPACE", r, e, rv.rmStatus) {
        std::process::exit(1);
    }

    // (4) The actual question, at two bases
    let mut fail = false;
    for base in [BASE_LIBCUDA, BASE_ARBITRARY] {
        let mut cr: sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS = unsafe { std::mem::zeroed() };
        cr.base = base;
        cr.length = POOL_BYTES as u64;
        // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
        let (r, e) = unsafe { uvm_ioctl(&uvm, UVM_CREATE_EXTERNAL_RANGE, &mut cr) };
        fail |= !step(
            &format!("CREATE_EXTERNAL_RANGE @{base:#x}"),
            r,
            e,
            cr.rmStatus,
        );

        let mut mp: Box<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS> =
            unsafe { Box::new(std::mem::zeroed()) };
        mp.base = base;
        mp.length = POOL_BYTES as u64;
        mp.offset = 0;
        mp.perGpuAttributes[0].gpuUuid = uuid;
        mp.perGpuAttributes[0].gpuMappingType = sys::UvmGpuMappingTypeReadWriteAtomic as u32;
        mp.perGpuAttributes[0].gpuCachingType = sys::UvmGpuCachingTypeDefault as u32;
        mp.gpuAttributesCount = 1;
        mp.rmCtrlFd = ctl_fd;
        mp.hClient = root;
        mp.hMemory = osdesc;
        // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
        let (r, e) = unsafe { uvm_ioctl(&uvm, UVM_MAP_EXTERNAL_ALLOCATION, mp.as_mut()) };
        fail |= !step(
            &format!("MAP_EXTERNAL_ALLOCATION @{base:#x}"),
            r,
            e,
            mp.rmStatus,
        );
    }

    // The negative test must reject a nonexistent memory handle.
    let neg_base = 0x6_2233_0000u64;
    let mut cr: sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS = unsafe { std::mem::zeroed() };
    cr.base = neg_base;
    cr.length = POOL_BYTES as u64;
    // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
    let (r, e) = unsafe { uvm_ioctl(&uvm, UVM_CREATE_EXTERNAL_RANGE, &mut cr) };
    fail |= !step(
        &format!("CREATE_EXTERNAL_RANGE @{neg_base:#x} (negative check)"),
        r,
        e,
        cr.rmStatus,
    );
    let mut mp: Box<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS> =
        unsafe { Box::new(std::mem::zeroed()) };
    mp.base = neg_base;
    mp.length = POOL_BYTES as u64;
    mp.perGpuAttributes[0].gpuUuid = uuid;
    mp.perGpuAttributes[0].gpuMappingType = sys::UvmGpuMappingTypeReadWriteAtomic as u32;
    mp.gpuAttributesCount = 1;
    mp.rmCtrlFd = ctl_fd;
    mp.hClient = root;
    mp.hMemory = 0xdead_beef;
    // SAFETY: the generated UVM payload matches cmd; embedded arrays remain live.
    let (r, e) = unsafe { uvm_ioctl(&uvm, UVM_MAP_EXTERNAL_ALLOCATION, mp.as_mut()) };
    let neg_ok = !(r == 0 && mp.rmStatus == sys::NV_OK);
    println!(
        "{} MAP_EXTERNAL_ALLOCATION hMemory=0xdeadbeef: ioctl={r} errno={e} rmStatus={:#x} \
         (failure expected)",
        if neg_ok { "ok  " } else { "FAIL" },
        mp.rmStatus,
    );
    fail |= !neg_ok;

    // CPU side untouched: the pages still belong to the memfd.
    let first = unsafe { std::ptr::read_volatile(host_va as *const u8) };
    println!("cpu byte = {first:#x} (expected 0xa5)");

    println!(
        "{}",
        if fail {
            "E1: FAIL"
        } else {
            "E1: PASS - the chain carries"
        }
    );
    std::process::exit(if fail { 1 } else { 0 });
}
