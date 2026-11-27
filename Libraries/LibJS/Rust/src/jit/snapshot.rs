/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! Snapshots of executables for compile jobs, captured on the main thread straight from the runtime's structures.
//! A snapshot refers to cells only by address; the cells it captured are kept alive by whoever owns the job.

use ak::Zone;
use core::num::NonZeroU64;
use libjs_abi::Builtin;
use std::sync::{Arc, OnceLock, PoisonError};

use libjs_jit::bytecode::{
    EnvironmentCoordinate, ExceptionHandler, FrameLayout, Instruction, OpCode, decode_instruction, opcode_and_length,
};
use libjs_jit::code::ExitKind;
use libjs_jit::snapshot::{
    AccessorFunctionSnapshot, BytecodeSlice, CallFeedbackSnapshot, CellId, ClosureTemplateSnapshot, CompileOptions,
    ConstructTarget, DerivedInstanceFields, DirectCallTarget, DirectGetterFields, DirectGetterValueKind,
    EnvironmentTemplateSnapshot,
    ExecutableSnapshot, FeedbackSnapshot, ForwardedCallSnapshot, Forwarding, FunctionFrameFields, GlobalCacheSnapshot,
    GlobalValueSnapshot, GlobalsSnapshot, InlinedFunctionSnapshot, InliningLimits, Intrinsic, KeyedFeedbackSnapshot,
    LexicalEnvironmentTemplateSnapshot, List, NativeCallTarget, ObjectShapeCacheSnapshot, OrdinaryHasInstanceSnapshot,
    PropertyCacheEntrySnapshot, PropertyCacheEntryType, PropertyCacheKind, PropertyCacheSnapshot, ShapeSnapshot,
    Snapshot, SnapshotWriter, StressOptions, SuperBaseSnapshot, SuperConstructTarget, TypedArraySnapshot,
};

use super::allocation::{
    SampleSlots, allocation_infos, closure_template_snapshots, closure_templates, function_environment_template,
    lexical_environment_templates,
};
use super::code::{CapturedBindingAccesses, InlinedFunction, SnapshotExecutable, SnapshotExecutables};
use super::runtime_info::RAW_NATIVE_FUNCTIONS_RETURN_IN_REGISTERS;
use crate::bytecode::executable::{Executable, PropertyLookupCacheEntryData, PropertyLookupCacheTier};
use crate::bytecode::executable_data::{ExecutableData, SHARED_CALL_FLAGS_MASK};
use crate::bytecode::feedback::{CallFeedback, CallFeedbackForwarding, call_feedback_flags};
use crate::bytecode::instruction::instruction_length_from_bytes;
use crate::gc::class::Extends;
use crate::gc::defer_gc::{DeferGc, Deferred};
use crate::gc::root::MarkedVec;
use crate::interpreter::vm::Vm;
use crate::layout::cell::{CellHeader, Gc};
use crate::layout::environment::DeclarativeEnvironment;
use crate::layout::execution_context::ScriptOrModuleSlot;
use crate::layout::function_object::{
    DirectGetterFunction, EcmascriptFunctionObject, FunctionObject, NativeFunction, RawNativeFunction,
};
use crate::layout::object::{Object, TYPED_ARRAY_CACHED_DATA_OFFSET_INVALID, TypedArrayBase};
use crate::layout::property_lookup_cache::{GlobalVariableCache, PropertyLookupCache, PropertyLookupCacheEntryType};
use crate::layout::value::Value;
use crate::layout_forward::RawNativeFunctionPointer;
use crate::runtime::array_constructor::ArrayConstructor;
use crate::runtime::array_prototype;
use crate::runtime::boolean_constructor::BooleanConstructor;
use crate::runtime::bound_function::BoundFunction;
use crate::runtime::class_field_definition::{ClassElementName, ClassFieldInitializer};
use crate::runtime::ecmascript_function_object::as_ecmascript_function_object;
use crate::runtime::function_prototype::{self, FunctionPrototype};
use crate::runtime::map_prototype;
use crate::runtime::math_object;
use crate::runtime::native_javascript_backed_function::NativeJavaScriptBackedFunction;
use crate::runtime::object_constructor::{self, ObjectConstructor};
use crate::runtime::object_prototype;
use crate::runtime::property_attributes::DEFAULT_ATTRIBUTES;
use crate::runtime::property_key::PropertyKey;
use crate::runtime::realm::Realm;
use crate::runtime::realm_watchpoints;
use crate::runtime::regexp_prototype;
use crate::runtime::set_prototype;
use crate::runtime::shape::{Shape, ShapeKey};
use crate::runtime::shared_function_instance_data::ClosureCount;
use crate::runtime::shared_function_instance_data::ConstructorKind;
use crate::runtime::shared_function_instance_data::FunctionKind;
use crate::runtime::shared_function_instance_data::ThisMode;
use crate::runtime::string_constructor::StringConstructor;
use crate::runtime::string_prototype;
use crate::runtime::typed_array_prototype;

/// Candidates with at most this many bytecode instructions are always inlined, without counting against the inlining
/// budget.
const ALWAYS_INLINED_INSTRUCTION_COUNT: u32 = 10;

/// Constructors of at most this many instructions are inlined into their constructs (see
/// `InliningLimits::max_construct_instructions`).
const MAX_INLINED_CONSTRUCT_INSTRUCTION_COUNT: u32 = 50;

/// A snapshot with the cells it refers to, which must stay alive (and in place) until the job's code is installed or
/// the job is abandoned.
pub struct CapturedSnapshot<'d> {
    pub snapshot: Box<Snapshot>,
    /// Strong handles to every `ExecutableData` whose bytecode is borrowed zero-copy by `snapshot`.
    pub bytecode_owners: Vec<Arc<ExecutableData>>,
    pub cells: Deferred<'d, Vec<Gc<CellHeader>>>,
    pub snapshot_executables: Deferred<'d, SnapshotExecutables>,
    /// The slots of the samples of the closure templates, whose samples are among the cells.
    pub sample_slots: Deferred<'d, SampleSlots>,
}

/// Reusable non-GC scratch vectors kept on `JitState` across snapshot captures on the VM thread.
#[derive(Default)]
pub(crate) struct SnapshotScratch {
    environment_templates: Vec<EnvironmentTemplateSnapshot>,
    bound_arguments: Vec<u64>,
    super_constructors: Vec<(u32, SuperConstructTarget)>,
    literal_instance_fields: Vec<(u32, LiteralFieldValues)>,
    derived_instance_fields: Vec<(u32, DerivedInstanceFields)>,
    property_cache_ranges: Vec<(u32, u32)>,
    shared_call_flags: Vec<u8>,
    shared_call_value: Vec<u16>,
    shared_max_cache_sizes: Vec<u8>,
}

impl SnapshotScratch {
    pub(crate) fn clear(&mut self) {
        self.environment_templates.clear();
        self.bound_arguments.clear();
        self.super_constructors.clear();
        self.literal_instance_fields.clear();
        self.derived_instance_fields.clear();
        self.property_cache_ranges.clear();
        self.shared_call_flags.clear();
        self.shared_call_value.clear();
        self.shared_max_cache_sizes.clear();
    }

    #[inline]
    fn shared_max_cache_size(&self, cache_index: usize) -> u8 {
        self.shared_max_cache_sizes.get(cache_index).copied().unwrap_or(0)
    }
}

/// An executable of the snapshot.
#[derive(Clone, Copy)]
struct Entry {
    executable: Gc<Executable>,
    /// None for the executable being compiled.
    function: Option<InlinedFunction>,
    realm: Gc<Realm>,
    depth: u32,
}

struct SnapshotBuilder<'vm> {
    vm: &'vm Vm,
    defer_gc: &'vm DeferGc<'vm>,
    /// The snapshot that is being captured.
    snapshot: SnapshotWriter<'vm>,
    /// Strong handles to every `ExecutableData` whose bytecode is borrowed zero-copy by `snapshot`.
    bytecode_owners: Vec<Arc<ExecutableData>>,
    cells: Vec<Gc<CellHeader>>,
    entries: Vec<Entry>,
    indices: Vec<(InlinedFunction, u32)>,
    /// Flat buffer accumulating speculated property lookup cache sizes across every entry in `entries`.
    property_cache_sizes: Vec<u8>,
    /// Reusable non-GC scratch vectors owned by `JitState`.
    scratch: &'vm mut SnapshotScratch,
    /// The realm of the compiled executable and its inlined callees, along with its pre-noted prototype/shape cells.
    compiled_realm: Gc<Realm>,
    realm_new_object_shape: ShapeSnapshot,
    realm_string_prototype: CellId,
    realm_number_prototype: CellId,
    realm_boolean_prototype: CellId,
    realm_primitive_prototype_shapes: [Gc<Shape>; 3],
    /// The function the compiled executable runs, or `None` for the top-level code of a script or of a module.
    compiled_function: Option<Gc<EcmascriptFunctionObject>>,
    /// The words the compiled executable's frames start with (see `FunctionFrameFields`).
    compiled_function_fields: FunctionFrameFields,
    /// The closure templates of the compiled executable.
    closure_templates: List<Option<ClosureTemplateSnapshot>>,
    /// The samples of the closure templates of inlined functions, by executable and template index, until the
    /// inlining candidates they are get their indices.
    closure_samples: Vec<(u32, usize, Gc<EcmascriptFunctionObject>)>,
    sample_slots: SampleSlots,
    /// The lexical environment templates of the compiled executable.
    lexical_environment_templates: List<Option<LexicalEnvironmentTemplateSnapshot>>,
    /// For the constructors among the inlining candidates, by index: the shape of the `this` value their inlined
    /// constructs create, whose entries their megamorphic caches describe.
    construct_this_shapes: Vec<(u32, Gc<Shape>)>,
    /// Whether this snapshot merges non-GC feedback, megamorphic hints, and exit sites from `ExecutableData`.
    use_shared_learning: bool,
}

#[inline]
fn cell_slice_as_values<T: Copy>(slice: &[core::cell::Cell<T>]) -> &[T] {
    // SAFETY: `Cell<T>` is `#[repr(transparent)]` over `T`, `T: Copy`, and no mutation occurs while this slice is live.
    unsafe { ak::slice_from_raw_parts(slice.as_ptr().cast::<T>(), slice.len()) }
}

#[inline]
fn vec_map_get<K: PartialEq, V: Copy>(entries: &[(K, V)], key: &K) -> Option<V> {
    for (entry_key, value) in entries {
        if entry_key == key {
            return Some(*value);
        }
    }
    None
}

#[inline]
fn vec_map_insert<K: PartialEq, V>(entries: &mut Vec<(K, V)>, key: K, value: V) {
    for (entry_key, entry_value) in &mut *entries {
        if *entry_key == key {
            *entry_value = value;
            return;
        }
    }
    entries.push((key, value));
}

/// The property offset of a literal field, its value, and the shape its definition gives the object if it adds the
/// property, if asked for (see `SnapshotBuilder::literal_field_definitions()`).
type FieldDefinition = (u32, u64, Option<CellId>);

/// The values of literal fields, by property offset (see `ExecutableSnapshot::literal_instance_fields`).
type LiteralFieldValues = List<(u32, u64)>;

/// The number of bytecode instructions of an executable, counted up to one past `limit`.
fn instruction_count(executable: &Executable, limit: u32) -> u32 {
    let code = executable.code();
    let (cached_count, is_exact) = code.cached_instruction_count();
    if is_exact || cached_count > limit {
        return cached_count;
    }
    let bytecode = executable.bytecode();
    let mut count = 0;
    let mut offset = 0;
    while offset < bytecode.len() && count <= limit {
        let Ok(length) = instruction_length_from_bytes(bytecode[offset], bytecode, offset) else {
            break;
        };
        offset += length;
        count += 1;
    }
    let exact = offset >= bytecode.len();
    if exact || count > cached_count {
        code.update_cached_instruction_count(count, exact);
    }
    count
}

pub(crate) fn ecmascript_function(cell: Option<Gc<CellHeader>>) -> Option<Gc<EcmascriptFunctionObject>> {
    as_ecmascript_function_object(cell?.downcast::<Object>()?)
}

/// The target of a call site if it is a builtin written in JavaScript that JIT code can inline: a normal function
/// without a function environment of the caller's realm, which already ran.
fn builtin_inlining_candidate_typed(
    builtin: Gc<NativeJavaScriptBackedFunction>,
    caller_realm: Gc<Realm>,
) -> Option<Gc<NativeJavaScriptBackedFunction>> {
    let shared_data = builtin.shared_data();
    (shared_data.kind() == FunctionKind::Normal
        && !builtin.function_environment_needed()
        && shared_data.executable().is_some()
        && builtin.realm() == caller_realm)
        .then_some(builtin)
}

fn builtin_inlining_candidate(
    target: Option<Gc<CellHeader>>,
    caller_realm: Gc<Realm>,
) -> Option<Gc<NativeJavaScriptBackedFunction>> {
    builtin_inlining_candidate_typed(target?.downcast::<NativeJavaScriptBackedFunction>()?, caller_realm)
}

/// The intrinsic the compiler knows a raw native function as, if it is one.
fn intrinsic_of_raw_native(vm: &Vm, function: Gc<RawNativeFunction>) -> Option<Intrinsic> {
    match function.builtin() {
        Some(Builtin::StringFromCharCode) => return Some(Intrinsic::StringFromCharCode),
        Some(Builtin::RegExpPrototypeExec) => return Some(Intrinsic::RegExpPrototypeExec),
        _ => {}
    }
    let address = function.native_function(vm)? as usize;
    lookup_native_intrinsic(address)
}

/// The intrinsic the compiler knows a non-raw-native, non-ECMAScript object as, if it is one.
fn intrinsic_of_constructor(cell: Gc<CellHeader>) -> Option<Intrinsic> {
    if cell.downcast::<StringConstructor>().is_some() {
        return Some(Intrinsic::StringConstructor);
    }
    if cell.downcast::<ArrayConstructor>().is_some() {
        return Some(Intrinsic::ArrayConstructor);
    }
    if cell.downcast::<ObjectConstructor>().is_some() {
        return Some(Intrinsic::ObjectConstructor);
    }
    if cell.downcast::<BooleanConstructor>().is_some() {
        return Some(Intrinsic::BooleanConstructor);
    }
    None
}

/// The intrinsic the compiler knows an object as, if it is one.
fn intrinsic_of(vm: &Vm, cell: Option<Gc<CellHeader>>) -> Option<Intrinsic> {
    let cell = cell?;
    if let Some(function) = cell.downcast::<RawNativeFunction>() {
        return intrinsic_of_raw_native(vm, function);
    }
    if cell.downcast::<EcmascriptFunctionObject>().is_some() {
        return None;
    }
    intrinsic_of_constructor(cell)
}

/// The intrinsics the compiler knows by their native functions (of any realm).
static NATIVE_INTRINSICS: [(&RawNativeFunctionPointer, Intrinsic); 31] = [
    (&function_prototype::APPLY_FUNCTION, Intrinsic::FunctionPrototypeApply),
    (&function_prototype::CALL_FUNCTION, Intrinsic::FunctionPrototypeCall),
    (&array_prototype::PUSH_FUNCTION, Intrinsic::ArrayPrototypePush),
    (&array_prototype::SLICE_FUNCTION, Intrinsic::ArrayPrototypeSlice),
    (&array_prototype::SPLICE_FUNCTION, Intrinsic::ArrayPrototypeSplice),
    (
        &object_prototype::HAS_OWN_PROPERTY_FUNCTION,
        Intrinsic::ObjectPrototypeHasOwnProperty,
    ),
    (&object_constructor::IS_FUNCTION, Intrinsic::ObjectIs),
    (&object_constructor::KEYS_FUNCTION, Intrinsic::ObjectKeys),
    (&object_constructor::ASSIGN_FUNCTION, Intrinsic::ObjectAssign),
    (&regexp_prototype::TEST_FUNCTION, Intrinsic::RegExpPrototypeTest),
    (
        &typed_array_prototype::LENGTH_GETTER,
        Intrinsic::TypedArrayPrototypeLength,
    ),
    (&math_object::MAX_FUNCTION, Intrinsic::MathMax),
    (&math_object::MIN_FUNCTION, Intrinsic::MathMin),
    (&math_object::IMUL_FUNCTION, Intrinsic::MathImul),
    (&string_prototype::INDEX_OF_FUNCTION, Intrinsic::StringPrototypeIndexOf),
    (
        &string_prototype::TO_LOWERCASE_FUNCTION,
        Intrinsic::StringPrototypeToLowerCase,
    ),
    (
        &string_prototype::TO_UPPERCASE_FUNCTION,
        Intrinsic::StringPrototypeToUpperCase,
    ),
    (
        &string_prototype::LAST_INDEX_OF_FUNCTION,
        Intrinsic::StringPrototypeLastIndexOf,
    ),
    (&string_prototype::INCLUDES_FUNCTION, Intrinsic::StringPrototypeIncludes),
    (
        &string_prototype::STARTS_WITH_FUNCTION,
        Intrinsic::StringPrototypeStartsWith,
    ),
    (
        &string_prototype::ENDS_WITH_FUNCTION,
        Intrinsic::StringPrototypeEndsWith,
    ),
    (&string_prototype::SLICE_FUNCTION, Intrinsic::StringPrototypeSlice),
    (
        &string_prototype::SUBSTRING_FUNCTION,
        Intrinsic::StringPrototypeSubstring,
    ),
    (&string_prototype::TRIM_FUNCTION, Intrinsic::StringPrototypeTrim),
    (
        &string_prototype::TRIM_START_FUNCTION,
        Intrinsic::StringPrototypeTrimStart,
    ),
    (&string_prototype::TRIM_END_FUNCTION, Intrinsic::StringPrototypeTrimEnd),
    (&map_prototype::GET_FUNCTION, Intrinsic::MapPrototypeGet),
    (&map_prototype::HAS_FUNCTION, Intrinsic::MapPrototypeHas),
    (&map_prototype::SET_FUNCTION, Intrinsic::MapPrototypeSet),
    (&set_prototype::HAS_FUNCTION, Intrinsic::SetPrototypeHas),
    (&set_prototype::ADD_FUNCTION, Intrinsic::SetPrototypeAdd),
];

const NATIVE_INTRINSIC_TABLE_SIZE: usize = 64;
const _: () = assert!(NATIVE_INTRINSICS.len() < NATIVE_INTRINSIC_TABLE_SIZE);

#[inline(always)]
fn native_intrinsic_slot(address: usize) -> usize {
    ((address as u64 >> 2).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize
}

#[inline]
fn lookup_native_intrinsic(address: usize) -> Option<Intrinsic> {
    static TABLE: OnceLock<[(usize, Option<Intrinsic>); NATIVE_INTRINSIC_TABLE_SIZE]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut entries = [(0usize, None); NATIVE_INTRINSIC_TABLE_SIZE];
        for &(native_function, intrinsic) in &NATIVE_INTRINSICS {
            let Some(pointer) = *native_function else {
                continue;
            };
            let key = pointer as usize;
            let mut slot = native_intrinsic_slot(key);
            while entries[slot].0 != 0 {
                slot = (slot + 1) & (NATIVE_INTRINSIC_TABLE_SIZE - 1);
            }
            entries[slot] = (key, Some(intrinsic));
        }
        entries
    });
    let mut slot = native_intrinsic_slot(address);
    loop {
        let (key, intrinsic) = table[slot];
        if key == address {
            return intrinsic;
        }
        if key == 0 {
            return None;
        }
        slot = (slot + 1) & (NATIVE_INTRINSIC_TABLE_SIZE - 1);
    }
}

/// The builtin `%ArrayIteratorPrototype%.next` of the realm, if `%ArrayIteratorPrototype%` holds it.
fn array_iterator_next(vm: &Vm, realm: Gc<Realm>) -> Option<Gc<FunctionObject>> {
    let next = realm
        .array_iterator_prototype()
        .get_without_side_effects(vm, &vm.names.next);
    if !next.is_function() {
        return None;
    }
    let next = next.as_function();
    (next.as_native_function().is_some() && next.is_array_prototype_next_builtin()).then_some(next)
}

/// Whether the own property an entry reads or writes is writable in the shape of the object, which makes it a data
/// property for as long as the object keeps the shape (see `Object::storage_set()`). Loads of the properties of
/// prototypes always check, as they are guarded by no check of the object they load from.
fn property_holds_data(entry: &PropertyLookupCacheEntryData) -> bool {
    if !matches!(
        entry.entry_type,
        PropertyLookupCacheEntryType::GetOwnProperty | PropertyLookupCacheEntryType::ChangeOwnProperty
    ) {
        return false;
    }
    let Some(ShapeKey { shape, .. }) = entry.shape else {
        return false;
    };
    shape
        .property_metadata_at_offset(entry.property_offset)
        .is_some_and(|metadata| metadata.attributes.is_writable())
}

/// The value the prototype of a GetPropertyInPrototypeChain or ChangePropertyInPrototypeChain entry holds in the
/// property, if the entry still applies.
fn prototype_property_value(entry: &PropertyLookupCacheEntryData) -> Option<Value> {
    if !matches!(
        entry.entry_type,
        PropertyLookupCacheEntryType::GetPropertyInPrototypeChain
            | PropertyLookupCacheEntryType::ChangePropertyInPrototypeChain
    ) {
        return None;
    }
    let holder = entry.prototype?;
    if !entry.prototype_chain_validity?.is_valid() {
        return None;
    }
    if holder.shape().is_dictionary() || entry.property_offset >= holder.shape().property_count() {
        return None;
    }
    Some(holder.get_direct(entry.property_offset))
}

/// The function of the accessor an entry calls: for a GetPropertyInPrototypeChain
/// entry, the getter of the accessor its prototype holds in the property, for a ChangePropertyInPrototypeChain entry
/// its setter, and for a GetOwnProperty entry the getter the interpreter last called.
fn entry_accessor_function_with_proto_value(
    entry: &PropertyLookupCacheEntryData,
    proto_value: Option<Value>,
) -> Option<Gc<FunctionObject>> {
    let (accessor, setter) = match entry.entry_type {
        PropertyLookupCacheEntryType::GetOwnProperty if entry.key == 0 => return entry.getter,
        entry_type => {
            let value = proto_value?;
            if !value.is_accessor() {
                return None;
            }
            (
                value.as_accessor(),
                entry_type == PropertyLookupCacheEntryType::ChangePropertyInPrototypeChain,
            )
        }
    };
    if setter { accessor.setter() } else { accessor.getter() }
}

/// Whether the property of an entry held an accessor: the accessor the interpreter last called the getter of for a
/// GetOwnProperty entry, or the value its prototype holds in the property for entries of a prototype.
fn entry_holds_accessor_with_proto_value(entry: &PropertyLookupCacheEntryData, proto_value: Option<Value>) -> bool {
    match entry.entry_type {
        PropertyLookupCacheEntryType::GetOwnProperty if entry.key == 0 => entry.getter.is_some(),
        _ => proto_value.is_some_and(|value| value.is_accessor()),
    }
}

/// The words of an ECMAScript function its frames start with, which stay the same for its lifetime.
fn function_frame_fields(function: Gc<EcmascriptFunctionObject>) -> FunctionFrameFields {
    let shared_data = function.shared_data();
    FunctionFrameFields {
        script_or_module: shared_data.script_or_module.word(),
        environment: function
            .environment()
            .map_or(0, |environment| environment.as_ptr() as u64),
    }
}

/// Adds `cell` to `cells`, the cells a snapshot refers to, and returns what the snapshot knows it as.
fn note_cell<T: Extends<CellHeader>>(cells: &mut Vec<Gc<CellHeader>>, cell: Gc<T>) -> CellId {
    cells.push(cell.upcast::<CellHeader>());
    CellId(cell.as_ptr() as u64)
}

/// Adds the environment of `function`, whose address its FunctionFrameFields hold, to `cells`.
fn note_frame_field_cells(cells: &mut Vec<Gc<CellHeader>>, function: Gc<EcmascriptFunctionObject>) {
    if let Some(environment) = function.environment() {
        note_cell(cells, environment);
    }
}

impl SnapshotBuilder<'_> {
    fn cell<T: Extends<CellHeader>>(&mut self, cell: Gc<T>) -> CellId {
        note_cell(&mut self.cells, cell)
    }

    /// The bits of `value`, whose cell, if it has one, is added to the cells the snapshot refers to.
    fn value_bits(&mut self, value: Value) -> u64 {
        if value.is_cell() {
            note_cell(&mut self.cells, value.as_cell());
        }
        value.0
    }

    /// function_frame_fields() of `function`, whose environment is added to the cells the snapshot refers to.
    fn frame_fields(&mut self, function: Gc<EcmascriptFunctionObject>) -> FunctionFrameFields {
        note_frame_field_cells(&mut self.cells, function);
        function_frame_fields(function)
    }

    fn optional_cell<T: Extends<CellHeader>>(&mut self, cell: Option<Gc<T>>) -> Option<CellId> {
        cell.map(|cell| self.cell(cell))
    }

    fn shape(&mut self, shape: Gc<Shape>) -> ShapeSnapshot {
        ShapeSnapshot {
            shape: self.cell(shape),
            property_count: shape.property_count(),
        }
    }

    fn property_cache_entry(
        &mut self,
        entry: PropertyLookupCacheEntryData,
        caller: &Entry,
        primitive_prototype_shapes: &[Gc<Shape>; 3],
        sees_primitives: &mut bool,
    ) -> Option<PropertyCacheEntrySnapshot> {
        let shape = entry.shape.map(|key| key.shape);
        if shape.is_some_and(|shape| primitive_prototype_shapes.contains(&shape)) {
            *sees_primitives = true;
            return None;
        }
        let entry_type = match entry.entry_type {
            // NB: Only the VM's keyed property lookup cache has entries for missing own properties.
            PropertyLookupCacheEntryType::Empty | PropertyLookupCacheEntryType::MissingOwnProperty => return None,
            PropertyLookupCacheEntryType::AddOwnProperty => PropertyCacheEntryType::AddOwnProperty,
            PropertyLookupCacheEntryType::ChangeOwnProperty => PropertyCacheEntryType::ChangeOwnProperty,
            PropertyLookupCacheEntryType::GetOwnProperty => PropertyCacheEntryType::GetOwnProperty,
            PropertyLookupCacheEntryType::ChangePropertyInPrototypeChain => {
                PropertyCacheEntryType::ChangePropertyInPrototypeChain
            }
            PropertyLookupCacheEntryType::GetPropertyInPrototypeChain => {
                PropertyCacheEntryType::GetPropertyInPrototypeChain
            }
            // NB: Like the interpreter, compiled code relies on an entry for a missing property only where the
            //     property is also missing from an unchanged prototype chain, which an entry without a prototype
            //     chain validity can only tell for shapes without a prototype.
            PropertyLookupCacheEntryType::GetMissingProperty
                if entry.prototype_chain_validity.is_none()
                    && shape.is_some_and(|shape| shape.prototype().is_some()) =>
            {
                return None;
            }
            PropertyLookupCacheEntryType::GetMissingProperty => PropertyCacheEntryType::GetMissingProperty,
        };
        let proto_value = prototype_property_value(&entry);
        let prototype_property = if entry.entry_type == PropertyLookupCacheEntryType::GetPropertyInPrototypeChain {
            proto_value
                .filter(|value| value.is_object())
                .map(|value| value.as_object())
        } else {
            None
        };
        let cached_value_offset = proto_value
            .filter(|value| value.is_accessor() && value.as_accessor().cache_on_receiver())
            .and_then(|value| value.as_accessor().cached_value_key())
            .and_then(|key| shape?.lookup(&PropertyKey::from_symbol(key)))
            .map(|metadata| metadata.offset);
        let accessor_function = entry_accessor_function_with_proto_value(&entry, proto_value).map(|function| {
            let cell = self.cell(function);
            let (intrinsic, inline_executable, native) =
                if let Some(raw_native) = function.downcast::<RawNativeFunction>() {
                    (
                        intrinsic_of_raw_native(self.vm, raw_native),
                        None,
                        self.native_accessor_target_raw(raw_native, cell, entry.getter == Some(function)),
                    )
                } else if let Some(ecmascript) = as_ecmascript_function_object(function.upcast::<Object>()) {
                    (
                        None,
                        self.inlining_candidate_ecmascript(ecmascript, caller.realm)
                            .and_then(|f| self.inlined_function_index(f, caller)),
                        None,
                    )
                } else if let Some(builtin) = function.downcast::<NativeJavaScriptBackedFunction>() {
                    (
                        None,
                        builtin_inlining_candidate_typed(builtin, caller.realm)
                            .and_then(|b| self.inlined_builtin_index(b, caller)),
                        None,
                    )
                } else {
                    (intrinsic_of_constructor(function.upcast::<CellHeader>()), None, None)
                };
            AccessorFunctionSnapshot {
                cached_value_offset,
                function: cell,
                intrinsic,
                inline_executable,
                native,
            }
        });
        let (prototype_property_intrinsic, prototype_property_inline_executable, prototype_property_direct_call) =
            match prototype_property {
                Some(object) => {
                    if let Some(raw_native) = object.downcast::<RawNativeFunction>() {
                        (intrinsic_of_raw_native(self.vm, raw_native), None, None)
                    } else if let Some(ecmascript) = as_ecmascript_function_object(object) {
                        (
                            None,
                            self.inlining_candidate_ecmascript(ecmascript, caller.realm)
                                .and_then(|f| self.inlined_function_index(f, caller)),
                            self.prototype_method_direct_call_ecmascript(ecmascript),
                        )
                    } else if let Some(builtin) = object.downcast::<NativeJavaScriptBackedFunction>() {
                        (
                            None,
                            builtin_inlining_candidate_typed(builtin, caller.realm)
                                .and_then(|b| self.inlined_builtin_index(b, caller)),
                            None,
                        )
                    } else {
                        (intrinsic_of_constructor(object.upcast::<CellHeader>()), None, None)
                    }
                }
                None => (None, None, None),
            };
        Some(PropertyCacheEntrySnapshot {
            entry_type,
            property_offset: entry.property_offset,
            shape_dictionary_generation: entry.shape.map_or(0, |key| key.dictionary_generation),
            shape_is_dictionary: shape.is_some_and(|shape| shape.is_dictionary()),
            shape_is_stable: shape.is_some_and(|shape| shape.is_stable()),
            writes_data_property: entry.writes_data_property,
            new_shape: self.optional_cell(entry.new_shape),
            shape: self.optional_cell(shape),
            prototype: self.optional_cell(entry.prototype),
            prototype_chain_validity: self.optional_cell(entry.prototype_chain_validity),
            prototype_chain_valid: entry
                .prototype_chain_validity
                .is_some_and(|validity| validity.is_valid()),
            key: (entry.key != 0).then(|| self.cell(Value::from_bits(entry.key).as_cell())),
            key_value: entry.key,
            prototype_property: self.optional_cell(prototype_property),
            prototype_property_intrinsic,
            prototype_property_inline_executable,
            prototype_property_direct_call,
            // NB: Watching a slot allocates, which collects no garbage while the snapshot is taken. Slots
            //     replaced while watched are checked instead, as invalidated code learns nothing, while the
            //     check learns from its exit.
            prototype_property_watchpoint: prototype_property
                .and(entry.prototype)
                .filter(|holder| !holder.shape().was_slot_replaced(entry.property_offset))
                .and_then(|holder| {
                    holder
                        .shape()
                        .watch_slot_for_replacements(self.vm, entry.property_offset)
                })
                .map(|set| self.cell(set)),
            accessor_function,
            property_holds_data: property_holds_data(&entry),
            holds_accessor: entry_holds_accessor_with_proto_value(&entry, proto_value),
        })
    }

    /// What property lookup cache `cache` of `caller` knows, along with its speculated cache size (matching
    /// `speculated_cache_size(cache)`). Of a megamorphic cache, that is the entry for `known_shape` if it has one:
    /// JIT code only uses megamorphic entries for objects whose shape it knows.
    fn property_cache(
        &mut self,
        cache: &PropertyLookupCache,
        caller: &Entry,
        known_shape: Option<Gc<Shape>>,
        primitive_prototype_shapes: &[Gc<Shape>; 3],
        shared_max_size: u8,
    ) -> (PropertyCacheSnapshot, u8) {
        if shared_max_size == u8::MAX && cache.tier() != PropertyLookupCacheTier::Megamorphic {
            return (
                PropertyCacheSnapshot {
                    kind: PropertyCacheKind::Megamorphic,
                    entries: List::empty(),
                },
                u8::MAX,
            );
        }
        if shared_max_size > 1 && cache.tier() != PropertyLookupCacheTier::Megamorphic {
            let local_size = super::code::speculated_cache_size(cache);
            if local_size > 0 && local_size < shared_max_size {
                return (
                    PropertyCacheSnapshot {
                        kind: PropertyCacheKind::Empty,
                        entries: List::empty(),
                    },
                    0,
                );
            }
        }
        let kind = match cache.tier() {
            PropertyLookupCacheTier::Empty => {
                return (
                    PropertyCacheSnapshot {
                        kind: PropertyCacheKind::Empty,
                        entries: List::empty(),
                    },
                    0,
                );
            }
            PropertyLookupCacheTier::Monomorphic => PropertyCacheKind::Monomorphic,
            PropertyLookupCacheTier::Polymorphic => PropertyCacheKind::Polymorphic,
            PropertyLookupCacheTier::Megamorphic => PropertyCacheKind::Megamorphic,
        };
        // NB: The interpreter caches the properties strings, numbers and booleans get from their prototypes as
        //     properties of those prototypes. JIT code speculating on such entries would check for an object and exit
        //     for every primitive, so caches with them are left to the fast paths, which look these up themselves.
        let is_megamorphic = kind == PropertyCacheKind::Megamorphic;
        let known_shape_slot = if is_megamorphic {
            known_shape.and_then(|shape| cache.megamorphic_entry_for_shape(shape, 0))
        } else {
            None
        };
        let slots = match known_shape_slot {
            Some(slot) => core::slice::from_ref(slot),
            None => cache.entries(),
        };
        if slots.len() == 1 {
            let slot = &slots[0];
            if slot.entry_type.get() == PropertyLookupCacheEntryType::Empty {
                return (
                    PropertyCacheSnapshot {
                        kind,
                        entries: List::empty(),
                    },
                    if is_megamorphic { u8::MAX } else { 0 },
                );
            }
            let mut sees_primitives = false;
            let entries =
                match self.property_cache_entry(slot.get(), caller, primitive_prototype_shapes, &mut sees_primitives) {
                    Some(row) if !sees_primitives => self.snapshot.add(core::slice::from_ref(&row)),
                    _ => List::empty(),
                };
            return (
                PropertyCacheSnapshot { kind, entries },
                if is_megamorphic { u8::MAX } else { 1 },
            );
        }
        let mut sees_primitives = false;
        let mut non_empty = 0usize;
        let entries = self.snapshot.begin();
        for slot in slots {
            if slot.entry_type.get() == PropertyLookupCacheEntryType::Empty {
                continue;
            }
            non_empty += 1;
            if let Some(row) =
                self.property_cache_entry(slot.get(), caller, primitive_prototype_shapes, &mut sees_primitives)
            {
                self.snapshot.push(row);
            }
        }
        if sees_primitives {
            self.snapshot.drop_rows(&entries);
        }
        let cache_size = if is_megamorphic {
            u8::MAX
        } else {
            u8::try_from(non_empty).expect("caches that are not megamorphic have few entries")
        };
        (
            PropertyCacheSnapshot {
                kind,
                entries: self.snapshot.end(entries),
            },
            cache_size,
        )
    }

    /// What the global variable caches of `executable`, whose frames run in the realm of `caller`, know about the global
    /// variables of the realm, as far as it still holds.
    fn globals(&mut self, executable: Gc<Executable>, caller: &Entry) -> GlobalsSnapshot {
        let realm = caller.realm;
        let object = realm.global_object();
        let environment = realm.global_declarative_environment();
        let environment_serial = environment.environment_serial_number();
        let shape = object.shape();
        let caches = self.snapshot.begin();
        for cache in executable.global_variable_caches() {
            let row = self.global_cache(cache, object, environment, environment_serial, shape, caller);
            self.snapshot.push(row);
        }
        let caches = self.snapshot.end(caches);
        GlobalsSnapshot {
            object: self.cell(object),
            declarative_environment: self.cell(environment),
            environment_serial,
            caches,
        }
    }

    /// What a global variable cache found, like `GetGlobal` and `SetGlobal` use it, if it still applies.
    fn global_cache(
        &mut self,
        cache: &GlobalVariableCache,
        object: Gc<Object>,
        environment: Gc<DeclarativeEnvironment>,
        environment_serial: u64,
        shape: Gc<Shape>,
        caller: &Entry,
    ) -> Option<GlobalCacheSnapshot> {
        let realm = caller.realm;
        if cache.environment_serial_number.get() != environment_serial {
            return None;
        }
        if let Some(entry) = cache.first_entry()
            && entry.shape == Some(ShapeKey::of(shape))
        {
            let value = object.get_direct(entry.property_offset);
            if value.is_accessor() {
                return None;
            }
            return Some(GlobalCacheSnapshot::Property {
                shape: self.cell(shape),
                dictionary_generation: shape.is_dictionary().then(|| shape.dictionary_generation()),
                offset: entry.property_offset,
                writes_data_property: entry.entry_type == PropertyLookupCacheEntryType::ChangeOwnProperty
                    && entry.writes_data_property,
                assigned: super::dependencies::global_object_slot_may_have_been_assigned(
                    self.vm,
                    &object,
                    entry.property_offset,
                ),
                value: self.global_value(value, realm, Some(caller)),
            });
        }
        if !cache.has_environment_binding_index.get() {
            return None;
        }
        let index = cache.environment_binding_index.get();
        let value = environment.get_initialized_binding_value_direct(index as usize);
        Some(GlobalCacheSnapshot::Binding {
            index,
            mutable: environment.binding_is_mutable(index as usize),
            assigned: environment.binding_may_have_been_assigned(index as usize),
            value: (!value.is_empty()).then(|| self.global_value(value, realm, Some(caller))),
        })
    }

    /// The value of a global variable. Functions are inlining candidates of the call sites of `caller`, if given,
    /// since the code may call them after passing them on, which no call feedback of `caller` records.
    fn global_value(&mut self, value: Value, realm: Gc<Realm>, caller: Option<&Entry>) -> GlobalValueSnapshot {
        let cell = value.is_cell().then(|| value.as_cell());
        GlobalValueSnapshot {
            bits: value.0,
            inline_executable: caller.and_then(|caller| self.inline_executable_index(cell, caller)),
            cell: cell.map(|cell| self.cell(cell)),
            intrinsic: intrinsic_of(self.vm, cell),
            has_instance: self.ordinary_has_instance(value, realm),
            typed_array: value
                .is_object()
                .then(|| value.as_object().downcast::<TypedArrayBase>())
                .flatten()
                .and_then(|array| {
                    use crate::runtime::typed_array::Kind;
                    use libjs_jit::ir::TypedArrayElement;
                    let element = match array.kind() {
                        Kind::Uint8Array => TypedArrayElement::Uint8,
                        Kind::Uint8ClampedArray => TypedArrayElement::Uint8Clamped,
                        Kind::Int8Array => TypedArrayElement::Int8,
                        Kind::Uint16Array => TypedArrayElement::Uint16,
                        Kind::Int16Array => TypedArrayElement::Int16,
                        Kind::Uint32Array => TypedArrayElement::Uint32,
                        Kind::Int32Array => TypedArrayElement::Int32,
                        Kind::Float32Array => TypedArrayElement::Float32,
                        Kind::Float64Array => TypedArrayElement::Float64,
                        _ => return None,
                    };
                    Some(TypedArraySnapshot {
                        element,
                        fixed_length: (array.cached_data_offset() != TYPED_ARRAY_CACHED_DATA_OFFSET_INVALID)
                            .then(|| array.array_length.length.get())
                            .and_then(|length| u32::try_from(length).ok()),
                    })
                }),
        }
    }

    /// How `instanceof` with `value` on its right-hand side runs, if `value` is a function that inherits the
    /// `@@hasInstance` of `%Function.prototype%` of `realm`, which is neither writable nor configurable: its shape, which
    /// tells that while the function keeps it, and where its `prototype` property is.
    fn ordinary_has_instance(&mut self, value: Value, realm: Gc<Realm>) -> Option<OrdinaryHasInstanceSnapshot> {
        if !value.is_function() || value.as_function().downcast::<BoundFunction>().is_some() {
            return None;
        }
        let function = value.as_object();
        let shape = function.shape();
        let has_instance = PropertyKey::from(self.vm.well_known_symbols().has_instance);
        if !function.has_ordinary_get_own_property_for(self.vm, &has_instance)
            || shape.prototype() != Some(realm.function_prototype())
            || shape.lookup(&has_instance).is_some()
        {
            return None;
        }
        // NB: ECMAScript functions create their prototype property lazily, but one in their shape is the one they have.
        let prototype = shape.lookup(&self.vm.names.prototype)?;
        if function.get_direct(prototype.offset).is_accessor() {
            return None;
        }
        Some(OrdinaryHasInstanceSnapshot {
            shape: self.cell(shape),
            dictionary_generation: shape.is_dictionary().then(|| shape.dictionary_generation()),
            prototype_offset: prototype.offset,
        })
    }

    fn inlined_function(&mut self, function: Gc<EcmascriptFunctionObject>) -> InlinedFunctionSnapshot {
        let realm = function.realm().expect("an ECMAScript function has a realm");
        let executable = function
            .bytecode_executable()
            .expect("an inlined function has an executable");
        InlinedFunctionSnapshot {
            function: self.cell(function),
            formal_parameter_count: function.formal_parameter_count(),
            strict: executable.is_strict_mode(),
            // NB: Arrow functions resolve `this` through their environments.
            uses_this: function.uses_this() && function.this_mode() != ThisMode::Lexical,
            global_this: self.cell(realm.global_environment().global_this_value()),
            realm: self.cell(realm),
            shared_data: self.cell(function.shared_data()),
            super_base: self.super_base(function),
            // NB: Arrow functions resolve new.target through their environments.
            has_own_new_target: function.this_mode() != ThisMode::Lexical,
        }
    }

    /// The base of `super` property access in `function`, if its home object gets its prototype from its shape.
    fn super_base(&mut self, function: Gc<EcmascriptFunctionObject>) -> Option<SuperBaseSnapshot> {
        let home = function.home_object()?;
        let home_shape = home.shape();
        if home_shape.is_dictionary() || !home.has_ordinary_get_prototype_of() {
            return None;
        }
        Some(SuperBaseSnapshot {
            home: self.cell(home),
            home_shape: self.cell(home_shape),
            prototype: home_shape.prototype().map(|prototype| self.cell(prototype)),
        })
    }

    fn inlined_builtin(&mut self, function: Gc<NativeJavaScriptBackedFunction>) -> InlinedFunctionSnapshot {
        let realm = function.realm();
        let shared_data = function.shared_data();
        InlinedFunctionSnapshot {
            function: self.cell(function),
            formal_parameter_count: shared_data.formal_parameter_count(),
            strict: shared_data.strict(),
            uses_this: true,
            global_this: self.cell(realm.global_environment().global_this_value()),
            realm: self.cell(realm),
            shared_data: self.cell(shared_data),
            super_base: None,
            has_own_new_target: true,
        }
    }

    /// Whether an ECMAScript function can be inlined, per the inlining contract in the JIT design.
    fn inlining_candidate_ecmascript(
        &self,
        function: Gc<EcmascriptFunctionObject>,
        caller_realm: Gc<Realm>,
    ) -> Option<Gc<EcmascriptFunctionObject>> {
        // NB: can_inline_call() rules out generators, async functions and class constructors.
        if !function.can_inline_call()
            || function.needs_environment_or_this_value_resolution()
            || function.contains_direct_call_to_eval()
            || function.realm() != Some(caller_realm)
        {
            return None;
        }
        let limit = self.vm.jit.options.inline_max_size;
        (instruction_count(&function.inline_call_executable(), limit) <= limit).then_some(function)
    }

    /// Whether the target of a call site can be inlined, per the inlining contract in the JIT design.
    fn inlining_candidate(
        &self,
        target: Option<Gc<CellHeader>>,
        caller_realm: Gc<Realm>,
    ) -> Option<Gc<EcmascriptFunctionObject>> {
        let function = ecmascript_function(target)?;
        self.inlining_candidate_ecmascript(function, caller_realm)
    }

    fn inline_executable_index(&mut self, target: Option<Gc<CellHeader>>, caller: &Entry) -> Option<u32> {
        if let Some(builtin) = builtin_inlining_candidate(target, caller.realm) {
            return self.inlined_builtin_index(builtin, caller);
        }
        let function = self.inlining_candidate(target, caller.realm)?;
        self.inlined_function_index(function, caller)
    }

    /// Adds a builtin written in JavaScript as an inlining candidate. Builtins are always inlined, outside the
    /// inlining budget: their calls are what the code would otherwise spend the most on.
    fn inlined_builtin_index(&mut self, builtin: Gc<NativeJavaScriptBackedFunction>, caller: &Entry) -> Option<u32> {
        let function = InlinedFunction::Builtin(builtin);
        if let Some(index) = vec_map_get(&self.indices, &function) {
            return Some(index);
        }
        if caller.depth + 1 > self.vm.jit.options.inline_depth {
            return None;
        }
        let executable = builtin.shared_data().executable()?;
        let index = u32::try_from(self.entries.len()).expect("the executable count fits in u32");
        self.entries.push(Entry {
            executable,
            function: Some(function),
            realm: builtin.realm(),
            depth: caller.depth + 1,
        });
        self.indices.push((function, index));
        Some(index)
    }

    /// Adds a function as an inlining candidate of the call sites of `caller`. The snapshot only bounds the candidates
    /// it captures by their size and depth; the graph builder decides which it inlines, within the budget of
    /// `InliningLimits`.
    fn inlined_function_index(&mut self, function: Gc<EcmascriptFunctionObject>, caller: &Entry) -> Option<u32> {
        let key = InlinedFunction::Ecmascript(function);
        if let Some(index) = vec_map_get(&self.indices, &key) {
            return Some(index);
        }
        let executable = function.bytecode_executable()?;
        if caller.depth + 1 > self.vm.jit.options.inline_depth {
            return None;
        }
        let index = u32::try_from(self.entries.len()).expect("the executable count fits in u32");
        self.entries.push(Entry {
            executable,
            function: Some(key),
            realm: function.realm().expect("an ECMAScript function has a realm"),
            depth: caller.depth + 1,
        });
        self.indices.push((key, index));
        Some(index)
    }

    /// Describes how the single callee of a call site forwarded its calls, if the snapshot can describe it.
    fn forwarded_call(&mut self, call: &CallFeedback, caller: &Entry) -> Option<ForwardedCallSnapshot> {
        let target = call.forwarded_target()?;
        let forwarding = match call.forwarding.get() {
            forwarding if forwarding == CallFeedbackForwarding::Apply as u8 => Forwarding::Apply,
            forwarding if forwarding == CallFeedbackForwarding::Call as u8 => Forwarding::Call,
            forwarding if forwarding == CallFeedbackForwarding::Bound as u8 => Forwarding::Bound,
            forwarding if forwarding == CallFeedbackForwarding::Callback as u8 => Forwarding::Callback,
            _ => return None,
        };
        let bound = match forwarding {
            Forwarding::Bound => Some(call.target()?.downcast::<BoundFunction>()?),
            _ => None,
        };
        let mut snapshot = ForwardedCallSnapshot {
            forwarding,
            target: self.cell(target),
            target_intrinsic: intrinsic_of(self.vm, Some(target)),
            argument_count: u32::from(call.forwarded_argument_count.get()),
            inline_executable: match bound {
                Some(bound) if bound.bound_arguments().len() > ForwardedCallSnapshot::MAX_INLINED_BOUND_ARGUMENTS => {
                    None
                }
                _ => self.inline_executable_index(Some(target), caller),
            },
            bound_this: 0,
            bound_arguments_start: 0,
            bound_argument_count: 0,
        };
        // NB: Only calls of inlining candidates use the bound values.
        if let Some(bound) = bound
            && snapshot.inline_executable.is_some()
        {
            // NB: The bound function, which the code checks the callee against and so embeds, keeps its bound values
            //     alive. The snapshot notes their cells, which keeps them in place until the code is installed.
            let bound_arguments = bound.bound_arguments();
            snapshot.bound_this = self.value_bits(bound.bound_this());
            snapshot.bound_arguments_start = u32::try_from(self.scratch.bound_arguments.len()).ok()?;
            snapshot.bound_argument_count = u32::try_from(bound_arguments.len()).ok()?;
            for &value in bound_arguments {
                let bits = self.value_bits(value);
                self.scratch.bound_arguments.push(bits);
            }
        }
        Some(snapshot)
    }

    /// Describes the single constructor of a construct site if JIT code can inline its construct: a base constructor
    /// without fields (whose construct only creates `this` before running its body), or a derived constructor whose
    /// chain of [[Prototype]]s up to such a base constructor are derived constructors, with a function object of a
    /// shape that has its "prototype" as a data property holding an object.
    fn construct_target_ecmascript(
        &mut self,
        function: Gc<EcmascriptFunctionObject>,
        caller: &Entry,
    ) -> Option<ConstructTarget> {
        // NB: The super() of an inlined derived constructor initializes its instance elements through the slow path of
        //     BindThisFromSuperCall.
        let inlinable = |constructor: Gc<EcmascriptFunctionObject>| {
            constructor.has_constructor()
                && (constructor.constructor_kind() == ConstructorKind::Derived
                    || !constructor.has_class_data()
                    || constructor.private_methods_count() == 0)
                && !constructor.allocates_function_environment()
                && !constructor.contains_direct_call_to_eval()
                && constructor.realm() == Some(caller.realm)
                // NB: The frame of a derived constructor holds its this value (see
                //     `function_with_frame_as_this_environment()`).
                && (constructor.constructor_kind() == ConstructorKind::Base
                    || !constructor.shared_data().function_environment_has_this_binding())
        };
        // NB: The constructors super() constructs with, from the target's parent down to a base constructor. Each
        //     level is one inlined call deeper.
        // NB: Making the shape of `this` below may collect garbage.
        let chain = MarkedVec::new(self.vm);
        chain.push(function);
        loop {
            let constructor = chain.get(chain.len() - 1).expect("the chain starts with the target");
            if !inlinable(constructor) {
                return None;
            }
            if constructor.constructor_kind() == ConstructorKind::Base {
                break;
            }
            if chain.len() > self.vm.jit.options.inline_depth as usize {
                return None;
            }
            chain.push(ecmascript_function(
                constructor.prototype().map(|parent| parent.upcast::<CellHeader>()),
            )?);
        }
        // NB: Class constructors cannot be called, only constructed, so their executable is no inline call executable,
        //     but it runs in an inline frame for a construct all the same.
        let limit = MAX_INLINED_CONSTRUCT_INSTRUCTION_COUNT;
        for level in 0..chain.len() {
            let constructor = chain.get(level).expect("the level is in the chain");
            if instruction_count(&*constructor.bytecode_executable()?, limit) > limit {
                return None;
            }
        }
        // NB: GetSuperConstructor in each derived constructor reads its [[Prototype]] from its shape.
        for level in 0..chain.len() - 1 {
            let constructor = chain.get(level).expect("the level is in the chain");
            if constructor.shape().is_dictionary() || !constructor.has_ordinary_get_prototype_of() {
                return None;
            }
        }
        let function_shape = function.shape();
        if function_shape.is_dictionary() {
            return None;
        }
        let metadata = function_shape.lookup(&self.vm.names.prototype)?;
        let prototype = function.get_direct(metadata.offset);
        if !prototype.is_object() {
            return None;
        }
        let prototype = prototype.as_object();
        // 10.1.13 OrdinaryCreateFromConstructor ( constructor, intrinsicDefaultProto [ , internalSlotsList ] ), https://tc39.es/ecma262/#sec-ordinarycreatefromconstructor
        // NB: The new object gets the shape of an empty object with the prototype, like Object's constructor with a
        //     prototype gives it.
        let empty_shape = caller.realm.intrinsics().empty_object_shape();
        let this_shape = if empty_shape.prototype() == Some(prototype) {
            empty_shape
        } else {
            empty_shape.create_prototype_transition(self.vm, Some(prototype))
        };
        let base = chain
            .get(chain.len() - 1)
            .expect("the chain ends with a base constructor");
        let (this_shape, literal_fields) = if base.has_class_data() {
            let (shape, fields) = self.literal_instance_fields(base, this_shape)?;
            (shape, Some(fields))
        } else {
            (this_shape, None)
        };
        let index = self.inlined_function_index(function, caller)?;
        let mut indices = vec![index];
        for level in 1..chain.len() {
            let entry = self.entries[*indices.last().expect("the chain starts with the target") as usize];
            let parent = chain.get(level).expect("the level is in the chain");
            indices.push(self.inlined_function_index(parent, &entry)?);
        }
        if let Some(fields) = literal_fields {
            vec_map_insert(
                &mut self.scratch.literal_instance_fields,
                *indices.last().expect("the chain ends with a base constructor"),
                fields,
            );
        }
        for (level, index) in indices.iter().enumerate().take(chain.len() - 1) {
            let constructor = chain.get(level).expect("the level is in the chain");
            if let Some(fields) = self.derived_instance_fields(constructor) {
                vec_map_insert(&mut self.scratch.derived_instance_fields, *index, fields);
            }
        }
        if chain.len() == 1 {
            vec_map_insert(&mut self.construct_this_shapes, index, this_shape);
        }
        let mut super_constructor = None;
        for level in (0..chain.len() - 1).rev() {
            let target = SuperConstructTarget {
                executable: indices[level + 1],
                function: self.cell(chain.get(level + 1).expect("the level is in the chain")),
                function_shape: self.cell(chain.get(level).expect("the level is in the chain").shape()),
                derived: level + 2 < chain.len(),
            };
            vec_map_insert(&mut self.scratch.super_constructors, indices[level], target);
            super_constructor = Some(target);
        }
        Some(ConstructTarget {
            executable: index,
            function_shape: self.cell(function_shape),
            prototype_offset: metadata.offset,
            prototype: self.cell(prototype),
            this_shape: self.cell(this_shape),
            reserve: base.shared_data().current_construct_reserve(),
            super_constructor,
        })
    }

    /// The values InitializeInstanceElements gives `this` of the base constructor `constructor`, whose construct
    /// creates it with `shape`, by property offset, if its instance elements are only fields with string or symbol
    /// names whose initial values are no cells (from literals) or absent, and the shape `this` has after that.
    fn literal_instance_fields(
        &mut self,
        constructor: Gc<EcmascriptFunctionObject>,
        shape: Gc<Shape>,
    ) -> Option<(Gc<Shape>, LiteralFieldValues)> {
        let (shape, fields) = self.literal_field_definitions(constructor, shape, false)?;
        let values = self
            .snapshot
            .add_exact_iter(fields.into_iter().map(|(offset, value, _)| (offset, value)));
        Some((shape, values))
    }

    /// How InitializeInstanceElements of the derived constructor `constructor` defines its fields on objects of the
    /// shape it always initialized so far (see `ExecutableSnapshot::derived_instance_fields`), if they are literal
    /// fields (see literal_instance_fields()) whose names objects of that shape do not have yet, and it allows
    /// extensions.
    fn derived_instance_fields(&mut self, constructor: Gc<EcmascriptFunctionObject>) -> Option<DerivedInstanceFields> {
        if constructor.constructor_kind() != ConstructorKind::Derived || !constructor.has_class_data() {
            return None;
        }
        let shape = constructor.instance_elements_shape()?;
        if shape.is_dictionary() || shape.is_non_extensible() {
            return None;
        }
        let own_property_count = shape.property_count();
        let (_, fields) = self.literal_field_definitions(constructor, shape, true)?;
        // NB: Defining a property the object has already may fail or change its attributes.
        if fields.iter().any(|(offset, _, _)| *offset < own_property_count) {
            return None;
        }
        Some(DerivedInstanceFields {
            shape: self.cell(shape),
            fields: self.snapshot.add(&fields),
        })
    }

    /// The fields InitializeInstanceElements of `constructor` defines on an ordinary object of `shape`, if its instance
    /// elements are only fields with string or symbol names whose initial values are no cells (from literals) or
    /// absent: the shape the object has after that, and in order, each field's property offset, its value, and with
    /// `with_shapes` the shape the object gets if the field adds the property.
    fn literal_field_definitions(
        &mut self,
        constructor: Gc<EcmascriptFunctionObject>,
        shape: Gc<Shape>,
        with_shapes: bool,
    ) -> Option<(Gc<Shape>, Vec<FieldDefinition>)> {
        if constructor.private_methods_count() != 0 {
            return None;
        }
        let mut shape = shape;
        let mut values = Vec::new();
        for index in 0..constructor.fields_count() {
            let field = constructor.field(index);
            let ClassElementName::PropertyKey(key) = &field.name else {
                return None;
            };
            // NB: Fields with index names go to the indexed elements.
            if key.is_number() {
                return None;
            }
            let value = match field.initializer {
                ClassFieldInitializer::Value(value) if !value.is_cell() => value,
                ClassFieldInitializer::Empty => Value::UNDEFINED,
                _ => return None,
            };
            // 7.3.33 DefineField ( receiver, fieldRecord ), https://tc39.es/ecma262/#sec-definefield
            // 6.b. Perform ? CreateDataPropertyOrThrow(receiver, fieldName, initValue).
            // NB: For a property the object has from an earlier field of the same name, this sets the value like the
            //     interpreter does, and otherwise it adds the property.
            let (offset, added) = match shape.lookup(key) {
                Some(metadata) => (metadata.offset, None),
                None => {
                    shape = shape.create_put_transition(self.vm, key, DEFAULT_ATTRIBUTES);
                    (shape.property_count() - 1, with_shapes.then(|| self.cell(shape)))
                }
            };
            values.push((offset, value.0, added));
        }
        Some((shape, values))
    }

    /// The environment of direct calls of `function`: Some(None) if they need none of their own, and Some(index of
    /// the template) if JIT code can allocate it. Frames of functions without one of their own have the function's
    /// [[Environment]], and those of arrow functions resolve `this` through it.
    fn direct_call_environment(&mut self, function: Gc<EcmascriptFunctionObject>) -> Option<Option<u32>> {
        if !function.function_environment_needed() {
            return Some(None);
        }
        let cells = &mut self.cells;
        let template =
            function_environment_template(self.vm, function, &mut self.snapshot, |cell| note_cell(cells, cell))?;
        let index = u32::try_from(self.scratch.environment_templates.len()).ok()?;
        self.scratch.environment_templates.push(template);
        Some(Some(index))
    }

    fn direct_call_target_snapshot(
        &mut self,
        function: Gc<EcmascriptFunctionObject>,
        environment: Option<u32>,
        closures: bool,
    ) -> DirectCallTarget {
        let executable = function.inline_call_executable();
        let entry = self
            .vm
            .jit
            .entry_table
            .as_ref()
            .expect("the VM has a JIT entry table while the JIT is on")
            .entry_address(executable.head.jit_entry_slot.get());
        DirectCallTarget {
            function: self.inlined_function(function),
            executable: self.cell(executable),
            entry,
            registers_and_locals_count: executable.registers_and_locals_count(),
            registers_and_locals_and_constants_count: executable.head.registers_and_locals_and_constants_count.get(),
            function_fields: self.frame_fields(function),
            environment,
            closures,
        }
    }

    /// The target through which JIT code may call `function`, a method a prototype held, directly, if it can and its
    /// calls need no function environment (whose template would belong to the executable of the call).
    fn prototype_method_direct_call_ecmascript(
        &mut self,
        function: Gc<EcmascriptFunctionObject>,
    ) -> Option<DirectCallTarget> {
        if !function.can_inline_call() || function.function_environment_needed() {
            return None;
        }
        Some(self.direct_call_target_snapshot(function, None, false))
    }

    fn call_feedback(
        &mut self,
        call: &CallFeedback,
        entry: &Entry,
        shared_flags: u8,
        shared_value: u16,
    ) -> CallFeedbackSnapshot {
        let target = call.target();
        let flags = call.flags.get() | (shared_flags & SHARED_CALL_FLAGS_MASK);
        let value = call.value.get() | shared_value;
        let Some(target) = target else {
            return CallFeedbackSnapshot {
                flags,
                value,
                ..CallFeedbackSnapshot::default()
            };
        };
        let target_cell = self.cell(target);
        let monomorphic = flags & call_feedback_flags::POLYMORPHIC == 0;
        let mut snapshot = CallFeedbackSnapshot {
            target: Some(target_cell),
            flags,
            value,
            ..CallFeedbackSnapshot::default()
        };
        if let Some(function) = ecmascript_function(Some(target)) {
            if monomorphic {
                snapshot.inline_executable = self
                    .inlining_candidate_ecmascript(function, entry.realm)
                    .and_then(|f| self.inlined_function_index(f, entry));
                if flags & call_feedback_flags::SAW_CONSTRUCT != 0 {
                    snapshot.construct = self.construct_target_ecmascript(function, entry);
                }
                if flags & call_feedback_flags::FORWARDED_POLYMORPHIC == 0 {
                    snapshot.forwarded = self.forwarded_call(call, entry);
                }
            }
            // NB: The environment of a direct call's function environment is the target's, so closures with their own
            //     environments are only called directly if they need no function environment.
            let closures = !monomorphic;
            if !(closures && flags & (call_feedback_flags::OTHER_FUNCTIONS | call_feedback_flags::SAW_NATIVE) != 0)
                && function.can_inline_call()
                && let Some(environment) = self.direct_call_environment(function)
                && !(closures && environment.is_some())
            {
                snapshot.direct_call = Some(self.direct_call_target_snapshot(function, environment, closures));
            }
            return snapshot;
        }
        if let Some(raw_native) = target.downcast::<RawNativeFunction>() {
            snapshot.target_intrinsic = intrinsic_of_raw_native(self.vm, raw_native);
            snapshot.target_builtin = raw_native.builtin().map(|builtin| builtin as u8);
            if monomorphic {
                if flags & call_feedback_flags::FORWARDED_POLYMORPHIC == 0 {
                    snapshot.forwarded = self.forwarded_call(call, entry);
                }
                if !FunctionPrototype::is_call_function(self.vm, &raw_native)
                    && !FunctionPrototype::is_apply_function(self.vm, &raw_native)
                {
                    let entry = raw_native
                        .native_function(self.vm)
                        .filter(|_| RAW_NATIVE_FUNCTIONS_RETURN_IN_REGISTERS)
                        .and_then(|pointer| NonZeroU64::new(pointer as usize as u64));
                    snapshot.native_call = Some(NativeCallTarget {
                        function: target_cell,
                        realm: self.cell(raw_native.realm()),
                        entry,
                        direct_getter: None,
                    });
                }
            }
            return snapshot;
        }
        snapshot.target_intrinsic = intrinsic_of_constructor(target);
        if monomorphic {
            if let Some(builtin) = builtin_inlining_candidate(Some(target), entry.realm) {
                snapshot.inline_executable = self.inlined_builtin_index(builtin, entry);
            }
            if flags & call_feedback_flags::FORWARDED_POLYMORPHIC == 0 {
                snapshot.forwarded = self.forwarded_call(call, entry);
            }
            if let Some(native) = target.downcast::<NativeFunction>() {
                let entry = native
                    .capturing_call_entry()
                    .filter(|_| RAW_NATIVE_FUNCTIONS_RETURN_IN_REGISTERS)
                    .and_then(|pointer| NonZeroU64::new(pointer as usize as u64));
                snapshot.native_call = Some(NativeCallTarget {
                    function: target_cell,
                    realm: self.cell(native.realm()),
                    entry,
                    direct_getter: None,
                });
            }
        }
        snapshot
    }

    /// The target through which JIT code may call an accessor's raw native function, whose cell is `function_cell`,
    /// directly. Like the interpreter, JIT code reads the value of a direct getter without calling it only through a
    /// cache entry on which the getter ran (`direct_getter_validated`).
    fn native_accessor_target_raw(
        &mut self,
        native_function: Gc<RawNativeFunction>,
        function_cell: CellId,
        direct_getter_validated: bool,
    ) -> Option<NativeCallTarget> {
        if !RAW_NATIVE_FUNCTIONS_RETURN_IN_REGISTERS {
            return None;
        }
        let entry = NonZeroU64::new(native_function.native_function(self.vm)? as usize as u64)?;
        let direct_getter = native_function
            .downcast::<DirectGetterFunction>()
            .filter(|_| direct_getter_validated)
            .map(|getter| {
                let bytes = |word_offset: u32| word_offset * size_of::<u64>() as u32;
                let value_kind = match getter.implementation_value_is_compressed.get() {
                    0 => DirectGetterValueKind::Pointer,
                    1 => DirectGetterValueKind::CompressedPointer,
                    2 => DirectGetterValueKind::UnsignedShort,
                    3 => DirectGetterValueKind::Boolean,
                    kind => unreachable!("invalid DirectGetterFunction value kind {kind}"),
                };
                DirectGetterFields {
                    wrapper_implementation: bytes(getter.wrapper_implementation_word_offset.get()),
                    implementation_value: getter.implementation_value_offset.get(),
                    value_kind,
                    main_world_wrapper: bytes(getter.main_world_wrapper_word_offset.get()),
                    weak_impl_value: bytes(getter.weak_impl_value_word_offset.get()),
                }
            });
        Some(NativeCallTarget {
            function: function_cell,
            realm: self.cell(native_function.realm()),
            entry: Some(entry),
            direct_getter,
        })
    }

    /// The closure templates of an inlined function, whose frames JIT code creates closures in like the compiled
    /// function's. Inlined frames have no private environment of their own, so a function with one has none.
    fn inlined_closure_templates(
        &mut self,
        entry: &Entry,
        function: Gc<EcmascriptFunctionObject>,
    ) -> List<Option<ClosureTemplateSnapshot>> {
        if function.private_environment().is_some() {
            return List::empty();
        }
        let index = vec_map_get(&self.indices, &InlinedFunction::Ecmascript(function))
            .expect("an inlined function has an index");
        let defer_gc = self.defer_gc;
        let templates = closure_templates(self.vm, entry.executable, function, defer_gc, &mut self.snapshot);
        for (template_index, template) in templates.iter().enumerate() {
            if let Some((sample, _)) = template {
                self.closure_samples.push((index, template_index, *sample));
            }
        }
        let cells = &mut self.cells;
        closure_template_snapshots(
            templates.into_inner(),
            |cell| note_cell(cells, cell),
            &mut self.sample_slots,
            defer_gc,
            &mut self.snapshot,
        )
    }

    /// The snapshot of an executable of the snapshot.
    fn executable_snapshot(&mut self, entry: &Entry) -> ExecutableSnapshot {
        let vm = self.vm;
        let Entry { executable, realm, .. } = *entry;
        // NB: Builtins never create arguments objects.
        let arguments_function = match entry.function {
            None => self.compiled_function,
            Some(InlinedFunction::Ecmascript(function)) => Some(function),
            Some(InlinedFunction::Builtin(_)) => None,
        };
        let code = executable.code();
        let bytecode = &*code.bytecode;
        self.bytecode_owners.push(Arc::clone(code));
        // SAFETY: `self.bytecode_owners` retains an `Arc<ExecutableData>` keeping `code.bytecode` alive and immutable
        //         until the snapshot is cleared or dropped.
        let bytecode_slice = unsafe { BytecodeSlice::borrowed(bytecode) };

        self.scratch.shared_call_flags.clear();
        self.scratch.shared_call_value.clear();
        self.scratch.shared_max_cache_sizes.clear();
        let caches_slice = executable.property_lookup_caches();

        // NB: Value feedback is only up to date once the values the interpreter recorded since the last update are
        //     folded in. Unprofiled inlined callees set `unprofiled = true` instead of allocating a zero-filled
        //     `ExecutableFeedback`, so `builder::has_run()` still reports unexecuted instructions as not having run.
        let (unprofiled, feedback) = if self.use_shared_learning {
            let local_feedback = executable.feedback();
            let (has_feedback, arith, value, cache_value, keyed) = {
                let mut learning = code
                    .ensure_shared_jit_learning()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                learning.merge_feedback(local_feedback, caches_slice);
                self.scratch
                    .shared_max_cache_sizes
                    .extend_from_slice(&learning.max_property_cache_sizes);
                if learning.has_feedback {
                    self.scratch.shared_call_flags.extend_from_slice(&learning.call_flags);
                    self.scratch.shared_call_value.extend_from_slice(&learning.call_value);
                    let arith = self.snapshot.add(&learning.arith);
                    let value = self.snapshot.add(&learning.value);
                    let cache_value = self.snapshot.add(&learning.cache_value);
                    let keyed = self
                        .snapshot
                        .add_exact_iter(learning.keyed_bits.iter().map(|&bits| KeyedFeedbackSnapshot { bits }));
                    (true, arith, value, cache_value, keyed)
                } else {
                    (false, List::empty(), List::empty(), List::empty(), List::empty())
                }
            };
            if has_feedback {
                let call = self.snapshot.begin();
                if let Some(local_feedback) = local_feedback {
                    for (slot, call) in local_feedback.call().iter().enumerate() {
                        let shared_flags = self.scratch.shared_call_flags.get(slot).copied().unwrap_or(0);
                        let shared_value = self.scratch.shared_call_value.get(slot).copied().unwrap_or(0);
                        let row = self.call_feedback(call, entry, shared_flags, shared_value);
                        self.snapshot.push(row);
                    }
                } else {
                    for index in 0..self.scratch.shared_call_flags.len() {
                        let flags = self.scratch.shared_call_flags[index] & SHARED_CALL_FLAGS_MASK;
                        let value = self.scratch.shared_call_value[index];
                        self.snapshot.push(CallFeedbackSnapshot {
                            flags,
                            value,
                            ..CallFeedbackSnapshot::default()
                        });
                    }
                }
                let call = self.snapshot.end(call);
                let snapshot = FeedbackSnapshot {
                    arith,
                    value,
                    cache_value,
                    call,
                    keyed,
                    bound_arguments: self.snapshot.add(&self.scratch.bound_arguments),
                };
                self.scratch.bound_arguments.clear();
                (false, snapshot)
            } else {
                (true, FeedbackSnapshot::default())
            }
        } else if let Some(feedback) = executable.feedback() {
            feedback.update_value_feedback();
            let call = self.snapshot.begin();
            for call in feedback.call() {
                let row = self.call_feedback(call, entry, 0, 0);
                self.snapshot.push(row);
            }
            let call = self.snapshot.end(call);
            let keyed = self.snapshot.add_exact_iter(
                feedback
                    .keyed()
                    .iter()
                    .map(|keyed| KeyedFeedbackSnapshot { bits: keyed.bits.get() }),
            );
            let snapshot = FeedbackSnapshot {
                arith: self.snapshot.add(cell_slice_as_values(feedback.arith())),
                value: self.snapshot.add(cell_slice_as_values(feedback.value())),
                cache_value: self.snapshot.add(cell_slice_as_values(feedback.cache_value())),
                call,
                keyed,
                bound_arguments: self.snapshot.add(&self.scratch.bound_arguments),
            };
            self.scratch.bound_arguments.clear();
            (false, snapshot)
        } else {
            (true, FeedbackSnapshot::default())
        };

        let index = entry
            .function
            .and_then(|function| vec_map_get(&self.indices, &function));
        let construct_this_shape = index.and_then(|index| vec_map_get(&self.construct_this_shapes, &index));
        let super_constructor = index.and_then(|index| vec_map_get(&self.scratch.super_constructors, &index));
        let literal_instance_fields =
            index.and_then(|index| vec_map_get(&self.scratch.literal_instance_fields, &index));
        let derived_instance_fields =
            index.and_then(|index| vec_map_get(&self.scratch.derived_instance_fields, &index));
        let has_instance_elements =
            matches!(entry.function, Some(InlinedFunction::Ecmascript(function)) if function.has_class_data());
        let primitive_prototype_shapes = if realm == self.compiled_realm {
            self.realm_primitive_prototype_shapes
        } else {
            [
                realm.string_prototype(vm).shape(),
                realm.number_prototype(vm).shape(),
                realm.boolean_prototype(vm).shape(),
            ]
        };
        let cache_sizes_start =
            u32::try_from(self.property_cache_sizes.len()).expect("property cache sizes count fits in u32");
        self.property_cache_sizes.reserve(caches_slice.len());
        let mut has_megamorphic_cache = false;
        let property_caches = self.snapshot.begin();
        for (cache_index, cache) in caches_slice.iter().enumerate() {
            let shared_max_size = if self.use_shared_learning {
                self.scratch.shared_max_cache_size(cache_index)
            } else {
                0
            };
            let (row, cache_size) = self.property_cache(
                cache,
                entry,
                construct_this_shape,
                &primitive_prototype_shapes,
                shared_max_size,
            );
            has_megamorphic_cache |= row.kind == PropertyCacheKind::Megamorphic;
            self.property_cache_sizes.push(cache_size);
            self.snapshot.push(row);
        }
        let property_caches = self.snapshot.end(property_caches);
        self.scratch.property_cache_ranges.push((
            cache_sizes_start,
            u32::try_from(caches_slice.len()).expect("property cache count fits in u32"),
        ));
        let object_shape_caches = self.snapshot.begin();
        for cache in executable.object_shape_caches() {
            let row = cache.shape.get().map(|shape| ObjectShapeCacheSnapshot {
                shape: self.shape(shape),
                property_offsets: self.snapshot.add(&cache.property_offsets.borrow()),
            });
            self.snapshot.push(row);
        }
        let object_shape_caches = self.snapshot.end(object_shape_caches);
        let environment_templates = self.snapshot.add(&self.scratch.environment_templates);
        self.scratch.environment_templates.clear();

        // NB: The same rules as ExecutionContext::keeps_private_environment() and keeps_variable_environment().
        let frame_function = match entry.function {
            Some(InlinedFunction::Ecmascript(function)) => Some(function),
            Some(InlinedFunction::Builtin(_)) => None,
            None => self.compiled_function,
        };
        let keeps_own_environments = executable.keeps_own_environments();
        let (frames_read_private_environment_from_function, frames_read_variable_environment_from_function) =
            match frame_function {
                Some(function) => (
                    !keeps_own_environments,
                    !keeps_own_environments && !function.function_environment_needed(),
                ),
                None => (false, false),
            };
        let (new_object_shape, string_prototype, number_prototype, boolean_prototype) = if realm == self.compiled_realm
        {
            (
                self.realm_new_object_shape,
                self.realm_string_prototype,
                self.realm_number_prototype,
                self.realm_boolean_prototype,
            )
        } else {
            let intrinsics = realm.intrinsics();
            (
                self.shape(intrinsics.new_object_shape()),
                self.cell(realm.string_prototype(vm)),
                self.cell(realm.number_prototype(vm)),
                self.cell(realm.boolean_prototype(vm)),
            )
        };
        ExecutableSnapshot {
            frames_read_private_environment_from_function,
            frames_read_variable_environment_from_function,
            unprofiled,
            bytecode: bytecode_slice,
            bytecode_address: bytecode.as_ptr() as u64,
            layout: FrameLayout {
                number_of_registers: executable.number_of_registers(),
                registers_and_locals_count: executable.registers_and_locals_count(),
                number_of_constants: u32::try_from(executable.constants().len())
                    .expect("the constant count fits in u32"),
                number_of_arguments: executable.number_of_arguments(),
            },
            constants: {
                let constants = executable.constants();
                for &constant in constants {
                    self.value_bits(constant);
                }
                const _: () =
                    assert!(size_of::<Value>() == size_of::<u64>() && align_of::<Value>() == align_of::<u64>());
                // SAFETY: `Value` is `#[repr(transparent)]` over `u64`.
                let bits = unsafe { ak::slice_from_raw_parts(constants.as_ptr().cast::<u64>(), constants.len()) };
                self.snapshot.add(bits)
            },
            exception_handlers: self
                .snapshot
                .add_exact_iter(executable.exception_handlers().iter().map(|handler| ExceptionHandler {
                    start_offset: handler.start_offset,
                    end_offset: handler.end_offset,
                    handler_offset: handler.handler_offset,
                })),
            exit_sites: if matches!(entry.function, Some(InlinedFunction::Builtin(_))) {
                if self.use_shared_learning {
                    let shared_sites = self.entries[0]
                        .executable
                        .code()
                        .shared_builtin_exit_sites_for(code.bytecode_fingerprint());
                    self.snapshot.add(&shared_sites)
                } else {
                    List::empty()
                }
            } else {
                self.snapshot
                    .add(&executable.jit_exit_sites_for_recompile_with_shared(self.use_shared_learning))
            },
            builtin_exit_sites: if entry.function.is_none() {
                self.snapshot.add_exact_iter(
                    executable
                        .jit_builtin_exit_sites()
                        .into_iter()
                        .map(|(builtin, pc, kind)| (CellId(builtin), pc, kind)),
                )
            } else {
                List::empty()
            },
            feedback,
            property_caches,
            cell: self.cell(executable),
            function: entry.function.map(|function| match function {
                InlinedFunction::Ecmascript(function) => self.inlined_function(function),
                InlinedFunction::Builtin(function) => self.inlined_builtin(function),
            }),
            function_fields: match entry.function {
                Some(InlinedFunction::Ecmascript(function)) => Some(self.frame_fields(function)),
                // NB: The compiled function's script tells which inlined calls
                //     run in it.
                None => Some(self.compiled_function_fields),
                Some(InlinedFunction::Builtin(_)) => None,
            },
            environment: match entry.function {
                Some(InlinedFunction::Ecmascript(function)) => {
                    function.environment().map(|environment| self.cell(environment))
                }
                _ => None,
            },
            captured_bindings: arguments_function
                .map(|function| self.captured_bindings(function, executable, realm))
                .unwrap_or_default(),
            mutable_bindings: arguments_function
                .map(|function| mutable_bindings(&mut self.snapshot, function, executable))
                .unwrap_or_default(),
            builtin: matches!(entry.function, Some(InlinedFunction::Builtin(_))),
            mapped_arguments_alias_parameters: arguments_function
                .is_some_and(|function| !function.mapped_argument_names().is_empty()),
            supports_legacy_arguments: arguments_function
                .is_some_and(|function| function.supports_legacy_caller_or_arguments()),
            new_object_shape: Some(new_object_shape),
            string_prototype: Some(string_prototype),
            number_prototype: Some(number_prototype),
            boolean_prototype: Some(boolean_prototype),
            object_shape_caches,
            closure_templates: match entry.function {
                None => self.closure_templates,
                Some(InlinedFunction::Ecmascript(function)) => self.inlined_closure_templates(entry, function),
                Some(InlinedFunction::Builtin(_)) => List::empty(),
            },
            environment_templates,
            lexical_environment_templates: if entry.function.is_none() {
                self.lexical_environment_templates
            } else {
                List::empty()
            },
            // NB: Builtins written in JavaScript never access global variables by name.
            globals: (!matches!(entry.function, Some(InlinedFunction::Builtin(_))))
                .then(|| self.globals(executable, entry)),
            super_constructor,
            has_instance_elements,
            literal_instance_fields,
            derived_instance_fields,
            // NB: Only the compiled function's own code creates bindings by name.
            identifiers: if entry.function.is_none() {
                self.snapshot.add_exact_iter(
                    executable
                        .identifier_table()
                        .iter()
                        .map(|identifier| identifier.raw_identity() as u64),
                )
            } else {
                List::empty()
            },
            property_key_names: if has_megamorphic_cache {
                self.snapshot
                    .add_exact_iter(executable.property_keys().iter().map(|key| {
                        if key.is_string() {
                            key.as_string().raw_identity() as u64
                        } else {
                            0
                        }
                    }))
            } else {
                List::empty()
            },
        }
    }

    /// Object values of captured bindings of functions whose lexical environment is always their closure's
    /// environment, while the function has no other closure, so that every frame running the code sees these
    /// bindings. Their loads are guarded, so a reassignment or a closure created later can change the value safely.
    /// A binding whose guard failed at any of its loads changes, so none of its loads speculates on it again.
    fn captured_bindings(
        &mut self,
        function: Gc<EcmascriptFunctionObject>,
        executable: Gc<Executable>,
        realm: Gc<Realm>,
    ) -> List<(EnvironmentCoordinate, GlobalValueSnapshot)> {
        if function.allocates_function_environment()
            || function.shared_data().closure_count() != ClosureCount::One
            || executable.code().captured_bindings_known_empty()
        {
            return List::empty();
        }
        let accesses = get_or_compute_captured_binding_accesses(&executable, self.snapshot.scratch());
        let loads = &accesses.loads;
        if loads.is_empty() {
            return List::empty();
        }
        let exit_sites = executable.jit_exit_sites();
        let has_shared_exits = self.use_shared_learning && executable.code().shared_jit_learning().is_some();
        let changed: Vec<EnvironmentCoordinate> = if exit_sites.is_empty() && !has_shared_exits {
            Vec::new()
        } else {
            loads
                .iter()
                .filter(|&&(_, pc)| {
                    exit_sites.contains(&(pc, ExitKind::UnexpectedValue))
                        || (has_shared_exits && executable.code().shared_has_unexpected_value_exit(pc))
                })
                .map(|&(coordinate, _)| coordinate)
                .collect()
        };
        let bindings = self.snapshot.begin::<(EnvironmentCoordinate, GlobalValueSnapshot)>();
        for &(coordinate, _) in loads {
            let is_captured = self
                .snapshot
                .rows_of(&bindings)
                .iter()
                .any(|(captured, _)| *captured == coordinate);
            if changed.contains(&coordinate) || is_captured {
                continue;
            }
            let mut environment = function.environment();
            for _ in 0..coordinate.hops {
                environment = environment.and_then(|environment| environment.outer_environment());
                if environment.is_none() {
                    break;
                }
            }
            let Some(environment) = environment else {
                continue;
            };
            let Some(environment) = environment.as_declarative_environment() else {
                continue;
            };
            if coordinate.index as usize >= environment.binding_count.get() as usize {
                continue;
            }
            let value = environment.get_initialized_binding_value_direct(coordinate.index as usize);
            if value.is_object() {
                let row = (coordinate, self.global_value(value, realm, None));
                self.snapshot.push(row);
            }
        }
        self.snapshot.end(bindings)
    }
}

/// The bindings of the environments of `function`'s closure that `executable` stores to and that are mutable in every
/// instance of their environment: declared mutable, and never deleted. Its stores to them need no check of that. Like
/// captured bindings, these are only found while the function has no other closure.
fn mutable_bindings(
    snapshot: &mut SnapshotWriter,
    function: Gc<EcmascriptFunctionObject>,
    executable: Gc<Executable>,
) -> List<EnvironmentCoordinate> {
    if function.allocates_function_environment()
        || function.shared_data().closure_count() != ClosureCount::One
        || executable.code().captured_bindings_known_empty()
    {
        return List::empty();
    }
    let accesses = get_or_compute_captured_binding_accesses(&executable, snapshot.scratch());
    let stores = &accesses.stores;
    if stores.is_empty() {
        return List::empty();
    }
    let bindings = snapshot.begin();
    for &coordinate in stores {
        if snapshot.rows_of(&bindings).contains(&coordinate) {
            continue;
        }
        let mut environment = function.environment();
        for _ in 0..coordinate.hops {
            environment = environment.and_then(|environment| environment.outer_environment());
        }
        let Some(environment) = environment else {
            continue;
        };
        let Some(environment) = environment.as_declarative_environment() else {
            continue;
        };
        if environment.binding_is_declared_mutable(coordinate.index as usize) {
            snapshot.push(coordinate);
        }
    }
    snapshot.end(bindings)
}

/// Returns the cached bytecode scan of captured binding loads and stores for `executable`, sharing the immutable
/// result across realms linking the same `ExecutableData`.
fn get_or_compute_captured_binding_accesses(executable: &Executable, zone: &mut Zone) -> Arc<CapturedBindingAccesses> {
    Arc::clone(executable.jit_state().captured_binding_accesses.get_or_init(|| {
        if let Some(cached) = executable.code().cached_captured_binding_accesses() {
            return cached;
        }
        let accesses = captured_binding_accesses(executable, zone);
        if accesses.loads.is_empty() && accesses.stores.is_empty() {
            executable.code().mark_captured_bindings_empty();
            return Arc::new(accesses);
        }
        executable
            .code()
            .set_cached_captured_binding_accesses(Arc::new(accesses))
    }))
}

/// The loads and stores of bindings of environments in `executable`, or none if its code may change its lexical
/// environment, so that they are not the environments of its closure. `zone` holds nothing after this.
fn captured_binding_accesses(executable: &Executable, zone: &mut Zone) -> CapturedBindingAccesses {
    let bytecode = executable.bytecode();
    let Ok(end) = u32::try_from(bytecode.len()) else {
        return CapturedBindingAccesses::default();
    };
    let mut loads = Vec::new();
    let mut stores = Vec::new();
    let mut pc = 0;
    // NB: Scan opcodes and lengths directly without decoding fields for instructions that neither alter lexical
    //     environments nor access captured bindings. Only the four binding-access opcodes enter `zone.scope`.
    while pc < end {
        let Ok((opcode, length)) = opcode_and_length(bytecode, pc) else {
            return CapturedBindingAccesses::default();
        };
        if opcode == OpCode::SetLexicalEnvironment as u8
            || opcode == OpCode::CreateLexicalEnvironment as u8
            || opcode == OpCode::CreateVariableEnvironment as u8
            || opcode == OpCode::EnterObjectEnvironment as u8
            || opcode == OpCode::NewClass as u8
            || opcode == OpCode::CallDirectEval as u8
            || opcode == OpCode::CallDirectEvalWithArgumentArray as u8
            || opcode == OpCode::Debugger as u8
        {
            return CapturedBindingAccesses::default();
        }
        if opcode == OpCode::GetBinding as u8
            || opcode == OpCode::GetInitializedBinding as u8
            || opcode == OpCode::GetCalleeAndThisFromEnvironment as u8
            || opcode == OpCode::SetLexicalBinding as u8
        {
            let ok = zone.scope(|zone| {
                let decoded = decode_instruction(bytecode, pc, zone).ok()?;
                match decoded.instruction {
                    Instruction::GetBinding { cache: coordinate, .. }
                    | Instruction::GetInitializedBinding { cache: coordinate, .. }
                    | Instruction::GetCalleeAndThisFromEnvironment { cache: coordinate, .. } => {
                        loads.push((coordinate, decoded.pc));
                    }
                    Instruction::SetLexicalBinding { cache: coordinate, .. } => stores.push(coordinate),
                    _ => {}
                }
                Some(())
            });
            if ok.is_none() {
                return CapturedBindingAccesses::default();
            }
        }
        pc += length as u32;
    }
    CapturedBindingAccesses {
        loads: loads.into_boxed_slice(),
        stores: stores.into_boxed_slice(),
    }
}

/// Captures the snapshot of a compile job for `executable`, whose frames run `function` in `realm`, or the top-level
/// code of a script or of a module without one, in the running frame. `osr_pc` is the loop back edge whose tier-up
/// budget ran out, if a loop triggered the compile.
pub fn capture_snapshot<'d>(
    vm: &Vm,
    executable: Gc<Executable>,
    function: Option<Gc<EcmascriptFunctionObject>>,
    realm: Gc<Realm>,
    osr_pc: Option<u32>,
    use_shared_learning: bool,
    defer_gc: &'d DeferGc<'_>,
) -> CapturedSnapshot<'d> {
    let compiled_function_fields = match function {
        Some(function) => function_frame_fields(function),
        None => FunctionFrameFields {
            script_or_module: ScriptOrModuleSlot::new(vm.running_execution_context_ref().script_or_module()).word(),
            ..FunctionFrameFields::default()
        },
    };
    let super::compile_queue::SpareSnapshot {
        snapshot: mut spare_snapshot,
        bytecode_owners: spare_bytecode_owners,
    } = super::compile_queue::spare_snapshot();
    let (runtime, bytecode_owners, cells, snapshot_executables, sample_slots) =
        vm.jit.with_snapshot_buffers(|open_lists, scratch| {
            let mut snapshot = SnapshotWriter::new(&mut spare_snapshot, open_lists);
            // NB: These allocate (and so may collect garbage), which they do before anything is captured.
            let allocation = allocation_infos(vm, realm, defer_gc, &mut snapshot);
            // NB: Closures that top-level code creates have its environments, which the code does not know.
            let closure_templates = match function {
                Some(function) => closure_templates(vm, executable, function, defer_gc, &mut snapshot),
                None => defer_gc.cover((0..executable.shared_function_data_count()).map(|_| None).collect()),
            };
            let closure_samples = closure_templates
                .iter()
                .map(|template| template.as_ref().map(|(sample, _)| *sample))
                .collect::<Vec<_>>();
            let mut cells = Vec::with_capacity(256);
            cells.push(*allocation.array_shape);
            let realm_new_object_shape_gc = realm.intrinsics().new_object_shape();
            let realm_new_object_shape = ShapeSnapshot {
                shape: note_cell(&mut cells, realm_new_object_shape_gc),
                property_count: realm_new_object_shape_gc.property_count(),
            };
            let realm_string_prototype_gc = realm.string_prototype(vm);
            let realm_number_prototype_gc = realm.number_prototype(vm);
            let realm_boolean_prototype_gc = realm.boolean_prototype(vm);
            let realm_string_prototype = note_cell(&mut cells, realm_string_prototype_gc);
            let realm_number_prototype = note_cell(&mut cells, realm_number_prototype_gc);
            let realm_boolean_prototype = note_cell(&mut cells, realm_boolean_prototype_gc);
            let realm_primitive_prototype_shapes = [
                realm_string_prototype_gc.shape(),
                realm_number_prototype_gc.shape(),
                realm_boolean_prototype_gc.shape(),
            ];
            let mut entries = Vec::with_capacity(16);
            entries.push(Entry {
                executable,
                function: None,
                realm,
                depth: 0,
            });
            let mut builder = SnapshotBuilder {
                vm,
                defer_gc,
                snapshot,
                bytecode_owners: spare_bytecode_owners,
                cells,
                entries,
                indices: Vec::with_capacity(16),
                property_cache_sizes: Vec::new(),
                scratch,
                compiled_realm: realm,
                realm_new_object_shape,
                realm_string_prototype,
                realm_number_prototype,
                realm_boolean_prototype,
                realm_primitive_prototype_shapes,
                compiled_function: function,
                compiled_function_fields,
                closure_templates: List::empty(),
                closure_samples: Vec::new(),
                sample_slots: Vec::new(),
                lexical_environment_templates: List::empty(),
                construct_this_shapes: Vec::new(),
                use_shared_learning,
            };
            if let Some(function) = function {
                note_frame_field_cells(&mut builder.cells, function);
            }
            let cells = &mut builder.cells;
            builder.closure_templates = closure_template_snapshots(
                closure_templates.into_inner(),
                |cell| note_cell(cells, cell),
                &mut builder.sample_slots,
                defer_gc,
                &mut builder.snapshot,
            );
            builder.lexical_environment_templates =
                lexical_environment_templates(vm, executable, realm, &mut builder.snapshot, |cell| {
                    note_cell(cells, cell)
                });
            // NB: executable_snapshot() appends the inlining candidates it finds.
            let mut index = 0;
            while index < builder.entries.len() {
                let entry = builder.entries[index];
                let row = builder.executable_snapshot(&entry);
                builder.snapshot.add_executable(row);
                index += 1;
            }
            // NB: Calls of closures the code creates can be inlined without feedback, as calls of the function its closure
            //     templates are samples of. These come last, so that they take what is left of the inlining budget. Their
            //     snapshots may have closure templates in turn.
            let mut samples = closure_samples
                .into_iter()
                .enumerate()
                .filter_map(|(template_index, sample)| Some((0, template_index, sample?)))
                .collect::<Vec<_>>();
            samples.append(&mut builder.closure_samples);
            while !samples.is_empty() {
                for (executable_index, template_index, sample) in samples {
                    let creator = builder.entries[executable_index as usize];
                    let inline_executable =
                        builder.inline_executable_index(Some(sample.upcast::<CellHeader>()), &creator);
                    // NB: A snapshot that had no room for an executable has no row for it (see `Snapshot::is_complete()`).
                    let Some(executable) = builder.snapshot.executables.get(executable_index as usize) else {
                        continue;
                    };
                    let templates = executable.closure_templates;
                    if let Some(Some(template)) = builder.snapshot[templates].get_mut(template_index) {
                        template.inline_executable = inline_executable;
                    }
                }
                while index < builder.entries.len() {
                    let entry = builder.entries[index];
                    let row = builder.executable_snapshot(&entry);
                    builder.snapshot.add_executable(row);
                    index += 1;
                }
                samples = core::mem::take(&mut builder.closure_samples);
            }
            let mut runtime = super::runtime_info::runtime_info(vm, realm, allocation, defer_gc);
            // NB: Code that embeds the prototypes of the runtime info lists them as embedded cells.
            builder.cell(realm.array_prototype());
            builder.cell(realm.object_prototype());
            runtime.array_iterator_next = builder.optional_cell(array_iterator_next(vm, realm));
            runtime.array_iterator_return_watchpoint_set = builder.optional_cell(
                realm
                    .array_iterator_return_watchpoint_set
                    .get()
                    .filter(|set| set.is_valid()),
            );
            // NB: Code that reads the cells of the realm embeds the realm.
            builder.cell(realm);
            runtime.array_iterator_next_watchpoint_set_address =
                realm_watchpoints::array_iterator_next_watchpoint_set_address(realm);
            runtime.array_iterator_method_watchpoint_set_address =
                realm_watchpoints::array_iterator_method_watchpoint_set_address(realm);
            let SnapshotBuilder {
                bytecode_owners,
                cells,
                entries,
                property_cache_sizes,
                scratch,
                sample_slots,
                ..
            } = builder;
            debug_assert_eq!(entries.len(), scratch.property_cache_ranges.len());
            let snapshot_executables = SnapshotExecutables {
                entries: entries
                    .iter()
                    .zip(scratch.property_cache_ranges.iter())
                    .map(|(entry, &(start, len))| SnapshotExecutable {
                        executable: entry.executable,
                        function: entry.function,
                        known_exit_sites: entry.executable.jit_exit_site_count(),
                        known_builtin_exit_sites: entry.executable.jit_builtin_exit_site_count(),
                        property_cache_sizes_start: start,
                        property_cache_sizes_len: len,
                    })
                    .collect(),
                property_cache_sizes: property_cache_sizes.into_boxed_slice(),
            };
            (runtime, bytecode_owners, cells, snapshot_executables, sample_slots)
        });
    let options = &vm.jit.options;
    let mut snapshot = spare_snapshot;
    snapshot.runtime = runtime;
    snapshot.options = {
        CompileOptions {
            dump_ir: options.dump_ir,
            dump_passes: options.dump_passes,
            dump_asm: options.dump_asm,
            verify_ir: options.verify_ir,
            osr_pc,
            runs_once: function.is_none(),
            inlining: InliningLimits {
                always_inlined_instructions: ALWAYS_INLINED_INSTRUCTION_COUNT,
                max_instructions: options.inline_max_size,
                budget_instructions: options.inline_budget,
                max_construct_instructions: MAX_INLINED_CONSTRUCT_INSTRUCTION_COUNT,
                max_depth: options.inline_depth,
            },
            coverage: options.coverage.is_some(),
            stress: StressOptions {
                exit_countdown: vm.jit.stress_exit_countdown_address(),
                osr_at_every_loop: options.stress_osr,
                few_registers: options.stress_registers,
            },
        }
    };
    CapturedSnapshot {
        snapshot,
        bytecode_owners,
        cells: defer_gc.cover(cells),
        sample_slots: defer_gc.cover(sample_slots),
        snapshot_executables: defer_gc.cover(snapshot_executables),
    }
}
