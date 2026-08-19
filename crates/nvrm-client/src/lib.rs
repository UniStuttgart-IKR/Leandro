// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! RM level: client, object tree, handle allocation. RM is NVIDIA's
//! Resource Manager, the kernel driver behind /dev/nvidiactl and
//! /dev/nvidiaN, whose ioctls are called escapes.
//!
//! Who uses this: the diagnostic binaries in `src/bin/`. The host daemon
//! does NOT -- `vhost-user-nvrm` holds no RM client of its own, because
//! the guest allocates its own `NV01_ROOT_CLIENT` through the forwarded
//! RM_ALLOC.
//!
//! Two facts carry the whole design and are therefore anchored here:
//!
//! 1. **Handles pass through verbatim.** `hObjectNew` is caller-specified,
//!    so a client that carries handles it did not choose needs *tracking*,
//!    not *translation* - and a handle range of its own (`handle.rs`), so
//!    that the two cannot collide.
//!
//! 2. **Free is transitive.** The dependency edges are not in the headers
//!    but in the driver's constructors (`refAddDependant`). They have to
//!    be read and entered here - they cannot be derived.

pub mod handle;
pub mod object;
pub mod mem;

use nvrm_abi::{check_status, sys, NvDevice, Result};

/// Null pointer in the representation RM expects.
///
/// `NvP64` is an `NvU64` in nvtypes.h. The detour through `usize` keeps
/// the cast valid whether bindgen turns it into an integer or a pointer
/// type - both occur in practice, depending on the header revision.
#[inline]
fn p64_null() -> sys::NvP64 {
    0usize as sys::NvP64
}

#[inline]
fn p64_of<T>(p: *mut T) -> sys::NvP64 {
    p as usize as sys::NvP64
}

/// One RM client = one `NV01_ROOT_CLIENT` = one fd on /dev/nvidiactl.
///
/// One per process that talks to RM directly. RM tears a client down with
/// the process that owns it, so everything allocated under it is released
/// on exit without a single RM_FREE crossing the interface -- which is why
/// error handling here can stay short.
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

    /// Like [`RmClient::new`], but without enforcing the version lockstep.
    ///
    /// WARNING: only for tools that run IN THE GUEST. There is no
    /// `/proc/driver/nvidia/version` there -- the guest module only
    /// provides `params` -- and `assert_driver_version` would panic.
    /// The lockstep still holds: the real driver is served by the HOST,
    /// and its daemon checks the version at startup. Using this on the
    /// host bypasses a safeguard that exists for a good reason (offsets
    /// shifting through silent struct changes).
    pub fn open_without_version_check() -> Result<Self> {
        Self::open_after_version_check()
    }

    fn open_after_version_check() -> Result<Self> {
        let ctl = NvDevice::open_ctl()?;

        // The only alloc in the whole program whose handle is *not* chosen
        // by the caller: hRoot == hObjectParent == hObjectNew == 0, and RM
        // writes the created client handle back into hObjectNew.
        //
        // NVOS64, not NVOS21: traces of libcuda show _IOC_SIZE 48 for every
        // RM_ALLOC, including this first one. The driver would still accept
        // the short form, but the guest never sends it - so this exercises
        // exactly the structure that real guest traffic uses.
        //
        // hClass is NV01_ROOT_CLIENT (0x41), not NV01_ROOT (0x0). 0x0 is
        // the privileged path and yields NV_ERR_INSUFFICIENT_PERMISSIONS
        // as a normal user. Confirmed from the trace.
        let mut p = sys::NVOS64_PARAMETERS::default();
        p.hRoot = 0;
        p.hObjectParent = 0;
        p.hObjectNew = 0;
        p.hClass = sys::NV01_ROOT_CLIENT;
        p.pAllocParms = p64_null();
        p.pRightsRequested = p64_null();
        p.paramsSize = 0;
        p.flags = 0;

        unsafe { ctl.ioctl_raw(sys::NV_ESC_RM_ALLOC, &mut p)? };
        check_status(sys::NV_ESC_RM_ALLOC, p.status as u32)?;

        let root = p.hObjectNew;
        let mut objects = object::ObjectTree::default();
        objects.insert(root, object::Object::root(root));

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

    /// `NV_ESC_RM_ALLOC` with a caller-chosen handle, always NVOS64.
    ///
    /// `params` is the class-specific alloc parameter block (e.g.
    /// `NV_CHANNEL_ALLOC_PARAMS`). `None` for parameterless classes.
    ///
    /// `paramsSize` stays 0. The alloc side is *not* self-describing:
    /// RM derives the size from `hClass`. This is exactly why forwarding
    /// needs a hand-maintained hClass->size table (`ClassDesc` in
    /// nvrm-wire) - unlike RM_CONTROL, which carries its `paramsSize`.
    pub fn alloc<P>(
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

        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_ALLOC, &mut p)? };
        check_status(sys::NV_ESC_RM_ALLOC, p.status as u32)?;

        self.objects
            .insert(p.hObjectNew, object::Object::new(p.hObjectNew, parent, class));
        Ok(p.hObjectNew)
    }

    /// `NV_ESC_RM_CONTROL`.
    pub fn control<P>(&self, object: u32, cmd: u32, params: &mut P) -> Result<()> {
        let mut p = sys::NVOS54_PARAMETERS::default();
        p.hClient = self.root;
        p.hObject = object;
        p.cmd = cmd;
        p.params = params as *mut P as *mut libc::c_void;
        p.paramsSize = std::mem::size_of::<P>() as u32;

        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_CONTROL, &mut p)? };
        check_status(sys::NV_ESC_RM_CONTROL, p.status as u32)
    }

    /// `NV_ESC_RM_FREE` - transitive over the object tree.
    pub fn free(&mut self, handle: u32) -> Result<()> {
        for h in self.objects.free_order(handle) {
            let mut p = sys::NVOS00_PARAMETERS::default();
            p.hRoot = self.root;
            p.hObjectParent = self.objects.parent_of(h).unwrap_or(self.root);
            p.hObjectOld = h;
            unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_FREE, &mut p)? };
            check_status(sys::NV_ESC_RM_FREE, p.status as u32)?;
            self.objects.remove(h);
        }
        Ok(())
    }

    /// Extra free edge that is *not* parent-child.
    ///
    /// Sources: the `refAddDependant` calls in the driver - and the
    /// NV_ESC_RM_FREE order observed in libcuda teardown traces.
    /// Example: the channel hangs off the VASpace and the memory object
    /// for USERD, although neither is its parent. Without these edges,
    /// `free()` releases memory the channel is still using.
    pub fn depends_on(&mut self, on: u32, dependant: u32) {
        self.objects.add_dependant(on, dependant);
    }

    pub fn next_handle(&mut self) -> u32 {
        self.handles.take()
    }
}
