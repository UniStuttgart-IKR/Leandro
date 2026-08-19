// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! What `nvidia-smi` asks for its process list, asked directly.
//!
//! `NV2080_CTRL_CMD_GPU_GET_PIDS` (0x2080018d) followed by
//! `NV2080_CTRL_CMD_GPU_GET_PID_INFO` (0x2080018e) per PID -- that is the
//! pair `nvidia-smi` issues (measured under the LD_PRELOAD tracer: 3x
//! GET_PIDS with paramsSize 0xee4, 2x GET_PID_INFO with 0x3848). The
//! tracer counts the calls but does not decode the parameter buffers, and
//! what this round needs is the CONTENT: how many PIDs come back, whose
//! they are, and what video-memory number RM attaches to each.
//!
//! Runs natively on the host and, once the guest carries it, in the guest
//! -- the same binary, so the two answers are comparable without a second
//! implementation to disagree with the first.
//!
//! ```text
//! smipids [class-hex]   # default NV20_SUBDEVICE_0 = every PID on the card
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

/// `NV2080_CTRL_GPU_GET_PIDS_ID_TYPE_CLASS` (ctrl2080gpu.h:3519).
///
/// WARNING: the sibling value `_VGPU_GUEST` (1) is not a shortcut to guest
/// PIDs -- it refers to the `KernelHostVgpuDeviceApi` object on the HOST
/// (subdevice_ctrl_gpu_kernel.c:2333).
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
        Self { id_type: 0, id: 0, pid_tbl_count: 0, pid_tbl: [0; PIDS_MAX] }
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
        Self { count: 0, _pad: 0, list: [PidInfo::default(); PID_INFO_MAX] }
    }
}

// The sizes are the point of this probe, so they are asserted rather than
// trusted: a transcribed offset that has gone stale would produce plausible
// numbers out of the wrong bytes.
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
    // WARNING: the version lockstep applies to the HOST. In the GUEST
    // `/proc/driver/nvidia/version` does not exist -- the guest module
    // provides only `params` -- and `assert_driver_version()` would panic
    // there. The same trap is documented at mmapping.rs, where it cost 242
    // failed measurements. This probe has to run on BOTH sides to be worth
    // anything, so: check WHEN the file is there, otherwise say so and
    // carry on.
    match nvrm_sys::running_driver_version() {
        Ok(v) if v == nvrm_sys::DRIVER_VERSION => {}
        Ok(v) => {
            eprintln!("smipids: driver mismatch: running {v}, built for {}",
                      nvrm_sys::DRIVER_VERSION);
            std::process::exit(1);
        }
        Err(_) => eprintln!(
            "smipids: /proc/driver/nvidia/version unreadable -- guest run, \
             version not checkable (the host holds the lockstep)"
        ),
    }

    let mut rm = RmClient::open_without_version_check().expect("NV01_ROOT_CLIENT");
    let root = rm.root();

    // The per-GPU node has to be OPEN, otherwise the card is not attached
    // to this client and NV01_DEVICE_0 fails with
    // NV_ERR_INSUFFICIENT_PERMISSIONS -- the same trap fbclients.rs
    // documents. The FD is only held, never used.
    let _gpu = NvDevice::open_gpu(0).expect("/dev/nvidia0");

    let device = rm.next_handle();
    let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    rm.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp)).expect("NV01_DEVICE_0");

    let subdevice = rm.next_handle();
    let mut sp = sys::NV2080_ALLOC_PARAMETERS::default();
    sp.subDeviceId = 0;
    rm.alloc(device, subdevice, sys::NV20_SUBDEVICE_0, Some(&mut sp)).expect("NV20_SUBDEVICE_0");

    // ---- GET_PIDS -------------------------------------------------------
    //
    // `id` is a CLASS number, not an index. The header is explicit
    // (ctrl2080gpu.h:3515-3518): with NV20_SUBDEVICE_0 the query returns
    // PIDs with OR without a GPU context; with any other class only those
    // with one. Passing 0 asks for class 0 and yields an empty table --
    // measured, and it looks exactly like "nothing is running".
    let class: u32 = std::env::args()
        .nth(1)
        .map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).expect("class as hex"))
        .unwrap_or(sys::NV20_SUBDEVICE_0);
    let mut p = GetPidsParams { id_type: ID_TYPE_CLASS, id: class, ..Default::default() };
    if let Err(e) = rm.control(subdevice, CMD_GPU_GET_PIDS, &mut p) {
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

    // ---- GET_PID_INFO, one entry per PID --------------------------------
    let mut q = GetPidInfoParams { count: n.min(PID_INFO_MAX) as u32, ..Default::default() };
    for (i, pid) in p.pid_tbl[..q.count as usize].iter().enumerate() {
        q.list[i].pid = *pid;
        q.list[i].index = PID_INFO_INDEX_VIDEO_MEMORY_USAGE;
    }
    if let Err(e) = rm.control(subdevice, CMD_GPU_GET_PID_INFO, &mut q) {
        eprintln!("smipids: GET_PID_INFO: {e}");
        std::process::exit(1);
    }
    println!("\nsmipids: GET_PID_INFO count={}", q.count);
    println!("{:>8} {:>8} {:>12} {:>12} {:>12}  comm",
             "pid", "result", "private MiB", "shOwned MiB", "shDuped MiB");
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
    println!("smipids: sum private = {:.1} MiB over {} pids", total as f64 / (1 << 20) as f64, q.count);
}
