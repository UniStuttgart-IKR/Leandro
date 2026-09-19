// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Free-order tracking for RM parents and explicit resource dependencies.
//!
//! Parent edges alone are insufficient: a channel can also depend on its
//! VASpace and USERD memory. Callers record those constructor-defined edges.

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
        Self {
            handle,
            parent,
            class,
            dependants: HashSet::new(),
        }
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
    pub fn insert(&mut self, obj: Object) {
        let handle = obj.handle;
        let parent = obj.parent;
        // A reused handle belongs to a new object, not its former dependencies.
        if self.map.contains_key(&handle) {
            self.remove(handle);
        }
        self.map.insert(handle, obj);
        if let Some(p) = self.map.get_mut(&parent) {
            p.dependants.insert(handle);
        }
    }

    /// Record a dependency between tracked objects.
    /// Callers must supply the driver's constructor-defined dependency edges.
    pub fn add_dependant(&mut self, on: u32, dependant: u32) {
        if self.map.contains_key(&dependant) {
            if let Some(o) = self.map.get_mut(&on) {
                o.dependants.insert(dependant);
            }
        }
    }

    pub fn parent_of(&self, h: u32) -> Option<u32> {
        self.map.get(&h).map(|o| o.parent)
    }

    pub fn get(&self, h: u32) -> Option<&Object> {
        self.map.get(&h)
    }

    /// Forget the object and all incoming parent/dependency edges.
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
        t.insert(Object::root(1));
        t.insert(Object::new(2, 1, 0x80));
        t.insert(Object::new(3, 2, 0x2080));
        let order = t.free_order(1);
        assert_eq!(order, vec![3, 2, 1]);
    }

    #[test]
    fn extra_dependency_is_honoured() {
        let mut t = ObjectTree::default();
        t.insert(Object::root(1));
        t.insert(Object::new(2, 1, 0x3e)); // memory
        t.insert(Object::new(3, 1, 0xc46f)); // channel, uses the memory
        t.add_dependant(2, 3);
        let order = t.free_order(2);
        assert_eq!(order, vec![3, 2]);
    }

    #[test]
    fn removing_an_object_takes_its_extra_edges_with_it() {
        let mut t = ObjectTree::default();
        t.insert(Object::root(1));
        t.insert(Object::new(2, 1, 0x3e)); // memory
        t.insert(Object::new(3, 1, 0xc46f)); // channel, uses the memory
        t.add_dependant(2, 3);
        assert_eq!(
            t.free_order(2),
            vec![3, 2],
            "the edge is there to begin with"
        );

        t.remove(3);
        assert_eq!(t.free_order(2), vec![2], "and gone with the object");
        // The parent edge goes too, which is what `remove` always did.
        assert_eq!(t.free_order(1), vec![2, 1]);
    }

    #[test]
    fn a_diamond_is_freed_once_and_dependants_first() {
        let mut t = ObjectTree::default();
        t.insert(Object::root(1));
        t.insert(Object::new(2, 1, 0x80)); // device
        t.insert(Object::new(3, 1, 0x3e)); // memory
        t.insert(Object::new(4, 2, 0xc46f)); // channel under the device
        t.add_dependant(3, 4); // ... and using the memory

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

    #[test]
    fn an_unknown_handle_is_freed_by_itself() {
        let t = ObjectTree::default();
        assert_eq!(t.free_order(0xdead), vec![0xdead]);
    }

    #[test]
    fn replacing_a_handle_drops_its_old_incoming_edges() {
        let mut t = ObjectTree::default();
        t.insert(Object::root(1));
        t.insert(Object::root(2));
        t.insert(Object::new(3, 1, 0x80));
        t.insert(Object::new(4, 1, 0x3e));
        t.add_dependant(4, 3);

        t.insert(Object::new(3, 2, 0x2080));
        assert_eq!(t.free_order(4), vec![4]);
        assert!(!t.free_order(1).contains(&3));
        assert_eq!(t.free_order(2), vec![3, 2]);
    }

    #[test]
    fn an_untracked_dependant_does_not_create_a_stale_free() {
        let mut t = ObjectTree::default();
        t.insert(Object::root(1));
        t.add_dependant(1, 99);
        assert_eq!(t.free_order(1), vec![1]);
        t.insert(Object::root(99));
        assert_eq!(t.free_order(1), vec![1]);
    }
}
