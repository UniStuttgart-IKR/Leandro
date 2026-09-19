// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Protect backend-owned RM clients from forwarded guest operations.

use nvrm_abi::{
    sys,
    xlate::{uvm, Dev},
};
use nvrm_sys::RmAbi;
use std::mem::offset_of;

/// Return a private client named by a supported copy/import or control request.
/// Request-shape validation must reject truncated and unknown layouts separately.
pub(crate) fn private_client<A: RmAbi>(
    dev: Dev,
    nr: u32,
    inline: &[u8],
    aux: &[u8],
    is_private: impl Fn(u32) -> bool,
) -> Option<u32> {
    let offsets = if dev.is_uvm() {
        let offset = match nr {
            uvm::REGISTER_GPU_VASPACE => offset_of!(sys::UVM_REGISTER_GPU_VASPACE_PARAMS, hClient),
            uvm::REGISTER_CHANNEL => offset_of!(sys::UVM_REGISTER_CHANNEL_PARAMS, hClient),
            uvm::UNREGISTER_CHANNEL => A::UVM_UNREGISTER_CHANNEL_PARAMS_OFF_hClient,
            uvm::MAP_EXTERNAL_ALLOCATION => {
                offset_of!(sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS, hClient)
            }
            uvm::REGISTER_GPU => offset_of!(sys::UVM_REGISTER_GPU_PARAMS, hClient),
            _ => return None,
        };
        [Some(offset), None]
    } else {
        match nr {
            sys::NV_ESC_RM_DUP_OBJECT => [
                Some(offset_of!(sys::NVOS55_PARAMETERS, hClient)),
                Some(offset_of!(sys::NVOS55_PARAMETERS, hClientSrc)),
            ],
            // These supported envelopes start with hClient or hRoot.
            sys::NV_ESC_RM_ALLOC
            | sys::NV_ESC_RM_ALLOC_OBJECT
            | sys::NV_ESC_RM_ALLOC_MEMORY
            | sys::NV_ESC_RM_FREE
            | sys::NV_ESC_RM_CONTROL
            | sys::NV_ESC_RM_SHARE
            | sys::NV_ESC_RM_MAP_MEMORY
            | sys::NV_ESC_RM_UNMAP_MEMORY
            | sys::NV_ESC_RM_VID_HEAP_CONTROL
            | sys::NV_ESC_RM_BIND_CONTEXT_DMA
            | sys::NV_ESC_RM_MAP_MEMORY_DMA
            | sys::NV_ESC_RM_UNMAP_MEMORY_DMA
            | nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT
            | nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT => [Some(0), None],
            _ => return None,
        }
    };
    let find = |bytes: &[u8], offsets: [Option<usize>; 2]| {
        offsets
            .into_iter()
            .flatten()
            .filter_map(|offset| bytes.get(offset..offset + 4))
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .find(|&client| is_private(client))
    };
    if let Some(client) = find(inline, offsets) {
        return Some(client);
    }
    if dev.is_uvm() || nr != sys::NV_ESC_RM_ALLOC {
        return None;
    }
    let class = u32::from_le_bytes(inline.get(12..16)?.try_into().unwrap());
    let offsets = match class {
        sys::NV01_DEVICE_0 => [
            Some(offset_of!(sys::NV0080_ALLOC_PARAMETERS, hClientShare)),
            Some(offset_of!(sys::NV0080_ALLOC_PARAMETERS, hTargetClient)),
        ],
        0xcb33 => [
            Some(offset_of!(
                sys::NV_CONFIDENTIAL_COMPUTE_ALLOC_PARAMS,
                hClient
            )),
            None,
        ],
        0x83de => [
            Some(offset_of!(sys::NV83DE_ALLOC_PARAMETERS, hAppClient)),
            None,
        ],
        // NV_UVM_CHANNEL_RETAINER_ALLOC_PARAMS.hClient and
        // NVB2CC_ALLOC_PARAMETERS.hClientTarget are the leading NvHandle.
        0xc574 | 0xb2cc => [Some(0), None],
        _ => return None,
    };
    find(aux, offsets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_destination_clients_are_checked_independently() {
        let private = 0x12345678_u32;
        for offset in [0, 12] {
            let mut inline = [0; 28];
            inline[offset..offset + 4].copy_from_slice(&private.to_le_bytes());
            assert_eq!(
                private_client::<sys::DefaultAbi>(
                    Dev::Ctl,
                    sys::NV_ESC_RM_DUP_OBJECT,
                    &inline,
                    &[],
                    |root| root == private
                ),
                Some(private)
            );
            assert_eq!(
                private_client::<sys::DefaultAbi>(
                    Dev::Ctl,
                    sys::NV_ESC_RM_DUP_OBJECT,
                    &inline,
                    &[],
                    |_| false
                ),
                None
            );
        }
    }

    #[test]
    fn private_clients_cannot_be_modified_through_frontend_envelopes() {
        let private = 0x12345678_u32;
        for nr in [
            sys::NV_ESC_RM_ALLOC,
            sys::NV_ESC_RM_ALLOC_OBJECT,
            sys::NV_ESC_RM_ALLOC_MEMORY,
            sys::NV_ESC_RM_FREE,
            sys::NV_ESC_RM_CONTROL,
            sys::NV_ESC_RM_SHARE,
            sys::NV_ESC_RM_MAP_MEMORY,
            sys::NV_ESC_RM_UNMAP_MEMORY,
            sys::NV_ESC_RM_VID_HEAP_CONTROL,
            sys::NV_ESC_RM_BIND_CONTEXT_DMA,
            sys::NV_ESC_RM_MAP_MEMORY_DMA,
            sys::NV_ESC_RM_UNMAP_MEMORY_DMA,
            nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT,
            nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT,
        ] {
            let mut inline = [0; 64];
            inline[..4].copy_from_slice(&private.to_le_bytes());
            assert_eq!(
                private_client::<sys::DefaultAbi>(Dev::Ctl, nr, &inline, &[], |root| root
                    == private),
                Some(private),
                "frontend {nr:#x}"
            );
            assert_eq!(
                private_client::<sys::DefaultAbi>(Dev::Ctl, nr, &inline, &[], |_| false),
                None
            );
        }
    }

    #[test]
    fn every_supported_uvm_client_field_is_checked() {
        let private = 0x12345678_u32;
        for (nr, offset, len) in [
            (uvm::REGISTER_GPU_VASPACE, 20, 32),
            (uvm::REGISTER_CHANNEL, 20, 48),
            (uvm::MAP_EXTERNAL_ALLOCATION, 9252, 9264),
            (uvm::REGISTER_GPU, 28, 40),
            (
                uvm::UNREGISTER_CHANNEL,
                <sys::DefaultAbi as RmAbi>::UVM_UNREGISTER_CHANNEL_PARAMS_OFF_hClient,
                std::mem::size_of::<<sys::DefaultAbi as RmAbi>::UvmUnregisterChannelParams>(),
            ),
        ] {
            let mut inline = vec![0; len];
            inline[offset..offset + 4].copy_from_slice(&private.to_le_bytes());
            assert_eq!(
                private_client::<sys::DefaultAbi>(Dev::Uvm, nr, &inline, &[], |root| root
                    == private),
                Some(private),
                "UVM {nr}"
            );
        }
    }

    #[test]
    fn allocation_parameters_cannot_share_private_clients() {
        let private = 0x12345678_u32;
        for (class, offset) in [
            (0x80_u32, 4),
            (0x80, 8),
            (0xcb33, 0),
            (0x83de, 4),
            (0xc574, 0),
            (0xb2cc, 0),
        ] {
            let mut inline = [0; 48];
            inline[12..16].copy_from_slice(&class.to_le_bytes());
            let mut aux = [0; 64];
            aux[offset..offset + 4].copy_from_slice(&private.to_le_bytes());
            assert_eq!(
                private_client::<sys::DefaultAbi>(
                    Dev::Ctl,
                    sys::NV_ESC_RM_ALLOC,
                    &inline,
                    &aux,
                    |root| root == private
                ),
                Some(private),
                "class {class:#x}"
            );
        }
    }
}
