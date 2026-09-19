// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Direct NVIDIA Resource Manager clients for the diagnostic tools.
//!
//! Allocations use caller-chosen handles; object tracking records parent and
//! explicit dependency edges so dependent objects are freed first.
//! The forwarding backend owns separate guest sessions and backing clients.

pub mod handle;
pub mod mem;
pub mod object;

use nvrm_abi::{check_status, sys, NvDevice, Result};

/// NvP64 may be generated as an integer or pointer, depending on the ABI.
#[inline]
fn p64_null() -> sys::NvP64 {
    0usize as sys::NvP64
}

#[inline]
fn p64_of<T>(p: *mut T) -> sys::NvP64 {
    p as usize as sys::NvP64
}

/// One direct RM client and its control FD. Closing the FD releases its client.
pub struct RmClient {
    ctl: NvDevice,
    root: u32,
    handles: handle::HandleAllocator,
    objects: object::ObjectTree,
}

impl RmClient {
    /// A fresh client on /dev/nvidiactl. Panics unless the running driver
    /// is the one the bindings were generated against.
    pub fn new() -> Result<Self> {
        sys::assert_driver_version();
        Self::open_after_version_check()
    }

    /// Open without checking the locally installed driver's version.
    ///
    /// Guest tools use this when the forwarding backend has checked the ABI.
    /// Native callers must establish the same version match themselves.
    pub fn open_without_version_check() -> Result<Self> {
        Self::open_after_version_check()
    }

    fn open_after_version_check() -> Result<Self> {
        let ctl = NvDevice::open_ctl()?;

        // RM chooses the root handle. NV01_ROOT_CLIENT is the unprivileged
        // class; NVOS64 matches the 48-byte RM_ALLOC used by guest libcuda.
        let mut p = sys::NVOS64_PARAMETERS::default();
        p.hRoot = 0;
        p.hObjectParent = 0;
        p.hObjectNew = 0;
        p.hClass = sys::NV01_ROOT_CLIENT;
        p.pAllocParms = p64_null();
        p.pRightsRequested = p64_null();
        p.paramsSize = 0;
        p.flags = 0;

        // SAFETY: NVOS64 root allocation has no class-specific payload.
        unsafe { ctl.ioctl_raw(sys::NV_ESC_RM_ALLOC, &mut p)? };
        check_status(sys::NV_ESC_RM_ALLOC, p.status as u32)?;

        let root = p.hObjectNew;
        let mut objects = object::ObjectTree::default();
        objects.insert(object::Object::root(root));

        Ok(Self {
            ctl,
            root,
            handles: handle::HandleAllocator::new(root),
            objects,
        })
    }

    pub fn root(&self) -> u32 {
        self.root
    }
    pub fn ctl(&self) -> &NvDevice {
        &self.ctl
    }
    pub fn objects(&self) -> &object::ObjectTree {
        &self.objects
    }

    /// Allocate a class under `parent` with a caller-chosen handle.
    ///
    /// Uses NVOS64 with paramsSize zero; RM derives the payload size from class.
    ///
    /// # Safety
    /// `P` must match the selected driver's allocation ABI for `class` and permit
    /// all values RM may write. Use `None` only when that class accepts no params.
    /// Embedded buffers and callbacks must remain valid, correctly sized and
    /// accessible for every driver use, including references retained after return.
    pub unsafe fn alloc<P>(
        &mut self,
        parent: u32,
        handle: u32,
        class: u32,
        params: Option<&mut P>,
    ) -> Result<u32> {
        let parms = match params {
            Some(r) => p64_of(r as *mut P),
            None => p64_null(),
        };

        let mut p = sys::NVOS64_PARAMETERS::default();
        p.hRoot = self.root;
        p.hObjectParent = parent;
        p.hObjectNew = handle;
        p.hClass = class;
        p.pAllocParms = parms;
        p.pRightsRequested = p64_null();
        p.paramsSize = 0;
        p.flags = 0;

        // SAFETY: NVOS64 is the escape ABI; the caller guarantees class payloads.
        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_ALLOC, &mut p)? };
        check_status(sys::NV_ESC_RM_ALLOC, p.status as u32)?;

        self.objects
            .insert(object::Object::new(p.hObjectNew, parent, class));
        Ok(p.hObjectNew)
    }

    /// Issue `NV_ESC_RM_CONTROL` with the size of `P`.
    ///
    /// # Safety
    /// `P` must match the selected driver's ABI for `cmd` and permit all output
    /// values. Embedded pointers must refer to initialized buffers of the lengths
    /// encoded in the payload, writable when required and valid for all driver use.
    pub unsafe fn control<P>(&self, object: u32, cmd: u32, params: &mut P) -> Result<()> {
        let mut p = sys::NVOS54_PARAMETERS::default();
        p.hClient = self.root;
        p.hObject = object;
        p.cmd = cmd;
        p.params = params as *mut P as *mut libc::c_void;
        p.paramsSize = std::mem::size_of::<P>()
            .try_into()
            .expect("RM control parameters exceed u32");

        // SAFETY: NVOS54 is the escape ABI; the caller guarantees command payloads.
        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_CONTROL, &mut p)? };
        check_status(sys::NV_ESC_RM_CONTROL, p.status as u32)
    }

    /// Free tracked dependants first, then the requested object.
    pub fn free(&mut self, handle: u32) -> Result<()> {
        for h in self.objects.free_order(handle) {
            let mut p = sys::NVOS00_PARAMETERS::default();
            p.hRoot = self.root;
            p.hObjectParent = self.objects.parent_of(h).unwrap_or(self.root);
            p.hObjectOld = h;
            // SAFETY: NVOS00 contains only handles and status, with no pointers.
            unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_FREE, &mut p)? };
            check_status(sys::NV_ESC_RM_FREE, p.status as u32)?;
            self.objects.remove(h);
        }
        Ok(())
    }

    /// Free `dependant` before `on`, including non-parent dependencies.
    /// These edges follow the driver's `refAddDependant` relationships.
    pub fn depends_on(&mut self, on: u32, dependant: u32) {
        self.objects.add_dependant(on, dependant);
    }

    pub fn next_handle(&mut self) -> u32 {
        self.handles.take()
    }
}
