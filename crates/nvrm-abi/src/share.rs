// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! RM object sharing within one backend process.
//!
//! UVM duplicates VASpaces, channels and memory into kernel clients while
//! handling the backend's ioctl. `RS_SHARE_TYPE_PID` permits these copies and
//! copies between guest sessions in the same backend. It compares client PIDs;
//! for a kernel destination it checks the calling process instead
//! (`cliresShareCallback_IMPL` in NVIDIA's `rmapi/client_resource.c`).
//!
//! A same-user policy would also let another VM's backend copy these objects.
//! The object-level PID grant survives libcuda replacing its inherited policy.
//! This grant alone is not a complete source-handle ownership check.

use crate::iowr_raw;
use std::os::fd::RawFd;

/// Classes duplicated by UVM with REJECT_KERNEL_DUP_PRIVILEGE, which
/// requires normal access rights even for a kernel client (`rs_client.c:542`).
///
/// - VASpace: REGISTER_GPU_VASPACE (`nv_gpu_ops.c:2752`).
/// - TSG and context share: REGISTER_CHANNEL (`nv_gpu_ops.c:10156,10265`).
///   The channel itself is looked up through CliGetKernelChannel.
/// - Memory: MAP_EXTERNAL_ALLOCATION (`uvm_ioctl.h:365`).
///
/// REGISTER_GPU's hSmcPartRef is zero without MIG (`uvm_ioctl.h:405`).
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

// Pure parameter builders; tests pin field offsets and constants.

/// `NV_PROC_NAME_MAX_LENGTH` (`nvlimits.h:47`).
const NAME_MAX: usize = 100;

/// `NV0000_CTRL_SET_SUB_PROCESS_ID_PARAMS` (`ctrl0000proc.h:56`):
/// subProcessID u32 @0, name char[100] @4. Reserve a trailing NUL for RM's
/// `portStringCopy`; shorter names retain zero padding.
fn sub_process_id_params(sub_id: u32, name: &str) -> [u8; 4 + NAME_MAX] {
    let mut params = [0u8; 4 + NAME_MAX];
    params[0..4].copy_from_slice(&sub_id.to_le_bytes());
    // Leave one byte for the NUL; portStringCopy expects it.
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
    params[12..14].copy_from_slice(&4u16.to_le_bytes()); // RS_SHARE_TYPE_PID (rs_access.h:246)
    params[14] = 1 << 2; // RS_SHARE_ACTION_FLAG_COMPOSE (rs_access.h:262)
    params
}

/// NVOS54 client control (`nvos.h`): hClient @0, hObject @4, cmd @8,
/// flags @12, params P64 @16, paramsSize @24, status @28; 32 bytes.
/// Both controls target the client itself. `params` must outlive the ioctl.
fn nvos54_client_control(hclient: u32, cmd: u32, params: &[u8]) -> [u8; 32] {
    let mut p = [0u8; 32];
    p[0..4].copy_from_slice(&hclient.to_le_bytes());
    p[4..8].copy_from_slice(&hclient.to_le_bytes());
    p[8..12].copy_from_slice(&cmd.to_le_bytes());
    p[16..24].copy_from_slice(&(params.as_ptr() as u64).to_le_bytes());
    p[24..28].copy_from_slice(&(params.len() as u32).to_le_bytes());
    p
}

/// Label a new RM client with `SET_SUB_PROCESS_ID` (0x901, ctrl0000proc.h).
///
/// RM groups USERD pages by domain, processID and subProcessID
/// (`kernel_fifo.c:508`). Guest sessions share a host PID, so the guest's
/// dense process ID distinguishes their USERD groups. It is guest-supplied
/// attribution, not a host security boundary; recycled host/guest PIDs must
/// not substitute for the dense ID.
///
/// Returns `(ioctl return, RM status)`. Failure leaves the default label 0.
///
/// # Safety
/// `fd` must be an open NVIDIA ctl or GPU node on which `hclient` was allocated.
pub unsafe fn set_sub_process_id(fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32) {
    let params = sub_process_id_params(sub_id, name);
    let mut p = nvos54_client_control(hclient, 0x901, &params);

    let ret = libc::ioctl(
        fd,
        iowr_raw(crate::sys::NV_ESC_RM_CONTROL, p.len() as u32) as libc::Ioctl,
        p.as_mut_ptr() as *mut libc::c_void,
    );
    let st = u32::from_le_bytes(p[28..32].try_into().unwrap());
    (ret, st)
}

/// Grant DUP_OBJECT to clients in the backend process, including UVM's kernel
/// client when it handles a call from that process.
///
/// Use an object policy: libcuda can replace the client's inherited policy
/// (`rsAccessGetActiveShareList` selects the nearest modified object list).
/// PID sharing avoids granting access to other backends running as the same
/// user. The PID comes from the source client; `target` is unused by this policy.
///
/// Returns `(ioctl return, RM status)`. A failed grant may make a later UVM
/// operation fail with `NV_ERR_INSUFFICIENT_PERMISSIONS`.
///
/// # Safety
/// `fd` must be an open `/dev/nvidiactl` (or per-GPU node) that `hclient`
/// was allocated on.
pub unsafe fn grant_dup_same_process(fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
    let params = share_object_params(hobject);
    // NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT (ctrl0000client.h:157).
    let mut p = nvos54_client_control(hclient, 0x0d06, &params);

    let ret = libc::ioctl(
        fd,
        iowr_raw(crate::sys::NV_ESC_RM_CONTROL, p.len() as u32) as libc::Ioctl,
        p.as_mut_ptr() as *mut libc::c_void,
    );
    let st = u32::from_le_bytes(p[28..32].try_into().unwrap());
    (ret, st)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The USERD attribution field is a little-endian u32 at offset 0.
    #[test]
    fn the_sub_process_id_is_a_little_endian_u32_at_offset_zero() {
        let p = sub_process_id_params(0x0403_0201, "");
        assert_eq!(&p[0..4], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(p.len(), 104, "4 + NV_PROC_NAME_MAX_LENGTH");
        assert_eq!(sub_process_id_params(0, "")[0..4], [0, 0, 0, 0]);
    }

    /// The full name field is sent, including its zero-filled tail.
    #[test]
    fn a_short_name_is_nul_padded_to_the_end_of_the_field() {
        let p = sub_process_id_params(7, "vectorAdd");
        assert_eq!(&p[4..13], b"vectorAdd");
        assert!(p[13..].iter().all(|&b| b == 0), "the tail must stay zero");
    }

    /// Names occupy at most 99 bytes, leaving the 100th byte NUL.
    #[test]
    fn a_long_name_is_truncated_leaving_room_for_the_nul() {
        let long = "x".repeat(500);
        let p = sub_process_id_params(1, &long);
        assert_eq!(&p[4..4 + 99], "x".repeat(99).as_bytes());
        assert_eq!(
            p[4 + 99],
            0,
            "the last byte of the name field must stay NUL"
        );
        // Exactly at the boundary: 100 characters still lose one.
        let p = sub_process_id_params(1, &"y".repeat(NAME_MAX));
        assert_eq!(p[4 + NAME_MAX - 1], 0);
        assert_eq!(&p[4..4 + NAME_MAX - 1], "y".repeat(NAME_MAX - 1).as_bytes());
        // One below the boundary fits whole.
        let p = sub_process_id_params(1, &"z".repeat(NAME_MAX - 1));
        assert_eq!(&p[4..4 + NAME_MAX - 1], "z".repeat(NAME_MAX - 1).as_bytes());
        assert_eq!(p[4 + NAME_MAX - 1], 0);
    }

    /// Pin the DUP access mask, PID policy, COMPOSE action and padding.
    #[test]
    fn the_share_policy_grants_dup_only_to_the_source_process() {
        let p = share_object_params(0xcafe_1234);
        assert_eq!(
            u32::from_le_bytes(p[0..4].try_into().unwrap()),
            0xcafe_1234,
            "hObject @0"
        );
        assert_eq!(
            u32::from_le_bytes(p[4..8].try_into().unwrap()),
            0,
            "target @4 stays 0"
        );
        // accessMask.limbs[0]: bit 0 == RS_ACCESS_DUP_OBJECT.
        assert_eq!(
            u32::from_le_bytes(p[8..12].try_into().unwrap()),
            1,
            "accessMask @8"
        );
        // RS_SHARE_TYPE_PID == 4. OS_SECURITY_TOKEN (2) also admits other
        // backends running as the same user.
        assert_eq!(
            u16::from_le_bytes(p[12..14].try_into().unwrap()),
            4,
            "type @12 must scope access to the source process"
        );
        // RS_SHARE_ACTION_FLAG_COMPOSE == 1 << 2. Without COMPOSE the
        // grant would REPLACE the list libcuda sets up moments later.
        assert_eq!(p[14], 0b100, "action @14");
        assert_eq!(p[15], 0, "the byte after action is padding");
        assert_eq!(p.len(), 16);
    }

    /// Both controls target the client object: hObject must equal hClient.
    #[test]
    fn the_control_block_addresses_the_client_object_itself() {
        let params = [0u8; 16];
        let p = nvos54_client_control(0x1234_5678, 0x0d06, &params);
        assert_eq!(
            u32::from_le_bytes(p[0..4].try_into().unwrap()),
            0x1234_5678,
            "hClient @0"
        );
        assert_eq!(
            u32::from_le_bytes(p[4..8].try_into().unwrap()),
            0x1234_5678,
            "hObject @4"
        );
        assert_eq!(
            u32::from_le_bytes(p[8..12].try_into().unwrap()),
            0x0d06,
            "cmd @8"
        );
        assert_eq!(
            u32::from_le_bytes(p[12..16].try_into().unwrap()),
            0,
            "flags @12 stays 0"
        );
        assert_eq!(
            u64::from_le_bytes(p[16..24].try_into().unwrap()),
            params.as_ptr() as u64,
            "params P64 @16"
        );
        assert_eq!(
            u32::from_le_bytes(p[24..28].try_into().unwrap()),
            16,
            "paramsSize @24"
        );
        assert_eq!(
            u32::from_le_bytes(p[28..32].try_into().unwrap()),
            0,
            "status @28 starts 0"
        );
        assert_eq!(
            p.len(),
            32,
            "NVOS54 is 32 bytes -- the _IOC size the driver checks"
        );
    }

    /// Each control must use its own parameter size for RM copying.
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

        // The host assigns 0x901 itself and refuses guest attempts to replace it.
        assert!(
            crate::xlate::ctrl_blocked(0x901),
            "0x901 must stay on the blocked list"
        );
        assert!(!crate::xlate::ctrl_blocked(0x0d06));
    }
}
