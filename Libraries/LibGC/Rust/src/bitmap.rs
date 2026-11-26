/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The per-cell bitmaps of a block: one bit for each cell, at the cell's index (see BlockDescriptor::cell_index()).

use crate::block::CELL_BITMAP_WORDS;

pub type CellBits = [u64; CELL_BITMAP_WORDS];

#[inline(always)]
pub fn bit(index: usize) -> u64 {
    1 << (index % 64)
}

#[inline(always)]
pub fn get(bits: &CellBits, index: usize) -> bool {
    bits[index / 64] & bit(index) != 0
}

#[inline(always)]
pub fn set(bits: &mut CellBits, index: usize) {
    bits[index / 64] |= bit(index);
}

#[inline(always)]
pub fn clear(bits: &mut CellBits, index: usize) {
    bits[index / 64] &= !bit(index);
}

#[cfg(test)]
pub fn count(bits: &CellBits) -> usize {
    bits.iter().map(|word| word.count_ones() as usize).sum()
}

pub fn set_range(bits: &mut CellBits, begin: usize, end: usize) {
    if begin >= end {
        return;
    }
    let first_word = begin / 64;
    let last_word = (end - 1) / 64;
    let start_mask = u64::MAX << (begin % 64);
    let end_shift = end % 64;
    let end_mask = if end_shift == 0 {
        u64::MAX
    } else {
        (1u64 << end_shift) - 1
    };
    if first_word == last_word {
        bits[first_word] |= start_mask & end_mask;
        return;
    }
    bits[first_word] |= start_mask;
    for value in &mut bits[first_word + 1..last_word] {
        *value = u64::MAX;
    }
    bits[last_word] |= end_mask;
}

pub fn clear_range(bits: &mut CellBits, begin: usize, end: usize) {
    if begin >= end {
        return;
    }
    let first_word = begin / 64;
    let last_word = (end - 1) / 64;
    let start_mask = u64::MAX << (begin % 64);
    let end_shift = end % 64;
    let end_mask = if end_shift == 0 {
        u64::MAX
    } else {
        (1u64 << end_shift) - 1
    };
    if first_word == last_word {
        bits[first_word] &= !(start_mask & end_mask);
        return;
    }
    bits[first_word] &= !start_mask;
    for value in &mut bits[first_word + 1..last_word] {
        *value = 0;
    }
    bits[last_word] &= !end_mask;
}

/// The first clear bit within `begin..end`.
#[inline]
pub fn first_clear(bits: &CellBits, begin: usize, end: usize) -> Option<usize> {
    first_with::<false>(bits, begin, end)
}

/// The first run of clear bits within `begin..end`, as the range of their indices.
pub fn first_clear_run(bits: &CellBits, begin: usize, end: usize) -> Option<(usize, usize)> {
    let start = first_with::<false>(bits, begin, end)?;
    let run_end = first_with::<true>(bits, start, end).unwrap_or(end);
    Some((start, run_end))
}

/// The first index within `begin..end` whose bit is `VALUE`.
#[inline]
fn first_with<const VALUE: bool>(bits: &CellBits, begin: usize, end: usize) -> Option<usize> {
    if begin >= end {
        return None;
    }
    let first_word = begin / 64;
    let last_word = (end - 1) / 64;
    let start_mask = u64::MAX << (begin % 64);
    let end_shift = end % 64;
    let end_mask = if end_shift == 0 {
        u64::MAX
    } else {
        (1u64 << end_shift) - 1
    };
    let select = |w: u64| if VALUE { w } else { !w };
    if first_word == last_word {
        let candidates = select(bits[first_word]) & start_mask & end_mask;
        return (candidates != 0).then(|| first_word * 64 + candidates.trailing_zeros() as usize);
    }
    let first_candidates = select(bits[first_word]) & start_mask;
    if first_candidates != 0 {
        return Some(first_word * 64 + first_candidates.trailing_zeros() as usize);
    }
    for (offset, &word_bits) in bits[first_word + 1..last_word].iter().enumerate() {
        let candidates = select(word_bits);
        if candidates != 0 {
            return Some((first_word + 1 + offset) * 64 + candidates.trailing_zeros() as usize);
        }
    }
    let last_candidates = select(bits[last_word]) & end_mask;
    (last_candidates != 0).then(|| last_word * 64 + last_candidates.trailing_zeros() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_and_clears_ranges_across_words() {
        let mut bits: CellBits = [0; CELL_BITMAP_WORDS];
        set_range(&mut bits, 3, 130);
        assert_eq!(count(&bits), 127);
        assert!(!get(&bits, 2) && get(&bits, 3) && get(&bits, 129) && !get(&bits, 130));
        clear_range(&mut bits, 60, 70);
        assert_eq!(count(&bits), 117);
        assert!(get(&bits, 59) && !get(&bits, 60) && !get(&bits, 69) && get(&bits, 70));
    }

    #[test]
    fn finds_runs_of_clear_bits() {
        let mut bits: CellBits = [0; CELL_BITMAP_WORDS];
        assert_eq!(first_clear_run(&bits, 2, 500), Some((2, 500)));
        set_range(&mut bits, 2, 100);
        set(&mut bits, 120);
        assert_eq!(first_clear_run(&bits, 2, 500), Some((100, 120)));
        set_range(&mut bits, 100, 120);
        assert_eq!(first_clear_run(&bits, 2, 500), Some((121, 500)));
        set_range(&mut bits, 121, 500);
        assert_eq!(first_clear_run(&bits, 2, 500), None);
        // NB: Bits past the end, which belong to no cell, never count.
        assert!(!get(&bits, 500));
    }
}
