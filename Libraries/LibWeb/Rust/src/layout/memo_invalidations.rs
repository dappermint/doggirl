/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! What the arena's writes leave the layout stage's memos to drop or reconsider. The memos live in the stage's scratch,
//! which the arena cannot reach, so the arena notes what a write did to a box and the stage applies the notes to its
//! memos before a layout pass reads them and before it sweeps them.
//!
//! The notes of a box coalesce. Only a layout pass stores memo entries, and a stage applies the notes before each pass,
//! so between two applications a box's entries change only through the notes: entries a note drops stay dropped, and
//! the notes after it see none.

use super::fc_run_cache::FcRunCacheStore;
use super::in_flow_block_cache::InFlowBlockCacheStore;
use super::layout_node_arena::IntrinsicSizeCaches;
use super::node_data::NodeSlotId;
use super::prepared_inline_content::PreparedInlineContentMemos;
use super::rendered_text::TextMemos;

mod slot_note_flag {
    pub(super) const NOTED: u8 = 1 << 0;
    pub(super) const PREPARED_INLINE_CONTENT_DROPPED: u8 = 1 << 1;
    pub(super) const RUN_CACHE_ENTRIES_DROPPED: u8 = 1 << 2;
    pub(super) const IN_FLOW_BLOCK_ENTRIES_DROPPED: u8 = 1 << 3;
    pub(super) const INTRINSIC_SIZES_DROPPED: u8 = 1 << 4;
    pub(super) const FRAGMENT_CACHE_EPOCH_BUMPED: u8 = 1 << 5;
}

#[derive(Clone, Copy, Default)]
struct SlotNotes {
    structural_inline_changes: u32,
    flags: u8,
    generation: u8,
}

const NOTES_PER_PAGE: usize = 64;

/// The notes of each box the arena wrote since the stage last applied them, in the order the boxes were first noted.
#[derive(Default)]
pub(crate) struct MemoInvalidations {
    /// Each page holds the notes of `NOTES_PER_PAGE` slots and is kept across passes so steady-state invalidations
    /// reuse their backing pages instead of allocating and zeroing them every layout round.
    pages: Vec<Option<Box<[SlotNotes; NOTES_PER_PAGE]>>>,
    noted_slots: Vec<u32>,
    dropped_text_states: Vec<u32>,
}

impl Clone for MemoInvalidations {
    fn clone(&self) -> Self {
        let mut cloned = Self {
            pages: Vec::new(),
            noted_slots: Vec::with_capacity(self.noted_slots.len()),
            dropped_text_states: self.dropped_text_states.clone(),
        };
        for &slot in &self.noted_slots {
            let page = slot as usize / NOTES_PER_PAGE;
            let row = slot as usize % NOTES_PER_PAGE;
            let source = self.pages[page].as_ref().expect("a noted slot has its page")[row];
            *cloned.notes_of(slot) = source;
        }
        cloned
    }
}

impl MemoInvalidations {
    #[inline]
    fn notes_of(&mut self, slot: u32) -> &mut SlotNotes {
        let (page, row) = (slot as usize / NOTES_PER_PAGE, slot as usize % NOTES_PER_PAGE);
        if self.pages.len() <= page {
            self.pages.resize_with(page + 1, || None);
        }
        let notes = &mut self.pages[page].get_or_insert_with(|| Box::new([SlotNotes::default(); NOTES_PER_PAGE]))[row];
        if notes.flags & slot_note_flag::NOTED == 0 {
            notes.flags |= slot_note_flag::NOTED;
            self.noted_slots.push(slot);
        }
        notes
    }

    /// The box in `slot` left its committed subtree or the arena, which drops its run cache entries.
    pub(crate) fn note_run_cache_entries_dropped(&mut self, slot: u32) {
        let notes = self.notes_of(slot);
        notes.flags =
            (notes.flags | slot_note_flag::RUN_CACHE_ENTRIES_DROPPED) & !slot_note_flag::FRAGMENT_CACHE_EPOCH_BUMPED;
        notes.structural_inline_changes = 0;
    }

    /// The box in `slot` left the arena, which drops all its memo entries.
    pub(crate) fn note_slot_freed(&mut self, slot: u32) {
        let notes = self.notes_of(slot);
        notes.flags = (notes.flags
            | slot_note_flag::RUN_CACHE_ENTRIES_DROPPED
            | slot_note_flag::PREPARED_INLINE_CONTENT_DROPPED
            | slot_note_flag::IN_FLOW_BLOCK_ENTRIES_DROPPED
            | slot_note_flag::INTRINSIC_SIZES_DROPPED)
            & !slot_note_flag::FRAGMENT_CACHE_EPOCH_BUMPED;
        notes.structural_inline_changes = 0;
    }

    /// Something the intrinsic sizes of the box in `slot` were measured from changed.
    pub(crate) fn note_intrinsic_sizes_dropped(&mut self, slot: u32) {
        self.notes_of(slot).flags |= slot_note_flag::INTRINSIC_SIZES_DROPPED;
    }

    /// The text state at `text_state` got other rendered text or was let go of, which the chunks and segmenter made of
    /// the old text no longer describe.
    pub(crate) fn note_text_state_dropped(&mut self, text_state: u32) {
        self.dropped_text_states.push(text_state);
    }

    /// The fragment cache epoch of `node` moved on: its run cache entries need the sweep, and its in-flow block entry
    /// and prepared inline content, valid for one epoch only, are dropped.
    pub(crate) fn note_fragment_cache_epoch_bumped(&mut self, node: NodeSlotId) {
        let notes = self.notes_of(node.slot_index());
        notes.flags |= slot_note_flag::IN_FLOW_BLOCK_ENTRIES_DROPPED | slot_note_flag::PREPARED_INLINE_CONTENT_DROPPED;
        if notes.flags & slot_note_flag::RUN_CACHE_ENTRIES_DROPPED == 0 {
            notes.flags |= slot_note_flag::FRAGMENT_CACHE_EPOCH_BUMPED;
            notes.generation = node.generation();
        }
    }

    /// The child list of `node` changed, which run cache entries holding line data count.
    pub(crate) fn note_structural_inline_change(&mut self, node: NodeSlotId) {
        let notes = self.notes_of(node.slot_index());
        if notes.flags & slot_note_flag::RUN_CACHE_ENTRIES_DROPPED == 0 {
            notes.structural_inline_changes += 1;
            notes.generation = node.generation();
        }
    }

    /// Applies the notes to the memos, in the order the boxes were first noted, and resets the noted entries while
    /// keeping the backing pages for subsequent rounds.
    pub(crate) fn apply_and_clear(
        &mut self,
        run_cache: &mut FcRunCacheStore,
        in_flow_block_cache: &mut InFlowBlockCacheStore,
        text_memos: &mut TextMemos,
        prepared_inline_content: &mut PreparedInlineContentMemos,
        intrinsic_size_caches: &mut IntrinsicSizeCaches,
    ) {
        for text_state in self.dropped_text_states.drain(..) {
            text_memos.forget(text_state);
        }
        for slot in self.noted_slots.drain(..) {
            let page = self.pages[slot as usize / NOTES_PER_PAGE]
                .as_mut()
                .expect("a noted slot has its page");
            let notes = std::mem::take(&mut page[slot as usize % NOTES_PER_PAGE]);
            let flags = notes.flags;
            if flags & slot_note_flag::RUN_CACHE_ENTRIES_DROPPED != 0 {
                run_cache.remove_entry(slot);
            } else if notes.structural_inline_changes > 0 || flags & slot_note_flag::FRAGMENT_CACHE_EPOCH_BUMPED != 0 {
                let node = NodeSlotId::new(slot, notes.generation);
                if notes.structural_inline_changes > 0 {
                    run_cache.note_inline_layout_damage_count(node, notes.structural_inline_changes);
                }
                if flags & slot_note_flag::FRAGMENT_CACHE_EPOCH_BUMPED != 0 {
                    run_cache.note_invalidated_entry(node);
                }
            }
            if flags & slot_note_flag::IN_FLOW_BLOCK_ENTRIES_DROPPED != 0 {
                in_flow_block_cache.remove_entry(slot);
            }
            if flags & slot_note_flag::PREPARED_INLINE_CONTENT_DROPPED != 0 {
                prepared_inline_content.forget(slot);
            }
            if flags & slot_note_flag::INTRINSIC_SIZES_DROPPED != 0 {
                intrinsic_size_caches.drop_entries_of_slot(slot);
            }
        }
    }

    /// Applies the notes to the memos, in the order the boxes were first noted.
    #[cfg(test)]
    pub(crate) fn apply_to(
        mut self,
        run_cache: &mut FcRunCacheStore,
        in_flow_block_cache: &mut InFlowBlockCacheStore,
        text_memos: &mut TextMemos,
        prepared_inline_content: &mut PreparedInlineContentMemos,
        intrinsic_size_caches: &mut IntrinsicSizeCaches,
    ) {
        self.apply_and_clear(
            run_cache,
            in_flow_block_cache,
            text_memos,
            prepared_inline_content,
            intrinsic_size_caches,
        );
    }
}
