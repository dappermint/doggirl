/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The JSON text parser of JSON.parse without a reviver. It reads the code units of the text once and makes the values
//! of the parse nodes as it goes, since nothing but the result needs the parse nodes when there is no reviver to give
//! their source text to.

use core::cell::{Cell, RefCell};
use core::hash::{Hash, Hasher};

use ak::{Utf16FlyString, Utf16String};

use crate::gc::visitor::{Trace, Visitor};
use crate::gc::weak::GcWeak;
use crate::interpreter::vm::Vm;
use crate::layout::cell::Gc;
use crate::layout::object::Object;
use crate::layout::value::Value;
use crate::runtime::array::Array;
use crate::runtime::completion::{Must, ThrowCompletionOr};
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

/// Storage width discovered while scanning an unescaped JSON string.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StringWidth {
    Ascii,
    Latin1,
    Utf16,
}

/// The code units of a JSON text: ASCII bytes, Latin-1 bytes or UTF-16 code units.
trait CodeUnit: Copy + Eq + Into<u32> + 'static {
    fn make_string(units: &[Self]) -> Utf16String;
    fn make_primitive_string(vm: &Vm, units: &[Self], width: StringWidth) -> Gc<PrimitiveString>;
    fn make_fly_string(units: &[Self], width: StringWidth) -> Utf16FlyString;
    fn equals_byte_slice(units: &[Self], bytes: &[u8]) -> bool;
    fn equals_u16_slice(units: &[Self], code_units: &[u16]) -> bool;

    /// The position of the first unit at or after `position` that is a quotation mark, a reverse solidus or a
    /// control character, or the length of `units` if there is none. Widens `*width` if any ordinary unit scanned
    /// before that position is above U+007F or above U+00FF.
    fn skip_ordinary_string_units(units: &[Self], position: usize, width: &mut StringWidth) -> usize;
}

impl CodeUnit for u8 {
    fn make_string(units: &[u8]) -> Utf16String {
        Utf16String::from_ascii(units)
    }

    fn make_primitive_string(vm: &Vm, units: &[u8], _width: StringWidth) -> Gc<PrimitiveString> {
        PrimitiveString::create_from_ascii(vm, units)
    }

    fn make_fly_string(units: &[u8], _width: StringWidth) -> Utf16FlyString {
        Utf16FlyString::from_utf8(core::str::from_utf8(units).expect("the text is ASCII"))
    }

    #[inline]
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

    fn skip_ordinary_string_units(units: &[u8], mut position: usize, _width: &mut StringWidth) -> usize {
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

    fn make_primitive_string(vm: &Vm, units: &[Latin1Byte], width: StringWidth) -> Gc<PrimitiveString> {
        let bytes = Self::as_bytes(units);
        if width == StringWidth::Ascii {
            return PrimitiveString::create_from_ascii(vm, bytes);
        }
        PrimitiveString::create(vm, Utf16String::from_latin1(bytes))
    }

    fn make_fly_string(units: &[Latin1Byte], width: StringWidth) -> Utf16FlyString {
        let bytes = Self::as_bytes(units);
        if width == StringWidth::Ascii {
            return Utf16FlyString::from_utf8(core::str::from_utf8(bytes).expect("the slice is ASCII"));
        }
        Utf16View::Latin1(bytes).to_utf16_fly_string()
    }

    #[inline]
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

    fn skip_ordinary_string_units(units: &[Latin1Byte], mut position: usize, width: &mut StringWidth) -> usize {
        let bytes = Self::as_bytes(units);
        let mut or_bits = 0u64;
        while let Some(chunk) = bytes.get(position..position + 8) {
            let word = u64::from_le_bytes(chunk.try_into().expect("the chunk has eight units"));
            let special_lanes = json_special_code_unit_lanes(word, 0x0101_0101_0101_0101);
            if special_lanes != 0 {
                let offset = (special_lanes.trailing_zeros() / 8) as usize;
                if offset != 0 {
                    or_bits |= word & ((1u64 << (offset * 8)) - 1);
                }
                if or_bits & 0x8080_8080_8080_8080 != 0 {
                    *width = StringWidth::Latin1;
                }
                return position + offset;
            }
            or_bits |= word;
            position += 8;
        }
        while position < bytes.len() && !is_json_special_code_unit(u32::from(bytes[position])) {
            or_bits |= u64::from(bytes[position]);
            position += 1;
        }
        if or_bits & 0x8080_8080_8080_8080 != 0 {
            *width = StringWidth::Latin1;
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

    fn make_primitive_string(vm: &Vm, units: &[u16], width: StringWidth) -> Gc<PrimitiveString> {
        match width {
            StringWidth::Ascii => {
                // OPTIMIZATION: Short ASCII strings narrow into an uninitialized stack buffer so `create_from_ascii`
                //               can store them inline inside `InlineString`; longer ones narrow directly into heap
                //               storage in a single pass.
                const MAX_INLINE_ASCII: usize = crate::layout::primitive_string::INLINE_STRING_CAPACITY;
                if units.len() <= MAX_INLINE_ASCII {
                    let mut ascii = [0u8; MAX_INLINE_ASCII];
                    let mut index = 0usize;
                    #[cfg(target_arch = "aarch64")]
                    {
                        use core::arch::aarch64::*;
                        // SAFETY: NEON is always available on aarch64 and every load/store stays within `units` and `ascii`.
                        unsafe {
                            while index + 8 <= units.len() {
                                let wide = vld1q_u16(units.as_ptr().add(index));
                                vst1_u8(ascii.as_mut_ptr().add(index), vmovn_u16(wide));
                                index += 8;
                            }
                        }
                    }
                    while index < units.len() {
                        ascii[index] = units[index] as u8;
                        index += 1;
                    }
                    return PrimitiveString::create_from_ascii(vm, &ascii[..units.len()]);
                }
                // SAFETY: `skip_ordinary_string_units` verified every code unit in `units` is `<= 0x7f`.
                PrimitiveString::create(vm, unsafe { Utf16String::from_utf16_known_ascii(units) })
            }
            // SAFETY: `skip_ordinary_string_units` verified every code unit is `<= 0xff` and at least one is `>= 0x80`.
            StringWidth::Latin1 => PrimitiveString::create(vm, unsafe { Utf16String::from_utf16_known_latin1(units) }),
            // SAFETY: `skip_ordinary_string_units` verified at least one code unit is `> 0xff`.
            StringWidth::Utf16 => {
                PrimitiveString::create(vm, unsafe { Utf16String::from_utf16_known_non_latin1(units) })
            }
        }
    }

    fn make_fly_string(units: &[u16], width: StringWidth) -> Utf16FlyString {
        if width == StringWidth::Ascii && units.len() <= 64 {
            let mut ascii = [0u8; 64];
            for (byte, &unit) in ascii.iter_mut().zip(units) {
                *byte = unit as u8;
            }
            return Utf16FlyString::from_utf8(core::str::from_utf8(&ascii[..units.len()]).expect("the slice is ASCII"));
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
        if len >= 8 {
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
        units == code_units
    }

    fn skip_ordinary_string_units(units: &[u16], mut position: usize, width: &mut StringWidth) -> usize {
        #[inline(always)]
        fn record_bits(width: &mut StringWidth, bits: u16) {
            if bits > 0xff {
                *width = StringWidth::Utf16;
            } else if bits >= 0x80 && *width == StringWidth::Ascii {
                *width = StringWidth::Latin1;
            }
        }

        #[cfg(target_arch = "aarch64")]
        {
            use core::arch::aarch64::*;
            const TAIL_MASKS: [[u16; 8]; 8] = [
                [0, 0, 0, 0, 0, 0, 0, 0],
                [0xFFFF, 0, 0, 0, 0, 0, 0, 0],
                [0xFFFF, 0xFFFF, 0, 0, 0, 0, 0, 0],
                [0xFFFF, 0xFFFF, 0xFFFF, 0, 0, 0, 0, 0],
                [0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0, 0, 0],
                [0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0, 0],
                [0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0],
                [0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0],
            ];
            // SAFETY: NEON is always available on aarch64 and every load stays within `units`.
            unsafe {
                let quote = vdupq_n_u16(0x22);
                let backslash = vdupq_n_u16(0x5C);
                let space = vdupq_n_u16(0x20);
                let mut or_vec = vdupq_n_u16(0);
                while position + 8 <= units.len() {
                    let chunk = vld1q_u16(units.as_ptr().add(position));
                    let special = vorrq_u16(
                        vorrq_u16(vceqq_u16(chunk, quote), vceqq_u16(chunk, backslash)),
                        vcltq_u16(chunk, space),
                    );
                    let mask = vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(special, 4)), 0);
                    if mask != 0 {
                        let offset = (mask.trailing_zeros() / 8) as usize;
                        let tail_mask = vld1q_u16(TAIL_MASKS[offset].as_ptr());
                        let combined = vorrq_u16(or_vec, vandq_u16(chunk, tail_mask));
                        record_bits(width, vmaxvq_u16(combined));
                        return position + offset;
                    }
                    or_vec = vorrq_u16(or_vec, chunk);
                    position += 8;
                }
                let mut or_bits = vmaxvq_u16(or_vec);
                while position < units.len() && !is_json_special_code_unit(u32::from(units[position])) {
                    or_bits |= units[position];
                    position += 1;
                }
                record_bits(width, or_bits);
                position
            }
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let mut or_bits = 0u64;
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
                    let offset = (special_lanes.trailing_zeros() / 16) as usize;
                    if offset != 0 {
                        or_bits |= word & ((1u64 << (offset * 16)) - 1);
                    }
                    let folded = (or_bits | (or_bits >> 32)) as u32;
                    record_bits(width, (folded | (folded >> 16)) as u16);
                    return position + offset;
                }
                or_bits |= word;
                position += 4;
            }
            let folded = (or_bits | (or_bits >> 32)) as u32;
            let mut scalar_or = (folded | (folded >> 16)) as u16;
            while position < units.len() && !is_json_special_code_unit(u32::from(units[position])) {
                scalar_or |= units[position];
                position += 1;
            }
            record_bits(width, scalar_or);
            position
        }
    }
}

/// What JSON.parse keeps between the texts it parses: the property keys and the shapes of the objects of the texts it
/// parsed last, which later texts usually share, and the values of the arrays and objects it has not finished making.
#[derive(Default)]
pub struct JsonParseCache {
    /// The elements of the arrays and the values of the properties of the objects that are being parsed, which the
    /// VM keeps alive until their array or object is made.
    values: RefCell<Vec<Value>>,
    tables: Cell<Option<Box<JsonParseTables>>>,
}

// SAFETY: The values being parsed are the only cells the cache keeps alive; its keys are strings and its shapes are
// weak.
unsafe impl Trace for JsonParseCache {
    fn trace(&self, visitor: &mut Visitor) {
        self.values.borrow().trace(visitor);
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
    let first_value = cache.values.borrow().len();
    let first_key = tables.property_keys.len();
    let result = match text {
        Utf16View::Ascii(bytes) => Parser::new(vm, realm, &mut tables, bytes).parse_text(),
        Utf16View::Latin1(bytes) => {
            Parser::new(vm, realm, &mut tables, Latin1Byte::slice_from_bytes(bytes)).parse_text()
        }
        Utf16View::Utf16(code_units) => Parser::new(vm, realm, &mut tables, code_units).parse_text(),
    };
    // NB: A text that is not valid leaves the values and keys of what it did not finish.
    cache.values.borrow_mut().truncate(first_value);
    tables.property_keys.truncate(first_key);
    cache.tables.set(Some(tables));
    result
}

fn json_malformed<T>(vm: &Vm) -> ThrowCompletionOr<T> {
    vm.throw_completion(ErrorKind::SyntaxError, ErrorType::JsonMalformed, &[])
}

/// Whether `units` are the code units of `key`.
#[inline(always)]
fn are_code_units_of<C: CodeUnit>(units: &[C], key: &PropertyKey) -> bool {
    match Utf16View::of_fly_string(key.as_string()) {
        Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes) => C::equals_byte_slice(units, bytes),
        Utf16View::Utf16(code_units) => C::equals_u16_slice(units, code_units),
    }
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

    fn key<C: CodeUnit>(&mut self, units: &[C], width: StringWidth) -> NonSymbolPropertyKey {
        let entry = &mut self.entries[Self::slot(units)];
        if let Some(key) = entry
            && are_code_units_of(units, key)
        {
            return key.clone();
        }
        let key = NonSymbolPropertyKey::from(C::make_fly_string(units, width));
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
    let mut ranges = Vec::with_capacity(keys.len());
    for key in keys {
        let (Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes)) = Utf16View::of_fly_string(key.as_string()) else {
            unreachable!("verified one-byte storage above");
        };
        let start = flat.len() as u32;
        flat.extend_from_slice(bytes);
        ranges.push((start, flat.len() as u32));
    }
    Some(SimpleByteKeys {
        bytes: flat.into_boxed_slice(),
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
    values: &'vm RefCell<Vec<Value>>,
    units: &'a [C],
    position: usize,
}

/// Where a string of the text is, with what tells how to make its value.
struct StringToken {
    start: usize,
    end: usize,
    has_escapes: bool,
    width: StringWidth,
}

impl<'a, 'vm, C: CodeUnit> Parser<'a, 'vm, C> {
    fn new(vm: &'vm Vm, realm: Gc<Realm>, tables: &'a mut JsonParseTables, units: &'a [C]) -> Self {
        Self {
            vm,
            realm,
            tables,
            values: &vm.json_parse_cache().values,
            units,
            position: 0,
        }
    }

    #[inline(always)]
    fn peek(&self) -> Option<u32> {
        self.units.get(self.position).map(|unit| (*unit).into())
    }

    #[inline(always)]
    fn skip_whitespace(&mut self) {
        // NB: JSON whitespace is only tab, line feed, carriage return and space, so any unit above 0x20 ends the run.
        while let Some(&unit) = self.units.get(self.position) {
            let code_unit: u32 = unit.into();
            if code_unit > 0x20 || !matches!(code_unit, 0x09 | 0x0A | 0x0D | 0x20) {
                break;
            }
            self.position += 1;
        }
    }

    #[inline(always)]
    fn consume(&mut self, expected: u8) -> ThrowCompletionOr<()> {
        if self.peek() != Some(u32::from(expected)) {
            return json_malformed(self.vm);
        }
        self.position += 1;
        Ok(())
    }

    #[inline]
    fn consume_literal(&mut self, literal: &[u8]) -> ThrowCompletionOr<()> {
        let Some(slice) = self.units.get(self.position..self.position + literal.len()) else {
            return json_malformed(self.vm);
        };
        if !C::equals_byte_slice(slice, literal) {
            return json_malformed(self.vm);
        }
        self.position += literal.len();
        Ok(())
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
                    return Ok(Value::from_string(C::make_primitive_string(
                        self.vm,
                        &self.units[token.start..token.end],
                        token.width,
                    )));
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
        let first_value = self.values.borrow().len();
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

        loop {
            if self.peek() != Some(0x22) {
                return json_malformed(self.vm);
            }
            // Fast path: directly match the expected key of the predicted shape without scanning the string twice.
            let mut matched_predicted = false;
            if let Some((slot, generation, key_count, true)) = predicted
                && predicted_matched < key_count
                && let Some(entry) = &self.tables.shapes.entries[slot]
                && entry.generation == generation
            {
                let start = self.position + 1;
                if let Some(ref simple_keys) = entry.simple_byte_keys {
                    let (key_start, key_end) = simple_keys.ranges[predicted_matched];
                    let expected_bytes = &simple_keys.bytes[key_start as usize..key_end as usize];
                    let end = start + expected_bytes.len();
                    if let Some(slice) = self.units.get(start..end)
                        && self.units.get(end).map(|u| (*u).into()) == Some(0x22)
                        && C::equals_byte_slice(slice, expected_bytes)
                    {
                        self.position = end + 1;
                        matched_predicted = true;
                    }
                } else {
                    let expected = &entry.keys[predicted_matched];
                    let key_len = Utf16View::of_fly_string(expected.as_string()).length_in_code_units();
                    let end = start + key_len;
                    if let Some(slice) = self.units.get(start..end)
                        && self.units.get(end).map(|u| (*u).into()) == Some(0x22)
                        && are_code_units_of(slice, expected)
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
                self.skip_whitespace();
                self.consume(b':')?;
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
                        self.values.borrow_mut().push(value);
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
                        self.values.borrow_mut().push(value);
                    }
                } else {
                    let value = self.parse_value()?;
                    self.values.borrow_mut().push(value);
                    predicted_matched += 1;
                }
            } else {
                let token = scanned_token.expect("unmatched key was scanned");
                let key = if token.has_escapes {
                    NonSymbolPropertyKey::from(&self.make_string(&token)?)
                } else {
                    self.tables.keys.key(&self.units[token.start..token.end], token.width)
                };
                self.skip_whitespace();
                self.consume(b':')?;
                self.skip_whitespace();
                let value = self.parse_value()?;
                self.tables.property_keys.push(key);
                self.values.borrow_mut().push(value);
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
                self.values.borrow_mut().truncate(first_value);
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
        self.values.borrow_mut().truncate(first_value);
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
            let value = self.values.borrow()[first_value + index];
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
        let first_value = self.values.borrow().len();
        self.skip_whitespace();
        if self.peek() == Some(0x5D) {
            self.position += 1;
            return Ok(Value::from_object(Array::create(self.vm, self.realm, 0, None).must()));
        }
        loop {
            let value = self.parse_value()?;
            self.values.borrow_mut().push(value);
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
        let array = Array::create(self.vm, self.realm, 0, None).must();
        array.set_indexed_property_elements(&self.values.borrow()[first_value..]);
        self.values.borrow_mut().truncate(first_value);
        Ok(Value::from_object(array))
    }

    /// Finds the end of the string whose opening quote is at the current position, and moves past its closing quote.
    fn scan_string(&mut self) -> ThrowCompletionOr<StringToken> {
        let start = self.position + 1;
        let mut position = start;
        let mut has_escapes = false;
        let mut width = StringWidth::Ascii;
        loop {
            position = C::skip_ordinary_string_units(self.units, position, &mut width);
            let Some(unit) = self.units.get(position).map(|unit| (*unit).into()) else {
                return json_malformed(self.vm);
            };
            match unit {
                0x22 => break,
                0x5C => {
                    has_escapes = true;
                    position += 2;
                }
                0x00..=0x1F => return json_malformed(self.vm),
                _ => unreachable!("skip_ordinary_string_units stops only on special JSON code units"),
            }
        }
        self.position = position + 1;
        Ok(StringToken {
            start,
            end: position,
            has_escapes,
            width,
        })
    }

    fn make_string(&self, token: &StringToken) -> ThrowCompletionOr<Utf16String> {
        let units = &self.units[token.start..token.end];
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
                return json_malformed(self.vm);
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
                        return json_malformed(self.vm);
                    };
                    index += 4;
                    let mut code_unit: u32 = 0;
                    for digit in hex_digits {
                        let Some(value) = char::from_u32((*digit).into()).and_then(|digit| digit.to_digit(16)) else {
                            return json_malformed(self.vm);
                        };
                        code_unit = (code_unit << 4) | value;
                    }
                    code_unit
                }
                _ => return json_malformed(self.vm),
            };
            code_units.push(code_unit as u16);
        }
        Ok(Utf16String::from_utf16(&code_units))
    }

    fn parse_number(&mut self) -> ThrowCompletionOr<Value> {
        let start = self.position;
        let is_digit = |unit: Option<u32>| unit.is_some_and(|unit| (0x30..=0x39).contains(&unit));

        let is_negative = self.peek() == Some(0x2D);
        if is_negative {
            self.position += 1;
        }

        // Integer part: 0, or a digit other than 0 followed by digits.
        let integer_start = self.position;
        let mut integer_value: i32 = 0;
        match self.peek() {
            Some(0x30) => self.position += 1,
            Some(first @ 0x31..=0x39) => {
                integer_value = (first - 0x30) as i32;
                self.position += 1;
                while let Some(&unit) = self.units.get(self.position) {
                    let digit = unit.into().wrapping_sub(0x30);
                    if digit >= 10 {
                        break;
                    }
                    integer_value = integer_value.wrapping_mul(10).wrapping_add(digit as i32);
                    self.position += 1;
                }
            }
            _ => return json_malformed(self.vm),
        }
        let integer_digits = self.position - integer_start;

        let mut is_integer = true;
        if self.peek() == Some(0x2E) {
            is_integer = false;
            self.position += 1;
            if !is_digit(self.peek()) {
                return json_malformed(self.vm);
            }
            while is_digit(self.peek()) {
                self.position += 1;
            }
        }
        if matches!(self.peek(), Some(0x45 | 0x65)) {
            is_integer = false;
            self.position += 1;
            if matches!(self.peek(), Some(0x2B | 0x2D)) {
                self.position += 1;
            }
            if !is_digit(self.peek()) {
                return json_malformed(self.vm);
            }
            while is_digit(self.peek()) {
                self.position += 1;
            }
        }

        // OPTIMIZATION: An integer of up to nine digits is exactly its value, which needs no float parsing.
        if is_integer && integer_digits <= 9 {
            if is_negative {
                if integer_value == 0 {
                    return Ok(Value::from_f64(-0.0));
                }
                integer_value = -integer_value;
            }
            return Ok(Value::from_i32(integer_value));
        }

        // NB: The units of a number are ASCII digits and signs, so each one is a byte of its literal.
        let units = &self.units[start..self.position];
        let mut buffer = [0u8; 64];
        let mut long_literal = Vec::new();
        let literal = match buffer.get_mut(..units.len()) {
            Some(bytes) => bytes,
            None => {
                long_literal.resize(units.len(), 0);
                &mut long_literal[..]
            }
        };
        for (byte, unit) in literal.iter_mut().zip(units) {
            *byte = (*unit).into() as u8;
        }
        let value: f64 = core::str::from_utf8(literal)
            .ok()
            .and_then(|literal| literal.parse().ok())
            .expect("a JSON number is a valid float literal");
        Ok(Value::from_f64(value))
    }
}
