/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

use core::cell::RefCell;
use core::ops::ControlFlow;

use crate::gc::class::{GcCell, define_cell};
use crate::gc::defer_gc::DeferGc;
use crate::gc::gc_ref_cell::GcRefCell;
use crate::gc::visitor::{Trace, Visitor};
use crate::interpreter::vm::Vm;
use crate::layout::cell::{CellHeader, Gc};
use crate::runtime::property_attributes::PropertyAttributes;
use crate::runtime::property_key::PropertyKey;

/// Where a shape keeps a property: its slot in the object's named storage and its attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropertyMetadata {
    pub offset: u32,
    pub attributes: PropertyAttributes,
}

#[derive(Clone)]
pub struct Entry {
    pub property_key: PropertyKey,
    pub offset: u32,
    pub attributes: u16,
    pub enum_index: u16,
}

impl Entry {
    pub fn metadata(&self) -> PropertyMetadata {
        PropertyMetadata {
            offset: self.offset,
            attributes: PropertyAttributes::new(self.attributes as u8),
        }
    }
}

/// The properties of the shapes along a chain of transitions, which share one array. A shape sees the entries whose
/// enum index is below its property count. Entries are sorted by key identity bits, and the enum indices keep insertion order.
#[repr(C)]
pub struct DescriptorArray {
    header: CellHeader,
    entries: GcRefCell<Vec<Entry>>,
    entry_indices_by_enum_index: RefCell<Vec<u32>>,
    hash_index: RefCell<Vec<u16>>,
}

define_cell!(DescriptorArray, Other);

// SAFETY: The keys of the entries are the only cells a descriptor array reaches.
unsafe impl Trace for DescriptorArray {
    fn trace(&self, visitor: &mut Visitor) {
        for entry in self.entries.borrow().iter() {
            entry.property_key.trace(visitor);
        }
    }
}

pub const MAX_DESCRIPTOR_COUNT: u32 = u16::MAX as u32 + 1;

// PropertyKey equality is raw bit equality and Symbol is not movable, so raw_bits() stays invariant
// for the lifetime of an entry and avoids hashing during binary search.
#[inline]
fn find(entries: &[Entry], property_key: &PropertyKey, descriptor_count: u32) -> Option<usize> {
    let key_bits = property_key.raw_bits();
    let len = entries.len();
    if len <= 12 {
        return entries.iter().position(|entry| {
            entry.property_key.raw_bits() == key_bits && u32::from(entry.enum_index) < descriptor_count
        });
    }

    // Branchless lower-bound binary search to avoid branch mispredictions in hot Shape::lookup calls.
    let mut base = 0usize;
    let mut size = len;
    while size > 1 {
        let half = size >> 1;
        let mid = base + half;
        // SAFETY: `base + (size >> 1) < len` holds by loop invariant.
        let mid_bits = unsafe { entries.get_unchecked(mid).property_key.raw_bits() };
        base = if mid_bits < key_bits { mid } else { base };
        size -= half;
    }
    // SAFETY: `len > 12`, so `base < len`.
    let mut index = base + usize::from(unsafe { entries.get_unchecked(base).property_key.raw_bits() } < key_bits);
    while index < len {
        // SAFETY: `index < len`.
        let entry = unsafe { entries.get_unchecked(index) };
        if entry.property_key.raw_bits() != key_bits {
            return None;
        }
        if u32::from(entry.enum_index) < descriptor_count {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn set_entry(
    entries: &mut Vec<Entry>,
    entry_indices_by_enum_index: &mut Vec<u32>,
    property_key: &PropertyKey,
    metadata: PropertyMetadata,
    enum_index: u32,
) {
    assert!(enum_index < MAX_DESCRIPTOR_COUNT);
    let key_bits = property_key.raw_bits();
    let mut insertion_index = entries.partition_point(|entry| entry.property_key.raw_bits() < key_bits);
    while insertion_index < entries.len() && entries[insertion_index].property_key.raw_bits() == key_bits {
        if u32::from(entries[insertion_index].enum_index) <= enum_index {
            let entry = &mut entries[insertion_index];
            entry.offset = metadata.offset;
            entry.attributes = u16::from(metadata.attributes.bits());
            return;
        }
        insertion_index += 1;
    }

    let insertion_index_u32 = u32::try_from(insertion_index).expect("the insertion index fits in u32");
    assert!(enum_index as usize == entry_indices_by_enum_index.len());
    for entry_index in entry_indices_by_enum_index.iter_mut() {
        if *entry_index >= insertion_index_u32 {
            *entry_index += 1;
        }
    }
    entries.insert(
        insertion_index,
        Entry {
            property_key: property_key.clone(),
            offset: metadata.offset,
            attributes: u16::from(metadata.attributes.bits()),
            enum_index: enum_index as u16,
        },
    );
    entry_indices_by_enum_index.push(insertion_index_u32);
}

fn copy_prefix_parts(other_entries: &[Entry], other_indices: &[u32], descriptor_count: u32) -> (Vec<Entry>, Vec<u32>) {
    if descriptor_count as usize == other_entries.len() {
        return (other_entries.to_vec(), other_indices.to_vec());
    }
    let count = descriptor_count as usize;
    let mut entries = Vec::with_capacity(count);
    let mut entry_indices_by_enum_index = vec![0u32; count];
    for entry in other_entries {
        if u32::from(entry.enum_index) < descriptor_count {
            let new_index = entries.len() as u32;
            entry_indices_by_enum_index[usize::from(entry.enum_index)] = new_index;
            entries.push(entry.clone());
        }
    }
    debug_assert_eq!(entries.len(), count);
    (entries, entry_indices_by_enum_index)
}

impl DescriptorArray {
    fn from_parts(_: &DeferGc<'_>, entries: Vec<Entry>, entry_indices_by_enum_index: Vec<u32>) -> Self {
        Self {
            header: CellHeader::for_class(Self::CLASS),
            entries: GcRefCell::new(entries),
            entry_indices_by_enum_index: RefCell::new(entry_indices_by_enum_index),
            hash_index: RefCell::new(Vec::new()),
        }
    }

    pub fn create(vm: &Vm) -> Gc<DescriptorArray> {
        vm.heap()
            .allocate(|defer_gc| Self::from_parts(defer_gc, Vec::new(), Vec::new()))
    }

    /// Creates a descriptor array from entries given in insertion order in O(n log n) instead of repeated O(n) inserts.
    pub fn create_from_insertion_order(
        vm: &Vm,
        count: usize,
        mut fill: impl FnMut(&mut Vec<Entry>),
    ) -> Gc<DescriptorArray> {
        assert!(count <= MAX_DESCRIPTOR_COUNT as usize);
        vm.heap().allocate(|defer_gc| {
            let mut entries = Vec::with_capacity(count);
            fill(&mut entries);
            debug_assert_eq!(entries.len(), count);
            entries.sort_unstable_by_key(|entry| (entry.property_key.raw_bits(), entry.enum_index));
            let mut entry_indices_by_enum_index = vec![0u32; entries.len()];
            for (index, entry) in entries.iter().enumerate() {
                entry_indices_by_enum_index[usize::from(entry.enum_index)] = index as u32;
            }
            Self::from_parts(defer_gc, entries, entry_indices_by_enum_index)
        })
    }

    /// The first `descriptor_count` properties of `other`, in its insertion order.
    pub fn create_copy(vm: &Vm, other: &DescriptorArray, descriptor_count: u32) -> Gc<DescriptorArray> {
        assert!(descriptor_count <= MAX_DESCRIPTOR_COUNT);
        vm.heap().allocate(|defer_gc| {
            let (entries, entry_indices_by_enum_index) = copy_prefix_parts(
                &other.entries.borrow(),
                &other.entry_indices_by_enum_index.borrow(),
                descriptor_count,
            );
            Self::from_parts(defer_gc, entries, entry_indices_by_enum_index)
        })
    }

    pub fn size(&self) -> u32 {
        self.entries.borrow().len() as u32
    }

    #[cold]
    fn build_hash_index(&self, entries: &[Entry]) {
        let mut table = self.hash_index.borrow_mut();
        if !table.is_empty() {
            return;
        }
        let cap = (entries.len() * 2).next_power_of_two().max(32);
        let mask = cap - 1;
        let shift = 64 - cap.trailing_zeros();
        table.resize(cap, u16::MAX);
        for (i, entry) in entries.iter().enumerate() {
            let mut slot =
                ((entry.property_key.raw_bits() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> shift) as usize & mask;
            while table[slot] != u16::MAX {
                slot = (slot + 1) & mask;
            }
            table[slot] = i as u16;
        }
    }

    #[inline]
    fn find_hashed(&self, entries: &[Entry], property_key: &PropertyKey, descriptor_count: u32) -> Option<usize> {
        // SAFETY: Mutations only happen in set()/remove(), which clear hash_index and never re-enter lookup().
        if unsafe { &*self.hash_index.as_ptr() }.is_empty() {
            self.build_hash_index(entries);
        }
        let table = unsafe { &*self.hash_index.as_ptr() };
        let key_bits = property_key.raw_bits();
        let mask = table.len() - 1;
        let shift = 64 - table.len().trailing_zeros();
        let mut slot = ((key_bits as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> shift) as usize & mask;
        loop {
            // SAFETY: `slot <= mask < table.len()`.
            let entry_idx = unsafe { *table.get_unchecked(slot) };
            if entry_idx == u16::MAX {
                return None;
            }
            // SAFETY: Every valid index in `table` is `< entries.len()`.
            let entry = unsafe { entries.get_unchecked(entry_idx as usize) };
            if entry.property_key.raw_bits() == key_bits && u32::from(entry.enum_index) < descriptor_count {
                return Some(entry_idx as usize);
            }
            slot = (slot + 1) & mask;
        }
    }

    #[inline]
    pub fn lookup(&self, property_key: &PropertyKey, descriptor_count: u32) -> Option<PropertyMetadata> {
        // SAFETY: The borrows that change the entries end within the methods of this array, which run
        // no other code that could get here.
        let entries = unsafe { &*self.entries.as_ptr() };
        let len = entries.len();
        let index = if len > 12 && len < u16::MAX as usize {
            self.find_hashed(entries, property_key, descriptor_count)?
        } else {
            find(entries, property_key, descriptor_count)?
        };
        Some(entries[index].metadata())
    }

    /// Looks up `property_key` without lazily building `hash_index` (for callers that are about to mutate the array).
    #[inline]
    pub fn lookup_without_hash_index(
        &self,
        property_key: &PropertyKey,
        descriptor_count: u32,
    ) -> Option<PropertyMetadata> {
        // SAFETY: As in `lookup()`.
        let entries = unsafe { &*self.entries.as_ptr() };
        let index = find(entries, property_key, descriptor_count)?;
        Some(entries[index].metadata())
    }

    /// The key and the attributes of the property with the given enum index.
    pub fn property_at_enum_index(&self, enum_index: u32) -> (PropertyKey, PropertyAttributes) {
        let entries = self.entries.borrow();
        let entry = &entries[self.entry_indices_by_enum_index.borrow()[enum_index as usize] as usize];
        (entry.property_key.clone(), entry.metadata().attributes)
    }

    /// Whether the property with the given enum index has the key and the attributes.
    ///
    /// OPTIMIZATION: Adding a property that some object of the shape already got comes through here, so it takes no
    /// borrows of the cells.
    #[inline(always)]
    pub fn has_property_at_enum_index(
        &self,
        enum_index: u32,
        property_key: &PropertyKey,
        attributes: PropertyAttributes,
    ) -> bool {
        // SAFETY: The borrows that change the entries and the indices end within the methods of this array, which run
        // no other code that could get here.
        let (entries, entry_indices) =
            unsafe { (&*self.entries.as_ptr(), &*self.entry_indices_by_enum_index.as_ptr()) };
        let entry = &entries[entry_indices[enum_index as usize] as usize];
        entry.property_key == *property_key && entry.metadata().attributes == attributes
    }

    #[inline]
    pub fn metadata_at_offset(&self, offset: u32, descriptor_count: u32) -> Option<PropertyMetadata> {
        // SAFETY: The borrows that change the entries and the indices end within the methods of this array, which run
        // no other code that could get here.
        let (entries, entry_indices) =
            unsafe { (&*self.entries.as_ptr(), &*self.entry_indices_by_enum_index.as_ptr()) };
        let count = descriptor_count as usize;
        if (offset as usize) < count {
            let entry = &entries[entry_indices[offset as usize] as usize];
            if entry.offset == offset {
                return Some(entry.metadata());
            }
        }
        for &entry_index in &entry_indices[..count] {
            let entry = &entries[entry_index as usize];
            if entry.offset == offset {
                return Some(entry.metadata());
            }
        }
        None
    }

    pub fn set(&self, vm: &Vm, property_key: &PropertyKey, metadata: PropertyMetadata, enum_index: u32) {
        self.hash_index.borrow_mut().clear();
        set_entry(
            &mut self.entries.borrow_mut(vm),
            &mut self.entry_indices_by_enum_index.borrow_mut(),
            property_key,
            metadata,
            enum_index,
        );
    }

    pub fn set_attributes(
        &self,
        vm: &Vm,
        property_key: &PropertyKey,
        attributes: PropertyAttributes,
        descriptor_count: u32,
    ) {
        let mut entries = self.entries.borrow_mut(vm);
        let index = find(&entries, property_key, descriptor_count).expect("the property has a descriptor");
        entries[index].attributes = u16::from(attributes.bits());
    }

    pub fn remove(&self, vm: &Vm, property_key: &PropertyKey, descriptor_count: u32) {
        self.hash_index.borrow_mut().clear();
        let mut entries = self.entries.borrow_mut(vm);
        let mut entry_indices_by_enum_index = self.entry_indices_by_enum_index.borrow_mut();
        let index = find(&entries, property_key, descriptor_count).expect("the property has a descriptor");

        let removed_offset = entries[index].offset;
        let removed_enum_index = entries[index].enum_index;
        entries.remove(index);
        entry_indices_by_enum_index.remove(usize::from(removed_enum_index));
        let index = index as u32;
        for entry_index in entry_indices_by_enum_index.iter_mut() {
            if *entry_index > index {
                *entry_index -= 1;
            }
        }

        for entry in entries.iter_mut() {
            if u32::from(entry.enum_index) >= descriptor_count {
                continue;
            }
            if entry.offset > removed_offset {
                entry.offset -= 1;
            }
            if entry.enum_index > removed_enum_index {
                entry.enum_index -= 1;
            }
        }
    }

    /// Calls `callback` with the first `descriptor_count` properties in insertion order, until it breaks. The array
    /// stays borrowed meanwhile, so the callback must not allocate or call anything that takes the VM.
    pub fn for_each_in_insertion_order(
        &self,
        mut callback: impl FnMut(&PropertyKey, PropertyMetadata) -> ControlFlow<()>,
        descriptor_count: u32,
    ) {
        let entries = self.entries.borrow();
        let entry_indices_by_enum_index = self.entry_indices_by_enum_index.borrow();
        assert!(descriptor_count as usize <= entry_indices_by_enum_index.len());
        for enum_index in 0..descriptor_count {
            let entry = &entries[entry_indices_by_enum_index[enum_index as usize] as usize];
            assert!(u32::from(entry.enum_index) == enum_index);
            if callback(&entry.property_key, entry.metadata()).is_break() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, PropertyMetadata, copy_prefix_parts, find, set_entry};
    use crate::runtime::property_attributes::PropertyAttributes;
    use crate::runtime::property_key::PropertyKey;

    #[test]
    fn raw_bits_lookup_and_prefix_copy_preserve_order_and_visibility() {
        let mut entries = Vec::<Entry>::new();
        let mut indices = Vec::<u32>::new();
        let keys: Vec<PropertyKey> = (0u32..24)
            .map(|i| PropertyKey::from_number(u64::from(100 - i)))
            .collect();

        for (i, key) in keys.iter().enumerate() {
            set_entry(
                &mut entries,
                &mut indices,
                key,
                PropertyMetadata {
                    offset: i as u32,
                    attributes: PropertyAttributes::new((i & 7) as u8),
                },
                i as u32,
            );
        }

        assert!(
            entries
                .windows(2)
                .all(|w| w[0].property_key.raw_bits() <= w[1].property_key.raw_bits())
        );
        for (i, key) in keys.iter().enumerate() {
            let found = find(&entries, key, 24).expect("entry present");
            assert_eq!(entries[found].offset, i as u32);
            assert_eq!(indices[i] as usize, found);
            assert!(find(&entries, key, i as u32).is_none());
        }

        let (prefix_entries, prefix_indices) = copy_prefix_parts(&entries, &indices, 10);
        assert_eq!(prefix_entries.len(), 10);
        assert_eq!(prefix_indices.len(), 10);
        assert!(
            prefix_entries
                .windows(2)
                .all(|w| w[0].property_key.raw_bits() <= w[1].property_key.raw_bits())
        );
        for (i, key) in keys.iter().enumerate() {
            let expected = (i < 10).then_some(i as u32);
            let actual = find(&prefix_entries, key, 10).map(|idx| prefix_entries[idx].offset);
            assert_eq!(actual, expected);
            if i < 10 {
                assert_eq!(prefix_entries[prefix_indices[i] as usize].property_key, *key);
            }
        }
    }
}
