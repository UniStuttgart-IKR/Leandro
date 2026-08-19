// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Who is holding VRAM right now -- and under which guest-process
//! identity?
//!
//! Asks `NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO` (0x20801349,
//! `ctrl2080fb.h:2512`) and prints, per RM client, `handle`, `pid`,
//! `subProcessID`, `subProcessName` and the sum of its allocations.
//!
//! What for: this is the counter-check for the sub-process identity. If the
//! host sets the guest process's identity when a client is created
//! (`SET_SUB_PROCESS_ID`, the RM control that stamps a client with a
//! sub-process identity), it has to show up here -- with the guest
//! module's dense ID (its never-reused per-process key) and the guest
//! process's name, while `pid` stays that
//! of the BACKEND (every guest process of a VM lives in its process; that
//! is exactly why the sub-field exists).
//!
//! Invocation (on the HOST, while something runs in the guest):
//!   cargo run --release -p nvrm-client --bin fbclients
//!
//! Two rounds, as the header prescribes (`ctrl2080fb.h:2507-2510`): ask
//! with counters 0 first, then with adequately sized buffers. Values come
//! back only when BOTH counters are large enough.
//!
//! Measured: **on a release driver RM always answers
//! `NV_ERR_NOT_SUPPORTED` (0x56) here -- including as root.** The reason is
//! in the source and is not a permission question: the whole
//! implementation sits behind
//! `#if defined(DEBUG) || defined(DEVELOP) || defined(NV_VERIF_FEATURES) ||
//! defined(NV_MODS)` (`mem_mgr_ctrl.c:449`) and is simply not present in a
//! production driver.
//!
//! The tool stays here anyway: against a debug driver it is the direct
//! counter-check, and the finding itself is an answer -- a per-guest-process
//! VRAM breakdown is NOT queryable on this driver, however cleanly the host
//! sets the IDs. Whoever needs it has to keep the books themselves (the
//! host sees every allocation) instead of asking RM. That is what
//! `vhost-user-nvrm/src/vram.rs` does.
//!
//! That the IDs DO arrive is therefore shown by other evidence:
//! `SET_SUB_PROCESS_ID` returns `NV_OK` only after it has written
//! `pClient->SubProcessID` (`client_resource.c:4856-4872`) -- and the host
//! logs every deviation from that loudly.

use nvrm_abi::{sys, NvDevice};
use nvrm_client::RmClient;

/// `NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO` (ctrl2080fb.h:2512).
const CMD_FB_GET_CLIENT_ALLOCATION_INFO: u32 = 0x2080_1349;

/// `NV_PROC_NAME_MAX_LENGTH` (nvlimits.h:47).
const PROC_NAME_MAX: usize = 100;

/// `NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO_PARAMS` (ctrl2080fb.h:2552):
/// allocCount u64 @0, pAllocInfo P64 @8, clientCount u64 @16,
/// pClientInfo P64 @24.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct FbInfoParams {
    alloc_count: u64,
    p_alloc_info: u64,
    client_count: u64,
    p_client_info: u64,
}

/// `NV2080_CTRL_CMD_FB_ALLOCATION_INFO` (ctrl2080fb.h:2534-2539):
/// client u32 @0, flags u32 @4, beginAddr u64 @8, size u64 @16.
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct AllocInfo {
    client: u32,
    flags: u32,
    begin_addr: u64,
    size: u64,
}

/// `NV2080_CTRL_CMD_FB_CLIENT_INFO` (ctrl2080fb.h:2541-2548):
/// handle u32 @0, pid u32 @4, subProcessID u32 @8, `subProcessName char[100]` @12.
#[repr(C)]
#[derive(Copy, Clone)]
struct ClientInfo {
    handle: u32,
    pid: u32,
    sub_process_id: u32,
    sub_process_name: [u8; PROC_NAME_MAX],
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self { handle: 0, pid: 0, sub_process_id: 0, sub_process_name: [0; PROC_NAME_MAX] }
    }
}

/// `NV2080_CTRL_CMD_FB_ALLOCATION_FLAGS_TYPE` 4:0, `_VIDMEM` == 1
/// (ctrl2080fb.h:2519-2521).
fn is_vidmem(flags: u32) -> bool {
    flags & 0x1f == 1
}

fn cstr(b: &[u8]) -> String {
    b.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' })
        .collect()
}

fn main() {
    sys::assert_driver_version();

    let mut rm = RmClient::new().expect("NV01_ROOT_CLIENT");
    let root = rm.root();

    // The per-GPU node must be OPEN, otherwise the card is not attached to
    // this client and NV01_DEVICE_0 fails with
    // NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b) -- measured, first without and
    // then with this line. The FD is only held, never used.
    let _gpu = NvDevice::open_gpu(0).expect("/dev/nvidia0");

    let device = rm.next_handle();
    let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    rm.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp))
        .expect("NV01_DEVICE_0");

    let subdevice = rm.next_handle();
    let mut sp = sys::NV2080_ALLOC_PARAMETERS::default();
    sp.subDeviceId = 0;
    rm.alloc(device, subdevice, sys::NV20_SUBDEVICE_0, Some(&mut sp))
        .expect("NV20_SUBDEVICE_0");

    // Round 1: count only.
    let mut p = FbInfoParams::default();
    if let Err(e) = rm.control(subdevice, CMD_FB_GET_CLIENT_ALLOCATION_INFO, &mut p) {
        eprintln!("FB_GET_CLIENT_ALLOCATION_INFO (counting round): {e}");
        eprintln!("  0x56 (NOT_SUPPORTED) is the RIGHT answer on a release driver");
        eprintln!("  and not a defect: the implementation sits behind");
        eprintln!("  #if defined(DEBUG)||DEVELOP||NV_VERIF_FEATURES||NV_MODS");
        eprintln!("  (mem_mgr_ctrl.c:449) and is not compiled in here. Running as");
        eprintln!("  root changes nothing about that (measured).");
        std::process::exit(1);
    }
    let (n_alloc, n_client) = (p.alloc_count as usize, p.client_count as usize);
    println!("RM reports {n_alloc} allocations across {n_client} clients.");
    if n_client == 0 {
        return;
    }

    // Round 2: with buffers. Some slack, so that a client which appeared
    // in the meantime does not make the round worthless -- RM delivers only
    // when BOTH counters suffice.
    let mut allocs = vec![AllocInfo::default(); n_alloc + 64];
    let mut clients = vec![ClientInfo::default(); n_client + 16];
    let mut p = FbInfoParams {
        alloc_count: allocs.len() as u64,
        p_alloc_info: allocs.as_mut_ptr() as u64,
        client_count: clients.len() as u64,
        p_client_info: clients.as_mut_ptr() as u64,
    };
    if let Err(e) = rm.control(subdevice, CMD_FB_GET_CLIENT_ALLOCATION_INFO, &mut p) {
        eprintln!("FB_GET_CLIENT_ALLOCATION_INFO (data round): {e}");
        std::process::exit(1);
    }
    let n_alloc = (p.alloc_count as usize).min(allocs.len());
    let n_client = (p.client_count as usize).min(clients.len());

    // Sum per client. `client` in AllocInfo is the INDEX into the client
    // list (ctrl2080fb.h:2535), not the handle.
    let mut vid = vec![0u64; n_client];
    let mut sys_b = vec![0u64; n_client];
    let mut chunks = vec![0u32; n_client];
    for a in &allocs[..n_alloc] {
        let i = a.client as usize;
        if i >= n_client {
            continue;
        }
        chunks[i] += 1;
        if is_vidmem(a.flags) {
            vid[i] += a.size;
        } else {
            sys_b[i] += a.size;
        }
    }

    println!(
        "\n{:>10} {:>8} {:>8}  {:<28} {:>10} {:>10} {:>7}",
        "hClient", "pid", "subProc", "subProcessName", "VRAM MiB", "SYS MiB", "chunks"
    );
    for (i, c) in clients[..n_client].iter().enumerate() {
        println!(
            "{:>#10x} {:>8} {:>8}  {:<28} {:>10.1} {:>10.1} {:>7}",
            c.handle,
            c.pid,
            c.sub_process_id,
            cstr(&c.sub_process_name),
            vid[i] as f64 / (1 << 20) as f64,
            sys_b[i] as f64 / (1 << 20) as f64,
            chunks[i]
        );
    }

    let with_sub = clients[..n_client].iter().filter(|c| c.sub_process_id != 0).count();
    println!(
        "\n{with_sub} of {n_client} clients carry a subProcessID \
         (0 = no attribution)."
    );
}
