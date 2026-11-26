/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The sweep callback of a collection, which runs once the marks are final and before the cells that died are swept,
//! and has what holds cells without keeping them alive forget the ones that died.

use crate::gc::capi;
use crate::interpreter::vm::Vm;
use crate::layout::cell::{CellHeader, CellState, Gc};
use crate::layout::value::Value;
use crate::runtime::finalization_registry::FinalizationRegistry;

const GC_BLOCK_SIZE: usize = 16 * 1024;
const GC_CELL_BITMAP_WORDS: usize = 11;

#[repr(C)]
struct BlockDescriptorMirror {
    _prefix: [u8; 20],
    cell_index_magic: u32,
    _middle: [u8; 16],
    marked: [u64; GC_CELL_BITMAP_WORDS],
    _suffix: [u8; 128],
}

const _: () = {
    assert!(size_of::<BlockDescriptorMirror>() == 256);
    assert!(core::mem::offset_of!(BlockDescriptorMirror, cell_index_magic) == 20);
    assert!(core::mem::offset_of!(BlockDescriptorMirror, marked) == 40);
};

/// What code can do while the sweep callback of a collection runs: tell which cells died, which only means anything
/// then, and hand finalization registries over for cleanup. It gives no VM, as nothing may allocate meanwhile.
pub struct Sweep<'vm> {
    vm: &'vm Vm,
    moves_cells: bool,
    heap_region_base: usize,
    block_descriptors: *const BlockDescriptorMirror,
}

impl<'vm> Sweep<'vm> {
    /// # Safety
    ///
    /// Only the VM's sweep callback makes one, while the marks of its collection are final and the cells that died are
    /// not swept yet.
    pub(crate) unsafe fn new(vm: &'vm Vm) -> Self {
        // NB: The VM lets go of its heap before the collection that destroys the heap, which moves nothing.
        let moves_cells = vm
            .heap_if_held()
            .is_some_and(|heap| capi::gc_heap_is_moving_cells(heap.handle()));
        Self {
            vm,
            moves_cells,
            heap_region_base: crate::layout_forward::heap_region_base(),
            block_descriptors: capi::gc_block_descriptors().cast::<BlockDescriptorMirror>(),
        }
    }

    /// Whether `cell`, which something held without keeping it alive, did not survive the collection.
    #[inline(always)]
    pub fn is_dead<T>(&self, cell: Gc<T>) -> bool {
        let addr = cell.as_ptr() as usize;
        // SAFETY: `cell` lies in the heap region whose descriptor table `block_descriptors` covers.
        // Outside of marking, nothing writes mark bits, so a plain load is sound.
        let descriptor = unsafe {
            &*self
                .block_descriptors
                .add((addr - self.heap_region_base) / GC_BLOCK_SIZE)
        };
        let index = (((addr & (GC_BLOCK_SIZE - 1)) as u64 * u64::from(descriptor.cell_index_magic)) >> 32) as usize;
        if (descriptor.marked[index / 64] & (1u64 << (index % 64))) == 0 {
            return true;
        }
        // SAFETY: Every cell starts with its header, and a cell that died is intact until the sweep that follows.
        let header = unsafe { &*cell.as_ptr().cast::<CellHeader>() };
        header.state.get() != CellState::Live
    }

    /// Whether `cell`, which a cache held without keeping it alive, did not survive the collection, or moves in it. A
    /// cache that keeps the address of a cell without visiting it forgets the cell either way, while what holds cells
    /// weakly by identity pins them as it is traced (see Visitor::pin()).
    #[inline(always)]
    pub fn is_dead_or_moving<T>(&self, cell: Gc<T>) -> bool {
        // SAFETY: The cell is a cell, intact until the sweep that follows.
        self.is_dead(cell) || (self.moves_cells && unsafe { capi::gc_cell_will_move(cell.as_ptr().cast()) })
    }

    /// Whether the collection may move cells, which is when what holds cells without visiting their slots hands over
    /// the slots of those that survived (see record_slot()).
    #[inline(always)]
    pub fn moves_cells(&self) -> bool {
        self.moves_cells
    }

    /// Has the collection rewrite `slot`, which holds a cell that survived it, if the cell moves, as it rewrites the slots
    /// marking visited. For what holds cells without keeping them alive or in place, like the keys of a weak map, once
    /// it forgot the cells that died.
    ///
    /// # Safety
    ///
    /// Nothing may free, move or write the slot until the collection is over.
    pub unsafe fn record_slot<T>(&self, slot: &Gc<T>) {
        // SAFETY: The slot holds a pointer to a cell that survived, and the caller vouches for its memory.
        unsafe {
            capi::gc_heap_record_slot(
                self.vm.heap().raw(),
                core::ptr::from_ref(slot).cast(),
                capi::GC_RECORDED_SLOT_CELL_POINTER,
            );
        }
    }

    /// Like record_slot(), for a slot that holds a value, which may be a cell that survived the collection.
    ///
    /// # Safety
    ///
    /// As for record_slot().
    pub unsafe fn record_value_slot(&self, slot: &Value) {
        // SAFETY: The slot holds a value, and the caller vouches for its memory.
        unsafe {
            capi::gc_heap_record_slot(
                self.vm.heap().raw(),
                core::ptr::from_ref(slot).cast(),
                capi::GC_RECORDED_SLOT_VALUE,
            );
        }
    }

    /// Hands `finalization_registry` to HostEnqueueFinalizationRegistryCleanupJob once the collection is over.
    pub fn enqueue_finalization_registry_cleanup_job(&self, finalization_registry: Gc<FinalizationRegistry>) {
        self.vm
            .enqueue_finalization_registry_cleanup_job_after_collection(self, finalization_registry);
    }
}
