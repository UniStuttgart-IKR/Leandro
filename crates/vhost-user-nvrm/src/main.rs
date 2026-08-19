// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! vhost-user-nvrm - host daemon. Pure forwarder + translator.
//!
//! RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN, whose ioctls are called escapes;
//! docs/ARCHITECTURE.md has the whole story.
//!
//! NO RM client of its own: the guest (libcuda/NVML) allocates its
//! ROOT_CLIENT (the top-level RM object everything else hangs off)
//! itself via the forwarded RM_ALLOC. The driver binds
//! clients to the OFD (open file description) - so the ioctl must run on
//! exactly the OFD the guest used, and the host holds that one as a
//! mirror.
//!
//! Exactly one transport: virtio-nvrm (`--nvrm`), the guest driver is
//! `virtio_nvrm.ko`. The SEQPACKET transport (an LD_PRELOAD shim in the
//! same kernel) and the virtio-gpu device were removed on 2026-08-04 --
//! two interpreters of the same knowledge were one too many.

use anyhow::Result;

use vhost_user_nvrm::nvrm;

fn usage() -> ! {
    eprintln!(
        "vhost-user-nvrm --nvrm <socket>\n\
         \n\
         --nvrm:  virtio-nvrm device for cloud-hypervisor, guest driver is virtio_nvrm.ko"
    );
    std::process::exit(2);
}

/// Say so when a signal ends us.
///
/// This exists because a backend died three times under a running guest
/// and left NOTHING: the log stopped mid-sentence, with no "VMM hung up"
/// (so not a clean return), no "Error:" (so not a failed one), no coredump
/// (so not SIGSEGV/SIGABRT) and no OOM kill. A guest then waits on a host
/// that is gone, and the first visible symptom is somewhere else entirely.
///
/// Write(2) straight to fd 2 -- the handler runs in signal context, where
/// `eprintln!` and the allocator behind it are not safe. Then restore the
/// default and re-raise, so the exit status still says what killed us.
extern "C" fn say_and_die(sig: libc::c_int) {
    let msg: &[u8] = match sig {
        libc::SIGTERM => b"vhost-user-nvrm: killed by SIGTERM\n",
        libc::SIGINT => b"vhost-user-nvrm: killed by SIGINT\n",
        libc::SIGHUP => b"vhost-user-nvrm: killed by SIGHUP\n",
        libc::SIGPIPE => b"vhost-user-nvrm: killed by SIGPIPE\n",
        _ => b"vhost-user-nvrm: killed by a signal\n",
    };
    // SAFETY: write(2) is async-signal-safe; a short write only truncates
    // the note, and the process is ending either way.
    unsafe {
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_signal_notes() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGPIPE] {
        // SAFETY: installing a handler that only writes and re-raises.
        unsafe {
            libc::signal(sig, say_and_die as libc::sighandler_t);
        }
    }
}

/// `CAP_SYS_ADMIN`, and by default we give it up.
///
/// RM grades every control: `RMCTRL_FLAGS_PRIVILEGED` means "admin is
/// enough" and `osIsAdministrator()` is `capable(CAP_SYS_ADMIN)` on Linux.
/// A few display controls sit behind that grade --
/// `NV0073_CTRL_CMD_SPECIFIC_GET_ALL_HEAD_MASK` is the first that NVKMS
/// (nvidia-modeset.ko in the guest) meets --
/// so the capability is what makes the display subsystem come further up.
///
/// It is NOT free, and it is not obviously a win:
///
///   * This process takes guest input apart. CAP_SYS_ADMIN is the
///     "almost root" capability; carrying it means the sentence "the only
///     boundary the host enforces is the VM" no longer holds unchanged.
///   * Measured 2026-08-08: WITH the capability NVKMS gets past the head
///     mask, then dies on `GET_PCLK_LIMIT` (kernel-privileged, which admin
///     does NOT reach) and nvidia-drm answers "Failed to allocate
///     NvKmsKapiDevice" -- so `/dev/dri/card1` disappears. WITHOUT it the
///     earlier failure is harmless and the render node is there. More
///     privilege made the outcome WORSE.
///
/// So: the file may carry the capability (`setcap cap_sys_admin+ep`), and
/// this process drops it unless `LEA_ADMIN_PRIV=1` says otherwise. The
/// default is the safe one even on a binary that was given the capability.
fn settle_admin_privilege() {
    // capget/capset, _LINUX_CAPABILITY_VERSION_3. No crate for three fields.
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const VERSION_3: u32 = 0x2008_0522;
    const CAP_SYS_ADMIN: u32 = 21;

    let mut hdr = CapHeader { version: VERSION_3, pid: 0 };
    let mut data = [CapData::default(); 2];
    let r = unsafe {
        libc::syscall(libc::SYS_capget, &mut hdr as *mut _, data.as_mut_ptr())
    };
    if r != 0 {
        eprintln!("vhost-user-nvrm: capget failed -- assuming no privilege");
        return;
    }
    let bit = 1u32 << CAP_SYS_ADMIN;
    let have = data[0].effective & bit != 0;

    let wanted = matches!(std::env::var("LEA_ADMIN_PRIV").as_deref(), Ok("1"));
    if wanted {
        eprintln!(
            "vhost-user-nvrm: LEA_ADMIN_PRIV=1 -- keeping CAP_SYS_ADMIN ({}). \
             RM's PRIVILEGED controls are open to this process, and so is \
             everything else that capability covers.",
            if have { "present" } else { "NOT present -- setcap first" }
        );
        return;
    }
    if !have {
        return;
    }
    data[0].effective &= !bit;
    data[0].permitted &= !bit;
    let r = unsafe {
        libc::syscall(libc::SYS_capset, &hdr as *const _, data.as_ptr())
    };
    if r == 0 {
        eprintln!("vhost-user-nvrm: dropped CAP_SYS_ADMIN (LEA_ADMIN_PRIV unset)");
    } else {
        eprintln!("vhost-user-nvrm: WARNING: could not drop CAP_SYS_ADMIN");
    }
}

fn main() -> Result<()> {
    nvrm_sys::assert_driver_version();
    settle_admin_privilege();
    install_signal_notes();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, socket] if flag == "--nvrm" => {
            let _ = std::fs::remove_file(socket);
            nvrm::serve(socket)
        }
        _ => usage(),
    }
}
