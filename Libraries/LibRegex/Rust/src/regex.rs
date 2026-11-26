/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

/// High-level regex API.
///
/// This is the main entry point for using the regex engine.
use crate::bytecode::NamedGroupEntry;
use crate::copy_out;
use crate::vm;
use crate::work_zone::with_work_zone;
use ak::Zone;
use libregex_compiler::ast::Flags;
use libregex_compiler::compiler;
use libregex_compiler::literal_hints;
use libregex_compiler::literal_hints::is_ascii_word_code_unit;
use libregex_compiler::parser;
use std::cell::RefCell;

/// Pre-computed first-code-unit acceleration for literal alternation patterns.
enum FirstUnitScan {
    One(u16),
    Two(u16, u16),
    Three(u16, u16, u16),
    #[cfg(target_arch = "aarch64")]
    AsciiLut {
        lut: vm::AsciiNibbleLut,
        bits: [u64; 4],
    },
    ByteBitset([u64; 4]),
    General,
}

#[derive(Clone, Copy)]
struct AltEntry {
    start: u32,
    len: u16,
    first: u16,
}

struct LiteralAltMatcher {
    /// Contiguous UTF-16 storage for all alternatives.
    alt_units: Vec<u16>,
    /// Contiguous Latin-1 storage when every alternative fits in u8; pre-lowercased when `ignore_case` is true.
    alt_latin1: Option<Vec<u8>>,
    /// Alternatives grouped by ASCII first code unit (preserving source order within each bucket),
    /// followed by any non-ASCII-first alternatives starting at `non_ascii_start`.
    entries: Vec<AltEntry>,
    ascii_buckets: Box<[(u16, u16); 128]>,
    non_ascii_start: u16,
    min_len: usize,
    all_single_unit: bool,
    first_scan: FirstUnitScan,
}

impl LiteralAltMatcher {
    fn new(alts: Vec<Vec<u16>>, ignore_case: bool) -> Self {
        let min_len = alts.iter().map(Vec::len).min().unwrap_or(0);
        let all_single_unit = min_len == 1 && alts.iter().all(|alt| alt.len() == 1);
        let first_scan = if min_len == 0 {
            FirstUnitScan::General
        } else {
            let mut distinct = Vec::<u16>::new();
            let mut all_latin1 = true;
            for alt in &alts {
                let first = alt[0];
                if ignore_case {
                    debug_assert!(first <= 0x7F);
                    let folded = fold_ascii_for_compare(first);
                    if !distinct.contains(&folded) {
                        distinct.push(folded);
                    }
                    if is_ascii_alpha(folded) {
                        let upper = folded - 32;
                        if !distinct.contains(&upper) {
                            distinct.push(upper);
                        }
                    }
                } else {
                    if first > 0xFF {
                        all_latin1 = false;
                    }
                    if !distinct.contains(&first) {
                        distinct.push(first);
                    }
                }
            }
            match distinct.as_slice() {
                [a] => FirstUnitScan::One(*a),
                [a, b] => FirstUnitScan::Two(*a, *b),
                [a, b, c] => FirstUnitScan::Three(*a, *b, *c),
                _ if all_latin1 => {
                    #[cfg(target_arch = "aarch64")]
                    let mut table = [false; 256];
                    let mut bits = [0u64; 4];
                    for &unit in &distinct {
                        let b = unit as usize;
                        #[cfg(target_arch = "aarch64")]
                        {
                            table[b] = true;
                        }
                        bits[b >> 6] |= 1u64 << (b & 63);
                    }
                    #[cfg(target_arch = "aarch64")]
                    if let Some(lut) = vm::AsciiNibbleLut::from_byte_table(&table) {
                        FirstUnitScan::AsciiLut { lut, bits }
                    } else {
                        FirstUnitScan::ByteBitset(bits)
                    }
                    #[cfg(not(target_arch = "aarch64"))]
                    {
                        FirstUnitScan::ByteBitset(bits)
                    }
                }
                _ => FirstUnitScan::General,
            }
        };

        // Flatten alternatives into contiguous storage and bucket by ASCII first unit while preserving
        // source order inside each bucket (preserving ECMAScript leftmost-first semantics).
        let total_units: usize = alts.iter().map(Vec::len).sum();
        let all_alts_latin1 = alts.iter().all(|alt| alt.iter().all(|&cu| cu <= 0xFF));
        let mut alt_units = Vec::with_capacity(total_units);
        let mut alt_latin1 = all_alts_latin1.then(|| Vec::with_capacity(total_units));
        let mut raw_entries = Vec::with_capacity(alts.len());
        let mut bucket_counts = [0u16; 128];
        let mut non_ascii_count = 0usize;

        for alt in &alts {
            if alt.is_empty() {
                continue;
            }
            let start = alt_units.len() as u32;
            let len = u16::try_from(alt.len()).unwrap_or(u16::MAX);
            let first = if ignore_case {
                fold_ascii_for_compare(alt[0])
            } else {
                alt[0]
            };
            alt_units.extend_from_slice(alt);
            if let Some(ref mut latin1) = alt_latin1 {
                if ignore_case {
                    latin1.extend(alt.iter().map(|&cu| fold_ascii_for_compare(cu) as u8));
                } else {
                    latin1.extend(alt.iter().map(|&cu| cu as u8));
                }
            }
            if first < 128 {
                bucket_counts[first as usize] += 1;
            } else {
                non_ascii_count += 1;
            }
            raw_entries.push(AltEntry { start, len, first });
        }

        let mut ascii_buckets = Box::new([(0u16, 0u16); 128]);
        let mut bucket_cursor = [0u16; 128];
        let mut running = 0u16;
        for (i, &count) in bucket_counts.iter().enumerate() {
            ascii_buckets[i] = (running, count);
            bucket_cursor[i] = running;
            running += count;
        }
        let non_ascii_start = running;
        let mut entries = vec![
            AltEntry {
                start: 0,
                len: 0,
                first: 0,
            };
            running as usize + non_ascii_count
        ];
        let mut non_ascii_cursor = non_ascii_start as usize;
        for entry in raw_entries {
            if entry.first < 128 {
                let idx = entry.first as usize;
                let dst = bucket_cursor[idx] as usize;
                entries[dst] = entry;
                bucket_cursor[idx] += 1;
            } else {
                entries[non_ascii_cursor] = entry;
                non_ascii_cursor += 1;
            }
        }

        Self {
            alt_units,
            alt_latin1,
            entries,
            ascii_buckets,
            non_ascii_start,
            min_len,
            all_single_unit,
            first_scan,
        }
    }

    #[inline(always)]
    fn find_next_candidate<I: vm::Input>(&self, input: I, start: usize, end: usize) -> Option<usize> {
        match &self.first_scan {
            FirstUnitScan::One(a) => input.find_code_unit(start, end, *a),
            FirstUnitScan::Two(a, b) => input.find_code_unit2(start, end, *a, *b),
            FirstUnitScan::Three(a, b, c) => input.find_code_unit3(start, end, *a, *b, *c),
            #[cfg(target_arch = "aarch64")]
            FirstUnitScan::AsciiLut { lut, bits } => input.find_ascii_lut_bitset(start, end, lut, bits),
            FirstUnitScan::ByteBitset(bits) => input.find_byte_in_bitset(start, end, bits),
            FirstUnitScan::General => (start < end).then_some(start),
        }
    }

    #[inline(always)]
    fn bucket_for_first(&self, first: u16) -> &[AltEntry] {
        if first < 128 {
            let (start, count) = self.ascii_buckets[first as usize];
            let start = start as usize;
            &self.entries[start..start + count as usize]
        } else {
            &self.entries[self.non_ascii_start as usize..]
        }
    }
}

/// A compiled regular expression.
pub struct Regex {
    /// Program for the backtracking VM (with fused loop optimizations).
    program: crate::bytecode::Program,
    flags: Flags,
    hints: vm::PatternHints,
    /// Pre-computed u16 literal for whole-pattern literal search.
    literal_u16: Option<Vec<u16>>,
    /// Pre-computed u16 literal for whole-pattern `\bliteral\b` matching.
    word_boundary_literal_u16: Option<Vec<u16>>,
    /// Pre-computed matcher for fast literal alternation matching.
    literal_alt: Option<LiteralAltMatcher>,
    /// Cached VM scratch space for reuse across exec calls.
    scratch: RefCell<vm::VmScratch>,
}

impl Regex {
    /// Compile a regex pattern with the given flags. If it does not compile, the result is what `error` makes of the
    /// parser error, which lives only for the call of `error`.
    pub fn compile<E>(pattern: &str, flags: Flags, error: impl FnOnce(parser::Error<'_>) -> E) -> Result<Self, E> {
        with_work_zone(|zone| {
            let source = zone.alloc_slice_from_iter(pattern.chars());
            Self::compile_in_zone(zone, source, flags).map_err(error)
        })
    }

    /// Compile a regex pattern of UTF-16 code units, in which a lone surrogate is U+FFFD (see `compile()`).
    pub fn compile_utf16<E>(
        pattern: &[u16],
        flags: Flags,
        error: impl FnOnce(parser::Error<'_>) -> E,
    ) -> Result<Self, E> {
        with_work_zone(|zone| {
            let source = zone.alloc_slice_from_iter(
                char::decode_utf16(pattern.iter().copied())
                    .map(|character| character.unwrap_or(char::REPLACEMENT_CHARACTER)),
            );
            Self::compile_in_zone(zone, source, flags).map_err(error)
        })
    }

    /// Compile a regex pattern of Latin-1 code units (see `compile()`).
    pub fn compile_latin1<E>(
        pattern: &[u8],
        flags: Flags,
        error: impl FnOnce(parser::Error<'_>) -> E,
    ) -> Result<Self, E> {
        with_work_zone(|zone| {
            let source = zone.alloc_slice_from_iter(pattern.iter().map(|byte| char::from(*byte)));
            Self::compile_in_zone(zone, source, flags).map_err(error)
        })
    }

    fn compile_in_zone<'z>(zone: &'z Zone, source: &'z [char], flags: Flags) -> Result<Self, parser::Error<'z>> {
        let parsed = parser::parse(zone, source, flags)?;
        let zone_program = compiler::compile(zone, &parsed)?;
        let required_literal_hint = literal_hints::extract_required_literal_hint(zone, &parsed, flags);
        let literal_u16 = literal_hints::extract_literal_u16(zone, &parsed, flags);
        let word_boundary_literal_u16 = literal_hints::extract_word_boundary_literal_u16(zone, &parsed, flags);
        let literal_alt_u16 = literal_hints::extract_literal_alternatives_u16(zone, &parsed, flags);

        let program = copy_out::program(&zone_program);
        let hints = vm::analyze_pattern(
            &program,
            required_literal_hint.as_ref().map(copy_out::required_literal_hint),
        );
        let literal_u16 = literal_u16.map(<[u16]>::to_vec);
        let word_boundary_literal_u16 = word_boundary_literal_u16.map(<[u16]>::to_vec);
        let literal_alt = literal_alt_u16
            .map(copy_out::literal_alternatives)
            .map(|alts| LiteralAltMatcher::new(alts, flags.ignore_case));

        Ok(Self {
            program,
            flags,
            hints,
            literal_u16,
            word_boundary_literal_u16,
            literal_alt,
            scratch: RefCell::new(vm::VmScratch::new()),
        })
    }

    /// Execute the regex, writing captures directly into a provided buffer.
    /// Returns a VmResult indicating match, no-match, or limit exceeded.
    /// The buffer should have `(capture_count + 1) * 2` i32 slots.
    /// On match, captures are written as pairs of (start, end) i32 values.
    pub fn exec_into(&self, input: &[u16], start: usize, out: &mut [i32]) -> vm::VmResult {
        self.exec_into_input(input, start, out)
    }

    pub fn exec_into_latin1(&self, input: &[u8], start: usize, out: &mut [i32]) -> vm::VmResult {
        self.exec_into_input(input, start, out)
    }

    pub(crate) fn exec_into_input<I: vm::Input>(&self, input: I, start: usize, out: &mut [i32]) -> vm::VmResult {
        if self.flags.sticky {
            let scratch = &mut *self.scratch.borrow_mut();
            return vm::execute_anchored_into_with_scratch(&self.program, input, start, &self.hints, out, scratch);
        }

        // Fast path for literal patterns: use fast substring search.
        // NB: Literal searches never hit the step limit.
        if let Some(ref needle) = self.literal_u16 {
            return if Self::literal_search(input, start, needle, &self.flags, out) {
                vm::VmResult::Match
            } else {
                vm::VmResult::NoMatch
            };
        }
        if let Some(ref needle) = self.word_boundary_literal_u16 {
            return if Self::word_boundary_literal_search(input, start, needle, &self.flags, out) {
                vm::VmResult::Match
            } else {
                vm::VmResult::NoMatch
            };
        }
        // Fast path for literal alternation patterns.
        if let Some(ref matcher) = self.literal_alt {
            return if Self::literal_alt_search(input, start, matcher, &self.flags, out) {
                vm::VmResult::Match
            } else {
                vm::VmResult::NoMatch
            };
        }
        let scratch = &mut *self.scratch.borrow_mut();
        vm::execute_into_with_scratch(&self.program, input, start, &self.hints, out, scratch)
    }

    /// Test whether the regex matches anywhere in the input.
    pub fn test(&self, input: &[u16], start: usize) -> vm::VmResult {
        self.test_input(input, start)
    }

    pub fn test_latin1(&self, input: &[u8], start: usize) -> vm::VmResult {
        self.test_input(input, start)
    }

    pub(crate) fn test_input<I: vm::Input>(&self, input: I, start: usize) -> vm::VmResult {
        if self.flags.sticky {
            let mut out = [-1i32; 2];
            let scratch = &mut *self.scratch.borrow_mut();
            return vm::execute_anchored_into_with_scratch(&self.program, input, start, &self.hints, &mut out, scratch);
        }

        if let Some(ref needle) = self.literal_u16 {
            return if Self::literal_test(input, start, needle, &self.flags) {
                vm::VmResult::Match
            } else {
                vm::VmResult::NoMatch
            };
        }
        if let Some(ref needle) = self.word_boundary_literal_u16 {
            return if Self::word_boundary_literal_test(input, start, needle, &self.flags) {
                vm::VmResult::Match
            } else {
                vm::VmResult::NoMatch
            };
        }
        if let Some(ref matcher) = self.literal_alt {
            let mut out = [-1i32; 2];
            return if Self::literal_alt_search(input, start, matcher, &self.flags, &mut out) {
                vm::VmResult::Match
            } else {
                vm::VmResult::NoMatch
            };
        }
        // Reuse cached scratch space for the VM. Only need group 0 for test().
        let mut out = [-1i32; 2];
        let scratch = &mut *self.scratch.borrow_mut();
        vm::execute_into_with_scratch(&self.program, input, start, &self.hints, &mut out, scratch)
    }

    /// Fast literal substring search for whole-pattern literal fast paths.
    fn literal_search<I: vm::Input>(input: I, start: usize, needle: &[u16], flags: &Flags, out: &mut [i32]) -> bool {
        if needle.is_empty() {
            // Empty pattern matches at start position.
            if out.len() >= 2 {
                out[0] = start as i32;
                out[1] = start as i32;
            }
            return true;
        }

        if flags.ignore_case {
            let needle_len = needle.len();
            if start + needle_len > input.len() {
                return false;
            }
            if needle.iter().all(|ch| *ch <= 0x7F) {
                let first = needle[0];
                let mut pos = start;
                let end = input.len() - needle_len + 1;
                while pos < end {
                    let next = if is_ascii_alpha(first) {
                        find_ascii_case_insensitive_code_unit(input, pos, end, first)
                    } else {
                        input.find_code_unit(pos, end, first)
                    };
                    match next {
                        Some(candidate_pos) => pos = candidate_pos,
                        None => return false,
                    }
                    if matches_ascii_case_insensitive_u16_at(input, pos, needle) {
                        if out.len() >= 2 {
                            out[0] = pos as i32;
                            out[1] = (pos + needle_len) as i32;
                        }
                        return true;
                    }
                    pos += 1;
                }
                return false;
            }
            // Non-ASCII case-insensitive literals fall back to linear scan with
            // full case folding.
            'outer: for pos in start..=input.len() - needle_len {
                for (j, expected) in needle.iter().enumerate() {
                    if !vm::case_fold_eq(
                        input.code_unit(pos + j) as u32,
                        *expected as u32,
                        flags.unicode || flags.unicode_sets,
                    ) {
                        continue 'outer;
                    }
                }
                if out.len() >= 2 {
                    out[0] = pos as i32;
                    out[1] = (pos + needle_len) as i32;
                }
                return true;
            }
            return false;
        }

        // Case-sensitive: use fast first-character scan + verify.
        let first = needle[0];
        let needle_len = needle.len();
        if start + needle_len > input.len() {
            return false;
        }
        let mut pos = start;
        let end = input.len() - needle_len + 1;
        while pos < end {
            // Bulk scan for first character.
            match input.find_code_unit(pos, end, first) {
                Some(candidate_pos) => pos = candidate_pos,
                None => return false,
            }
            // Verify rest of needle.
            if input.matches_u16_at(pos, needle) {
                if out.len() >= 2 {
                    out[0] = pos as i32;
                    out[1] = (pos + needle_len) as i32;
                }
                return true;
            }
            pos += 1;
        }
        false
    }

    /// Fast literal test (no captures needed).
    fn literal_test<I: vm::Input>(input: I, start: usize, needle: &[u16], flags: &Flags) -> bool {
        let mut out = [0i32; 2];
        Self::literal_search(input, start, needle, flags, &mut out)
    }

    fn word_boundary_literal_search<I: vm::Input>(
        input: I,
        start: usize,
        needle: &[u16],
        flags: &Flags,
        out: &mut [i32],
    ) -> bool {
        let mut pos = start;
        let mut literal_out = [-1i32; 2];
        while pos < input.len() {
            literal_out[0] = -1;
            literal_out[1] = -1;
            if !Self::literal_search(input, pos, needle, flags, &mut literal_out) {
                return false;
            }

            let match_start = literal_out[0] as usize;
            let match_end = literal_out[1] as usize;
            if has_ascii_word_boundaries_at(input, match_start, match_end) {
                if out.len() >= 2 {
                    out[0] = match_start as i32;
                    out[1] = match_end as i32;
                }
                return true;
            }

            pos = match_start + 1;
        }
        false
    }

    fn word_boundary_literal_test<I: vm::Input>(input: I, start: usize, needle: &[u16], flags: &Flags) -> bool {
        let mut out = [0i32; 2];
        Self::word_boundary_literal_search(input, start, needle, flags, &mut out)
    }

    /// Fast literal alternation search: find the first matching alternative.
    /// Alternatives are in source order to preserve ECMAScript leftmost-first semantics.
    fn literal_alt_search<I: vm::Input>(
        input: I,
        start: usize,
        matcher: &LiteralAltMatcher,
        flags: &Flags,
        out: &mut [i32],
    ) -> bool {
        if flags.ignore_case {
            return Self::literal_alt_search_ascii_ignore_case(input, start, matcher, out);
        }

        if matcher.min_len == 0 || start + matcher.min_len > input.len() {
            return false;
        }
        let end = input.len() - matcher.min_len + 1;
        if matcher.all_single_unit && !matches!(matcher.first_scan, FirstUnitScan::General) {
            let Some(pos) = matcher.find_next_candidate(input, start, end) else {
                return false;
            };
            if out.len() >= 2 {
                out[0] = pos as i32;
                out[1] = (pos + 1) as i32;
            }
            return true;
        }
        let mut pos = start;
        while pos < end {
            let Some(candidate_pos) = matcher.find_next_candidate(input, pos, end) else {
                return false;
            };
            pos = candidate_pos;
            let first_ch = input.code_unit(pos);
            let bucket = matcher.bucket_for_first(first_ch);
            if let Some(ref latin1) = matcher.alt_latin1 {
                for entry in bucket {
                    if first_ch >= 128 && entry.first != first_ch {
                        continue;
                    }
                    let alt_len = entry.len as usize;
                    if pos + alt_len > input.len() {
                        continue;
                    }
                    let alt_start = entry.start as usize;
                    if alt_len == 1 || input.matches_latin1_at(pos + 1, &latin1[alt_start + 1..alt_start + alt_len]) {
                        if out.len() >= 2 {
                            out[0] = pos as i32;
                            out[1] = (pos + alt_len) as i32;
                        }
                        return true;
                    }
                }
            } else {
                for entry in bucket {
                    if first_ch >= 128 && entry.first != first_ch {
                        continue;
                    }
                    let alt_len = entry.len as usize;
                    if pos + alt_len > input.len() {
                        continue;
                    }
                    let alt_start = entry.start as usize;
                    if alt_len == 1
                        || input.matches_u16_at(pos + 1, &matcher.alt_units[alt_start + 1..alt_start + alt_len])
                    {
                        if out.len() >= 2 {
                            out[0] = pos as i32;
                            out[1] = (pos + alt_len) as i32;
                        }
                        return true;
                    }
                }
            }
            pos += 1;
        }
        false
    }

    fn literal_alt_search_ascii_ignore_case<I: vm::Input>(
        input: I,
        start: usize,
        matcher: &LiteralAltMatcher,
        out: &mut [i32],
    ) -> bool {
        if matcher.min_len == 0 || start + matcher.min_len > input.len() {
            return false;
        }

        let end = input.len() - matcher.min_len + 1;
        if matcher.all_single_unit && !matches!(matcher.first_scan, FirstUnitScan::General) {
            let Some(pos) = matcher.find_next_candidate(input, start, end) else {
                return false;
            };
            if out.len() >= 2 {
                out[0] = pos as i32;
                out[1] = (pos + 1) as i32;
            }
            return true;
        }
        let latin1 = matcher
            .alt_latin1
            .as_deref()
            .expect("ascii ignore-case literal alternatives are always ASCII");
        let mut pos = start;
        while pos < end {
            let Some(candidate_pos) = matcher.find_next_candidate(input, pos, end) else {
                return false;
            };
            pos = candidate_pos;

            let folded_first = fold_ascii_for_compare(input.code_unit(pos));
            if folded_first < 128 {
                let (bucket_start, bucket_count) = matcher.ascii_buckets[folded_first as usize];
                let bucket_start = bucket_start as usize;
                for entry in &matcher.entries[bucket_start..bucket_start + bucket_count as usize] {
                    let alt_len = entry.len as usize;
                    if pos + alt_len > input.len() {
                        continue;
                    }
                    let alt_start = entry.start as usize;
                    if alt_len == 1
                        || input
                            .matches_ascii_ignore_case_latin1_at(pos + 1, &latin1[alt_start + 1..alt_start + alt_len])
                    {
                        if out.len() >= 2 {
                            out[0] = pos as i32;
                            out[1] = (pos + alt_len) as i32;
                        }
                        return true;
                    }
                }
            }
            pos += 1;
        }
        false
    }

    /// Find all literal matches, writing (start, end) pairs into result_buf.
    fn literal_find_all<I: vm::Input>(
        input: I,
        start: usize,
        needle: &[u16],
        flags: &Flags,
        result_buf: &mut [i32],
    ) -> i32 {
        let capacity = result_buf.len();
        let mut count = 0i32;
        let mut pos = start;
        let mut out = [-1i32; 2];
        loop {
            if pos > input.len() {
                break;
            }
            out[0] = -1;
            out[1] = -1;
            if !Self::literal_search(input, pos, needle, flags, &mut out) {
                break;
            }
            let idx = count as usize * 2;
            if idx + 1 >= capacity {
                return -1;
            }
            result_buf[idx] = out[0];
            result_buf[idx + 1] = out[1];
            count += 1;
            if out[1] == out[0] {
                pos = out[1] as usize + 1;
            } else {
                pos = out[1] as usize;
            }
        }
        count
    }

    fn word_boundary_literal_find_all<I: vm::Input>(
        input: I,
        start: usize,
        needle: &[u16],
        flags: &Flags,
        result_buf: &mut [i32],
    ) -> i32 {
        let capacity = result_buf.len();
        let mut count = 0i32;
        let mut pos = start;
        let mut out = [-1i32; 2];
        loop {
            if pos > input.len() {
                break;
            }
            out[0] = -1;
            out[1] = -1;
            if !Self::word_boundary_literal_search(input, pos, needle, flags, &mut out) {
                break;
            }
            let idx = count as usize * 2;
            if idx + 1 >= capacity {
                return -1;
            }
            result_buf[idx] = out[0];
            result_buf[idx + 1] = out[1];
            count += 1;
            pos = out[1] as usize;
        }
        count
    }

    /// Find all literal alternation matches, writing (start, end) pairs into result_buf.
    fn literal_alt_find_all<I: vm::Input>(
        input: I,
        start: usize,
        matcher: &LiteralAltMatcher,
        flags: &Flags,
        result_buf: &mut [i32],
    ) -> i32 {
        let capacity = result_buf.len();
        let mut count = 0i32;
        let mut pos = start;
        let mut out = [-1i32; 2];
        loop {
            if pos > input.len() {
                break;
            }
            out[0] = -1;
            out[1] = -1;
            if !Self::literal_alt_search(input, pos, matcher, flags, &mut out) {
                break;
            }
            let idx = count as usize * 2;
            if idx + 1 >= capacity {
                return -1;
            }
            result_buf[idx] = out[0];
            result_buf[idx + 1] = out[1];
            count += 1;
            if out[1] == out[0] {
                pos = out[1] as usize + 1;
            } else {
                pos = out[1] as usize;
            }
        }
        count
    }

    /// Get the number of capture groups (not counting group 0).
    pub fn capture_count(&self) -> u32 {
        self.program.capture_count
    }

    /// Get the named capture groups (in order of appearance in the pattern).
    pub fn named_groups(&self) -> &[NamedGroupEntry] {
        &self.program.named_groups
    }

    pub fn is_single_non_bmp_literal(&self) -> bool {
        (self.flags.unicode || self.flags.unicode_sets)
            && matches!(
                self.literal_u16.as_deref(),
                Some([high, low])
                    if (0xD800..=0xDBFF).contains(high) && (0xDC00..=0xDFFF).contains(low)
            )
    }

    /// Find all non-overlapping matches starting from `start`.
    /// Writes (match_start, match_end) i32 pairs directly into `result_buf`.
    /// Returns number of matches found, or -1 if buffer is too small.
    pub fn find_all_into(&self, input: &[u16], start: usize, result_buf: &mut [i32]) -> i32 {
        self.find_all_into_input(input, start, result_buf)
    }

    pub fn find_all_into_latin1(&self, input: &[u8], start: usize, result_buf: &mut [i32]) -> i32 {
        self.find_all_into_input(input, start, result_buf)
    }

    pub(crate) fn find_all_into_input<I: vm::Input>(&self, input: I, start: usize, result_buf: &mut [i32]) -> i32 {
        // Fast path for literal patterns.
        if let Some(ref needle) = self.literal_u16 {
            return Self::literal_find_all(input, start, needle, &self.flags, result_buf);
        }
        if let Some(ref needle) = self.word_boundary_literal_u16 {
            return Self::word_boundary_literal_find_all(input, start, needle, &self.flags, result_buf);
        }
        // Fast path for literal alternation patterns.
        if let Some(ref matcher) = self.literal_alt {
            return Self::literal_alt_find_all(input, start, matcher, &self.flags, result_buf);
        }
        // Use the VM-internal find_all loop which reuses a single VM across matches.
        let scratch = &mut *self.scratch.borrow_mut();
        vm::find_all_with_scratch(&self.program, input, start, &self.hints, result_buf, scratch)
    }
}

#[inline(always)]
fn is_ascii_alpha(ch: u16) -> bool {
    matches!(ch, 0x41..=0x5A | 0x61..=0x7A)
}

#[inline(always)]
fn fold_ascii_for_compare(ch: u16) -> u16 {
    match ch {
        0x41..=0x5A => ch + 32,
        _ => ch,
    }
}

#[inline(always)]
fn find_ascii_case_insensitive_code_unit<I: vm::Input>(
    input: I,
    start: usize,
    end: usize,
    needle: u16,
) -> Option<usize> {
    let folded_needle = fold_ascii_for_compare(needle);
    let mut pos = start;
    while pos < end {
        let code_unit = input.code_unit(pos);
        if code_unit <= 0x7F && fold_ascii_for_compare(code_unit) == folded_needle {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

#[inline(always)]
fn matches_ascii_case_insensitive_u16_at<I: vm::Input>(input: I, pos: usize, needle: &[u16]) -> bool {
    if pos + needle.len() > input.len() {
        return false;
    }
    for (offset, expected) in needle.iter().enumerate() {
        let actual = input.code_unit(pos + offset);
        if actual > 0x7F || fold_ascii_for_compare(actual) != fold_ascii_for_compare(*expected) {
            return false;
        }
    }
    true
}

#[inline(always)]
fn has_ascii_word_boundaries_at<I: vm::Input>(input: I, match_start: usize, match_end: usize) -> bool {
    let left_is_word = match_start > 0 && is_ascii_word_code_unit(input.code_unit(match_start - 1));
    let right_is_word = match_end < input.len() && is_ascii_word_code_unit(input.code_unit(match_end));
    !left_is_word && !right_is_word
}

#[cfg(test)]
mod tests {
    use super::Regex;
    use crate::vm::VmResult;
    use libregex_compiler::ast::Flags;

    fn exec_both(pattern: &str, flags: Flags, text: &str) -> (VmResult, [i32; 2]) {
        let regex = Regex::compile(pattern, flags, |err| format!("{err:?}")).expect("pattern compiles");
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let mut out_u16 = [-1i32; 2];
        let res_u16 = regex.exec_into(&utf16, 0, &mut out_u16);
        if text.is_ascii() {
            let mut out_u8 = [-1i32; 2];
            let res_u8 = regex.exec_into_latin1(text.as_bytes(), 0, &mut out_u8);
            assert_eq!(res_u8, res_u16);
            assert_eq!(out_u8, out_u16);
        }
        (res_u16, out_u16)
    }

    #[test]
    fn literal_alternation_acceleration_preserves_leftmost_first_semantics() {
        let flags = Flags::default();
        assert_eq!(exec_both("foo|fbar", flags, "xx fbar foo"), (VmResult::Match, [3, 7]));
        assert_eq!(exec_both("foo|foobar", flags, "xx foobar"), (VmResult::Match, [3, 6]));
        assert_eq!(exec_both("foobar|foo", flags, "xx foobar"), (VmResult::Match, [3, 9]));
        assert_eq!(
            exec_both("alpha|beta|gamma", flags, "zzz gamma beta"),
            (VmResult::Match, [4, 9])
        );
        assert_eq!(
            exec_both("one|two|three|four|five", flags, "012345 four five"),
            (VmResult::Match, [7, 11])
        );
        assert_eq!(
            exec_both("one|two|three|four|five", flags, "nothing here"),
            (VmResult::NoMatch, [-1, -1])
        );

        let icase = Flags {
            ignore_case: true,
            ..Flags::default()
        };
        assert_eq!(
            exec_both("foo|bar|baz|qux", icase, "zzz BAZ"),
            (VmResult::Match, [4, 7])
        );
        assert_eq!(exec_both("foo|bar", icase, "zzz BAR"), (VmResult::Match, [4, 7]));
    }

    #[test]
    fn simd_long_input_scans_match_scalar_semantics() {
        let flags = Flags::default();
        let pad_a = "a".repeat(80);
        let pad_space = " ".repeat(80);
        let pad_punct = ".".repeat(80);

        // Character class + negated class over >= 64 bytes (exercises NEON nibble LUT and 16-byte loops).
        let text_lut = format!("{pad_a}X123");
        assert_eq!(exec_both("[0-9X]+", flags, &text_lut), (VmResult::Match, [80, 84]));
        assert_eq!(exec_both("[^a]+", flags, &text_lut), (VmResult::Match, [80, 84]));

        // Whitespace scan including non-ASCII NBSP (U+00A0) in both Latin-1 and UTF-16 inputs.
        let regex_ws = Regex::compile(r"\s+", flags, |err| format!("{err:?}")).expect("compiles");
        let mut latin1_nbsp = vec![b'x'; 75];
        latin1_nbsp.push(0xA0);
        latin1_nbsp.push(b' ');
        let mut out_l1 = [-1i32; 2];
        assert_eq!(regex_ws.exec_into_latin1(&latin1_nbsp, 0, &mut out_l1), VmResult::Match);
        assert_eq!(out_l1, [75, 77]);

        let text_nbsp = format!("{pad_a}\u{00A0}z");
        assert_eq!(exec_both(r"\s+", flags, &text_nbsp), (VmResult::Match, [80, 81]));
        assert_eq!(
            exec_both(r"\S+", flags, &format!("{pad_space}hi")),
            (VmResult::Match, [80, 82])
        );

        // Digits and word characters + bounded quantifiers over >= 64 bytes.
        assert_eq!(
            exec_both(r"\d{2,4}", flags, &format!("{pad_a}98765")),
            (VmResult::Match, [80, 84])
        );
        assert_eq!(
            exec_both(r"\w+", flags, &format!("{pad_punct}abc_1")),
            (VmResult::Match, [80, 85])
        );
        assert_eq!(exec_both(r"a{10,70}", flags, &pad_a), (VmResult::Match, [0, 70]));

        // UTF-16 1-, 2-, and 3-unit first-unit scans past 64 code units.
        let pad_wide = "\u{03B1}".repeat(70);
        assert_eq!(
            exec_both("Z", flags, &format!("{pad_wide}Z")),
            (VmResult::Match, [70, 71])
        );
        assert_eq!(
            exec_both("foo|bar", flags, &format!("{pad_wide}bar")),
            (VmResult::Match, [70, 73])
        );
        assert_eq!(
            exec_both("alpha|beta|gamma", flags, &format!("{pad_wide}gamma")),
            (VmResult::Match, [70, 75])
        );
    }
}
