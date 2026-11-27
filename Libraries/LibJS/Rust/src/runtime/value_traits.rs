/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! Hashing values the way Map and Set tell their keys apart.

use core::hash::{BuildHasher, Hash, Hasher};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::layout::value::Value;
use crate::runtime::value::same_value;

/// Process-wide randomized seed for `Map` and `Set` index tables, avoiding a TLS lookup per hash.
static KEYED_COLLECTION_SEED: AtomicU64 = AtomicU64::new(0);

#[cold]
#[inline(never)]
fn init_keyed_collection_seed() -> u64 {
    let generated = foldhash::fast::RandomState::default().hash_one(0x9e37_79b9_7f4a_7c15u64) | 1;
    match KEYED_COLLECTION_SEED.compare_exchange(0, generated, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => generated,
        Err(existing) => existing,
    }
}

#[inline(always)]
fn keyed_collection_seed() -> u64 {
    let seed = KEYED_COLLECTION_SEED.load(Ordering::Relaxed);
    if seed != 0 {
        seed
    } else {
        init_keyed_collection_seed()
    }
}

#[inline(always)]
fn mix_u64(value: u64, seed: u64) -> u64 {
    let wide = u128::from(value ^ seed).wrapping_mul(0x9e37_79b9_7f4a_7c15u128);
    (wide as u64) ^ ((wide >> 64) as u64)
}

/// A value as a hash key: hashed by its contents and compared with SameValue.
#[derive(Clone, Copy)]
pub struct ValueTraitsKey(pub Value);

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
            state.write_u8(1);
            state.write_u32(value.as_string().cached_hash32());
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

/// Fast randomized hash for `Map` and `Set` index tables, reading cached AK string hashes in O(1)
/// and mixing with a process-wide atomic seed instead of looking up a thread-local `RandomState`.
#[inline]
pub fn fast_hash_value_key(mut value: Value) -> u64 {
    assert!(!value.is_empty());
    let seed = keyed_collection_seed();
    if value.is_int32() {
        return mix_u64(value.0, seed);
    }
    if !value.is_cell() {
        if value.is_nan() {
            value = Value::from_f64(f64::NAN);
        }
        return mix_u64(value.0, seed);
    }
    if value.is_string() {
        let string_hash32 = value.as_string().cached_hash32();
        return mix_u64((1u64 << 32) | u64::from(string_hash32), seed);
    }
    if value.is_bigint() {
        return foldhash::fast::FixedState::with_seed(seed).hash_one(value.as_bigint().big_integer());
    }
    // SAFETY: A cell value points to a live cell.
    let identity_hash = unsafe { crate::layout_forward::cell_identity_hash(value.as_cell().as_ptr().cast()) };
    mix_u64((2u64 << 32) | u64::from(identity_hash), seed)
}

/// ValueTraits::hash(), for tables that keep their values elsewhere and only hold the hashes.
pub fn value_traits_hash(value: Value) -> u64 {
    foldhash::fast::FixedState::default().hash_one(ValueTraitsKey(value))
}
