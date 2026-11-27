/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! Hashing values the way Map and Set tell their keys apart.

use core::hash::{BuildHasher, Hash, Hasher};

use crate::layout::value::Value;
use crate::runtime::value::same_value;
use crate::utf16::Utf16View;

/// A value as a hash key: hashed by its contents and compared with SameValue.
#[derive(Clone, Copy)]
pub struct ValueTraitsKey(pub Value);

#[inline]
fn hash_utf16_view<H: Hasher>(view: Utf16View<'_>, state: &mut H) {
    match view {
        Utf16View::Ascii(bytes) | Utf16View::Latin1(bytes) => {
            state.write_u8(0);
            state.write(bytes);
        }
        Utf16View::Utf16(units) => {
            let mut or_all = 0u16;
            for &u in units {
                or_all |= u;
            }
            if or_all <= 0xff {
                state.write_u8(0);
                if units.len() <= 64 {
                    let mut buf = [0u8; 64];
                    for (dst, &src) in buf.iter_mut().zip(units) {
                        *dst = src as u8;
                    }
                    state.write(&buf[..units.len()]);
                } else {
                    let narrowed: Vec<u8> = units.iter().map(|&u| u as u8).collect();
                    state.write(&narrowed);
                }
            } else {
                state.write_u8(1);
                // SAFETY: `u16` has no padding bytes and its byte slice is valid for reads.
                let bytes =
                    unsafe { ak::slice_from_raw_parts(units.as_ptr().cast::<u8>(), core::mem::size_of_val(units)) };
                state.write(bytes);
            }
        }
    }
}

impl Hash for ValueTraitsKey {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut value = self.0;
        assert!(!value.is_empty());
        if value.is_int32() {
            state.write_u64(value.0);
            return;
        }
        if !value.is_cell() {
            // In the IEEE 754 standard a NaN value is encoded as any value from 0x7ff0000000000001 to 0x7fffffffffffffff,
            // with the least significant bits (referred to as the 'payload') carrying some kind of diagnostic information
            // indicating the source of the NaN. Since ECMA262 does not differentiate between different kinds of NaN values,
            // Sets and Maps must not differentiate between them either.
            // This is achieved by replacing any NaN value by a canonical qNaN.
            if value.is_nan() {
                value = Value::from_f64(f64::NAN);
            }
            state.write_u64(value.0);
            return;
        }
        if value.is_string() {
            let string = value.as_string();
            if string.deferred_kind() == crate::layout::primitive_string::DeferredKind::Inline {
                state.write_u8(0);
                state.write(string.inline_characters());
                return;
            }
            hash_utf16_view(string.utf16_string_view(), state);
            return;
        }

        if value.is_bigint() {
            value.as_bigint().big_integer().hash(state);
            return;
        }

        // NB: A cell may move, so it is hashed by its identity, not its address.
        value.as_cell().hash(state);
    }
}

impl PartialEq for ValueTraitsKey {
    fn eq(&self, other: &Self) -> bool {
        same_value(self.0, other.0)
    }
}

impl Eq for ValueTraitsKey {}

/// ValueTraits::hash(), for tables that keep their values elsewhere and only hold the hashes.
pub fn value_traits_hash(value: Value) -> u64 {
    foldhash::fast::FixedState::default().hash_one(ValueTraitsKey(value))
}
