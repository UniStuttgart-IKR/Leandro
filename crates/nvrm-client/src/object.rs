// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Object tracking with transitive free.
//!
//! Model: gVisor nvproxy `object.go`.
//!
//! Two kinds of edges that are not the same thing:
//! - `parent`: the tree as RM knows it (hObjectParent at alloc)
//! - `deps`: extra dependencies from `refAddDependant` in the driver.
//!   Example: a channel hangs off the VASpace *and* the memory object
//!   for USERD (its user-space doorbell page), although neither is its
//!   parent.
//!
//! Tearing down only the tree frees memory a channel is still using.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Object {
    pub handle: u32,
    pub parent: u32,
    pub class: u32,
    /// Objects that use *this* object and therefore must go first.
    pub dependants: HashSet<u32>,
}

impl Object {
    pub fn new(handle: u32, parent: u32, class: u32) -> Self {
        Self { handle, parent, class, dependants: HashSet::new() }
    }
    pub fn root(handle: u32) -> Self {
        Self::new(handle, 0, nvrm_sys::NV01_ROOT_CLIENT)
    }
}

#[derive(Default)]
pub struct ObjectTree {
    map: HashMap<u32, Object>,
}

impl ObjectTree {
    pub fn insert(&mut self, handle: u32, obj: Object) {
        let parent = obj.parent;
        self.map.insert(handle, obj);
        if let Some(p) = self.map.get_mut(&parent) {
            p.dependants.insert(handle);
        }
    }

    /// Enter an extra edge (not parent-child).
    ///
    /// TODO(leandro): the list of these edges comes from the constructors
    /// in open-gpu-kernel-modules. It is the part that can only be read,
    /// not derived. Until it is complete, every free is potentially too
    /// early.
    pub fn add_dependant(&mut self, on: u32, dependant: u32) {
        if let Some(o) = self.map.get_mut(&on) {
            o.dependants.insert(dependant);
        }
    }

    pub fn parent_of(&self, h: u32) -> Option<u32> {
        self.map.get(&h).map(|o| o.parent)
    }

    pub fn get(&self, h: u32) -> Option<&Object> {
        self.map.get(&h)
    }

    /// Forget `h`, and with it every edge that pointed at it.
    ///
    /// The scrub runs over *all* objects, not just the parent: an extra
    /// edge from [`add_dependant`](Self::add_dependant) has no counterpart
    /// in `h.parent`, so clearing the parent alone leaves the dependency
    /// edges behind. A later `free_order` would then hand the caller a
    /// handle that is already gone, and `RmClient::free` would send an
    /// NV_ESC_RM_FREE for a dead handle - on a *live* client, where the
    /// number may since have been handed out again.
    pub fn remove(&mut self, h: u32) {
        self.map.remove(&h);
        for o in self.map.values_mut() {
            o.dependants.remove(&h);
        }
    }

    /// Order for transitive free: dependants first, `root` last.
    pub fn free_order(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.visit(root, &mut seen, &mut out);
        out
    }

    fn visit(&self, h: u32, seen: &mut HashSet<u32>, out: &mut Vec<u32>) {
        if !seen.insert(h) {
            return;
        }
        if let Some(o) = self.map.get(&h) {
            for d in &o.dependants {
                self.visit(*d, seen, out);
            }
        }
        out.push(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_order_is_bottom_up() {
        let mut t = ObjectTree::default();
        t.insert(1, Object::root(1));
        t.insert(2, Object::new(2, 1, 0x80));
        t.insert(3, Object::new(3, 2, 0x2080));
        let order = t.free_order(1);
        assert_eq!(order, vec![3, 2, 1]);
    }

    #[test]
    fn extra_dependency_is_honoured() {
        let mut t = ObjectTree::default();
        t.insert(1, Object::root(1));
        t.insert(2, Object::new(2, 1, 0x3e));   // memory
        t.insert(3, Object::new(3, 1, 0xc46f)); // channel, uses the memory
        t.add_dependant(2, 3);
        let order = t.free_order(2);
        assert_eq!(order, vec![3, 2]);
    }

    /// An extra edge outlives the object it points at unless `remove`
    /// scrubs it. It has no counterpart in the parent field, so a `remove`
    /// that only cleaned up `parent.dependants` left it in place -- and the
    /// next `free_order` over the object it hangs off handed back a handle
    /// that was freed one call earlier. `RmClient::free` would then send an
    /// NV_ESC_RM_FREE for a dead handle on a live client.
    #[test]
    fn removing_an_object_takes_its_extra_edges_with_it() {
        let mut t = ObjectTree::default();
        t.insert(1, Object::root(1));
        t.insert(2, Object::new(2, 1, 0x3e));   // memory
        t.insert(3, Object::new(3, 1, 0xc46f)); // channel, uses the memory
        t.add_dependant(2, 3);
        assert_eq!(t.free_order(2), vec![3, 2], "the edge is there to begin with");

        t.remove(3);
        assert_eq!(t.free_order(2), vec![2], "and gone with the object");
        // The parent edge goes too, which is what `remove` always did.
        assert_eq!(t.free_order(1), vec![2, 1]);
    }

    /// A diamond -- two objects under the root, one object hanging off both
    /// -- is freed once and in a valid order. Twice would be an
    /// NV_ESC_RM_FREE on a handle that is already gone; the wrong way round
    /// would tear out memory a channel is still using, which is the reason
    /// the dependency edges exist at all.
    #[test]
    fn a_diamond_is_freed_once_and_dependants_first() {
        let mut t = ObjectTree::default();
        t.insert(1, Object::root(1));
        t.insert(2, Object::new(2, 1, 0x80));   // device
        t.insert(3, Object::new(3, 1, 0x3e));   // memory
        t.insert(4, Object::new(4, 2, 0xc46f)); // channel under the device
        t.add_dependant(3, 4);                  // ... and using the memory

        let order = t.free_order(1);
        assert_eq!(order.len(), 4, "each object exactly once: {order:?}");
        assert_eq!(
            order.iter().copied().collect::<HashSet<u32>>(),
            HashSet::from([1, 2, 3, 4]),
        );

        let pos = |h: u32| order.iter().position(|&x| x == h).unwrap();
        assert!(pos(4) < pos(2), "the channel goes before its parent");
        assert!(pos(4) < pos(3), "and before the memory it uses");
        assert!(pos(2) < pos(1));
        assert!(pos(3) < pos(1));
    }

    /// A handle the tree never saw still gets freed, alone. `RmClient::free`
    /// is called with handles the caller owns; answering an untracked one
    /// with an empty order would silently skip the RM_FREE and leak the
    /// object for the lifetime of the client.
    #[test]
    fn an_unknown_handle_is_freed_by_itself() {
        let t = ObjectTree::default();
        assert_eq!(t.free_order(0xdead), vec![0xdead]);
    }
}
