// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Host daemon for the virtio-nvrm guest module.
//!
//! One process serves one VM. Forwarded RM calls use mirrored device FDs;
//! the driver ABI is selected at startup.

use anyhow::{Context, Result};

use vhost_user_nvrm::nvrm;

fn usage() -> ! {
    eprintln!(
        "vhost-user-nvrm --nvrm <socket>\n\
         \n\
         --nvrm:  virtio-nvrm device for cloud-hypervisor, guest driver is virtio_nvrm.ko"
    );
    std::process::exit(2);
}

/// Log termination using async-signal-safe calls, then restore the default
/// handler and re-raise to preserve the signal exit status.
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

/// Drop CAP_SYS_ADMIN unless explicitly enabled for driver diagnostics.
/// Failure to inspect or drop capabilities aborts startup.
fn settle_admin_privilege() -> Result<()> {
    // capget/capset, _LINUX_CAPABILITY_VERSION_3. No crate for three fields.
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    const VERSION_3: u32 = 0x2008_0522;

    let mut hdr = CapHeader {
        version: VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: version 3 requires a header and two initialized capability words.
    let r = unsafe { libc::syscall(libc::SYS_capget, &mut hdr as *mut _, data.as_mut_ptr()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("read backend capabilities");
    }
    let have = data[0].effective & ADMIN_BIT != 0;

    let wanted = matches!(std::env::var("LEA_ADMIN_PRIV").as_deref(), Ok("1"));
    if wanted {
        eprintln!(
            "vhost-user-nvrm: LEA_ADMIN_PRIV=1 -- keeping CAP_SYS_ADMIN ({}). \
             RM's PRIVILEGED controls are open to this process, and so is \
             everything else that capability covers.",
            if have {
                "present"
            } else {
                "NOT present -- setcap first"
            }
        );
        return Ok(());
    }
    if !clear_admin_capability(&mut data) {
        return Ok(());
    }
    // SAFETY: same ABI buffers as capget; only CAP_SYS_ADMIN bits were cleared.
    let r = unsafe { libc::syscall(libc::SYS_capset, &hdr as *const _, data.as_ptr()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("drop CAP_SYS_ADMIN");
    }
    eprintln!("vhost-user-nvrm: dropped CAP_SYS_ADMIN (LEA_ADMIN_PRIV unset)");
    Ok(())
}

const ADMIN_BIT: u32 = 1 << 21;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn clear_admin_capability(data: &mut [CapData; 2]) -> bool {
    let low = &mut data[0];
    let had_admin = (low.effective | low.permitted | low.inheritable) & ADMIN_BIT != 0;
    low.effective &= !ADMIN_BIT;
    low.permitted &= !ADMIN_BIT;
    low.inheritable &= !ADMIN_BIT;
    had_admin
}

fn main() -> Result<()> {
    // nvrm::serve selects the detected driver ABI and rejects unknown versions.
    settle_admin_privilege()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_admin_even_when_only_permitted_or_inheritable() {
        for capability in [
            CapData {
                effective: ADMIN_BIT,
                ..CapData::default()
            },
            CapData {
                permitted: ADMIN_BIT,
                ..CapData::default()
            },
            CapData {
                inheritable: ADMIN_BIT,
                ..CapData::default()
            },
        ] {
            let mut data = [capability, CapData::default()];
            assert!(clear_admin_capability(&mut data));
            assert_eq!(data, [CapData::default(); 2]);
            assert!(!clear_admin_capability(&mut data));
        }
    }

    #[test]
    fn retains_unrelated_capabilities() {
        let other = CapData {
            effective: 7,
            permitted: 15,
            inheritable: 3,
        };
        let mut data = [other; 2];
        data[0].permitted |= ADMIN_BIT;
        assert!(clear_admin_capability(&mut data));
        assert_eq!(data, [other; 2]);
    }
}
