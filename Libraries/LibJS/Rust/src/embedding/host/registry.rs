/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The runtime class of each host class table.
//!
//! The process derives a class for a table the first time a VM sees it, and all VMs share it. The class extends the class of the table's parent, or
//! the root class of its kind for a table without a parent of its kind, so that Class::is_subclass_of() follows the
//! chain of tables. Its name is the table's, and its internal methods come from the table's hooks.

use core::cell::Cell;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::embedding::host::host_array::{HostArray, derive_host_array_class};
use crate::embedding::host::host_function::{HostFunction, derive_host_function_class};
use crate::embedding::host::host_module::{HostModule, derive_host_module_class};
use crate::embedding::host::host_object::{HostObject, derive_host_object_class};
use crate::gc::class::{Class, GcCell};
use crate::gc::heap::RuntimeClassAllocator;
use crate::interpreter::vm::Vm;
use crate::layout::host_class::{
    JS_HOST_CLASS_ARRAY, JS_HOST_CLASS_FUNCTION, JS_HOST_CLASS_MODULE, JS_HOST_CLASS_OBJECT, JSHostClass,
};

/// What the VM derived for a host class table.
#[derive(Clone, Copy)]
pub struct RegisteredHostClass {
    class: &'static Class,
    /// The allocator of the table's cells, which the first of them creates. A table that only ever is the parent of
    /// others has none.
    allocator: Option<RuntimeClassAllocator>,
}

const HOST_CLASS_CACHE_SIZE: usize = 256;

#[derive(Clone, Copy)]
struct HostClassCacheEntry {
    table_identity: usize,
    class: *const Class,
    allocator: Option<RuntimeClassAllocator>,
}

impl HostClassCacheEntry {
    const EMPTY: Self = Self {
        table_identity: 0,
        class: core::ptr::null(),
        allocator: None,
    };
}

/// Direct-mapped fast cache in front of `HostClassRegistry` to avoid hashing and RefCell borrows on hot wrapper creation.
pub struct HostClassFastCache {
    entries: Box<[Cell<HostClassCacheEntry>; HOST_CLASS_CACHE_SIZE]>,
}

impl Default for HostClassFastCache {
    fn default() -> Self {
        Self {
            entries: Box::new([const { Cell::new(HostClassCacheEntry::EMPTY) }; HOST_CLASS_CACHE_SIZE]),
        }
    }
}

impl HostClassFastCache {
    #[inline(always)]
    fn slot(identity: usize) -> usize {
        (((identity as u64 >> 4).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize) & (HOST_CLASS_CACHE_SIZE - 1)
    }

    #[inline(always)]
    fn get(&self, identity: usize) -> Option<(&'static Class, RuntimeClassAllocator)> {
        let entry = self.entries[Self::slot(identity)].get();
        if entry.table_identity == identity
            && let Some(allocator) = entry.allocator
        {
            // SAFETY: `class` was stored from a `&'static Class` when inserting into this cache.
            return Some((unsafe { &*entry.class }, allocator));
        }
        None
    }

    #[inline(always)]
    fn get_class(&self, identity: usize) -> Option<&'static Class> {
        let entry = self.entries[Self::slot(identity)].get();
        if entry.table_identity == identity && !entry.class.is_null() {
            // SAFETY: `class` was stored from a `&'static Class` when inserting into this cache.
            return Some(unsafe { &*entry.class });
        }
        None
    }

    #[inline(always)]
    fn set(&self, identity: usize, class: &'static Class, allocator: Option<RuntimeClassAllocator>) {
        self.entries[Self::slot(identity)].set(HostClassCacheEntry {
            table_identity: identity,
            class,
            allocator,
        });
    }
}

/// The classes of a VM, by the address of their JSHostClass.
pub type HostClassRegistry = HashMap<usize, RegisteredHostClass, foldhash::fast::RandomState>;

/// The classes derived for host class tables so far, by the address of their JSHostClass. A class lives as long as the
/// process, and a cell may outlive the VM that allocated it in a block that another VM reuses, so each table must have
/// one class in all VMs.
static DERIVED_CLASSES: LazyLock<Mutex<HashMap<usize, &'static Class, foldhash::fast::RandomState>>> =
    LazyLock::new(Mutex::default);

fn root_class_of_kind(kind: u8) -> &'static Class {
    match kind {
        JS_HOST_CLASS_OBJECT => HostObject::CLASS,
        JS_HOST_CLASS_FUNCTION => HostFunction::CLASS,
        JS_HOST_CLASS_ARRAY => HostArray::CLASS,
        JS_HOST_CLASS_MODULE => HostModule::CLASS,
        kind => panic!("{kind} is not a kind of host class"),
    }
}

fn derive_class_of_kind(table: &'static JSHostClass, parent: &'static Class) -> &'static Class {
    match table.kind {
        JS_HOST_CLASS_OBJECT => derive_host_object_class(table, parent),
        JS_HOST_CLASS_FUNCTION => derive_host_function_class(table, parent),
        JS_HOST_CLASS_ARRAY => derive_host_array_class(table, parent),
        JS_HOST_CLASS_MODULE => derive_host_module_class(table, parent),
        kind => panic!("{kind} is not a kind of host class"),
    }
}

/// The runtime class of `table`.
pub fn runtime_class_of_host_class(vm: &Vm, table: &'static JSHostClass) -> &'static Class {
    let identity = table.identity();
    if let Some(class) = vm.host_class_fast_cache().get_class(identity) {
        return class;
    }
    let registered = vm.host_classes().borrow().get(&identity).copied();
    if let Some(registered) = registered {
        vm.host_class_fast_cache()
            .set(identity, registered.class, registered.allocator);
        return registered.class;
    }

    table.validate(table.kind);
    let parent = match table.parent_class() {
        Some(parent) if parent.kind == table.kind => runtime_class_of_host_class(vm, parent),
        _ => root_class_of_kind(table.kind),
    };
    let class = *DERIVED_CLASSES
        .lock()
        .expect("deriving a class does not panic")
        .entry(identity)
        .or_insert_with(|| derive_class_of_kind(table, parent));
    vm.host_classes()
        .borrow_mut()
        .insert(identity, RegisteredHostClass { class, allocator: None });
    vm.host_class_fast_cache().set(identity, class, None);
    class
}

/// The runtime class of `table`, which must be of `kind`, and the allocator its cells come from: the one that all
/// the host classes of the kind share. Their cells have one layout, that of the root class of the kind, and each cell
/// keeps its class, so no cell can be taken for one with another layout.
pub fn runtime_class_and_allocator_of_host_class(
    vm: &Vm,
    table: &'static JSHostClass,
    kind: u8,
) -> (&'static Class, RuntimeClassAllocator) {
    assert!(
        table.kind == kind,
        "the host class {} is of kind {}, not {kind}",
        table.class_name(),
        table.kind
    );
    let identity = table.identity();
    if let Some(hit) = vm.host_class_fast_cache().get(identity) {
        return hit;
    }
    let registered = vm.host_classes().borrow().get(&identity).copied();
    if let Some(RegisteredHostClass {
        class,
        allocator: Some(allocator),
    }) = registered
    {
        vm.host_class_fast_cache().set(identity, class, Some(allocator));
        return (class, allocator);
    }

    let class = runtime_class_of_host_class(vm, table);
    let allocator = vm
        .heap()
        .shared_runtime_class_allocator(root_class_of_kind(kind), class);
    vm.host_classes()
        .borrow_mut()
        .get_mut(&identity)
        .expect("the class was registered above")
        .allocator = Some(allocator);
    vm.host_class_fast_cache().set(identity, class, Some(allocator));
    (class, allocator)
}
