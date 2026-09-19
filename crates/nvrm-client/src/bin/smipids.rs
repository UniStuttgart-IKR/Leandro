// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Query GPU process IDs and their memory usage with GET_PIDS/GET_PID_INFO.
//! Run the same binary on host and guest to compare the forwarded results.
//!
//! ```text
//! smipids [class-hex] # default NV20_SUBDEVICE_0 includes every GPU PID
//! ```

use nvrm_abi::{sys, NvDevice};
use nvrm_client::RmClient;

/// `NV2080_CTRL_CMD_GPU_GET_PIDS` (ctrl2080gpu.h:3501).
const CMD_GPU_GET_PIDS: u32 = 0x2080_018d;
/// `NV2080_CTRL_CMD_GPU_GET_PID_INFO` (ctrl2080gpu.h:3643).
const CMD_GPU_GET_PID_INFO: u32 = 0x2080_018e;

/// `NV2080_CTRL_GPU_GET_PIDS_MAX_COUNT` (ctrl2080gpu.h:3505).
const PIDS_MAX: usize = 950;
/// `NV2080_CTRL_GPU_GET_PID_INFO_MAX_COUNT` (ctrl2080gpu.h:3645).
const PID_INFO_MAX: usize = 200;

/// `NV2080_CTRL_GPU_PID_INFO_INDEX_VIDEO_MEMORY_USAGE` (ctrl2080gpu.h:3570).
const PID_INFO_INDEX_VIDEO_MEMORY_USAGE: u32 = 0;

/// CLASS identity. VGPU_GUEST instead refers to a host KernelHostVgpuDeviceApi
/// object (subdevice_ctrl_gpu_kernel.c), not this project's guest process IDs.
const ID_TYPE_CLASS: u32 = 0;

/// `NV2080_CTRL_GPU_GET_PIDS_PARAMS` (ctrl2080gpu.h:3508), 3812 bytes:
/// idType @0, id @4, pidTblCount @8, `pidTbl[950]` @12.
#[repr(C)]
struct GetPidsParams {
    id_type: u32,
    id: u32,
    pid_tbl_count: u32,
    pid_tbl: [u32; PIDS_MAX],
}

impl Default for GetPidsParams {
    fn default() -> Self {
        Self {
            id_type: 0,
            id: 0,
            pid_tbl_count: 0,
            pid_tbl: [0; PIDS_MAX],
        }
    }
}

/// `NV2080_CTRL_GPU_PID_INFO_VIDEO_MEMORY_USAGE_DATA` (ctrl2080gpu.h:3561),
/// the only member of the `data` union.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct VidMemUsage {
    mem_private: u64,
    mem_shared_owned: u64,
    mem_shared_duped: u64,
    protected_mem_private: u64,
    protected_mem_shared_owned: u64,
    protected_mem_shared_duped: u64,
}

/// `NV2080_CTRL_GPU_PID_INFO` (ctrl2080gpu.h:3617), 72 bytes:
/// pid @0, index @4, result @8, data @16, smcSubscription @64.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct PidInfo {
    pid: u32,
    index: u32,
    result: u32,
    _pad: u32,
    data: VidMemUsage,
    smc_compute_instance_id: u32,
    smc_gpu_instance_id: u32,
}

/// `NV2080_CTRL_GPU_GET_PID_INFO_PARAMS` (ctrl2080gpu.h:3649), 14408 bytes:
/// pidInfoListCount @0, `pidInfoList[200]` @8.
#[repr(C)]
struct GetPidInfoParams {
    count: u32,
    _pad: u32,
    list: [PidInfo; PID_INFO_MAX],
}

impl Default for GetPidInfoParams {
    fn default() -> Self {
        Self {
            count: 0,
            _pad: 0,
            list: [PidInfo::default(); PID_INFO_MAX],
        }
    }
}

// Match the vendor layouts, including implicit alignment.
const _: () = {
    assert!(core::mem::size_of::<GetPidsParams>() == 3812);
    assert!(core::mem::size_of::<PidInfo>() == 72);
    assert!(core::mem::size_of::<GetPidInfoParams>() == 14408);
    assert!(core::mem::offset_of!(PidInfo, data) == 16);
    assert!(core::mem::offset_of!(PidInfo, smc_compute_instance_id) == 64);
    assert!(core::mem::offset_of!(GetPidInfoParams, list) == 8);
};

fn comm_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<no /proc entry>".to_string())
}

fn main() {
    // Check the local driver when available; the backend checks the guest ABI.
    match nvrm_sys::running_driver_version() {
        Ok(v) if v == nvrm_sys::DRIVER_VERSION => {}
        Ok(v) => {
            eprintln!(
                "smipids: driver mismatch: running {v}, built for {}",
                nvrm_sys::DRIVER_VERSION
            );
            std::process::exit(1);
        }
        Err(_) => eprintln!(
            "smipids: /proc/driver/nvidia/version unreadable -- guest run, \
             version not checkable (the host holds the lockstep)"
        ),
    }

    let mut rm = RmClient::open_without_version_check().expect("NV01_ROOT_CLIENT");
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

    // NV20_SUBDEVICE_0 includes PIDs without a GPU context. Other class values
    // select only processes with that class (ctrl2080gpu.h).
    let class: u32 = std::env::args()
        .nth(1)
        .map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).expect("class as hex"))
        .unwrap_or(sys::NV20_SUBDEVICE_0);
    let mut p = GetPidsParams {
        id_type: ID_TYPE_CLASS,
        id: class,
        ..Default::default()
    };
    // SAFETY: GetPidsParams matches the command and owns its complete inline output array.
    if let Err(e) = unsafe { rm.control(subdevice, CMD_GPU_GET_PIDS, &mut p) } {
        eprintln!("smipids: GET_PIDS: {e}");
        std::process::exit(1);
    }
    let n = (p.pid_tbl_count as usize).min(PIDS_MAX);
    println!("smipids: GET_PIDS idType={ID_TYPE_CLASS} id={class:#x} -> pidTblCount={n}");
    for pid in &p.pid_tbl[..n] {
        println!("  pid {pid:>8}  {}", comm_of(*pid));
    }
    if n == 0 {
        println!("smipids: no PIDs with a GPU context");
        return;
    }

    // GET_PID_INFO, one entry per PID
    let mut q = GetPidInfoParams {
        count: n.min(PID_INFO_MAX) as u32,
        ..Default::default()
    };
    for (i, pid) in p.pid_tbl[..q.count as usize].iter().enumerate() {
        q.list[i].pid = *pid;
        q.list[i].index = PID_INFO_INDEX_VIDEO_MEMORY_USAGE;
    }
    // SAFETY: GetPidInfoParams matches the command; count fits the inline list.
    if let Err(e) = unsafe { rm.control(subdevice, CMD_GPU_GET_PID_INFO, &mut q) } {
        eprintln!("smipids: GET_PID_INFO: {e}");
        std::process::exit(1);
    }
    if q.count as usize > q.list.len() {
        eprintln!("smipids: GET_PID_INFO returned invalid count {}", q.count);
        std::process::exit(1);
    }
    println!("\nsmipids: GET_PID_INFO count={}", q.count);
    println!(
        "{:>8} {:>8} {:>12} {:>12} {:>12}  comm",
        "pid", "result", "private MiB", "shOwned MiB", "shDuped MiB"
    );
    let mut total = 0u64;
    for e in &q.list[..q.count as usize] {
        total += e.data.mem_private;
        println!(
            "{:>8} {:>8} {:>12.1} {:>12.1} {:>12.1}  {}",
            e.pid,
            e.result,
            e.data.mem_private as f64 / (1 << 20) as f64,
            e.data.mem_shared_owned as f64 / (1 << 20) as f64,
            e.data.mem_shared_duped as f64 / (1 << 20) as f64,
            comm_of(e.pid),
        );
    }
    println!(
        "smipids: sum private = {:.1} MiB over {} pids",
        total as f64 / (1 << 20) as f64,
        q.count
    );
}
