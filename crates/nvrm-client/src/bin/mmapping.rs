// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! mmapping -- round-trip latency of ONE window mapping, with no CUDA
//! around it.
//!
//! Same construction as `probe/c/ioctlping.c`, only for `mmap` instead of
//! `ioctl`. It answers the one number a measurement night left open:
//!
//! Measured: `cuInit` costs **40 ms more** over the module than over the
//! retired LD_PRELOAD path -- at a **counted identical number of ioctls**
//! (90 in both variants) and although the transport is **faster** per call
//! (12.2 vs 17.6 us, large payloads included). So the surcharge sits
//! neither in the count nor in the latency of the ioctls. What was left
//! was the mapping path: where no window mapping is needed (nvidia-smi,
//! hundreds of ioctls, zero mappings) the module is faster; with two
//! mappings it is +40 ms, with dozens +126 ms.
//!
//! What is measured is therefore **only** the mapping: one RM allocation,
//! then N times `NV_ESC_RM_MAP_MEMORY` + `mmap` + `munmap`. The allocation
//! itself stands outside the measuring loop.
//!
//! WARNING: why a fresh FD is needed per round, and why the measurement
//! therefore counts it in: the driver allows **exactly one mmap context per
//! FD** (`mem.rs`, head comment). An `open` + `REGISTER_FD` is thus part of
//! every mapping -- libcuda does it the same way (a fresh `open` before
//! every MAP_MEMORY, read off the trace). Measuring only the `mmap` syscall
//! does not measure the path a real mapping takes.
//!
//! ```text
//! mmapping [iters] [warmup] [bytes]      (default 200, 20, 65536)
//! ```
//!
//! Output: one grep-friendly line in us -- min/p10/p50/p90/p99/max/mean,
//! the same format as ioctlping, so the same scripts collect them.

use nvrm_abi::NvDevice;
use nvrm_client::{mem, RmClient};

fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let iters: usize = args.first().map_or(Ok(200), |s| s.parse())?;
    let warmup: usize = args.get(1).map_or(Ok(20), |s| s.parse())?;
    let bytes: usize = args.get(2).map_or(Ok(65536), |s| s.parse())?;
    if iters == 0 {
        return Err("iters > 0 required".into());
    }

    // WARNING: the version lockstep applies to the HOST -- that is where
    // the real driver runs. In the GUEST `/proc/driver/nvidia/version` does
    // not exist: the guest module provides only `params` (nvrm-setup.sh,
    // provision params). `assert_driver_version()` would panic there, and
    // it did: 242 failed measurements in both guest variants while the
    // native reference ran through. Hence: check WHEN the file is there,
    // otherwise write a line and keep measuring.
    match nvrm_sys::running_driver_version() {
        Ok(v) if v == nvrm_sys::DRIVER_VERSION => {}
        Ok(v) => {
            return Err(format!(
                "driver mismatch: running {v}, built for {}",
                nvrm_sys::DRIVER_VERSION
            )
            .into())
        }
        Err(_) => eprintln!(
            "mmapping: /proc/driver/nvidia/version unreadable -- \
             guest run, version not checkable (the host holds the lockstep)"
        ),
    }

    // Without an enforced lockstep: the version was checked above WHEN it
    // was checkable. In the guest it is not, and that is where the
    // measurement happens.
    let mut rm = RmClient::open_without_version_check()?;
    let gpu = NvDevice::open_gpu(0)?;
    let device = rm.next_handle();
    let mut dp = nvrm_sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    rm.alloc(rm.root(), device, nvrm_sys::NV01_DEVICE_0, Some(&mut dp))?;

    // The allocation stands ONCE and outside the measuring loop -- what is
    // measured is the mapping, not the getting of the memory.
    let handle = mem::alloc_sysmem(&mut rm, device, bytes, nvrm_sys::NVOS32_ATTR_COHERENCY_CACHED)?;

    // One trial mapping before measuring: a measurement against a path
    // that fails quietly would be worthless.
    {
        let m = mem::map_cpu(&rm, &gpu, device, handle, bytes)?;
        m.write_u32(0, 0xa5a5_a5a5);
        let back = m.read_u32(0);
        println!("mmapping probe: {bytes} B mapped, write-back check {back:#x}");
        if back != 0xa5a5_a5a5 {
            return Err("the mapping carries no data".into());
        }
    }

    for _ in 0..warmup {
        let _ = mem::map_cpu(&rm, &gpu, device, handle, bytes)?;
    }

    let mut ns = Vec::with_capacity(iters);
    let mut fails = 0usize;
    for _ in 0..iters {
        let t0 = now_ns();
        let m = mem::map_cpu(&rm, &gpu, device, handle, bytes);
        // The drop (munmap + close) is part of the mapping: a window
        // mapping that is never given back is not a measurement but a
        // leak.
        let ok = m.is_ok();
        drop(m);
        let t1 = now_ns();
        ns.push(t1 - t0);
        if !ok {
            fails += 1;
        }
    }

    ns.sort_unstable();
    let pct = |p: usize| ns[p * (iters - 1) / 100] as f64 / 1000.0;
    let mean = ns.iter().sum::<u64>() as f64 / iters as f64 / 1000.0;
    println!(
        "mmapping n={iters} bytes={bytes} fails={fails} min_us={:.2} p10_us={:.2} \
         p50_us={:.2} p90_us={:.2} p99_us={:.2} max_us={:.2} mean_us={mean:.2}",
        ns[0] as f64 / 1000.0,
        pct(10),
        pct(50),
        pct(90),
        pct(99),
        *ns.last().unwrap() as f64 / 1000.0,
    );

    if fails > 0 {
        return Err(format!("{fails} of {iters} mappings failed").into());
    }
    Ok(())
}
