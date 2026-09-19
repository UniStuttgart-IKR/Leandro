// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Validate request envelopes and translation metadata before acquiring resources.
//! This covers known descriptors; unannotated control/class fields need an audit.

use nvrm_abi::{
    sys,
    xlate::{self, Dev},
};
use nvrm_wire::{Req, NONE_U32, NONE_U64};

#[derive(Debug)]
pub(crate) struct Error {
    pub errno: i32,
    pub why: &'static str,
}

type Result<T> = std::result::Result<T, Error>;

fn invalid(why: &'static str) -> Error {
    Error {
        errno: libc::EINVAL,
        why,
    }
}

fn unsupported(why: &'static str) -> Error {
    Error {
        errno: libc::ENOTSUP,
        why,
    }
}

#[derive(Debug, Default)]
pub(crate) struct Shape {
    zero_inline: Option<usize>,
    zero_aux: Vec<usize>,
}

impl Shape {
    /// Zero-length pointers were not gathered by the guest. Do not forward
    /// their raw addresses, even though the driver should ignore them.
    pub fn clear_unused_pointers(&self, inline: &mut [u8], aux: &mut [u8]) {
        if let Some(off) = self.zero_inline {
            inline[off..off + 8].fill(0);
        }
        for &off in &self.zero_aux {
            aux[off..off + 8].fill(0);
        }
    }
}

fn u32_at(bytes: &[u8], off: usize) -> Result<u32> {
    bytes
        .get(off..off + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| invalid("descriptor field outside its parameter buffer"))
}

fn u64_at(bytes: &[u8], off: usize) -> Result<u64> {
    bytes
        .get(off..off + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| invalid("descriptor field outside its parameter buffer"))
}

fn fd_field(
    bytes: &[u8],
    expected: Option<(u32, usize)>,
    off: u32,
    token: u64,
    owner: u32,
) -> Result<()> {
    let Some((expected_off, width)) = expected else {
        return if off == NONE_U32 && token == NONE_U64 && owner == NONE_U32 {
            Ok(())
        } else {
            Err(invalid("unexpected FD translation metadata"))
        };
    };
    if off != expected_off {
        return Err(invalid("missing or misplaced FD translation metadata"));
    }
    let negative = match width {
        4 => (u32_at(bytes, off as usize)? as i32) < 0,
        8 => {
            let raw = u64_at(bytes, off as usize)? as i64;
            // RM narrows NvP64 event handles to NvU32. Upper bits must not
            // disguise a nonnegative host FD as a negative guest sentinel.
            raw < 0 && raw == raw as i32 as i64
        }
        _ => unreachable!("descriptor FD width"),
    };
    // Kernel forwarding can supply a resolved token even when the raw field
    // was a negative placeholder. Only an absent token requires a sentinel.
    if token == NONE_U64 && (!negative || owner != NONE_U32) {
        return Err(invalid("nonnegative FD requires a resolved guest token"));
    }
    Ok(())
}

pub(crate) fn validate<A: sys::RmAbi>(
    dev: Dev,
    req: &Req,
    inline: &[u8],
    aux: &[u8],
) -> Result<Shape> {
    let mut shape = Shape::default();
    let nr = req.ioctl_nr;
    if inline.len() != req.inline_len as usize || aux.len() != req.aux_len as usize {
        return Err(invalid("request lengths do not match its buffers"));
    }
    if dev == Dev::UvmTools {
        return Err(unsupported(
            "UVM tools forwarding has no request descriptors",
        ));
    }
    if dev == Dev::Uvm {
        let size =
            xlate::uvm_param_size_for::<A>(nr).ok_or_else(|| unsupported("unknown UVM request"))?;
        if req.inline_len != size {
            return Err(invalid("UVM inline size differs from the host ABI"));
        }
    } else {
        if req.inline_len >= 1 << 14 {
            return Err(Error {
                errno: libc::EMSGSIZE,
                why: "frontend request exceeds _IOC size",
            });
        }
        let size =
            xlate::frontend_size(nr).ok_or_else(|| unsupported("unsupported frontend request"))?;
        if !size.accepts(req.inline_len) {
            return Err(invalid("frontend inline size differs from the host ABI"));
        }
    }

    fd_field(
        inline,
        xlate::fd_field_offset(dev, nr, req.inline_len).map(|o| (o, 4)),
        req.fd_field_off,
        req.fd_field_token,
        req.fd_field_proc,
    )?;

    let alloc = !dev.is_uvm() && nr == sys::NV_ESC_RM_ALLOC;
    let control = !dev.is_uvm() && nr == sys::NV_ESC_RM_CONTROL;
    if !dev.is_uvm()
        && matches!(
            nr,
            sys::NV_ESC_RM_ALLOC | sys::NV_ESC_RM_ALLOC_OBJECT | sys::NV_ESC_RM_ALLOC_MEMORY
        )
        && xlate::alloc_class_blocked(u32_at(inline, 12)?)
    {
        return Err(unsupported("allocation capability FD is not translated"));
    }
    if control && xlate::ctrl_blocked(u32_at(inline, 8)?) {
        return Err(Error {
            errno: libc::EPERM,
            why: "control is not forwarded",
        });
    }
    if control && u32_at(inline, 12)? & sys::NVOS54_FLAGS_FINN_SERIALIZED != 0 {
        return Err(unsupported(
            "serialized controls have no translation layout",
        ));
    }
    if alloc
        && inline.len() == std::mem::size_of::<sys::NVOS64_PARAMETERS>()
        && u32_at(inline, 36)? & sys::NVOS64_FLAGS_FINN_SERIALIZED != 0
    {
        return Err(unsupported(
            "serialized allocations have no translation layout",
        ));
    }
    if !dev.is_uvm()
        && nr == sys::NV_ESC_RM_VID_HEAP_CONTROL
        && matches!(
            u32_at(inline, 8)?,
            sys::NVOS32_FUNCTION_HW_ALLOC | sys::NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR
        )
    {
        return Err(unsupported(
            "VID_HEAP callback/OS-descriptor parameters are not translated",
        ));
    }
    if alloc
        && inline.len() == std::mem::size_of::<sys::NVOS64_PARAMETERS>()
        && u64_at(inline, 24)? != 0
    {
        return Err(unsupported("pRightsRequested is not translated"));
    }
    if control && u64_at(inline, 16)? != 0 && u32_at(inline, 24)? == 0 {
        shape.zero_inline = Some(16);
    }

    // SAFETY: inline has exactly req.inline_len initialized bytes; the helper
    // checks the envelope before reading its pointer and size fields.
    let embedded = unsafe { xlate::embedded_ptr::<A>(dev, nr, inline.as_ptr(), req.inline_len) }
        .map_err(|_| unsupported("unknown allocation parameter layout"))?;
    let params_len = match embedded {
        Some(e) => {
            if req.embedded_ptr_off != e.ptr_off || e.len as usize > aux.len() {
                return Err(invalid(
                    "missing embedded pointer metadata or truncated parameters",
                ));
            }
            e.len as usize
        }
        None => {
            if req.embedded_ptr_off != NONE_U32 {
                return Err(invalid("unexpected embedded pointer metadata"));
            }
            0
        }
    };
    let params = &aux[..params_len];

    let osdesc = !dev.is_uvm()
        && (alloc || nr == sys::NV_ESC_RM_ALLOC_MEMORY)
        && u32_at(inline, 12)? == sys::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
    if req.gpa_run_count != 0 {
        if !osdesc || (alloc && inline.len() != std::mem::size_of::<sys::NVOS64_PARAMETERS>()) {
            return Err(invalid("GPA runs require an OS-descriptor allocation"));
        }
        let run_bytes = (req.gpa_run_count as usize)
            .checked_mul(16)
            .ok_or_else(|| invalid("GPA run length overflow"))?;
        if params_len.checked_add(run_bytes) != Some(aux.len()) {
            return Err(invalid("GPA run bytes differ from the declared count"));
        }
    } else if osdesc {
        return Err(unsupported(
            "OS-descriptor allocation requires pinned GPA runs",
        ));
    } else if params_len == 0 && !aux.is_empty() {
        return Err(invalid("aux buffer has no host-described target"));
    }

    let aux_fd = if alloc && params_len != 0 {
        let class = u32_at(inline, 12)?;
        let enabled = match xlate::alloc_fd_guard(class) {
            Some((off, value)) => u32_at(params, off as usize)? == value,
            None => true,
        };
        xlate::alloc_fd_field(class)
            .filter(|_| enabled)
            .map(|off| (off, 8))
    } else if control && params_len != 0 {
        xlate::ctrl_fd_offset(u32_at(inline, 8)?).map(|off| (off, 4))
    } else {
        None
    };
    fd_field(
        params,
        aux_fd,
        req.aux_fd_field_off,
        req.aux_fd_field_token,
        req.aux_fd_field_proc,
    )?;

    if req.nested_count as usize > req.nested.len() {
        return Err(invalid("nested_count exceeds the wire descriptor array"));
    }
    let specs = if control && params_len != 0 {
        xlate::nested_ptrs(u32_at(inline, 8)?)
    } else {
        &[]
    };
    let mut used = 0;
    let mut ranges = Vec::new();
    for spec in specs {
        let pointer = u64_at(params, spec.ptr_off as usize)?;
        // SAFETY: params is limited to the host-derived primary buffer length.
        let len = unsafe { spec.len.resolve(params.as_ptr(), params.len() as u32) }
            .ok_or_else(|| invalid("nested length exceeds the parameter layout"))?;
        if pointer == 0 || len == 0 {
            if pointer != 0 {
                shape.zero_aux.push(spec.ptr_off as usize);
            }
            continue;
        }
        let d = req
            .nested
            .get(used)
            .filter(|_| used < req.nested_count as usize)
            .ok_or_else(|| invalid("missing nested pointer translation metadata"))?;
        if d.ptr_off != spec.ptr_off || d.len < len {
            return Err(invalid("nested descriptor differs from the host layout"));
        }
        let start = d.aux_off as usize;
        let end = start
            .checked_add(d.len as usize)
            .ok_or_else(|| invalid("nested buffer length overflow"))?;
        if start < params_len
            || end > aux.len()
            || ranges.iter().any(|&(a, b)| start < b && a < end)
        {
            return Err(invalid("nested buffers overlap or extend outside aux"));
        }
        ranges.push((start, end));
        used += 1;
    }
    if used != req.nested_count as usize {
        return Err(invalid("unexpected nested pointer translation metadata"));
    }
    Ok(shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvrm_wire::NestedDesc;

    fn control(cmd: u32, params_len: usize, total_aux: usize) -> (Req, Vec<u8>, Vec<u8>) {
        let req = Req {
            ioctl_nr: sys::NV_ESC_RM_CONTROL,
            inline_len: 32,
            aux_len: total_aux as u32,
            embedded_ptr_off: 16,
            ..Req::default()
        };
        let mut inline = vec![0; 32];
        inline[8..12].copy_from_slice(&cmd.to_le_bytes());
        inline[16..24].copy_from_slice(&0x1234_0000u64.to_le_bytes());
        inline[24..28].copy_from_slice(&(params_len as u32).to_le_bytes());
        (req, inline, vec![0; total_aux])
    }

    fn check(req: &Req, inline: &[u8], aux: &[u8]) -> Result<Shape> {
        validate::<sys::DefaultAbi>(Dev::Ctl, req, inline, aux)
    }

    #[test]
    fn nonnull_primary_pointers_require_the_host_descriptor() {
        let (mut req, inline, aux) = control(0x2080_0101, 16, 16);
        assert!(check(&req, &inline, &aux).is_ok());
        req.embedded_ptr_off = NONE_U32;
        assert!(check(&req, &inline, &aux).is_err());
        req.embedded_ptr_off = 8;
        assert!(check(&req, &inline, &aux).is_err());
        req.embedded_ptr_off = 16;
        req.aux_len = 8;
        assert!(check(&req, &inline, &aux[..8]).is_err());
    }

    #[test]
    fn optional_null_and_zero_length_pointers_remain_supported() {
        let (mut req, mut inline, mut aux) = control(0x2080_0101, 0, 0);
        req.embedded_ptr_off = NONE_U32;
        let shape = check(&req, &inline, &aux).unwrap();
        shape.clear_unused_pointers(&mut inline, &mut aux);
        assert_eq!(u64_at(&inline, 16).unwrap(), 0);
        inline[24..28].copy_from_slice(&4096u32.to_le_bytes());
        assert!(
            check(&req, &inline, &aux).is_ok(),
            "NULL with a size is the driver's query/error case"
        );
    }

    #[test]
    fn null_alloc_params_do_not_bypass_the_rights_pointer_guard() {
        let req = Req {
            ioctl_nr: sys::NV_ESC_RM_ALLOC,
            inline_len: 48,
            ..Req::default()
        };
        let mut inline = vec![0; 48];
        inline[12..16].copy_from_slice(&0xffffu32.to_le_bytes());
        assert!(
            check(&req, &inline, &[]).is_ok(),
            "parameterless classes need no size row"
        );
        inline[24..32].copy_from_slice(&0x1234_0000u64.to_le_bytes());
        assert_eq!(check(&req, &inline, &[]).unwrap_err().errno, libc::ENOTSUP);
    }

    #[test]
    fn native_descriptors_cannot_validate_serialized_parameters() {
        let (req, mut inline, aux) = control(0x2080_0101, 16, 16);
        inline[12..16].copy_from_slice(&sys::NVOS54_FLAGS_FINN_SERIALIZED.to_le_bytes());
        assert_eq!(check(&req, &inline, &aux).unwrap_err().errno, libc::ENOTSUP);

        let req = Req {
            ioctl_nr: sys::NV_ESC_RM_ALLOC,
            inline_len: 48,
            ..Req::default()
        };
        let mut inline = vec![0; 48];
        inline[36..40].copy_from_slice(&sys::NVOS64_FLAGS_FINN_SERIALIZED.to_le_bytes());
        assert_eq!(check(&req, &inline, &[]).unwrap_err().errno, libc::ENOTSUP);
    }

    #[test]
    fn capability_fd_classes_are_refused_even_with_null_parameters() {
        for class in [0xc637u32, 0xc638, 0xc639, 0xc640, 0xb0cd, 0xb0ce, 0xcdcd] {
            for (nr, len) in [
                (sys::NV_ESC_RM_ALLOC, 32),
                (sys::NV_ESC_RM_ALLOC, 48),
                (sys::NV_ESC_RM_ALLOC_OBJECT, 20),
                (sys::NV_ESC_RM_ALLOC_MEMORY, 56),
            ] {
                let req = Req {
                    ioctl_nr: nr,
                    inline_len: len,
                    ..Req::default()
                };
                let mut inline = vec![0; len as usize];
                inline[12..16].copy_from_slice(&class.to_le_bytes());
                if nr == sys::NV_ESC_RM_ALLOC_MEMORY {
                    // The inline FD is unrelated to the class capability FD.
                    inline[48..52].copy_from_slice(&(-1i32).to_le_bytes());
                }
                let req = Req {
                    fd_field_off: xlate::fd_field_offset(Dev::Ctl, nr, len).unwrap_or(NONE_U32),
                    ..req
                };
                assert_eq!(
                    check(&req, &inline, &[]).unwrap_err().errno,
                    libc::ENOTSUP,
                    "class {class:#x}, escape {nr:#x}, inline {len}"
                );
                if nr == sys::NV_ESC_RM_ALLOC {
                    inline[16..24].copy_from_slice(&0x1234_0000u64.to_le_bytes());
                    assert_eq!(check(&req, &inline, &[]).unwrap_err().errno, libc::ENOTSUP);
                }
            }
        }
    }

    #[test]
    fn unimplemented_pointer_and_fd_controls_are_blocked_even_with_valid_primary_metadata() {
        for cmd in [0x127, 0x130, 0x3d08, 0x3d0a, 0x3d0b, 0x3d0c, 0x83de0315] {
            let (req, inline, aux) = control(cmd, 40, 40);
            assert_eq!(check(&req, &inline, &aux).unwrap_err().errno, libc::EPERM);
        }
        for &cmd in xlate::nested_cmds().iter().chain(xlate::ctrl_fd_cmds()) {
            assert!(
                !xlate::ctrl_blocked(cmd),
                "translated control {cmd:#x} was blocked"
            );
        }
    }

    #[test]
    fn only_resolved_tokens_or_negative_fd_sentinels_are_forwarded() {
        let mut req = Req {
            ioctl_nr: nvrm_abi::nvgpu::NV_ESC_REGISTER_FD,
            inline_len: 4,
            ..Req::default()
        };
        assert!(check(&req, &12i32.to_le_bytes(), &[]).is_err());
        req.fd_field_off = 0;
        assert!(check(&req, &12i32.to_le_bytes(), &[]).is_err());
        assert!(check(&req, &(-1i32).to_le_bytes(), &[]).is_ok());
        req.fd_field_token = 7;
        assert!(check(&req, &12i32.to_le_bytes(), &[]).is_ok());
        assert!(check(&req, &(-1i32).to_le_bytes(), &[]).is_ok());
        req.fd_field_off = 1;
        assert!(check(&req, &12i32.to_le_bytes(), &[]).is_err());
    }

    #[test]
    fn an_aux_fd_requires_metadata_and_cannot_overwrite_an_arbitrary_field() {
        let (mut req, inline, aux) = control(0x3d06, 20, 20);
        assert!(check(&req, &inline, &aux).is_err());
        req.aux_fd_field_off = 0;
        req.aux_fd_field_token = 3;
        assert!(check(&req, &inline, &aux).is_ok());
        req.aux_fd_field_off = 4;
        assert!(check(&req, &inline, &aux).is_err());
    }

    #[test]
    fn allocation_fd_guard_distinguishes_callbacks_from_os_events() {
        let mut req = Req {
            ioctl_nr: sys::NV_ESC_RM_ALLOC,
            inline_len: 48,
            aux_len: 24,
            embedded_ptr_off: 16,
            ..Req::default()
        };
        let mut inline = vec![0; 48];
        inline[12..16].copy_from_slice(&sys::NV01_EVENT.to_le_bytes());
        inline[16..24].copy_from_slice(&1u64.to_le_bytes());
        let mut aux = vec![0; 24];
        aux[8..12].copy_from_slice(&sys::NV01_EVENT_KERNEL_CALLBACK_EX.to_le_bytes());
        assert!(check(&req, &inline, &aux).is_ok());
        aux[8..12].copy_from_slice(&sys::NV01_EVENT_OS_EVENT.to_le_bytes());
        assert!(check(&req, &inline, &aux).is_err());
        req.aux_fd_field_off = 16;
        req.aux_fd_field_token = 4;
        assert!(check(&req, &inline, &aux).is_ok());
        req.aux_fd_field_token = NONE_U64;
        aux[16..24].copy_from_slice(&(-1i64).to_le_bytes());
        assert!(check(&req, &inline, &aux).is_ok());
        aux[16..24].copy_from_slice(&0xffff_ffff_0000_0010u64.to_le_bytes());
        assert!(check(&req, &inline, &aux).is_err());
    }

    #[test]
    fn nested_metadata_is_required_only_for_nonnull_nonempty_targets() {
        let (mut req, mut inline, mut aux) = control(0x101, 40, 44);
        aux[..4].copy_from_slice(&4u32.to_le_bytes());
        aux[16..24].copy_from_slice(&0x1234_0000u64.to_le_bytes());
        assert!(check(&req, &inline, &aux).is_err());
        req.nested_count = 1;
        req.nested[0] = NestedDesc {
            ptr_off: 16,
            aux_off: 40,
            len: 4,
            ..NestedDesc::default()
        };
        assert!(
            check(&req, &inline, &aux).is_ok(),
            "two NULL pointers are omitted"
        );
        aux[..4].copy_from_slice(&0u32.to_le_bytes());
        req.nested_count = 0;
        let shape = check(&req, &inline, &aux).unwrap();
        shape.clear_unused_pointers(&mut inline, &mut aux);
        assert_eq!(u64_at(&aux, 16).unwrap(), 0);
    }

    #[test]
    fn nested_buffers_cannot_alias_parameters_or_each_other() {
        let (mut req, inline, mut aux) = control(0x101, 40, 48);
        aux[..4].copy_from_slice(&4u32.to_le_bytes());
        aux[8..16].copy_from_slice(&1u64.to_le_bytes());
        aux[16..24].copy_from_slice(&2u64.to_le_bytes());
        req.nested_count = 2;
        req.nested[0] = NestedDesc {
            ptr_off: 8,
            aux_off: 40,
            len: 4,
            ..NestedDesc::default()
        };
        req.nested[1] = NestedDesc {
            ptr_off: 16,
            aux_off: 44,
            len: 4,
            ..NestedDesc::default()
        };
        assert!(check(&req, &inline, &aux).is_ok());
        req.nested[0].aux_off = 8;
        assert!(check(&req, &inline, &aux).is_err());
        req.nested[0].aux_off = 40;
        req.nested[1].aux_off = 42;
        assert!(check(&req, &inline, &aux).is_err());
        req.nested[1].aux_off = 46;
        assert!(check(&req, &inline, &aux).is_err());
        req.nested[1].aux_off = 44;
        req.nested[1].len = 3;
        assert!(check(&req, &inline, &aux).is_err());
    }

    #[test]
    fn osdesc_allocations_require_gpa_backing_and_exact_run_bytes() {
        let mut req = Req {
            ioctl_nr: sys::NV_ESC_RM_ALLOC_MEMORY,
            inline_len: 56,
            fd_field_off: 48,
            ..Req::default()
        };
        let mut inline = vec![0; 56];
        inline[12..16].copy_from_slice(&sys::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR.to_le_bytes());
        inline[48..52].copy_from_slice(&(-1i32).to_le_bytes());
        assert!(check(&req, &inline, &[]).is_err());
        req.gpa_run_count = 1;
        req.aux_len = 16;
        assert!(check(&req, &inline, &[0; 16]).is_ok());
        req.gpa_run_count = 2;
        assert!(check(&req, &inline, &[0; 16]).is_err());
        req.gpa_run_count = 1;
        inline[12..16].copy_from_slice(&sys::NV01_MEMORY_SYSTEM.to_le_bytes());
        assert!(check(&req, &inline, &[0; 16]).is_err());
    }

    #[test]
    fn unsupported_pointer_bearing_escapes_are_refused() {
        for nr in [
            0xff,
            sys::NV_ESC_IOCTL_XFER_CMD,
            sys::NV_ESC_RM_IDLE_CHANNELS,
            sys::NV_ESC_RM_ACCESS_REGISTRY,
        ] {
            let req = Req {
                ioctl_nr: nr,
                inline_len: 32,
                ..Req::default()
            };
            assert_eq!(check(&req, &[0; 32], &[]).unwrap_err().errno, libc::ENOTSUP);
        }
        let req = Req {
            ioctl_nr: sys::NV_ESC_RM_VID_HEAP_CONTROL,
            inline_len: std::mem::size_of::<sys::NVOS32_PARAMETERS>() as u32,
            ..Req::default()
        };
        let mut inline = vec![0; req.inline_len as usize];
        for function in [
            sys::NVOS32_FUNCTION_HW_ALLOC,
            sys::NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR,
        ] {
            inline[8..12].copy_from_slice(&function.to_le_bytes());
            assert_eq!(check(&req, &inline, &[]).unwrap_err().errno, libc::ENOTSUP);
        }
    }

    #[test]
    fn known_flat_requests_and_variable_card_arrays_are_supported() {
        for (nr, size) in [
            (
                sys::NV_ESC_RM_FREE,
                std::mem::size_of::<sys::NVOS00_PARAMETERS>(),
            ),
            (
                sys::NV_ESC_RM_DUP_OBJECT,
                std::mem::size_of::<sys::NVOS55_PARAMETERS>(),
            ),
            (
                nvrm_abi::nvgpu::NV_ESC_CARD_INFO,
                2 * std::mem::size_of::<sys::nv_ioctl_card_info_t>(),
            ),
            (
                nvrm_abi::nvgpu::NV_ESC_CHECK_VERSION_STR,
                std::mem::size_of::<sys::nv_ioctl_rm_api_version_t>(),
            ),
            (
                nvrm_abi::nvgpu::NV_ESC_NUMA_INFO,
                std::mem::size_of::<nvrm_abi::nvgpu::IoctlNumaInfo>(),
            ),
        ] {
            let req = Req {
                ioctl_nr: nr,
                inline_len: size as u32,
                ..Req::default()
            };
            assert!(check(&req, &vec![0; size], &[]).is_ok(), "nr {nr:#x}");
            let bad = Req {
                inline_len: size as u32 - 1,
                ..req
            };
            assert!(check(&bad, &vec![0; size - 1], &[]).is_err(), "nr {nr:#x}");
        }
    }

    fn check_uvm<A: sys::RmAbi>() {
        for nr in (0..=2047).chain([xlate::uvm::INITIALIZE, xlate::uvm::DEINITIALIZE]) {
            let Some(size) = xlate::uvm_param_size_for::<A>(nr) else {
                continue;
            };
            let mut req = Req {
                ioctl_nr: nr,
                inline_len: size,
                ..Req::default()
            };
            if let Some(off) = xlate::fd_field_offset(Dev::Uvm, nr, size) {
                req.fd_field_off = off;
                req.fd_field_token = 1;
            }
            assert!(validate::<A>(Dev::Uvm, &req, &vec![0; size as usize], &[]).is_ok());
            req.inline_len += 1;
            assert!(validate::<A>(Dev::Uvm, &req, &vec![0; req.inline_len as usize], &[]).is_err());
        }
    }

    #[test]
    fn uvm_envelopes_use_each_selected_compiled_abi() {
        #[cfg(feature = "v580")]
        check_uvm::<sys::V580>();
        #[cfg(feature = "v595")]
        check_uvm::<sys::V595>();
        #[cfg(feature = "v610")]
        check_uvm::<sys::V610>();
        #[cfg(feature = "v615")]
        check_uvm::<sys::V615>();
        let unknown = Req {
            ioctl_nr: 0x12345678,
            ..Req::default()
        };
        assert_eq!(
            validate::<sys::DefaultAbi>(Dev::Uvm, &unknown, &[], &[])
                .unwrap_err()
                .errno,
            libc::ENOTSUP
        );
        assert_eq!(
            validate::<sys::DefaultAbi>(Dev::UvmTools, &unknown, &[], &[])
                .unwrap_err()
                .errno,
            libc::ENOTSUP
        );
    }
}
