// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Cross-process DUP_OBJECT grants.
//!
//! RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN; UVM is its unified-memory driver
//! (/dev/nvidia-uvm). DUP_OBJECT is the access right RM checks when one
//! client duplicates another client's object.
//!
//! Why this exists. The RM client is allocated by vhost-user-nvrm, so
//! `RmClient::ProcID` is the daemon's. UVM however runs natively in the
//! guest, because it binds its va_space to the caller's mm
//! (`uvm_va_space_mm.c:195`) and every vma underneath must come from that mm
//! (`uvm.c:782-788`). UVM therefore duplicates RM objects into its own
//! kernel client on behalf of the GUEST process, and RM's default share
//! policy grants DUP_OBJECT only to the client's own process
//! (`sharing.c:344-353`, RS_SHARE_TYPE_PID). Result without a grant:
//! NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b, `rs_access_map.c:205`).
//!
//! Moving the client to the guest instead does not work: RmCreateMmapContext
//! requires `ProcID == osGetCurrentProcess()` (`osapi.c:2540-2542`) and
//! NV_ESC_RM_MAP_MEMORY is forwarded, i.e. runs in the daemon. Measured: a
//! guest-owned client makes 0x4e fail with NV_ERR_INVALID_CLIENT (0x23)
//! where a client owned by the calling process returns 0x0. One client
//! cannot belong to two processes, so the grant has to go the other way.
//!
//! Across the VM boundary the host runs every ioctl itself and no
//! cross-process dup happens there. `grant_dup_same_user` below is still
//! live: session.rs calls it for the classes UVM duplicates (VASpace,
//! TSG, ctxshare, memory), and host_pool.rs for the OS descriptor.
//! (A fresh ROOT_CLIENT takes the other door, `set_sub_process_id`.)

use crate::iowr_raw;
use std::os::fd::RawFd;

/// Classes whose objects UVM duplicates on behalf of the calling process.
/// Each has a DupObject in the RM sources, all of them passing
/// NV04_DUP_HANDLE_FLAGS_REJECT_KERNEL_DUP_PRIVILEGE, so they take the
/// access-rights path even though UVM's client is a kernel one
/// (`rs_client.c:542-552`):
///
/// - UVM_REGISTER_GPU_VASPACE (`uvm_ioctl.h:290-297`) dups hVaSpace
///   (`nv_gpu_ops.c:2752-2759`).
/// - UVM_REGISTER_CHANNEL (`:315-324`) dups the channel's TSG
///   (`nv_gpu_ops.c:10156-10162`) and its context share (`:10265-10271`) —
///   not the channel object itself, which is only looked up via
///   CliGetKernelChannel.
/// - UVM_MAP_EXTERNAL_ALLOCATION (`:365-378`) dups hMemory.
///
/// hSmcPartRef from UVM_REGISTER_GPU (`:405-416`) is 0 without MIG.
pub fn uvm_dupes_class(hclass: u32) -> bool {
    matches!(
        hclass,
        0x90f1                                  // FERMI_VASPACE_A
            | 0xa06c                            // KEPLER_CHANNEL_GROUP_A (TSG)
            | 0x9067                            // FERMI_CONTEXT_SHARE_A
            | 0x003e | 0x0040 | 0x0071 | 0x50a0 // NV*_MEMORY* (sysmem, vidmem,
                                                // os descriptor, virtual)
    )
}

// ---------------------------------------------------------------------------
// Parameter blocks
// ---------------------------------------------------------------------------
// The three builders below are pure: no fd, no ioctl, no driver -- which is
// what makes the bytes that reach RM testable at all.
//
// Invariant: they must not change a single byte on the wire relative to the
// escapes they feed. Every field offset and constant is pinned by the tests
// below; the arrays are built by
// a named function and returned by value.

/// `NV_PROC_NAME_MAX_LENGTH` (`nvlimits.h:47`).
const NAME_MAX: usize = 100;

/// `NV0000_CTRL_SET_SUB_PROCESS_ID_PARAMS` (`ctrl0000proc.h:56-59`):
/// subProcessID u32 @0, subProcessName char[NV_PROC_NAME_MAX_LENGTH] @4.
///
/// RM copies the name with `portStringCopy`, which needs a NUL terminator,
/// so a name of NAME_MAX bytes or longer is truncated to NAME_MAX - 1 and
/// the last byte stays zero. Anything shorter is NUL-padded to the end of
/// the fixed-size field, because the whole 104-byte block is sent.
fn sub_process_id_params(sub_id: u32, name: &str) -> [u8; 4 + NAME_MAX] {
    let mut params = [0u8; 4 + NAME_MAX];
    params[0..4].copy_from_slice(&sub_id.to_le_bytes());
    // Leave one byte for the NUL -- portStringCopy expects it.
    let n = name.len().min(NAME_MAX - 1);
    params[4..4 + n].copy_from_slice(&name.as_bytes()[..n]);
    params
}

/// `NV0000_CTRL_CLIENT_SHARE_OBJECT_PARAMS` (`ctrl0000client.h:159-162`):
/// hObject @0, then RS_SHARE_POLICY (`rs_access.h:268-273`):
/// target @4, accessMask.limbs[0] @8 (SDK_RS_ACCESS_MAX_LIMBS == 1,
/// RsAccessLimb == NvU32), type u16 @12, action u8 @14 -> 16 bytes.
fn share_object_params(hobject: u32) -> [u8; 16] {
    let mut params = [0u8; 16];
    params[0..4].copy_from_slice(&hobject.to_le_bytes());
    params[8..12].copy_from_slice(&(1u32 << 0).to_le_bytes()); // RS_ACCESS_DUP_OBJECT == 0 (rs_access.h:59)
    params[12..14].copy_from_slice(&2u16.to_le_bytes()); // RS_SHARE_TYPE_OS_SECURITY_TOKEN (rs_access.h:244)
    params[14] = 1 << 2; // RS_SHARE_ACTION_FLAG_COMPOSE (rs_access.h:262)
    params
}

/// The NVOS54 block (the RM_CONTROL parameter block from `nvos.h`) for a
/// control aimed at the client object itself: hClient @0, hObject @4,
/// cmd @8, flags @12, params P64 @16, paramsSize @24, status @28 -> 32
/// bytes.
///
/// Both controls here target `RmClientResource`, i.e. the client itself,
/// hence hObject == hClient. `params` must outlive the ioctl -- the block
/// stores a pointer to it, not a copy.
fn nvos54_client_control(hclient: u32, cmd: u32, params: &[u8]) -> [u8; 32] {
    let mut p = [0u8; 32];
    p[0..4].copy_from_slice(&hclient.to_le_bytes());
    p[4..8].copy_from_slice(&hclient.to_le_bytes());
    p[8..12].copy_from_slice(&cmd.to_le_bytes());
    p[16..24].copy_from_slice(&(params.as_ptr() as u64).to_le_bytes());
    p[24..28].copy_from_slice(&(params.len() as u32).to_le_bytes());
    p
}

/// Tell RM which GUEST process a freshly allocated client belongs to, via
/// `NV0000_CTRL_CMD_SET_SUB_PROCESS_ID` (0x901, `ctrl0000proc.h:93`).
///
/// Why this matters beyond bookkeeping: RM decides USERD page sharing on
/// exactly three fields — `domain`, `processID`, `subProcessID`
/// (`kernel_fifo.c:508-511`). Every guest process of a VM lives inside the
/// same host process (this daemon), so `processID` is identical for all of
/// them; without a `subProcessID` their USERD pages may share a physical
/// page, which cannot happen between two native processes. Setting the ID
/// restores that separation inside the VM. RM's own wording for the field is
/// "In vGPU environment, sub process means the guest user/kernel process
/// running within a single VM" (`ctrl0000proc.h:37-50`) — exactly our shape.
///
/// The ID is a dense number the GUEST MODULE hands out per device, not a raw
/// tgid: a recycled pid would attribute a dead process's allocations to a new
/// one. `NV_PROC_NAME_MAX_LENGTH` is 100 (`nvlimits.h:47`); RM copies with
/// `portStringCopy`, so the buffer must be NUL-terminated.
///
/// WARNING: This is attribution and RM-side isolation INSIDE one VM. It is a
/// label the guest supplied, so no host-side enforcement may rest on it —
/// the only boundary the host can enforce is the VM itself.
///
/// Returns `(ioctl return, RM status)`; callers log. A failure leaves the
/// client at `subProcessID = 0`, i.e. exactly today's behaviour.
///
/// # Safety
/// `fd` must be an open `/dev/nvidiactl` (or per-GPU node) that `hclient`
/// was allocated on.
pub unsafe fn set_sub_process_id(fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32) {
    let params = sub_process_id_params(sub_id, name);
    let mut p = nvos54_client_control(hclient, 0x901, &params);

    let ret = libc::ioctl(
        fd,
        iowr_raw(crate::sys::NV_ESC_RM_CONTROL, p.len() as u32) as libc::c_ulong,
        p.as_mut_ptr() as *mut libc::c_void,
    );
    let st = u32::from_le_bytes(p[28..32].try_into().unwrap());
    (ret, st)
}

/// Grant DUP_OBJECT on `hobject` to other processes of the same user, via
/// NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT on `fd`.
///
/// The grant goes on the OBJECT, not on the client, on purpose: libcuda
/// issues its own SET_INHERITED_SHARE_POLICY (0xd04) a few calls after
/// creating the client, and a policy without COMPOSE clears the entire list
/// (`clientShareResource_IMPL:233-236`) — a client-level grant would be
/// wiped. `rsAccessGetActiveShareList` returns the first modified list
/// walking UP from the object (`rs_access_map.c:430-452`), so an
/// object-level list wins and is independent of that ordering.
///
/// OS_SECURITY_TOKEN means "same euid", not "anyone": osValidateClientTokens
/// compares euid or pid (`os.c:4086-4107`).
///
/// Returns `(ioctl return, RM status)`. Callers log; a failure here is not
/// fatal, the later dup then fails visibly with 0x1b instead of silently.
///
/// # Safety
/// `fd` must be an open `/dev/nvidiactl` (or per-GPU node) that `hclient`
/// was allocated on.
pub unsafe fn grant_dup_same_user(fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
    let params = share_object_params(hobject);
    // NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT (ctrl0000client.h:157).
    let mut p = nvos54_client_control(hclient, 0x0d06, &params);

    let ret = libc::ioctl(
        fd,
        iowr_raw(crate::sys::NV_ESC_RM_CONTROL, p.len() as u32) as libc::c_ulong,
        p.as_mut_ptr() as *mut libc::c_void,
    );
    let st = u32::from_le_bytes(p[28..32].try_into().unwrap());
    (ret, st)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `subProcessID` is a plain little-endian u32 at offset 0 of
    /// `NV0000_CTRL_SET_SUB_PROCESS_ID_PARAMS`. RM keys USERD page sharing
    /// on this number (`kernel_fifo.c:508-511`), so a byte-swapped or
    /// misplaced value would put two guest processes into the same
    /// isolation bucket -- silently, since RM has no way to notice.
    #[test]
    fn the_sub_process_id_is_a_little_endian_u32_at_offset_zero() {
        let p = sub_process_id_params(0x0403_0201, "");
        assert_eq!(&p[0..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(p.len(), 104, "4 + NV_PROC_NAME_MAX_LENGTH");
        assert_eq!(sub_process_id_params(0, "")[0..4], [0, 0, 0, 0]);
    }

    /// A short name sits at offset 4 and the rest of the fixed-size field
    /// stays zero. The whole 104-byte block is sent, so whatever is not
    /// written is what RM reads.
    #[test]
    fn a_short_name_is_nul_padded_to_the_end_of_the_field() {
        let p = sub_process_id_params(7, "vectorAdd");
        assert_eq!(&p[4..13], b"vectorAdd");
        assert!(p[13..].iter().all(|&b| b == 0), "the tail must stay zero");
    }

    /// `NV_PROC_NAME_MAX_LENGTH` is 100 and RM copies the name with
    /// `portStringCopy`, which walks to a NUL. A name that filled all 100
    /// bytes would leave the buffer unterminated and RM would read past it,
    /// so the name is truncated to 99 BYTES and byte 103 stays zero.
    #[test]
    fn a_long_name_is_truncated_leaving_room_for_the_nul() {
        let long = "x".repeat(500);
        let p = sub_process_id_params(1, &long);
        assert_eq!(&p[4..4 + 99], "x".repeat(99).as_bytes());
        assert_eq!(p[4 + 99], 0, "the last byte of the name field must stay NUL");
        // Exactly at the boundary: 100 characters still lose one.
        let p = sub_process_id_params(1, &"y".repeat(NAME_MAX));
        assert_eq!(p[4 + NAME_MAX - 1], 0);
        assert_eq!(&p[4..4 + NAME_MAX - 1], "y".repeat(NAME_MAX - 1).as_bytes());
        // One below the boundary fits whole.
        let p = sub_process_id_params(1, &"z".repeat(NAME_MAX - 1));
        assert_eq!(&p[4..4 + NAME_MAX - 1], "z".repeat(NAME_MAX - 1).as_bytes());
        assert_eq!(p[4 + NAME_MAX - 1], 0);
    }

    /// `NV0000_CTRL_CLIENT_SHARE_OBJECT_PARAMS` field by field. Every one
    /// of the four values is a magic number from the RM headers, and a
    /// wrong one does not fail loudly: the grant is simply not the grant
    /// that was meant, and the later DUP_OBJECT fails with
    /// NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b) far away from here.
    #[test]
    fn the_share_policy_names_dup_object_for_the_same_security_token() {
        let p = share_object_params(0xcafe_1234);
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), 0xcafe_1234, "hObject @0");
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 0, "target @4 stays 0");
        // accessMask.limbs[0]: bit 0 == RS_ACCESS_DUP_OBJECT.
        assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), 1, "accessMask @8");
        // RS_SHARE_TYPE_OS_SECURITY_TOKEN == 2, a u16 -- "same euid", not
        // "anyone".
        assert_eq!(u16::from_le_bytes(p[12..14].try_into().unwrap()), 2, "type @12");
        // RS_SHARE_ACTION_FLAG_COMPOSE == 1 << 2. Without COMPOSE the
        // grant would REPLACE the list libcuda sets up moments later.
        assert_eq!(p[14], 0b100, "action @14");
        assert_eq!(p[15], 0, "the byte after action is padding");
        assert_eq!(p.len(), 16);
    }

    /// The NVOS54 block both calls send: the control targets the client
    /// object itself, so hObject must repeat hClient rather than name the
    /// object being shared. Sending the object handle in hObject routes the
    /// control to the wrong resource and RM answers with a status.
    #[test]
    fn the_control_block_addresses_the_client_object_itself() {
        let params = [0u8; 16];
        let p = nvos54_client_control(0x1234_5678, 0x0d06, &params);
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), 0x1234_5678, "hClient @0");
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 0x1234_5678, "hObject @4");
        assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), 0x0d06, "cmd @8");
        assert_eq!(u32::from_le_bytes(p[12..16].try_into().unwrap()), 0, "flags @12 stays 0");
        assert_eq!(
            u64::from_le_bytes(p[16..24].try_into().unwrap()),
            params.as_ptr() as u64,
            "params P64 @16"
        );
        assert_eq!(u32::from_le_bytes(p[24..28].try_into().unwrap()), 16, "paramsSize @24");
        assert_eq!(u32::from_le_bytes(p[28..32].try_into().unwrap()), 0, "status @28 starts 0");
        assert_eq!(p.len(), 32, "NVOS54 is 32 bytes -- the _IOC size the driver checks");
    }

    /// The two commands and their parameter sizes, as the two callers pair
    /// them up. `paramsSize` is what RM's `copy_from_user` uses: pairing
    /// 0x901 with the 16-byte block (or 0x0d06 with the 104-byte one) would
    /// hand RM a block of the wrong length for the struct it expects.
    #[test]
    fn each_command_travels_with_its_own_parameter_block() {
        // NV0000_CTRL_CMD_SET_SUB_PROCESS_ID (0x901, ctrl0000proc.h:93).
        let params = sub_process_id_params(3, "guest");
        let p = nvos54_client_control(0xaa, 0x901, &params);
        assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), 0x901);
        assert_eq!(u32::from_le_bytes(p[24..28].try_into().unwrap()), 104);

        // NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT (0x0d06, ctrl0000client.h:157).
        let params = share_object_params(0xbb);
        let p = nvos54_client_control(0xaa, 0x0d06, &params);
        assert_eq!(u32::from_le_bytes(p[8..12].try_into().unwrap()), 0x0d06);
        assert_eq!(u32::from_le_bytes(p[24..28].try_into().unwrap()), 16);

        // 0x901 is the one control the host BLOCKS from the guest (it is a
        // host-assigned label); the block lives in xlate, and this is the
        // one place that still sends it -- from the host itself.
        assert!(crate::xlate::ctrl_blocked(0x901), "0x901 must stay on the blocked list");
        assert!(!crate::xlate::ctrl_blocked(0x0d06));
    }
}
