// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Measure RM_MAP_MEMORY, mmap, munmap and mapping-FD lifetime without CUDA.
//!
//! Allocation happens before timing. Each iteration includes a fresh FD
//! because RM permits one mmap context per FD; measuring mmap alone omits it.
//!
//! ```text
//! mmapping [iters] [warmup] [bytes] # defaults: 200, 20, 65536
//! ```
//!
//! Reports min/p10/p50/p90/p99/max/mean in microseconds, like ioctlping.

use nvrm_abi::NvDevice;
use nvrm_client::{mem, RmClient};

fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let iters: usize = args.first().map_or(Ok(200), |s| s.parse())?;
    let warmup: usize = args.get(1).map_or(Ok(20), |s| s.parse())?;
    let bytes: usize = args.get(2).map_or(Ok(65536), |s| s.parse())?;
    if iters == 0 || bytes < 4 {
        return Err("iters > 0 and bytes >= 4 required".into());
    }

    // Check the local driver when available; the backend checks the guest ABI.
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

    let mut rm = RmClient::open_without_version_check()?;
    let _gpu = NvDevice::open_gpu(0)?;
    let device = rm.next_handle();
    let mut dp = nvrm_sys::NV0080_ALLOC_PARAMETERS::default();
    dp.deviceId = 0;
    // SAFETY: NV0080_ALLOC_PARAMETERS matches NV01_DEVICE_0; no embedded buffers.
    unsafe { rm.alloc(rm.root(), device, nvrm_sys::NV01_DEVICE_0, Some(&mut dp)) }?;

    // Allocate once outside the timing loop.
    let handle = mem::alloc_sysmem(
        &mut rm,
        device,
        bytes,
        nvrm_sys::NVOS32_ATTR_COHERENCY_CACHED,
    )?;

    // Verify mapping access before measuring.
    {
        let m = mem::map_cpu(&rm, device, handle, bytes)?;
        m.write_u32(0, 0xa5a5_a5a5);
        let back = m.read_u32(0);
        println!("mmapping probe: {bytes} B mapped, write-back check {back:#x}");
        if back != 0xa5a5_a5a5 {
            return Err("the mapping carries no data".into());
        }
    }

    for _ in 0..warmup {
        let _ = mem::map_cpu(&rm, device, handle, bytes)?;
    }

    let mut ns = Vec::with_capacity(iters);
    let mut fails = 0usize;
    for _ in 0..iters {
        let t0 = now_ns();
        let m = mem::map_cpu(&rm, device, handle, bytes);
        // Include munmap and FD close in the measured interval.
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
