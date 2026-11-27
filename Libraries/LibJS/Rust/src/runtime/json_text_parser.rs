/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The JSON text parser of JSON.parse without a reviver. It reads the code units of the text once and makes the values
//! of the parse nodes as it goes, since nothing but the result needs the parse nodes when there is no reviver to give
//! their source text to.

use core::cell::{Cell, UnsafeCell};
use core::hash::{Hash, Hasher};

use ak::{Utf16FlyString, Utf16String};

use crate::gc::sweep::Sweep;
use crate::gc::visitor::{Trace, Visitor};
use crate::gc::weak::GcWeak;
use crate::interpreter::vm::Vm;
use crate::layout::cell::Gc;
use crate::layout::object::Object;
use crate::layout::value::Value;
use crate::runtime::array::Array;
use crate::runtime::completion::ThrowCompletionOr;
use crate::runtime::error::ErrorKind;
use crate::runtime::error_types::ErrorType;
use crate::runtime::object::MAX_TRANSITIONS_BEFORE_CONVERTING_TO_DICTIONARY;
use crate::runtime::primitive_string::PrimitiveString;
use crate::runtime::property_attributes::DEFAULT_ATTRIBUTES;
use crate::runtime::property_key::{NonSymbolPropertyKey, PropertyKey};
use crate::runtime::realm::Realm;
use crate::runtime::shape::Shape;
use crate::utf16::{Utf16View, is_json_special_code_unit, json_special_code_unit_lanes};

/// A Latin-1 code unit, stored as one byte and potentially above U+007F.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Latin1Byte(u8);

impl From<Latin1Byte> for u32 {
    #[inline(always)]
    fn from(unit: Latin1Byte) -> u32 {
        u32::from(unit.0)
    }
}

/// Inline word-wise equality for UTF-16 slices, avoiding PLT calls to `memcmp` on short JSON keys and cached strings.
#[inline(always)]
fn u16_slices_equal(a: &[u16], b: &[u16]) -> bool {
    let len = a.len();
    if len != b.len() {
        return false;
    }
    let pa = a.as_ptr();
    let pb = b.as_ptr();
    let mut index = 0usize;
    // SAFETY: Every unaligned read stays strictly within `0..len` of both `a` and `b`.
    unsafe {
        while index + 4 <= len {
            if core::ptr::read_unaligned(pa.add(index).cast::<u64>())
                != core::ptr::read_unaligned(pb.add(index).cast::<u64>())
            {
                return false;
            }
            index += 4;
        }
        if index + 2 <= len {
            if core::ptr::read_unaligned(pa.add(index).cast::<u32>())
                != core::ptr::read_unaligned(pb.add(index).cast::<u32>())
            {
                return false;
            }
            index += 2;
        }
        if index < len && *pa.add(index) != *pb.add(index) {
            return false;
        }
    }
    true
}

/// The code units of a JSON text: ASCII bytes, Latin-1 bytes or UTF-16 code units.
trait CodeUnit: Copy + Eq + Into<u32> + 'static {
    fn make_string(units: &[Self]) -> Utf16String;
    fn make_primitive_string(vm: &Vm, units: &[Self]) -> Gc<PrimitiveString>;
    fn make_fly_string(units: &[Self]) -> Utf16FlyString;
    fn equals_byte_slice(units: &[Self], bytes: &[u8]) -> bool;
    fn equals_u16_slice(units: &[Self], code_units: &[u16]) -> bool;
    fn equals_simple_key(units: &[Self], simple: &SimpleByteKeys, start: usize, end: usize) -> bool;

    /// The position of the first unit at or after `position` that is a quotation mark, a reverse solidus or a
    /// control character, or the length of `units` if there is none.
    fn skip_ordinary_string_units(units: &[Self], position: usize) -> usize;
}

impl CodeUnit for u8 {
    fn make_string(units: &[u8]) -> Utf16String {
        Utf16String::from_ascii(units)
    }

    fn make_primitive_string(vm: &Vm, units: &[u8]) -> Gc<PrimitiveString> {
        PrimitiveString::create_from_ascii(vm, units)
    }

    fn make_fly_string(units: &[u8]) -> Utf16FlyString {
        Utf16FlyString::from_utf8(core::str::from_utf8(units).expect("the text is ASCII"))
    }

    #[inline(always)]
    fn equals_byte_slice(units: &[u8], bytes: &[u8]) -> bool {
        units == bytes
    }

    #[inline]
    fn equals_u16_slice(units: &[u8], code_units: &[u16]) -> bool {
        units.len() == code_units.len()
            && units
                .iter()
                .zip(code_units)
                .all(|(&byte, &code_unit)| u16::from(byte) == code_unit)
    }

    #[inline(always)]
    fn equals_simple_key(units: &[u8], simple: &SimpleByteKeys, start: usize, end: usize) -> bool {
        units == &simple.bytes[start..end]
    }

    fn skip_ordinary_string_units(units: &[u8], mut position: usize) -> usize {
        while let Some(chunk) = units.get(position..position + 8) {
            let word = u64::from_le_bytes(chunk.try_into().expect("the chunk has eight units"));
            let special_lanes = json_special_code_unit_lanes(word, 0x0101_0101_0101_0101);
            if special_lanes != 0 {
                return position + (special_lanes.trailing_zeros() / 8) as usize;
            }
            position += 8;
        }
        while position < units.len() && !is_json_special_code_unit(u32::from(units[position])) {
            position += 1;
        }
        position
    }
}

impl CodeUnit for Latin1Byte {
    fn make_string(units: &[Latin1Byte]) -> Utf16String {
        let bytes = Self::as_bytes(units);
        Utf16String::from_latin1(bytes)
    }

    fn make_primitive_string(vm: &Vm, units: &[Latin1Byte]) -> Gc<PrimitiveString> {
        let bytes = Self::as_bytes(units);
        if bytes.is_ascii() {
            return PrimitiveString::create_from_ascii(vm, bytes);
        }
        PrimitiveString::create(vm, Utf16String::from_latin1(bytes))
    }

    fn make_fly_string(units: &[Latin1Byte]) -> Utf16FlyString {
        let bytes = Self::as_bytes(units);
        if bytes.is_ascii() {
            return Utf16FlyString::from_utf8(core::str::from_utf8(bytes).expect("the slice is ASCII"));
        }
        Utf16View::Latin1(bytes).to_utf16_fly_string()
    }

    #[inline(always)]
    fn equals_byte_slice(units: &[Latin1Byte], bytes: &[u8]) -> bool {
        Self::as_bytes(units) == bytes
    }

    #[inline]
    fn equals_u16_slice(units: &[Latin1Byte], code_units: &[u16]) -> bool {
        let bytes = Self::as_bytes(units);
        bytes.len() == code_units.len()
            && bytes
                .iter()
                .zip(code_units)
                .all(|(&byte, &code_unit)| u16::from(byte) == code_unit)
    }

    #[inline(always)]
    fn equals_simple_key(units: &[Latin1Byte], simple: &SimpleByteKeys, start: usize, end: usize) -> bool {
        Self::as_bytes(units) == &simple.bytes[start..end]
    }

    fn skip_ordinary_string_units(units: &[Latin1Byte], mut position: usize) -> usize {
        let bytes = Self::as_bytes(units);
        while let Some(chunk) = bytes.get(position..position + 8) {
            let word = u64::from_le_bytes(chunk.try_into().expect("the chunk has eight units"));
            let special_lanes = json_special_code_unit_lanes(word, 0x0101_0101_0101_0101);
            if special_lanes != 0 {
                return position + (special_lanes.trailing_zeros() / 8) as usize;
            }
            position += 8;
        }
        while position < bytes.len() && !is_json_special_code_unit(u32::from(bytes[position])) {
            position += 1;
        }
        position
    }
}

impl Latin1Byte {
    #[inline(always)]
    fn slice_from_bytes(bytes: &[u8]) -> &[Self] {
        // SAFETY: `Latin1Byte` is `#[repr(transparent)]` over `u8`.
        unsafe { ak::slice_from_raw_parts(bytes.as_ptr().cast::<Self>(), bytes.len()) }
    }

    #[inline(always)]
    fn as_bytes(units: &[Self]) -> &[u8] {
        // SAFETY: `Latin1Byte` is `#[repr(transparent)]` over `u8`.
        unsafe { ak::slice_from_raw_parts(units.as_ptr().cast::<u8>(), units.len()) }
    }
}

impl CodeUnit for u16 {
    fn make_string(units: &[u16]) -> Utf16String {
        Utf16String::from_utf16(units)
    }

    fn make_primitive_string(vm: &Vm, units: &[u16]) -> Gc<PrimitiveString> {
        const MAX_INLINE_ASCII: usize = crate::layout::primitive_string::INLINE_STRING_CAPACITY;
        let len = units.len();
        if len <= MAX_INLINE_ASCII {
            let mut ascii = [0u8; MAX_INLINE_ASCII];
            let mut or_bits = 0u16;
            let mut index = 0;
            #[cfg(target_arch = "aarch64")]
            if len >= 8 {
                use core::arch::aarch64::*;
                // SAFETY: NEON is always available on aarch64 and every load/store stays in bounds.
                unsafe {
                    let mut acc = vdupq_n_u16(0);
                    while index + 8 <= len {
                        let v = vld1q_u16(units.as_ptr().add(index));
                        acc = vorrq_u16(acc, v);
                        vst1_u8(ascii.as_mut_ptr().add(index), vmovn_u16(v));
                        index += 8;
                    }
                    or_bits = vmaxvq_u16(acc);
                }
            }
            while index < len {
                let unit = units[index];
                or_bits |= unit;
                ascii[index] = unit as u8;
                index += 1;
            }
            if or_bits <= 0x7f {
                if len < size_of::<usize>() {
                    return PrimitiveString::create(vm, Utf16String::from_ascii(&ascii[..len]));
                }
                return PrimitiveString::create_from_inline_characters(vm, ascii, len);
            }
        }
        PrimitiveString::create(vm, Utf16String::from_utf16(units))
    }

    fn make_fly_string(units: &[u16]) -> Utf16FlyString {
        if units.len() <= 64 {
            let mut ascii = [0u8; 64];
            let mut or_bits = 0u16;
            for (byte, &unit) in ascii.iter_mut().zip(units) {
                or_bits |= unit;
                *byte = unit as u8;
            }
            if or_bits <= 0x7f {
                return Utf16FlyString::from_utf8(
                    core::str::from_utf8(&ascii[..units.len()]).expect("the slice is ASCII"),
                );
            }
        }
        Utf16FlyString::from_utf16(units)
    }

    #[inline(always)]
    fn equals_byte_slice(units: &[u16], bytes: &[u8]) -> bool {
        if units.len() != bytes.len() {
            return false;
        }
        let len = units.len();
        let mut index = 0;
        #[cfg(target_arch = "aarch64")]
        {
            use core::arch::aarch64::*;
            // SAFETY: NEON is always available on aarch64 and every load stays within `units` and `bytes`.
            unsafe {
                while index + 8 <= len {
                    let wide_bytes = vmovl_u8(vld1_u8(bytes.as_ptr().add(index)));
                    let wide_units = vld1q_u16(units.as_ptr().add(index));
                    if vminvq_u16(vceqq_u16(wide_bytes, wide_units)) == 0 {
                        return false;
                    }
                    index += 8;
                }
                if index + 4 <= len {
                    let b4 = u32::from_ne_bytes([
                        bytes[index],
                        bytes[index + 1],
                        bytes[index + 2],
                        bytes[index + 3],
                    ]);
                    let wide_bytes = vget_low_u16(vmovl_u8(vreinterpret_u8_u32(vdup_n_u32(b4))));
                    let wide_units = vld1_u16(units.as_ptr().add(index));
                    if vminv_u16(vceq_u16(wide_bytes, wide_units)) == 0 {
                        return false;
                    }
                    index += 4;
                }
            }
        }
        while index < len {
            if units[index] != u16::from(bytes[index]) {
                return false;
            }
            index += 1;
        }
        true
    }

    #[inline(always)]
    fn equals_u16_slice(units: &[u16], code_units: &[u16]) -> bool {
        u16_slices_equal(units, code_units)
    }

    #[inline(always)]
    fn equals_simple_key(units: &[u16], simple: &SimpleByteKeys, start: usize, end: usize) -> bool {
        u16_slices_equal(units, &simple.u16_units[start..end])
    }

    #[inline]
    fn skip_ordinary_string_units(units: &[u16], mut position: usize) -> usize {
        #[cfg(target_arch = "aarch64")]
        {
            use core::arch::aarch64::*;
            // SAFETY: NEON is always available on aarch64 and every load stays within `units`.
            unsafe {
                let quote = vdupq_n_u16(0x22);
                let backslash = vdupq_n_u16(0x5C);
                let space = vdupq_n_u16(0x20);
                while position + 8 <= units.len() {
                    let chunk = vld1q_u16(units.as_ptr().add(position));
                    let special = vorrq_u16(
                        vorrq_u16(vceqq_u16(chunk, quote), vceqq_u16(chunk, backslash)),
                        vcltq_u16(chunk, space),
                    );
                    let mask = vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(special, 4)), 0);
                    if mask != 0 {
                        return position + (mask.trailing_zeros() >> 3) as usize;
                    }
                    position += 8;
                }
                while position < units.len() && !is_json_special_code_unit(u32::from(units[position])) {
                    position += 1;
                }
                position
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            while let Some(chunk) = units.get(position..position + 4) {
                #[cfg(target_endian = "little")]
                let word = {
                    // SAFETY: `chunk` holds four valid `u16` values (eight bytes).
                    unsafe { core::ptr::read_unaligned(chunk.as_ptr().cast::<u64>()) }
                };
                #[cfg(not(target_endian = "little"))]
                let word = chunk.iter().rev().fold(0, |word, unit| (word << 16) | u64::from(*unit));
                let special_lanes = json_special_code_unit_lanes(word, 0x0001_0001_0001_0001);
                if special_lanes != 0 {
                    return position + (special_lanes.trailing_zeros() / 16) as usize;
                }
                position += 4;
            }
            while position < units.len() && !is_json_special_code_unit(u32::from(units[position])) {
                position += 1;
            }
            position
        }
    }
}

const STRING_VALUE_CACHE_SETS: usize = 256;
const STRING_VALUE_CACHE_SLOTS: usize = STRING_VALUE_CACHE_SETS * 2;
const STRING_VALUE_CACHE_MAX_LEN: usize = 128;

/// Scratch stack of GC-traced `Value`s used while parsing a reviver-less JSON text.
///
/// `parse_json_text` executes no JavaScript callbacks; the only concurrent reader is GC tracing during cell
/// allocations, when no `push` or `truncate` is active. Using `UnsafeCell` avoids per-element `RefCell` borrow flag
/// churn on every parsed array element and object property.
pub struct JsonValueStack(UnsafeCell<Vec<Value>>);

impl Default for JsonValueStack {
    fn default() -> Self {
        Self(UnsafeCell::new(Vec::new()))
    }
}

impl JsonValueStack {
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        // SAFETY: No mutable borrow is active across calls.
        unsafe { (&*self.0.get()).len() }
    }

    #[inline(always)]
    fn push(&self, value: Value) {
        // SAFETY: `Vec::push` cannot trigger JS GC or re-enter `JsonValueStack`, so the exclusive borrow is unaliased.
        unsafe { (&mut *self.0.get()).push(value) };
    }

    #[inline(always)]
    fn truncate(&self, len: usize) {
        // SAFETY: `Vec::truncate` cannot trigger JS GC or re-enter `JsonValueStack`, so the exclusive borrow is
        //         unaliased.
        unsafe { (&mut *self.0.get()).truncate(len) };
    }

    #[inline(always)]
    fn get(&self, index: usize) -> Value {
        // SAFETY: No mutable borrow is active across calls.
        unsafe { (&*self.0.get())[index] }
    }

    /// Returns a shared view of the stacked values.
    ///
    /// # Safety
    /// No `push` or `truncate` may be called on `self` while the returned slice is alive.
    #[inline(always)]
    pub(crate) unsafe fn as_slice(&self) -> &[Value] {
        // SAFETY: Caller upholds that no exclusive borrow (`push`/`truncate`) overlaps the returned slice.
        unsafe { (&*self.0.get()).as_slice() }
    }
}

/// What JSON.parse keeps between the texts it parses: the property keys and the shapes of the objects of the texts it
/// parsed last, which later texts usually share, a weak cache of recently parsed string values, and the values of the
/// arrays and objects it has not finished making.
pub struct JsonParseCache {
    /// The elements of the arrays and the values of the properties of the objects that are being parsed, which the
    /// VM keeps alive until their array or object is made.
    values: JsonValueStack,
    /// 2-way set-associative weak cache of recently parsed unescaped string values, swept on GC.
    string_values: Box<[Cell<Option<Gc<PrimitiveString>>>; STRING_VALUE_CACHE_SLOTS]>,
    tables: Cell<Option<Box<JsonParseTables>>>,
}

impl Default for JsonParseCache {
    fn default() -> Self {
        Self {
            values: JsonValueStack::default(),
            string_values: Box::new([const { Cell::new(None) }; STRING_VALUE_CACHE_SLOTS]),
            tables: Cell::new(None),
        }
    }
}

impl JsonParseCache {
    pub(crate) fn remove_dead_strings(&self, sweep: &Sweep<'_>) {
        for slot in self.string_values.iter() {
            if slot.get().is_some_and(|string| sweep.is_dead_or_moving(string)) {
                slot.set(None);
            }
        }
    }
}

// SAFETY: The values being parsed are the only cells the cache keeps alive; its keys are strings and its shapes are
// weak.
unsafe impl Trace for JsonParseCache {
    fn trace(&self, visitor: &mut Visitor) {
        // SAFETY: GC never runs inside `JsonValueStack::push` or `truncate`, so no mutable borrow is active.
        unsafe { self.values.as_slice() }.trace(visitor);
    }
}

#[derive(Default)]
struct JsonParseTables {
    keys: KeyCache,
    shapes: ShapeCache,
    /// The keys of the properties of the objects that are being parsed.
    property_keys: Vec<NonSymbolPropertyKey>,
}

/// Parses `text` as a JSON text, and returns the value of its parse node as ParseJSON does, or throws a SyntaxError
/// if it is not a valid JSON text as specified in ECMA-404.
pub fn parse_json_text(vm: &Vm, realm: Gc<Realm>, text: Utf16View<'_>) -> ThrowCompletionOr<Value> {
    let cache = vm.json_parse_cache();
    let mut tables = cache.tables.take().unwrap_or_default();
    let first_value = cache.values.len();
    let first_key = tables.property_keys.len();
    let result = match text {
        Utf16View::Ascii(bytes) => Parser::new(vm, realm, &mut tables, bytes).parse_text(),
        Utf16View::Latin1(bytes) => {
            Parser::new(vm, realm, &mut tables, Latin1Byte::slice_from_bytes(bytes)).parse_text()
        }
        Utf16View::Utf16(code_units) => Parser::new(vm, realm, &mut tables, code_units).parse_text(),
    };
    // NB: A text that is not valid leaves the values and keys of what it did not finish.
    cache.values.truncate(first_value);
    tables.property_keys.truncate(first_key);
    cache.tables.set(Some(tables));
    result
}

fn json_malformed<T>(vm: &Vm) -> ThrowCompletionOr<T> {
    vm.throw_completion(ErrorKind::SyntaxError, ErrorType::JsonMalformed, &[])
}

/// Whether `units` are the code units of `view`.
#[inline(always)]
fn are_code_units_of_view<C: CodeUnit>(units: &[C], view: Utf16View<'_>) -> bool {
    match view {
        Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes) => C::equals_byte_slice(units, bytes),
        Utf16View::Utf16(code_units) => C::equals_u16_slice(units, code_units),
    }
}

/// Whether `units` are the code units of `key`.
#[inline(always)]
fn are_code_units_of<C: CodeUnit>(units: &[C], key: &PropertyKey) -> bool {
    are_code_units_of_view(units, Utf16View::of_fly_string(key.as_string()))
}

#[inline(always)]
fn string_value_cache_set<C: CodeUnit>(units: &[C]) -> usize {
    let len = units.len();
    let hash = (len as u32)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(units[0].into())
        .wrapping_mul(0x85EB_CA6B)
        .wrapping_add(units[len >> 2].into())
        .wrapping_mul(0xC2B2_AE35)
        .wrapping_add(units[len >> 1].into())
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(units[(len * 3) >> 2].into())
        .wrapping_mul(0x85EB_CA6B)
        .wrapping_add(units[len - 1].into());
    (hash ^ (hash >> 15)) as usize & (STRING_VALUE_CACHE_SETS - 1)
}

/// Property keys by their code units, in a small table indexed by a hash of them.
struct KeyCache {
    entries: Box<[Option<NonSymbolPropertyKey>; KeyCache::ENTRY_COUNT]>,
}

impl Default for KeyCache {
    fn default() -> Self {
        Self {
            entries: Box::new([const { None }; KeyCache::ENTRY_COUNT]),
        }
    }
}

impl KeyCache {
    const ENTRY_COUNT: usize = 256;

    fn slot<C: CodeUnit>(units: &[C]) -> usize {
        let (Some(first), Some(last)) = (units.first(), units.last()) else {
            return 0;
        };
        let middle: u32 = units[units.len() / 2].into();
        let hash = (units.len() as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add((*first).into())
            .wrapping_mul(0x85EB_CA6B)
            .wrapping_add(middle)
            .wrapping_mul(0xC2B2_AE35)
            .wrapping_add((*last).into());
        (hash ^ (hash >> 15)) as usize % Self::ENTRY_COUNT
    }

    fn key<C: CodeUnit>(&mut self, units: &[C]) -> NonSymbolPropertyKey {
        let entry = &mut self.entries[Self::slot(units)];
        if let Some(key) = entry
            && are_code_units_of(units, key)
        {
            return key.clone();
        }
        let key = NonSymbolPropertyKey::from(C::make_fly_string(units));
        // NB: Array indices are not kept, since they are numbers rather than strings.
        if key.is_string() {
            *entry = Some(key.clone());
        }
        key
    }
}

/// The shapes of objects by their keys, in a small table indexed by a hash of them. The shapes are weak, so that the
/// table never keeps a realm alive.
struct ShapeCache {
    entries: Box<[Option<ShapeCacheEntry>; ShapeCache::ENTRY_COUNT]>,
    pin_counts: [u8; ShapeCache::ENTRY_COUNT],
    last_slot: Option<usize>,
    next_generation: u64,
}

struct SimpleByteKeys {
    bytes: Box<[u8]>,
    u16_units: Box<[u16]>,
    ranges: Box<[(u32, u32)]>,
}

struct ShapeCacheEntry {
    keys: Box<[NonSymbolPropertyKey]>,
    shape: GcWeak<Shape>,
    generation: u64,
    /// True when none of `keys` contains a quote, backslash, or control character, so a raw slice match
    /// directly validates the JSON string token without running `scan_string`.
    all_keys_simple: bool,
    /// Flat ASCII bytes and `(start, end)` ranges of each key when `all_keys_simple` and every key has ASCII/Latin-1
    /// storage, avoiding per-key `Utf16View::of_fly_string` lookups during predicted-shape object parsing.
    simple_byte_keys: Option<SimpleByteKeys>,
}

fn build_simple_byte_keys(keys: &[NonSymbolPropertyKey]) -> Option<SimpleByteKeys> {
    let mut total_len = 0usize;
    for key in keys {
        match Utf16View::of_fly_string(key.as_string()) {
            Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes) => {
                if bytes.iter().any(|&b| is_json_special_code_unit(u32::from(b))) {
                    return None;
                }
                total_len = total_len.checked_add(bytes.len())?;
            }
            Utf16View::Utf16(_) => return None,
        }
    }
    if total_len > u32::MAX as usize {
        return None;
    }
    let mut flat = Vec::with_capacity(total_len);
    let mut flat_u16 = Vec::with_capacity(total_len);
    let mut ranges = Vec::with_capacity(keys.len());
    for key in keys {
        let (Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes)) = Utf16View::of_fly_string(key.as_string()) else {
            unreachable!("verified one-byte storage above");
        };
        let start = flat.len() as u32;
        flat.extend_from_slice(bytes);
        flat_u16.extend(bytes.iter().map(|&byte| u16::from(byte)));
        ranges.push((start, flat.len() as u32));
    }
    Some(SimpleByteKeys {
        bytes: flat.into_boxed_slice(),
        u16_units: flat_u16.into_boxed_slice(),
        ranges: ranges.into_boxed_slice(),
    })
}

fn is_simple_json_key(key: &PropertyKey) -> bool {
    match Utf16View::of_fly_string(key.as_string()) {
        Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes) => {
            bytes.iter().all(|&b| !is_json_special_code_unit(u32::from(b)))
        }
        Utf16View::Utf16(units) => units.iter().all(|&u| !is_json_special_code_unit(u32::from(u))),
    }
}

impl Default for ShapeCache {
    fn default() -> Self {
        Self {
            entries: Box::new([const { None }; ShapeCache::ENTRY_COUNT]),
            pin_counts: [0; ShapeCache::ENTRY_COUNT],
            last_slot: None,
            next_generation: 1,
        }
    }
}

impl ShapeCache {
    const ENTRY_COUNT: usize = 64;

    fn slot(keys: &[NonSymbolPropertyKey]) -> usize {
        let mut hasher = KeysHasher(keys.len() as u64);
        keys.hash(&mut hasher);
        hasher.finish() as usize % Self::ENTRY_COUNT
    }

    fn predicted_entry(&self, realm: Gc<Realm>) -> Option<(usize, u64, usize, bool)> {
        let slot = self.last_slot?;
        let entry = self.entries[slot].as_ref()?;
        let shape = entry.shape.get()?;
        (shape.realm() == realm && !entry.keys.is_empty()).then_some((
            slot,
            entry.generation,
            entry.keys.len(),
            entry.all_keys_simple,
        ))
    }

    /// The shape of an object of `realm` with the properties `keys` in that order, as data properties with the default
    /// attributes, made by put transitions from the shape of new objects. Returns None for keys that an object
    /// cannot have in that shape: a key that repeats, an array index, or more keys than a shape takes.
    fn shape_for_keys(&mut self, vm: &Vm, realm: Gc<Realm>, keys: &[NonSymbolPropertyKey]) -> Option<Gc<Shape>> {
        if keys.len() > MAX_TRANSITIONS_BEFORE_CONVERTING_TO_DICTIONARY as usize {
            return None;
        }
        let slot = Self::slot(keys);
        if let Some(entry) = &self.entries[slot]
            && let Some(shape) = entry.shape.get()
            && shape.realm() == realm
            && *entry.keys == *keys
        {
            self.last_slot = Some(slot);
            return Some(shape);
        }

        let mut shape = realm.new_object_shape();
        for key in keys {
            if key.is_number() || shape.lookup(key).is_some() {
                return None;
            }
            shape = shape.create_put_transition(vm, key, DEFAULT_ATTRIBUTES);
        }
        if self.pin_counts[slot] == 0 {
            let generation = self.next_generation;
            self.next_generation = self.next_generation.wrapping_add(1);
            let simple_byte_keys = build_simple_byte_keys(keys);
            let all_keys_simple = simple_byte_keys.is_some() || keys.iter().all(|key| is_simple_json_key(key));
            self.entries[slot] = Some(ShapeCacheEntry {
                keys: keys.into(),
                shape: GcWeak::new(vm.heap(), shape),
                generation,
                all_keys_simple,
                simple_byte_keys,
            });
            self.last_slot = Some(slot);
        }
        Some(shape)
    }
}

/// Mixes the bits of property keys, which are interned, for the slot of their shape.
struct KeysHasher(u64);

impl Hasher for KeysHasher {
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 29)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
}

struct Parser<'a, 'vm, C: CodeUnit> {
    vm: &'vm Vm,
    realm: Gc<Realm>,
    tables: &'a mut JsonParseTables,
    values: &'vm JsonValueStack,
    string_values: &'vm [Cell<Option<Gc<PrimitiveString>>>; STRING_VALUE_CACHE_SLOTS],
    units: &'a [C],
    position: usize,
}

/// Where a string of the text is, with what tells how to make its value.
struct StringToken {
    start: usize,
    end: usize,
    has_escapes: bool,
}

enum PredictedFlatOutcome {
    Complete,
    EarlyClose { matched: usize },
    FallbackAtKey { matched: usize },
    FallbackAtNestedValue { matched: usize },
}

#[inline(always)]
fn skip_whitespace_at<C: CodeUnit>(units: &[C], position: &mut usize) {
    if let Some(&unit) = units.get(*position)
        && unit.into() > 0x20
    {
        return;
    }
    skip_whitespace_slow(units, position);
}

#[cold]
#[inline(never)]
fn skip_whitespace_slow<C: CodeUnit>(units: &[C], position: &mut usize) {
    // NB: JSON whitespace is only tab, line feed, carriage return and space, so any unit above 0x20 ends the run.
    while let Some(&unit) = units.get(*position) {
        let code_unit: u32 = unit.into();
        if code_unit > 0x20 || !matches!(code_unit, 0x09 | 0x0A | 0x0D | 0x20) {
            break;
        }
        *position += 1;
    }
}

#[inline(always)]
fn consume_literal_at<C: CodeUnit>(vm: &Vm, units: &[C], position: &mut usize, literal: &[u8]) -> ThrowCompletionOr<()> {
    let Some(slice) = units.get(*position..*position + literal.len()) else {
        return json_malformed(vm);
    };
    if !C::equals_byte_slice(slice, literal) {
        return json_malformed(vm);
    }
    *position += literal.len();
    Ok(())
}

#[inline]
fn make_unescaped_primitive_string_from<C: CodeUnit>(
    vm: &Vm,
    string_values: &[Cell<Option<Gc<PrimitiveString>>>; STRING_VALUE_CACHE_SLOTS],
    units: &[C],
) -> Gc<PrimitiveString> {
    let len = units.len();
    if len == 0 {
        return vm.empty_string();
    }
    if len == 1 {
        let unit: u32 = units[0].into();
        if unit < 0x80 {
            return vm.single_ascii_character_string(unit as u8);
        }
    }
    if len <= STRING_VALUE_CACHE_MAX_LEN {
        let base = string_value_cache_set(units) * 2;
        let slot0 = &string_values[base];
        if let Some(cached) = slot0.get()
            && cached.length_in_utf16_code_units() == len
            && are_code_units_of_view(units, cached.utf16_string_view())
        {
            return cached;
        }
        let slot1 = &string_values[base + 1];
        if let Some(cached) = slot1.get()
            && cached.length_in_utf16_code_units() == len
            && are_code_units_of_view(units, cached.utf16_string_view())
        {
            slot1.set(slot0.get());
            slot0.set(Some(cached));
            return cached;
        }
        let created = C::make_primitive_string(vm, units);
        slot1.set(slot0.get());
        slot0.set(Some(created));
        return created;
    }
    C::make_primitive_string(vm, units)
}

#[inline]
fn scan_string_at<C: CodeUnit>(vm: &Vm, units: &[C], position: &mut usize) -> ThrowCompletionOr<StringToken> {
    let start = *position + 1;
    let mut pos = start;
    let mut has_escapes = false;
    loop {
        pos = C::skip_ordinary_string_units(units, pos);
        let Some(unit) = units.get(pos).map(|unit| (*unit).into()) else {
            return json_malformed(vm);
        };
        match unit {
            0x22 => break,
            0x5C => {
                has_escapes = true;
                pos += 2;
            }
            0x00..=0x1F => return json_malformed(vm),
            _ => unreachable!("skip_ordinary_string_units stops only on special JSON code units"),
        }
    }
    *position = pos + 1;
    Ok(StringToken {
        start,
        end: pos,
        has_escapes,
    })
}

fn make_string_from<C: CodeUnit>(vm: &Vm, units: &[C], token: &StringToken) -> ThrowCompletionOr<Utf16String> {
    let units = &units[token.start..token.end];
    if !token.has_escapes {
        return Ok(C::make_string(units));
    }

    let mut code_units: Vec<u16> = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit: u32 = units[index].into();
        index += 1;
        if unit != 0x5C {
            code_units.push(unit as u16);
            continue;
        }
        let Some(escaped) = units.get(index).map(|unit| (*unit).into()) else {
            return json_malformed(vm);
        };
        index += 1;
        let code_unit = match escaped {
            0x22 => 0x22,
            0x5C => 0x5C,
            0x2F => 0x2F,
            0x62 => 0x08,
            0x66 => 0x0C,
            0x6E => 0x0A,
            0x72 => 0x0D,
            0x74 => 0x09,
            0x75 => {
                let Some(hex_digits) = units.get(index..index + 4) else {
                    return json_malformed(vm);
                };
                index += 4;
                let mut code_unit: u32 = 0;
                for digit in hex_digits {
                    let Some(value) = char::from_u32((*digit).into()).and_then(|digit| digit.to_digit(16)) else {
                        return json_malformed(vm);
                    };
                    code_unit = (code_unit << 4) | value;
                }
                code_unit
            }
            _ => return json_malformed(vm),
        };
        code_units.push(code_unit as u16);
    }
    Ok(Utf16String::from_utf16(&code_units))
}

#[inline]
fn parse_number_at<C: CodeUnit>(vm: &Vm, units: &[C], position: &mut usize) -> ThrowCompletionOr<Value> {
    let start = *position;
    let len = units.len();
    let mut pos = start;
    if pos >= len {
        return json_malformed(vm);
    }

    let is_negative = units[pos].into() == 0x2D;
    if is_negative {
        pos += 1;
        if pos >= len {
            return json_malformed(vm);
        }
    }

    // Integer part: 0, or a digit other than 0 followed by digits.
    let integer_start = pos;
    let mut integer_value: u64 = 0;
    match units[pos].into() {
        0x30 => pos += 1,
        first @ 0x31..=0x39 => {
            integer_value = u64::from(first - 0x30);
            pos += 1;
            while pos < len {
                let digit = units[pos].into().wrapping_sub(0x30);
                if digit >= 10 {
                    break;
                }
                integer_value = integer_value.wrapping_mul(10).wrapping_add(u64::from(digit));
                pos += 1;
            }
        }
        _ => return json_malformed(vm),
    }
    let integer_digits = pos - integer_start;

    let mut is_integer = true;
    if pos < len && units[pos].into() == 0x2E {
        is_integer = false;
        pos += 1;
        if pos >= len || !(0x30..=0x39).contains(&units[pos].into()) {
            return json_malformed(vm);
        }
        pos += 1;
        while pos < len && (0x30..=0x39).contains(&units[pos].into()) {
            pos += 1;
        }
    }
    if pos < len && matches!(units[pos].into(), 0x45 | 0x65) {
        is_integer = false;
        pos += 1;
        if pos < len && matches!(units[pos].into(), 0x2B | 0x2D) {
            pos += 1;
        }
        if pos >= len || !(0x30..=0x39).contains(&units[pos].into()) {
            return json_malformed(vm);
        }
        pos += 1;
        while pos < len && (0x30..=0x39).contains(&units[pos].into()) {
            pos += 1;
        }
    }
    *position = pos;

    // OPTIMIZATION: Every integer up to 15 digits is below 2^53 and converts exactly to i32 or f64 with no
    //               decimal string float parsing.
    if is_integer && integer_digits <= 15 {
        if is_negative {
            if integer_value == 0 {
                return Ok(Value::from_f64(-0.0));
            }
            if integer_value <= (i32::MAX as u64) + 1 {
                return Ok(Value::from_i32(integer_value.wrapping_neg() as i32));
            }
            return Ok(Value::from_f64(-(integer_value as f64)));
        }
        if integer_value <= i32::MAX as u64 {
            return Ok(Value::from_i32(integer_value as i32));
        }
        return Ok(Value::from_f64(integer_value as f64));
    }

    // NB: The units of a number are ASCII digits and signs, so each one is a byte of its literal.
    let slice = &units[start..*position];
    let mut buffer = [0u8; 64];
    let mut long_literal = Vec::new();
    let literal = match buffer.get_mut(..slice.len()) {
        Some(bytes) => bytes,
        None => {
            long_literal.resize(slice.len(), 0);
            &mut long_literal[..]
        }
    };
    for (byte, unit) in literal.iter_mut().zip(slice) {
        *byte = (*unit).into() as u8;
    }
    let value: f64 = core::str::from_utf8(literal)
        .ok()
        .and_then(|literal| literal.parse().ok())
        .expect("a JSON number is a valid float literal");
    Ok(Value::from_f64(value))
}

/// Fast loop for flat objects whose keys match `simple_keys` in order and whose values are primitives.
/// Borrows `simple_keys` once outside the property loop, avoiding repeated shape-table indexing,
/// generation checks, and recursive `parse_value` dispatch.
fn try_parse_predicted_flat_object<C: CodeUnit>(
    vm: &Vm,
    units: &[C],
    position: &mut usize,
    values: &JsonValueStack,
    string_values: &[Cell<Option<Gc<PrimitiveString>>>; STRING_VALUE_CACHE_SLOTS],
    simple_keys: &SimpleByteKeys,
) -> ThrowCompletionOr<PredictedFlatOutcome> {
    let len = units.len();
    let key_count = simple_keys.ranges.len();
    let mut pos = *position;
    for index in 0..key_count {
        if pos >= len || units[pos].into() != 0x22 {
            *position = pos;
            return json_malformed(vm);
        }
        let (key_start, key_end) = simple_keys.ranges[index];
        let key_start = key_start as usize;
        let key_end = key_end as usize;
        let start = pos + 1;
        let end = start + (key_end - key_start);
        if end >= len || units[end].into() != 0x22 || !C::equals_simple_key(&units[start..end], simple_keys, key_start, key_end) {
            *position = pos;
            return Ok(PredictedFlatOutcome::FallbackAtKey { matched: index });
        }

        pos = end + 1;
        if pos < len && units[pos].into() == 0x3A {
            pos += 1;
        } else {
            skip_whitespace_at(units, &mut pos);
            if pos >= len || units[pos].into() != 0x3A {
                *position = pos;
                return json_malformed(vm);
            }
            pos += 1;
        }
        if pos < len && matches!(units[pos].into(), 0x20 | 0x09 | 0x0A | 0x0D) {
            skip_whitespace_at(units, &mut pos);
        }
        if pos >= len {
            *position = pos;
            return json_malformed(vm);
        }

        let value = match units[pos].into() {
            0x22 => {
                let token = scan_string_at(vm, units, &mut pos)?;
                if !token.has_escapes {
                    Value::from_string(make_unescaped_primitive_string_from(
                        vm,
                        string_values,
                        &units[token.start..token.end],
                    ))
                } else {
                    Value::from_string(PrimitiveString::create(vm, make_string_from(vm, units, &token)?))
                }
            }
            0x74 => {
                consume_literal_at(vm, units, &mut pos, b"true")?;
                Value::TRUE
            }
            0x66 => {
                consume_literal_at(vm, units, &mut pos, b"false")?;
                Value::FALSE
            }
            0x6E => {
                consume_literal_at(vm, units, &mut pos, b"null")?;
                Value::NULL
            }
            0x2D | 0x30..=0x39 => parse_number_at(vm, units, &mut pos)?,
            0x7B | 0x5B => {
                *position = pos;
                return Ok(PredictedFlatOutcome::FallbackAtNestedValue { matched: index });
            }
            _ => {
                *position = pos;
                return json_malformed(vm);
            }
        };
        values.push(value);

        if pos < len && matches!(units[pos].into(), 0x20 | 0x09 | 0x0A | 0x0D) {
            skip_whitespace_at(units, &mut pos);
        }
        if pos >= len {
            *position = pos;
            return json_malformed(vm);
        }
        match units[pos].into() {
            0x2C => {
                pos += 1;
                if pos < len && matches!(units[pos].into(), 0x20 | 0x09 | 0x0A | 0x0D) {
                    skip_whitespace_at(units, &mut pos);
                }
                if index + 1 == key_count {
                    *position = pos;
                    return Ok(PredictedFlatOutcome::FallbackAtKey { matched: key_count });
                }
            }
            0x7D => {
                *position = pos + 1;
                if index + 1 == key_count {
                    return Ok(PredictedFlatOutcome::Complete);
                }
                return Ok(PredictedFlatOutcome::EarlyClose { matched: index + 1 });
            }
            _ => {
                *position = pos;
                return json_malformed(vm);
            }
        }
    }
    unreachable!("key_count > 0 returns from the loop above")
}

impl<'a, 'vm, C: CodeUnit> Parser<'a, 'vm, C> {
    fn new(vm: &'vm Vm, realm: Gc<Realm>, tables: &'a mut JsonParseTables, units: &'a [C]) -> Self {
        let cache = vm.json_parse_cache();
        Self {
            vm,
            realm,
            tables,
            values: &cache.values,
            string_values: &cache.string_values,
            units,
            position: 0,
        }
    }

    #[inline]
    fn make_unescaped_primitive_string(&self, units: &[C]) -> Gc<PrimitiveString> {
        make_unescaped_primitive_string_from(self.vm, self.string_values, units)
    }

    #[inline(always)]
    fn peek(&self) -> Option<u32> {
        self.units.get(self.position).map(|unit| (*unit).into())
    }

    #[inline(always)]
    fn skip_whitespace(&mut self) {
        skip_whitespace_at(self.units, &mut self.position);
    }

    #[inline(always)]
    fn consume(&mut self, expected: u8) -> ThrowCompletionOr<()> {
        if self.peek() != Some(u32::from(expected)) {
            return json_malformed(self.vm);
        }
        self.position += 1;
        Ok(())
    }

    #[inline(always)]
    fn consume_literal(&mut self, literal: &[u8]) -> ThrowCompletionOr<()> {
        consume_literal_at(self.vm, self.units, &mut self.position, literal)
    }

    fn parse_text(mut self) -> ThrowCompletionOr<Value> {
        self.skip_whitespace();
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.position != self.units.len() {
            return json_malformed(self.vm);
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> ThrowCompletionOr<Value> {
        match self.peek() {
            Some(0x7B) => self.parse_object(),
            Some(0x5B) => self.parse_array(),
            Some(0x22) => {
                let token = self.scan_string()?;
                if !token.has_escapes {
                    return Ok(Value::from_string(
                        self.make_unescaped_primitive_string(&self.units[token.start..token.end]),
                    ));
                }
                Ok(Value::from_string(PrimitiveString::create(
                    self.vm,
                    self.make_string(&token)?,
                )))
            }
            Some(0x74) => {
                self.consume_literal(b"true")?;
                Ok(Value::TRUE)
            }
            Some(0x66) => {
                self.consume_literal(b"false")?;
                Ok(Value::FALSE)
            }
            Some(0x6E) => {
                self.consume_literal(b"null")?;
                Ok(Value::NULL)
            }
            Some(0x2D | 0x30..=0x39) => self.parse_number(),
            _ => json_malformed(self.vm),
        }
    }

    fn parse_object(&mut self) -> ThrowCompletionOr<Value> {
        // NB: Deeply nested texts would otherwise run out of stack.
        if self.vm.did_reach_stack_space_limit() {
            return self
                .vm
                .throw_completion(ErrorKind::InternalError, ErrorType::CallStackSizeExceeded, &[]);
        }
        self.position += 1;
        let first_key = self.tables.property_keys.len();
        let first_value = self.values.len();
        self.skip_whitespace();
        if self.peek() == Some(0x7D) {
            self.position += 1;
            return Ok(Value::from_object(Object::create(
                self.vm,
                self.realm,
                Some(self.realm.object_prototype()),
            )));
        }

        // Predict the most recently used object shape so homogeneous arrays of flat objects skip
        // KeyCache lookups, ShapeCache hashing, and PropertyKey refcount churn.
        let mut predicted = self.tables.shapes.predicted_entry(self.realm);
        let mut predicted_matched = 0usize;
        let mut finished_object = false;

        if let Some((slot, generation, key_count, true)) = predicted
            && let Some(entry) = &self.tables.shapes.entries[slot]
            && entry.generation == generation
            && let Some(ref simple_keys) = entry.simple_byte_keys
        {
            match try_parse_predicted_flat_object(
                self.vm,
                self.units,
                &mut self.position,
                self.values,
                self.string_values,
                simple_keys,
            )? {
                PredictedFlatOutcome::Complete => {
                    if let Some(entry) = &self.tables.shapes.entries[slot]
                        && entry.generation == generation
                        && let Some(shape) = entry.shape.get()
                        && shape.realm() == self.realm
                    {
                        self.tables.shapes.last_slot = Some(slot);
                        let object = Object::create_with_premade_shape_from_values(self.vm, shape, self.values, first_value);
                        self.values.truncate(first_value);
                        return Ok(Value::from_object(object));
                    }
                    predicted_matched = key_count;
                    finished_object = true;
                }
                PredictedFlatOutcome::EarlyClose { matched } => {
                    predicted_matched = matched;
                    finished_object = true;
                }
                PredictedFlatOutcome::FallbackAtKey { matched } => {
                    predicted_matched = matched;
                }
                PredictedFlatOutcome::FallbackAtNestedValue { matched } => {
                    predicted_matched = matched;
                    let pin = self.tables.shapes.pin_counts[slot];
                    if pin < u8::MAX {
                        self.tables.shapes.pin_counts[slot] = pin + 1;
                        let result = self.parse_value();
                        self.tables.shapes.pin_counts[slot] = pin;
                        let value = result?;
                        self.values.push(value);
                        predicted_matched += 1;
                    } else {
                        predicted = None;
                        let entry = self.tables.shapes.entries[slot]
                            .as_ref()
                            .expect("predicted shape entry must remain live");
                        debug_assert_eq!(entry.generation, generation);
                        self.tables
                            .property_keys
                            .extend(entry.keys[..=predicted_matched].iter().cloned());
                        let value = self.parse_value()?;
                        self.values.push(value);
                    }
                    self.skip_whitespace();
                    match self.peek() {
                        Some(0x2C) => {
                            self.position += 1;
                            self.skip_whitespace();
                        }
                        Some(0x7D) => {
                            self.position += 1;
                            finished_object = true;
                        }
                        _ => return json_malformed(self.vm),
                    }
                }
            }
        }

        while !finished_object {
            if self.peek() != Some(0x22) {
                return json_malformed(self.vm);
            }
            // Fast path: directly match the expected key of the predicted shape without scanning the string twice.
            let mut matched_predicted = false;
            let mut consumed_colon = false;
            if let Some((slot, generation, key_count, true)) = predicted
                && predicted_matched < key_count
                && let Some(entry) = &self.tables.shapes.entries[slot]
                && entry.generation == generation
            {
                let start = self.position + 1;
                let units_len = self.units.len();
                if let Some(ref simple_keys) = entry.simple_byte_keys {
                    let (key_start, key_end) = simple_keys.ranges[predicted_matched];
                    let key_start = key_start as usize;
                    let key_end = key_end as usize;
                    let end = start + (key_end - key_start);
                    if end < units_len
                        && self.units[end].into() == 0x22
                        && C::equals_simple_key(&self.units[start..end], simple_keys, key_start, key_end)
                    {
                        if end + 1 < units_len && self.units[end + 1].into() == 0x3A {
                            self.position = end + 2;
                            consumed_colon = true;
                        } else {
                            self.position = end + 1;
                        }
                        matched_predicted = true;
                    }
                } else {
                    let expected = &entry.keys[predicted_matched];
                    let key_len = Utf16View::of_fly_string(expected.as_string()).length_in_code_units();
                    let end = start + key_len;
                    if end < units_len
                        && self.units[end].into() == 0x22
                        && are_code_units_of(&self.units[start..end], expected)
                    {
                        self.position = end + 1;
                        matched_predicted = true;
                    }
                }
            }

            let mut scanned_token = None;
            if !matched_predicted {
                let token = self.scan_string()?;
                if let Some((slot, generation, key_count, _)) = predicted {
                    if !token.has_escapes
                        && predicted_matched < key_count
                        && let Some(entry) = &self.tables.shapes.entries[slot]
                        && entry.generation == generation
                        && are_code_units_of(&self.units[token.start..token.end], &entry.keys[predicted_matched])
                    {
                        matched_predicted = true;
                    } else {
                        if predicted_matched > 0 {
                            let entry = self.tables.shapes.entries[slot]
                                .as_ref()
                                .expect("predicted shape entry must remain live");
                            debug_assert_eq!(entry.generation, generation);
                            self.tables
                                .property_keys
                                .extend(entry.keys[..predicted_matched].iter().cloned());
                        }
                        predicted = None;
                    }
                }
                if !matched_predicted {
                    scanned_token = Some(token);
                }
            }

            if matched_predicted {
                if !consumed_colon {
                    self.skip_whitespace();
                    self.consume(b':')?;
                }
                self.skip_whitespace();
                let (slot, generation, _, _) = predicted.expect("predicted entry is active");
                // Pin the predicted slot across nested containers so its entry cannot be evicted mid-object.
                if matches!(self.peek(), Some(0x7B | 0x5B)) {
                    let pin = self.tables.shapes.pin_counts[slot];
                    if pin < u8::MAX {
                        self.tables.shapes.pin_counts[slot] = pin + 1;
                        let result = self.parse_value();
                        self.tables.shapes.pin_counts[slot] = pin;
                        let value = result?;
                        self.values.push(value);
                        predicted_matched += 1;
                    } else {
                        predicted = None;
                        let entry = self.tables.shapes.entries[slot]
                            .as_ref()
                            .expect("predicted shape entry must remain live");
                        debug_assert_eq!(entry.generation, generation);
                        self.tables
                            .property_keys
                            .extend(entry.keys[..=predicted_matched].iter().cloned());
                        let value = self.parse_value()?;
                        self.values.push(value);
                    }
                } else {
                    let value = self.parse_value()?;
                    self.values.push(value);
                    predicted_matched += 1;
                }
            } else {
                let token = scanned_token.expect("unmatched key was scanned");
                let key = if token.has_escapes {
                    NonSymbolPropertyKey::from(&self.make_string(&token)?)
                } else {
                    self.tables.keys.key(&self.units[token.start..token.end])
                };
                self.skip_whitespace();
                self.consume(b':')?;
                self.skip_whitespace();
                let value = self.parse_value()?;
                self.tables.property_keys.push(key);
                self.values.push(value);
            }

            self.skip_whitespace();
            match self.peek() {
                Some(0x2C) => {
                    self.position += 1;
                    self.skip_whitespace();
                }
                Some(0x7D) => {
                    self.position += 1;
                    break;
                }
                _ => return json_malformed(self.vm),
            }
        }

        if let Some((slot, generation, key_count, _)) = predicted {
            if predicted_matched == key_count
                && let Some(entry) = &self.tables.shapes.entries[slot]
                && entry.generation == generation
                && let Some(shape) = entry.shape.get()
                && shape.realm() == self.realm
            {
                self.tables.shapes.last_slot = Some(slot);
                let object = Object::create_with_premade_shape_from_values(self.vm, shape, self.values, first_value);
                self.values.truncate(first_value);
                return Ok(Value::from_object(object));
            }
            if predicted_matched > 0 {
                let entry = self.tables.shapes.entries[slot]
                    .as_ref()
                    .expect("predicted shape entry must remain live");
                self.tables
                    .property_keys
                    .extend(entry.keys[..predicted_matched].iter().cloned());
            }
        }

        let object = self.create_object(first_key, first_value);
        self.tables.property_keys.truncate(first_key);
        self.values.truncate(first_value);
        Ok(Value::from_object(object))
    }

    /// Creates the object of the properties parsed last, whose keys and values start at `first_key` and
    /// `first_value`.
    fn create_object(&mut self, first_key: usize, first_value: usize) -> Gc<Object> {
        let tables = &mut *self.tables;
        let keys = &tables.property_keys[first_key..];
        if let Some(shape) = tables.shapes.shape_for_keys(self.vm, self.realm, keys) {
            return Object::create_with_premade_shape_from_values(self.vm, shape, self.values, first_value);
        }

        // NB: Keys that repeat or are array indices, and too many keys, take the generic path.
        let object = Object::create(self.vm, self.realm, Some(self.realm.object_prototype()));
        for (index, key) in keys.iter().enumerate() {
            let value = self.values.get(first_value + index);
            object.define_direct_property(self.vm, key, value, DEFAULT_ATTRIBUTES);
        }
        object
    }

    fn parse_array(&mut self) -> ThrowCompletionOr<Value> {
        // NB: Deeply nested texts would otherwise run out of stack.
        if self.vm.did_reach_stack_space_limit() {
            return self
                .vm
                .throw_completion(ErrorKind::InternalError, ErrorType::CallStackSizeExceeded, &[]);
        }
        self.position += 1;
        let first_value = self.values.len();
        self.skip_whitespace();
        if self.peek() == Some(0x5D) {
            self.position += 1;
            return Ok(Value::from_object(Array::create_empty(self.vm, self.realm)));
        }
        loop {
            let value = self.parse_value()?;
            self.values.push(value);
            self.skip_whitespace();
            match self.peek() {
                Some(0x2C) => {
                    self.position += 1;
                    self.skip_whitespace();
                }
                Some(0x5D) => {
                    self.position += 1;
                    break;
                }
                _ => return json_malformed(self.vm),
            }
        }

        // NB: The elements become the packed indexed storage of the array in one step.
        let array = Array::create_empty(self.vm, self.realm);
        // SAFETY: `set_indexed_property_elements` does not mutate `self.values` or trigger JS callbacks.
        array.set_indexed_property_elements(unsafe { &self.values.as_slice()[first_value..] });
        self.values.truncate(first_value);
        Ok(Value::from_object(array))
    }

    /// Finds the end of the string whose opening quote is at the current position, and moves past its closing quote.
    fn scan_string(&mut self) -> ThrowCompletionOr<StringToken> {
        scan_string_at(self.vm, self.units, &mut self.position)
    }

    fn make_string(&self, token: &StringToken) -> ThrowCompletionOr<Utf16String> {
        make_string_from(self.vm, self.units, token)
    }

    fn parse_number(&mut self) -> ThrowCompletionOr<Value> {
        parse_number_at(self.vm, self.units, &mut self.position)
    }
}
