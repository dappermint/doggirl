/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! The call stub: the code JIT code shares for every call it does not make
//! directly (`Generic` call nodes, and direct calls whose callee is not their
//! target). It makes the call the way the interpreter's `Call` fast path
//! does, with what it needs read from the callee at call time, and takes the
//! generic path through `RuntimeInfo::jit_call` for every other callee.
//!
//! Calling convention: the vm and frame registers hold the VM and the calling
//! frame, which the stub preserves (it clobbers every other register). The
//! argument registers hold:
//!
//! 0. the callee value,
//! 1. the `this` value,
//! 2. the argument count,
//! 3. the pc of the call instruction in the low 32 bits, and the pc of the
//!    instruction after it in the high 32 bits,
//! 4. the call's destination slot index,
//!
//! and the arguments are on the machine stack, where the stub's caller stored
//! them right before the call (8 bytes each, the first one lowest). The stub
//! returns a slow path control word: a continuation at the next pc once the
//! result is in the destination slot, or what the runtime returned.

use ak::Column;
use ak::ZoneVec;

use super::Codegen;
use super::Locals;
use super::STUB_CODE_LIMIT;
use super::STUB_ZONE_LIMIT;
use super::call::Field;
use super::probes::MegamorphicKey;
use super::target_registers;
use crate::CompileFailure;
use crate::asm::Address;
use crate::asm::Condition;
use crate::asm::FprSet;
use crate::asm::Gpr;
use crate::asm::GprSet;
use crate::asm::Label;
use crate::asm::PortableMacroAssembler;
use crate::asm::Scale;
use crate::bytecode::RESERVED_REGISTER_COUNT;
use crate::bytecode::THIS_VALUE_REGISTER;
use crate::code::JitStatus;
use crate::ir::BlockId;
use crate::ir::Graph;
use crate::ir::value;
use crate::regalloc::Allocation;
use crate::snapshot::COMPRESSED_POINTER_SHIFT;
use crate::snapshot::CellId;
use crate::snapshot::RuntimeInfo;
use crate::snapshot::Snapshot;
use ak::ZoneMap;
use ak::ZoneSet;

/// Where the stub keeps its inputs and what must survive calls into the
/// runtime and the callee, relative to the stack pointer.
mod local {
    pub const CALLEE: u32 = 0;
    pub const THIS: u32 = 8;
    pub const ARGUMENT_COUNT: u32 = 16;
    pub const PCS: u32 = 24;
    pub const DST: u32 = 32;
    /// The callee's environment from `prepare_call_environment`, or 0.
    pub const ENVIRONMENT: u32 = 40;
    /// The callee's `this` from `prepare_call_environment`.
    pub const PREPARED_THIS: u32 = 48;
    /// The callee's executable while its frame is built, then the frame.
    pub const EXECUTABLE: u32 = 56;
    /// The new target of a construct.
    pub const NEW_TARGET: u32 = 64;
    /// Boxed packed Array value for the `Function.prototype.apply` fast path, or 0 for caller stack arguments.
    pub const ARG_SOURCE: u32 = 72;
    /// Caller's `vm.current_realm` saved across cross-realm ECMAScript calls.
    pub const CALLER_REALM: u32 = 80;
    pub const SIZE: u32 = 96;
}

/// Returned in `M::RETURN_GPRS[0]` by accessor stubs when they decline before
/// doing anything observable (`SlowPathControl` values are `-1` or `>= 0`).
pub(super) const ACCESSOR_STUB_DECLINED: i64 = -2;

/// Which stub `generate_stub()` generates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StubKind {
    Call,
    Construct,
    CallForwarding,
    AccessorGet,
    PutByValueSetter,
}

/// Generates the call stub, or nothing if the runtime lacks a helper it
/// needs (then JIT code makes those calls through `RuntimeInfo::jit_call`).
pub fn generate_call_stub<M: PortableMacroAssembler>(
    runtime: &RuntimeInfo,
) -> Result<Option<Column<u8>>, CompileFailure> {
    let layout = runtime.dynamic_calls;
    if layout.object_flag_is_ecmascript_function == 0 || runtime.jit_call == 0 || runtime.finish_direct_call == 0 {
        return Ok(None);
    }
    generate_stub::<M>(runtime, StubKind::Call).map(Some)
}

/// Generates the call forwarding stub for `Op::CallForwardingArguments`
/// (`f.apply(thisArg, arguments)`), copying passed arguments directly from
/// the caller frame into the callee frame.
pub fn generate_call_forwarding_stub<M: PortableMacroAssembler>(
    runtime: &RuntimeInfo,
) -> Result<Option<Column<u8>>, CompileFailure> {
    let layout = runtime.dynamic_calls;
    if layout.object_flag_is_ecmascript_function == 0
        || runtime.call_forwarding_arguments == 0
        || runtime.finish_direct_call == 0
    {
        return Ok(None);
    }
    generate_stub::<M>(runtime, StubKind::CallForwarding).map(Some)
}

/// Generates the accessor get stub for `CallSlowPath(GetById)`, probing the
/// site cache and VM keyed lookup cache for an accessor and calling its
/// getter directly, returning `ACCESSOR_STUB_DECLINED` on miss or fallback.
pub fn generate_get_by_id_accessor_stub<M: PortableMacroAssembler>(
    runtime: &RuntimeInfo,
) -> Result<Option<Column<u8>>, CompileFailure> {
    let layout = runtime.dynamic_calls;
    if layout.object_flag_is_ecmascript_function == 0
        || runtime.finish_direct_call == 0
        || runtime.layout.property_lookup_cache_entry_size == 0
    {
        return Ok(None);
    }
    generate_stub::<M>(runtime, StubKind::AccessorGet).map(Some)
}

/// Generates the prototype setter stub for `CallSlowPath(PutByValue)`,
/// probing the VM keyed store cache read-only for a
/// `ChangePropertyInPrototypeChain` accessor and calling its setter directly,
/// returning `ACCESSOR_STUB_DECLINED` on miss or fallback.
pub fn generate_put_by_value_setter_stub<M: PortableMacroAssembler>(
    runtime: &RuntimeInfo,
) -> Result<Option<Column<u8>>, CompileFailure> {
    let layout = runtime.dynamic_calls;
    if layout.object_flag_is_ecmascript_function == 0
        || runtime.finish_direct_call == 0
        || runtime.layout.keyed_store_cache_entries == 0
        || runtime.layout.keyed_lookup_cache_prototype == 0
        || runtime.layout.keyed_lookup_cache_prototype_chain_validity == 0
        || runtime.layout.property_lookup_cache_entry_type_change_property_in_prototype_chain == 0
    {
        return Ok(None);
    }
    generate_stub::<M>(runtime, StubKind::PutByValueSetter).map(Some)
}

/// Generates the construct stub (see `Codegen::emit_call_stub()`), or
/// nothing if the runtime lacks a helper it needs (then JIT code makes
/// constructs through `RuntimeInfo::jit_call`).
pub fn generate_construct_stub<M: PortableMacroAssembler>(
    runtime: &RuntimeInfo,
) -> Result<Option<Column<u8>>, CompileFailure> {
    let layout = runtime.dynamic_calls;
    if layout.object_flag_is_ecmascript_function == 0
        || layout.jit_entry_table == 0
        || runtime.construct_this == 0
        || runtime.throw_call_stack_size_exceeded == 0
        || runtime.prepare_construct == 0
        || runtime.construct_result_error == 0
        || runtime.finish_direct_construct == 0
    {
        return Ok(None);
    }
    generate_stub::<M>(runtime, StubKind::Construct).map(Some)
}

/// Generates the code of the call stub, construct stub, or call forwarding stub.
fn generate_stub<M: PortableMacroAssembler>(
    runtime: &RuntimeInfo,
    kind: StubKind,
) -> Result<Column<u8>, CompileFailure> {
    // NB: A stub has no graph to build: the graph stays empty. The zone holds the lists of the code generator.
    let zone = ak::Zone::new(STUB_ZONE_LIMIT, c"JIT stub");
    let graph = Graph::new_in(&zone);
    let allocation = Allocation::new_in(&zone);
    // NB: A stub is code for no executable. It reads no list of the runtime info.
    let snapshot = Snapshot::new();
    let (_, pinned) = target_registers::<M>();
    let mut code = Column::new(STUB_CODE_LIMIT, c"JIT stub code");
    let mut masm = M::new(&mut code, &zone);
    let exit_stub = masm.new_label();
    let resume = masm.new_label();
    let exit_interpreter = masm.new_label();
    {
        let mut codegen = Codegen::<M> {
            masm,
            graph: &graph,
            allocation: &allocation,
            snapshot: &snapshot,
            work: &zone,
            decoded_instructions: ZoneMap::new_in(&zone),
            runtime,
            // NB: Stubs are shared by all code, and embed no cells.
            cell_encoding: super::cell_encoding(runtime),
            embedded: zone.alloc(crate::relocation::EmbeddedCells::new_in(
                &[],
                super::cell_encoding(runtime),
                &zone,
            )),
            stress: crate::snapshot::StressOptions::default(),
            pinned,
            frame: M::frame(GprSet::EMPTY, FprSet::EMPTY, local::SIZE),
            locals: Locals::default(),
            gpr_dump_count: 0,
            fpr_dump_count: 0,
            block_labels: &[],
            landing_blocks: &[],
            exit_stub,
            resume,
            entry_resume: resume,
            entry_overflow: resume,
            exit_interpreter,
            sites: ZoneVec::new_in(&zone),
            exit_site_stubs: ZoneVec::new_in(&zone),
            annotations: ZoneVec::new_in(&zone),
            block: BlockId(0),
            deferred_cache_probes: ZoneVec::new_in(&zone),
            deferred_vm_cache_probes: ZoneVec::new_in(&zone),
            leave_arguments: ZoneVec::new_in(&zone),
            leave_stubs: ZoneVec::new_in(&zone),
            leave_frame: None,
            leave_tail: exit_stub,
            frame_initialization_stub: None,
            slow_path_values: None,
            input_of_operand: ZoneMap::new_in(&zone),
            slow_path_ssa_outputs: ZoneVec::new_in(&zone),
            spare_operands: ZoneVec::new_in(&zone),
            spare_outputs: ZoneVec::new_in(&zone),
            spare_call_operands: ZoneVec::new_in(&zone),
            spare_fields: ZoneVec::new_in(&zone),
            spare_frame_values: ZoneVec::new_in(&zone),
            spare_bytes: ZoneVec::new_in(&zone),
            invalidation_points: ZoneVec::new_in(&zone),
            fused_probe_failure: None,
            deferred_allocations: ZoneVec::new_in(&zone),
            deferred_storage_growths: ZoneVec::new_in(&zone),
            absorbed_initializations: ZoneVec::new_in(&zone),
            inline_initializations: ZoneSet::new_in(&zone),
            // NB: The stub lowers no nodes.
            verify: false,
            emitting: None,
            requested_temps: &[],
            undeclared_saves_area_uses: core::cell::RefCell::new(ZoneVec::new_in(&zone)),
        };
        codegen.emit_call_stub(kind)?;
        let mut cells = crate::asm::NoCells::default();
        codegen.masm.finish(&mut cells).map_err(super::assembly_failure)?;
        if cells.held {
            return Err(CompileFailure::CodeGeneration);
        }
    }
    Ok(code)
}

/// The registers `emit_stub_call_environment()` works with: the callee's
/// shared data (which it clobbers), the callee and its call metadata, and two
/// it clobbers.
struct StubCallEnvironmentRegisters {
    shared_data: Gpr,
    function: Gpr,
    metadata: Gpr,
    template: Gpr,
    cell: Gpr,
}

impl<M: PortableMacroAssembler> Codegen<'_, '_, M> {
    /// Allocates the function environment of a call of the callee inline from
    /// its shared data's call environment template (see
    /// `DynamicCallLayout::shared_data_call_environment_template`), and keeps
    /// it and the `this` value of the call in the stub's locals. Branches to
    /// `from_runtime` without a template, without room on the local free list
    /// or before the next collection, and for `this` values the call would
    /// convert. Clobbers the scratch register.
    fn emit_stub_call_environment(&mut self, registers: StubCallEnvironmentRegisters, from_runtime: Label) {
        let StubCallEnvironmentRegisters {
            shared_data,
            function,
            metadata,
            template,
            cell,
        } = registers;
        let layout = self.runtime.dynamic_calls;
        let heap_info = self.runtime.object_allocation;
        let scratch = self.pinned.scratch;
        let at = |base: Gpr, offset: u32| Address::new(base, offset as i32);
        let environment_local = self.stub_local(local::ENVIRONMENT);
        let prepared_this = self.stub_local(local::PREPARED_THIS);
        let this_local = self.stub_local(local::THIS);

        self.emit_load_compressed_cell_or_branch(
            template,
            &at(shared_data, layout.shared_data_call_environment_template),
            from_runtime,
        );

        // The `this` value: as is for strict callees and objects only for
        // sloppy ones, like the runtime binds it then, or none.
        let binds_this = self.masm.new_label();
        let bound = self.masm.new_label();
        self.masm
            .load64(scratch, &at(template, layout.call_environment_template_binds_this));
        self.masm
            .branch_test64(Condition::NonZero, scratch, u64::MAX, binds_this);
        self.masm.store_imm64(&prepared_this, value::EMPTY);
        self.masm.jump(bound);
        self.masm.bind(binds_this);
        self.masm.load64(scratch, &this_local);
        let strict = self.masm.new_label();
        self.masm
            .branch_test64(Condition::NonZero, metadata, layout.metadata_strict, strict);
        self.branch_on_tag(Condition::NotEqual, scratch, value::OBJECT_TAG, cell, from_runtime);
        self.masm.bind(strict);
        self.masm.store64(&prepared_this, scratch);
        self.masm.bind(bound);

        // Room before the next collection, and a cell from the local free list.
        let heap = shared_data;
        self.masm.move_imm64(heap, heap_info.heap);
        self.masm
            .load64(cell, &at(template, layout.call_environment_template_cell_size));
        self.masm.load64(scratch, &at(heap, heap_info.heap_threshold_offset));
        self.masm.sub64(scratch, scratch, cell);
        self.masm.branch64_memory(
            Condition::GreaterThan,
            &at(heap, heap_info.heap_allocated_bytes_offset),
            scratch,
            from_runtime,
        );
        self.masm
            .load64(scratch, &at(template, layout.call_environment_template_size_class));
        self.masm
            .and32_imm(scratch, scratch, layout.function_environment_size_class_mask);
        self.masm.move_imm64(cell, layout.function_environment_free_lists);
        self.masm.load64(
            scratch,
            &Address {
                base: cell,
                index: Some((scratch, Scale::Eight)),
                displacement: 0,
            },
        );
        self.emit_pop_free_list_of(scratch, cell, heap, from_runtime);

        // The environment, from the template's words and the call.
        for index in 0..layout.declarative_environment_words {
            self.masm.load64(
                scratch,
                &at(template, layout.call_environment_template_words + 8 * index),
            );
            self.masm.store64(&at(cell, 8 * index), scratch);
        }
        let outside_the_cell = self.masm.new_label();
        self.masm.load64(
            scratch,
            &at(template, layout.call_environment_template_binding_values_offset),
        );
        self.masm
            .branch_test64(Condition::Zero, scratch, u64::MAX, outside_the_cell);
        self.masm.add64(scratch, scratch, cell);
        self.masm
            .store64(&at(cell, layout.function_environment_binding_values), scratch);
        self.masm.bind(outside_the_cell);
        // NB: The outer environment is the callee's environment, which stays
        //     compressed on the way.
        self.masm.load32(
            scratch,
            &at(function, self.runtime.offsets.ecmascript_function_environment),
        );
        self.masm.store32(&at(cell, layout.function_environment_outer), scratch);
        // The rest of a function environment, which only some callees need.
        let no_function_fields = self.masm.new_label();
        self.masm.load64(
            scratch,
            &at(template, layout.call_environment_template_has_function_fields),
        );
        self.masm
            .branch_test64(Condition::Zero, scratch, u64::MAX, no_function_fields);
        for index in layout.declarative_environment_words..layout.function_environment_words {
            self.masm.load64(
                scratch,
                &at(template, layout.call_environment_template_words + 8 * index),
            );
            self.masm.store64(&at(cell, 8 * index), scratch);
        }
        self.masm.shr64_imm(scratch, function, COMPRESSED_POINTER_SHIFT);
        self.masm
            .store32(&at(cell, layout.function_environment_function_object), scratch);
        let no_this = self.masm.new_label();
        self.masm
            .load64(scratch, &at(template, layout.call_environment_template_binds_this));
        self.masm.branch_test64(Condition::Zero, scratch, u64::MAX, no_this);
        self.masm.load64(scratch, &prepared_this);
        self.masm
            .store64(&at(cell, layout.function_environment_this_value), scratch);
        self.masm.bind(no_this);
        self.masm.bind(no_function_fields);
        self.masm.store64(&environment_local, cell);

        // Count the cell like the heap does.
        self.masm
            .load64(template, &at(template, layout.call_environment_template_cell_size));
        self.masm.move_imm64(heap, heap_info.heap);
        for offset in [
            heap_info.heap_allocated_bytes_offset,
            heap_info.heap_total_allocated_bytes_offset,
        ] {
            self.masm.load64(scratch, &at(heap, offset));
            self.masm.add64(scratch, scratch, template);
            self.masm.store64(&at(heap, offset), scratch);
        }
    }

    fn stub_local(&self, offset: u32) -> Address {
        self.frame.local(M::STACK_POINTER, offset)
    }

    /// The call stub, construct stub, or call forwarding stub.
    fn emit_call_stub(&mut self, kind: StubKind) -> Result<(), CompileFailure> {
        let construct = kind == StubKind::Construct;
        let runtime = self.runtime;
        let offsets = runtime.offsets;
        let layout = runtime.dynamic_calls;
        let registers = M::ARGUMENT_GPRS;
        let [callee_value, this_argument, argument_count, pcs, dst] =
            [registers[0], registers[1], registers[2], registers[3], registers[4]];
        let executable_register = registers[0];
        let callee_frame = registers[1];
        let temp = registers[2];
        let other = registers[3];
        let function_register = registers[4];
        let receiver = registers[5];
        let scratch = self.pinned.scratch;
        let vm = self.pinned.vm;
        let frame = self.pinned.frame;
        let vm_field = |offset: u32| Address::new(vm, offset as i32);
        let slot = |index: u32| offsets.execution_context_slots + 8 * index;

        let slow = self.masm.new_label();
        let leave = self.masm.new_label();

        let machine_frame = self.frame;
        self.masm.emit_prologue(&machine_frame);
        match kind {
            StubKind::Call | StubKind::Construct | StubKind::CallForwarding => {
                for (offset, register) in [
                    (local::CALLEE, callee_value),
                    (local::THIS, this_argument),
                    (local::ARGUMENT_COUNT, argument_count),
                    (local::PCS, pcs),
                    (local::DST, dst),
                ] {
                    let address = self.stub_local(offset);
                    self.masm.store64(&address, register);
                }
                if kind == StubKind::Call {
                    let arg_source = self.stub_local(local::ARG_SOURCE);
                    self.masm.store_imm64(&arg_source, 0);
                }
            }
            StubKind::AccessorGet => {
                for (offset, register) in [
                    (local::THIS, registers[0]),
                    (local::ARG_SOURCE, registers[2]),
                    (local::PCS, registers[3]),
                    (local::DST, registers[4]),
                ] {
                    let address = self.stub_local(offset);
                    self.masm.store64(&address, register);
                }
                let arg_count = self.stub_local(local::ARGUMENT_COUNT);
                self.masm.store_imm64(&arg_count, 0);
            }
            StubKind::PutByValueSetter => {
                for (offset, register) in [
                    (local::THIS, registers[0]),
                    (local::ARG_SOURCE, registers[2]),
                    (local::PCS, registers[3]),
                    (local::DST, registers[4]),
                ] {
                    let address = self.stub_local(offset);
                    self.masm.store64(&address, register);
                }
                let arg_count = self.stub_local(local::ARGUMENT_COUNT);
                self.masm.store_imm64(&arg_count, 1);
            }
        }
        if !construct {
            self.masm.load64(scratch, &vm_field(offsets.vm_current_realm));
            let caller_realm_local = self.stub_local(local::CALLER_REALM);
            self.masm.store64(&caller_realm_local, scratch);
        }

        // Stack traces show the caller at its call.
        self.masm.store32(
            &Address::new(frame, offsets.execution_context_program_counter as i32),
            pcs,
        );

        match kind {
            StubKind::Call | StubKind::Construct | StubKind::CallForwarding => {
                // The callee must be an object.
                self.masm.move64(temp, callee_value);
                self.branch_on_tag(Condition::NotEqual, temp, value::OBJECT_TAG, scratch, slow);
                self.emit_unbox_cell(function_register, temp);
                self.masm
                    .load16(temp, &Address::new(function_register, offsets.object_flags as i32));
            }
            StubKind::AccessorGet => {
                self.emit_accessor_get_probe(slow, leave);
            }
            StubKind::PutByValueSetter => {
                self.emit_put_by_value_setter_probe(slow);
            }
        }

        // Raw native functions are called in their lightweight frame, except
        // `Function.prototype.apply` with nullish or packed Array argArray,
        // which unwraps straight to its target function with one argument copy.
        let ecmascript_function = self.masm.new_label();
        let native_function_no_apply = self.masm.new_label();
        let has_apply_fast_path = kind == StubKind::Call
            && layout.object_flag_is_raw_native_function != 0
            && runtime.raw_native_exception != 0
            && layout.apply_function_address != 0
            && runtime.layout.object_indexed_elements != 0;
        let apply_unwrap = has_apply_fast_path.then(|| self.masm.new_label());
        self.masm.branch_test32(
            Condition::NonZero,
            temp,
            u32::from(layout.object_flag_is_ecmascript_function),
            ecmascript_function,
        );
        if !construct && layout.object_flag_is_raw_native_function != 0 && runtime.raw_native_exception != 0 {
            self.masm.branch_test32(
                Condition::Zero,
                temp,
                u32::from(layout.object_flag_is_raw_native_function),
                slow,
            );
            if let Some(apply_unwrap) = apply_unwrap {
                self.masm.load32(
                    temp,
                    &Address::new(function_register, layout.raw_native_function_index as i32),
                );
                self.masm.and32_imm(temp, temp, layout.native_function_table_index_mask);
                self.masm.shl64_imm(
                    temp,
                    temp,
                    u8::try_from(layout.native_function_table_entry_size.trailing_zeros()).unwrap_or(0),
                );
                self.masm.load64(scratch, &vm_field(layout.vm_native_function_table));
                self.masm.load64(
                    scratch,
                    &Address {
                        base: scratch,
                        index: Some((temp, Scale::One)),
                        displacement: layout.native_function_table_entry_function as i32,
                    },
                );
                self.masm.move_imm64(temp, layout.apply_function_address);
                self.masm.branch64(Condition::Equal, scratch, temp, apply_unwrap);
            }
            self.masm.bind(native_function_no_apply);
            self.emit_stub_native_call(kind, slow, leave)?;
            if let Some(apply_unwrap) = apply_unwrap {
                self.masm.bind(apply_unwrap);
                let this_local = self.stub_local(local::THIS);
                let argument_count_local = self.stub_local(local::ARGUMENT_COUNT);
                let callee_local = self.stub_local(local::CALLEE);
                let arg_source_local = self.stub_local(local::ARG_SOURCE);
                let rt_layout = runtime.layout;

                // 1. target_fn = this_value (must be an object).
                self.masm.load64(function_register, &this_local);
                self.branch_on_tag(Condition::NotEqual, function_register, value::OBJECT_TAG, scratch, slow);

                // 2. target_this = arg0 (or undefined), arg_array = arg1 (or undefined).
                let apply_empty_args = self.masm.new_label();
                let apply_ready = self.masm.new_label();
                self.masm.move_imm64(other, value::UNDEFINED);
                self.masm.load64(temp, &argument_count_local);
                self.masm.branch32_imm(Condition::Equal, temp, 0, apply_empty_args);
                self.masm.load64(
                    other,
                    &Address::new(M::STACK_POINTER, self.frame.caller_stack_offset as i32),
                );
                self.masm.branch32_imm(Condition::Below, temp, 2, apply_empty_args);
                self.masm.load64(
                    receiver,
                    &Address::new(M::STACK_POINTER, self.frame.caller_stack_offset as i32 + 8),
                );
                // Nullish argArray -> 0 arguments.
                self.masm.shr64_imm(scratch, receiver, 48);
                self.masm.or32_imm(scratch, scratch, 1);
                self.masm
                    .branch32_imm(Condition::Equal, scratch, i32::from(value::NULL_TAG), apply_empty_args);
                // Otherwise require a non-interfering packed Array with length <= 64 and length <= capacity.
                self.branch_on_tag(Condition::NotEqual, receiver, value::OBJECT_TAG, scratch, slow);
                self.emit_unbox_cell(temp, receiver);
                let required = u32::from(rt_layout.object_flag_has_magical_length);
                let forbidden = u32::from(rt_layout.object_flag_may_interfere);
                self.masm
                    .load16(scratch, &Address::new(temp, offsets.object_flags as i32));
                self.masm.and32_imm(scratch, scratch, required | forbidden);
                self.masm
                    .branch32_imm(Condition::NotEqual, scratch, required as i32, slow);
                self.masm.load32(
                    callee_frame,
                    &Address::new(temp, rt_layout.object_indexed_array_like_size as i32),
                );
                self.masm
                    .branch32_imm(Condition::Equal, callee_frame, 0, apply_empty_args);
                self.masm.branch32_imm(Condition::Above, callee_frame, 64, slow);
                self.masm.load8(
                    scratch,
                    &Address::new(temp, rt_layout.object_indexed_storage_kind as i32),
                );
                self.masm.branch32_imm(
                    Condition::NotEqual,
                    scratch,
                    i32::from(rt_layout.indexed_storage_kind_packed),
                    slow,
                );
                self.masm.load64(
                    temp,
                    &Address::new(temp, rt_layout.object_indexed_elements as i32),
                );
                self.masm.branch_test64(Condition::Zero, temp, u64::MAX, slow);
                self.masm
                    .load32(scratch, &Address::new(temp, rt_layout.indexed_elements_capacity));
                self.masm
                    .branch32(Condition::Above, callee_frame, scratch, slow);
                self.masm.store64(&argument_count_local, callee_frame);
                self.masm.store64(&arg_source_local, receiver);
                self.masm.jump(apply_ready);

                self.masm.bind(apply_empty_args);
                self.masm.store_imm64(&argument_count_local, 0);
                self.masm.store_imm64(&arg_source_local, 0);

                self.masm.bind(apply_ready);
                self.masm.store64(&callee_local, function_register);
                self.masm.store64(&this_local, other);
                self.emit_unbox_cell(function_register, function_register);
                self.masm
                    .load16(temp, &Address::new(function_register, offsets.object_flags as i32));
                self.masm.branch_test32(
                    Condition::NonZero,
                    temp,
                    u32::from(layout.object_flag_is_ecmascript_function),
                    ecmascript_function,
                );
                self.masm.branch_test32(
                    Condition::NonZero,
                    temp,
                    u32::from(layout.object_flag_is_raw_native_function),
                    native_function_no_apply,
                );
                self.masm.jump(slow);
            }
        } else {
            self.masm.jump(slow);
        }

        // An ECMAScript function the interpreter's call fast path calls in an
        // inline frame. For calls (not constructs), cross-realm callees switch
        // `vm.current_realm` to the callee's realm and restore it on return,
        // slow-path fallback, or interpreter finish.
        self.masm.bind(ecmascript_function);
        if construct {
            self.emit_load_compressed_cell(temp, &Address::new(function_register, offsets.object_shape as i32));
            self.emit_load_compressed_cell(temp, &Address::new(temp, layout.shape_realm as i32));
            self.masm
                .branch64_memory(Condition::NotEqual, &vm_field(offsets.vm_current_realm), temp, slow);
        }
        // NB: The executable is decoded once it is known not to be null,
        //     which it stays: the runtime compiles the function again if a
        //     collection in it flushed the executable.
        let load_function_data = |codegen: &mut Self| {
            codegen.emit_load_compressed_cell(
                other,
                &Address::new(function_register, layout.ecmascript_function_shared_data as i32),
            );
            codegen.masm.load32(
                executable_register,
                &Address::new(other, layout.shared_data_executable as i32),
            );
            codegen.masm.load64(
                receiver,
                &Address::new(other, layout.shared_data_asm_call_metadata as i32),
            );
        };
        load_function_data(self);
        if !construct {
            self.masm
                .branch_test64(Condition::Zero, receiver, layout.metadata_can_inline_call, slow);
        }
        self.masm
            .branch_test32(Condition::Zero, executable_register, u32::MAX, slow);
        self.emit_decompress_cell(executable_register);
        if !construct {
            self.emit_load_compressed_cell(temp, &Address::new(function_register, offsets.object_shape as i32));
            self.emit_load_compressed_cell(temp, &Address::new(temp, layout.shape_realm as i32));
            self.masm.store64(&vm_field(offsets.vm_current_realm), temp);
        }
        let environment_local = self.stub_local(local::ENVIRONMENT);

        // A construct gets its this value and new target from the runtime.
        // NB: From here on, the construct cannot take the slow path anymore.
        let stack_overflow = self.masm.new_label();
        let failed_after_this = if construct { stack_overflow } else { slow };
        if construct {
            self.emit_native_stack_check(slow);
            self.masm.branch_test64(
                Condition::NonZero,
                receiver,
                layout.metadata_needs_environment_or_this_value_resolution,
                slow,
            );
            let this_local = self.stub_local(local::THIS);
            let pcs_local = self.stub_local(local::PCS);
            self.masm.load64(registers[2], &this_local);
            self.masm.move64(registers[1], function_register);
            self.masm.load32(registers[3], &pcs_local);
            self.masm.move64(registers[0], vm);
            self.masm.call_absolute(runtime.construct_this);
            let [this, new_target] = [M::RETURN_GPRS[0], M::RETURN_GPRS[1]];
            self.masm.branch64_imm(Condition::Equal, this, 0, slow);
            let constructed = self.masm.new_label();
            self.masm.branch64_imm(Condition::NotEqual, this, 1, constructed);
            // NB: The runtime threw, and the second register has the control word.
            self.masm.move64(M::RETURN_GPRS[0], new_target);
            self.masm.jump(leave);
            self.masm.bind(constructed);
            let prepared_this = self.stub_local(local::PREPARED_THIS);
            self.masm.store64(&prepared_this, this);
            let new_target_local = self.stub_local(local::NEW_TARGET);
            self.masm.store64(&new_target_local, new_target);
            // The runtime may have collected garbage: load the function and
            // what comes from it again.
            let callee_local = self.stub_local(local::CALLEE);
            self.masm.load64(temp, &callee_local);
            self.emit_unbox_cell(function_register, temp);
            load_function_data(self);
            self.emit_decompress_cell(executable_register);
        } else {
            // Callees that need a function environment or the resolution of
            // their `this` value get both from the runtime before their frame
            // is built.
            self.masm.store_imm64(&environment_local, 0);
            let resume = self.masm.new_label();
            if layout.prepare_call_environment != 0 {
                self.masm.branch_test64(
                    Condition::Zero,
                    receiver,
                    layout.metadata_needs_environment_or_this_value_resolution,
                    resume,
                );
                if layout.shared_data_call_environment_template != 0 && runtime.object_allocation.heap != 0 {
                    let from_runtime = self.masm.new_label();
                    self.emit_stub_call_environment(
                        StubCallEnvironmentRegisters {
                            shared_data: other,
                            function: function_register,
                            metadata: receiver,
                            template: callee_frame,
                            cell: temp,
                        },
                        from_runtime,
                    );
                    self.masm.jump(resume);
                    self.masm.bind(from_runtime);
                }
                let this_local = self.stub_local(local::THIS);
                self.masm.load64(registers[2], &this_local);
                self.masm.move64(registers[1], function_register);
                self.masm.move64(registers[0], vm);
                self.masm.call_absolute(layout.prepare_call_environment);
                let [environment, this] = [M::RETURN_GPRS[0], M::RETURN_GPRS[1]];
                self.masm.branch64_imm(Condition::Equal, environment, 0, slow);
                self.masm.store64(&environment_local, environment);
                let prepared_this = self.stub_local(local::PREPARED_THIS);
                self.masm.store64(&prepared_this, this);
                // The runtime may have collected garbage: load the function and
                // what comes from it again.
                let callee_local = self.stub_local(local::CALLEE);
                self.masm.load64(temp, &callee_local);
                self.emit_unbox_cell(function_register, temp);
                load_function_data(self);
                self.emit_decompress_cell(executable_register);
            } else {
                self.masm.branch_test64(
                    Condition::NonZero,
                    receiver,
                    layout.metadata_needs_environment_or_this_value_resolution,
                    slow,
                );
            }
            self.masm.bind(resume);
        }

        // The number of argument slots: the formal parameter count (in the
        // low bits of the metadata), or more if more were passed.
        let formal_count = other;
        let argument_count_local = self.stub_local(local::ARGUMENT_COUNT);
        self.masm.move32(formal_count, receiver);
        self.masm.load64(temp, &argument_count_local);
        let enough = self.masm.new_label();
        self.masm.branch32(Condition::AboveOrEqual, formal_count, temp, enough);
        self.masm.move32(formal_count, temp);
        self.masm.bind(enough);

        // A construct binds the `this` value the runtime made. A call binds
        // `this` like the interpreter's call fast path: as is for strict
        // callees, objects only for sloppy ones (the generic call boxes
        // primitives and resolves null and undefined), and as the runtime
        // prepared it for callees with an environment.
        let prepared_this = self.stub_local(local::PREPARED_THIS);
        if construct {
            self.masm.load64(receiver, &prepared_this);
        } else {
            let have_receiver = self.masm.new_label();
            let own_environment = self.masm.new_label();
            self.masm
                .branch64_memory_imm(Condition::Equal, &environment_local, 0, own_environment);
            self.masm.load64(receiver, &prepared_this);
            self.masm.jump(have_receiver);
            self.masm.bind(own_environment);
            let uses_this = self.masm.new_label();
            self.masm
                .branch_test64(Condition::NonZero, receiver, layout.metadata_uses_this, uses_this);
            self.masm.move_imm64(receiver, value::EMPTY);
            self.masm.jump(have_receiver);
            self.masm.bind(uses_this);
            let strict = self.masm.new_label();
            self.masm
                .branch_test64(Condition::NonZero, receiver, layout.metadata_strict, strict);
            let this_local = self.stub_local(local::THIS);
            self.masm.load64(receiver, &this_local);
            self.branch_on_tag(Condition::NotEqual, receiver, value::OBJECT_TAG, temp, slow);
            self.masm.jump(have_receiver);
            self.masm.bind(strict);
            self.masm.load64(receiver, &this_local);
            self.masm.bind(have_receiver);
            self.emit_native_stack_check(slow);
        }

        // The frame's slot count, and its allocation on the interpreter stack.
        self.masm.load32(
            temp,
            &Address::new(
                executable_register,
                layout.executable_registers_and_locals_and_constants_count as i32,
            ),
        );
        self.masm
            .branch_add32_overflow(temp, temp, formal_count, failed_after_this);
        self.masm
            .load64(callee_frame, &vm_field(offsets.vm_interpreter_stack_top));
        self.masm.load_effective_address(
            scratch,
            &Address {
                base: callee_frame,
                index: Some((temp, Scale::Eight)),
                displacement: offsets.execution_context_slots as i32,
            },
        );
        self.masm.branch64_memory(
            Condition::Below,
            &vm_field(offsets.vm_interpreter_stack_limit),
            scratch,
            failed_after_this,
        );
        self.masm.store64(&vm_field(offsets.vm_interpreter_stack_top), scratch);

        // Fill in the frame like the interpreter's call fast path.
        let field = |offset: u32| Address::new(callee_frame, offset as i32);
        self.emit_function_fields(callee_frame, function_register, scratch);
        if !construct {
            let prepared_environment = self.masm.new_label();
            self.masm.load64(scratch, &environment_local);
            self.masm
                .branch64_imm(Condition::Equal, scratch, 0, prepared_environment);
            self.masm
                .store64(&field(offsets.execution_context_lexical_environment), scratch);
            self.masm
                .store64(&field(offsets.execution_context_variable_environment), scratch);
            self.masm.bind(prepared_environment);
        }
        self.masm
            .store64(&field(offsets.execution_context_executable), executable_register);
        self.masm.store32(&field(offsets.execution_context_slot_count), temp);
        self.masm
            .store32(&field(offsets.execution_context_argument_count), formal_count);
        self.masm
            .store64(&field(offsets.execution_context_this_value), receiver);
        self.masm.store64(&field(slot(THIS_VALUE_REGISTER)), receiver);
        let mut constants = self.spare_fields.take();
        constants.clear();
        constants.extend(self.emit_stub_call_site_fields(callee_frame, scratch, true));
        for register in (0..RESERVED_REGISTER_COUNT).filter(|register| *register != THIS_VALUE_REGISTER) {
            constants.push((slot(register), 8, Field::Value(value::EMPTY)));
        }
        self.emit_constant_fields_with_vector(callee_frame, scratch, &mut constants, Some(M::ARGUMENT_FPRS[0]), &[])?;
        self.spare_fields = constants;
        if construct {
            // The frame of a constructor has its new target, and that of a
            // derived constructor its this value, empty until super() binds
            // it (see `ExecutionContext::caller_is_derived_construct`).
            let new_target_local = self.stub_local(local::NEW_TARGET);
            self.masm.load64(scratch, &new_target_local);
            self.masm.store64(&field(offsets.execution_context_new_target), scratch);
            let base = self.masm.new_label();
            self.masm
                .branch64_imm(Condition::NotEqual, receiver, value::EMPTY as i64, base);
            self.masm
                .store_imm8(&field(offsets.execution_context_caller_is_construct), 1);
            self.masm
                .store_imm8(&field(offsets.execution_context_caller_is_derived_construct), 1);
            self.masm.bind(base);
        }

        // The arguments start after the registers, locals and constants: slot
        // count minus formal count. Copy the passed ones and fill the formal
        // parameters not passed with undefined.
        self.masm.sub32(temp, temp, formal_count);
        self.masm.load_effective_address(
            scratch,
            &Address {
                base: callee_frame,
                index: Some((temp, Scale::Eight)),
                displacement: offsets.execution_context_slots as i32,
            },
        );
        let executable_local = self.stub_local(local::EXECUTABLE);
        self.masm.store64(&executable_local, executable_register);
        self.masm.load64(temp, &argument_count_local);
        self.emit_stub_copy_arguments(kind, scratch, temp, executable_register, receiver, function_register);
        // The formal parameters not passed, up to formal_count.
        self.masm.sub32(formal_count, formal_count, temp);
        let fill = self.masm.new_label();
        let filled = self.masm.new_label();
        self.masm.branch32_imm(Condition::Equal, formal_count, 0, filled);
        self.masm.move_imm64(receiver, value::UNDEFINED);
        self.masm.bind(fill);
        self.masm.store64(
            &Address {
                base: scratch,
                index: Some((temp, Scale::Eight)),
                displacement: 0,
            },
            receiver,
        );
        self.masm.add32_imm(temp, temp, 1);
        self.masm.sub32_imm(formal_count, formal_count, 1);
        self.masm.branch32_imm(Condition::NotEqual, formal_count, 0, fill);
        self.masm.bind(filled);
        self.masm.load64(other, &executable_local);

        // Enter the callee through its executable's entry in the JIT entry
        // table, with its frame (in callee_frame) as the second argument: its
        // JIT code, which makes the frame the running one when something else
        // may see it (see `Op::PublishFrame`), or else (also while a debugger
        // is attached) an entry that has the interpreter run the frame. The
        // frame's address replaces the executable in its local. A slot that
        // does not belong to the executable enters through slot 0, which has
        // the interpreter run the frame.
        self.masm.store64(&executable_local, callee_frame);
        if construct {
            self.emit_construct_stub_tail(slow, stack_overflow, leave);
            return Ok(());
        }
        self.emit_stub_enter_frame(other);

        // The callee returned: pop its frame like the interpreter's Return.
        let [value_register, status] = [M::RETURN_GPRS[0], M::RETURN_GPRS[1]];
        let not_returned = self.masm.new_label();
        self.masm
            .branch64_imm(Condition::NotEqual, status, JitStatus::Returned as i64, not_returned);
        // NB: JIT code never returns the empty value.
        self.emit_stub_store_result(kind, value_register, temp);
        let caller_realm_local = self.stub_local(local::CALLER_REALM);
        self.masm.load64(temp, &caller_realm_local);
        self.masm.store64(&vm_field(offsets.vm_current_realm), temp);
        self.masm.load64(temp, &executable_local);
        self.masm.store64(&vm_field(offsets.vm_interpreter_stack_top), temp);
        self.masm
            .store64(&vm_field(offsets.vm_running_execution_context), frame);
        self.masm
            .add32_to_memory_imm(&vm_field(offsets.vm_execution_generation), 1);
        self.emit_stub_continuation();
        self.masm.jump(leave);

        // The callee's frame still runs: let the runtime finish the call.
        self.masm.bind(not_returned);
        self.masm.move64(registers[3], status);
        self.masm.move64(registers[0], vm);
        self.masm.move64(registers[1], frame);
        let pcs_local = self.stub_local(local::PCS);
        self.masm.load32(registers[2], &pcs_local);
        self.masm.call_absolute(runtime.finish_direct_call);
        self.masm.jump(leave);

        // Everything else takes the generic path.
        self.masm.bind(slow);
        self.masm.load64(temp, &caller_realm_local);
        self.masm.store64(&vm_field(offsets.vm_current_realm), temp);
        match kind {
            StubKind::AccessorGet | StubKind::PutByValueSetter => {
                self.masm.move_imm64(M::RETURN_GPRS[0], ACCESSOR_STUB_DECLINED as u64);
            }
            StubKind::CallForwarding => {
                self.masm.move64(registers[0], vm);
                self.masm.move64(registers[1], frame);
                self.masm.load32(registers[2], &pcs_local);
                self.masm.call_absolute(runtime.call_forwarding_arguments);
            }
            StubKind::Call | StubKind::Construct => {
                self.masm.move64(registers[0], vm);
                self.masm.move64(registers[1], frame);
                self.masm.load32(registers[2], &pcs_local);
                self.masm.call_absolute(runtime.jit_call);
            }
        }

        self.masm.bind(leave);
        self.masm.emit_epilogue(&machine_frame);
        self.masm.ret();
        Ok(())
    }

    /// Enters the frame in the second argument register, which runs the
    /// executable in `executable`, through the executable's entry in the JIT
    /// entry table (see `emit_call_stub()`). Clobbers the argument registers
    /// other than the second.
    fn emit_stub_enter_frame(&mut self, executable: Gpr) {
        let layout = self.runtime.dynamic_calls;
        let registers = M::ARGUMENT_GPRS;
        let (entry_slot, table, entry) = (registers[2], registers[0], registers[5]);
        self.masm.load32(
            entry_slot,
            &Address::new(executable, layout.executable_jit_entry_slot as i32),
        );
        self.masm.and32_imm(entry_slot, entry_slot, layout.jit_entry_slot_mask);
        self.masm.move_imm64(table, layout.jit_entry_table);
        let owned = self.masm.new_label();
        self.masm.branch64_memory(
            Condition::Equal,
            &Address {
                base: table,
                index: Some((entry_slot, Scale::Eight)),
                displacement: layout.jit_entry_table_owners as i32,
            },
            executable,
            owned,
        );
        self.masm.move_imm32(entry_slot, 0);
        self.masm.bind(owned);
        self.masm.load64(
            entry,
            &Address {
                base: table,
                index: Some((entry_slot, Scale::Eight)),
                displacement: 0,
            },
        );
        self.masm.move64(registers[0], self.pinned.vm);
        self.masm.call_register(entry);
    }

    /// The rest of the construct stub (see `emit_call_stub()`) once it built
    /// the constructor's frame, which is in the second argument register
    /// (and the frame local), with the constructor's executable in the
    /// fourth: enters the frame, pops it and finishes the construct. Also
    /// emits the paths of `slow` constructs, which the runtime prepares,
    /// and of constructs that ran out of stack after the runtime made their
    /// this value.
    fn emit_construct_stub_tail(&mut self, slow: Label, stack_overflow: Label, leave: Label) {
        let runtime = self.runtime;
        let offsets = runtime.offsets;
        let registers = M::ARGUMENT_GPRS;
        let (vm, frame, scratch) = (self.pinned.vm, self.pinned.frame, self.pinned.scratch);
        let vm_field = |offset: u32| Address::new(vm, offset as i32);
        let [result, status] = [M::RETURN_GPRS[0], M::RETURN_GPRS[1]];
        let (temp, other) = (registers[3], registers[4]);
        let pcs_local = self.stub_local(local::PCS);
        let frame_local = self.stub_local(local::EXECUTABLE);
        let value_local = self.stub_local(local::PREPARED_THIS);
        let enter = self.masm.new_label();

        self.masm.bind(enter);
        self.emit_stub_enter_frame(registers[3]);

        // The constructor returned: pop its frame like the interpreter's
        // Return.
        let not_returned = self.masm.new_label();
        self.masm
            .branch64_imm(Condition::NotEqual, status, JitStatus::Returned as i64, not_returned);
        self.masm.load64(temp, &frame_local);
        // NB: The frame of a derived constructor returns its this value for
        //     undefined (see `ExecutionContext::caller_is_derived_construct`).
        let returned_value = self.masm.new_label();
        self.masm
            .branch64_imm(Condition::NotEqual, result, value::UNDEFINED as i64, returned_value);
        self.masm.load64(other, &value_local);
        self.branch_on_tag(Condition::Equal, other, value::OBJECT_TAG, scratch, returned_value);
        self.masm
            .load64(result, &Address::new(temp, offsets.execution_context_this_value as i32));
        self.masm.bind(returned_value);
        self.masm.store64(&vm_field(offsets.vm_interpreter_stack_top), temp);
        self.masm
            .store64(&vm_field(offsets.vm_running_execution_context), frame);
        self.masm
            .add32_to_memory_imm(&vm_field(offsets.vm_execution_generation), 1);

        // 12. If Type(result.[[Value]]) is Object, return result.[[Value]].
        // 13. If kind is base, return thisArgument.
        // NB: The value from the runtime is thisArgument for a base constructor, and the empty value for a derived one.
        let have_result = self.masm.new_label();
        self.branch_on_tag(Condition::Equal, result, value::OBJECT_TAG, scratch, have_result);
        self.masm.load64(temp, &value_local);
        let error = self.masm.new_label();
        self.branch_on_tag(Condition::NotEqual, temp, value::OBJECT_TAG, scratch, error);
        self.masm.move64(result, temp);
        self.masm.bind(have_result);
        self.emit_stub_store_result(StubKind::Construct, result, temp);
        self.emit_stub_continuation();
        self.masm.jump(leave);

        // A derived constructor returned something other than an object.
        self.masm.bind(error);
        self.masm.move64(registers[3], result);
        self.masm.move64(registers[0], vm);
        self.masm.move64(registers[1], frame);
        self.masm.load32(registers[2], &pcs_local);
        self.masm.call_absolute(runtime.construct_result_error);
        self.masm.jump(leave);

        // The constructor's frame still runs: let the runtime finish the
        // construct.
        self.masm.bind(not_returned);
        self.masm.move64(registers[3], status);
        self.masm.load64(registers[4], &value_local);
        self.masm.move64(registers[0], vm);
        self.masm.move64(registers[1], frame);
        self.masm.load32(registers[2], &pcs_local);
        self.masm.call_absolute(runtime.finish_direct_construct);
        self.masm.jump(leave);

        // The runtime made the this value, but the stacks are too full.
        self.masm.bind(stack_overflow);
        self.masm.move64(registers[0], vm);
        self.masm.move64(registers[1], frame);
        self.masm.load32(registers[2], &pcs_local);
        self.masm.call_absolute(runtime.throw_call_stack_size_exceeded);
        self.masm.jump(leave);

        // Everything else: the runtime pushes the constructor's frame, or
        // makes the construct.
        self.masm.bind(slow);
        self.masm.move64(registers[0], vm);
        self.masm.move64(registers[1], frame);
        self.masm.load32(registers[2], &pcs_local);
        self.masm.call_absolute(runtime.prepare_construct);
        let pushed = self.masm.new_label();
        self.masm.branch64_imm(Condition::NotEqual, result, 0, pushed);
        self.masm.move64(result, status);
        self.masm.jump(leave);
        self.masm.bind(pushed);
        self.masm.store64(&frame_local, result);
        self.masm.store64(&value_local, status);
        self.masm.load64(registers[1], &frame_local);
        self.masm.load64(
            registers[3],
            &Address::new(registers[1], offsets.execution_context_executable as i32),
        );
        self.masm.jump(enter);

        self.masm.bind(leave);
        // NB: The this value must not outlive the construct on the stack, which the garbage collector scans
        //     conservatively.
        self.masm.store_imm64(&value_local, value::UNDEFINED);
        let machine_frame = self.frame;
        self.masm.emit_epilogue(&machine_frame);
        self.masm.ret();
    }

    /// Stores the fields of a new frame at `new_frame` that describe the call
    /// site (from the stub's locals): its passed argument count, return pc
    /// and destination. Returns the constant fields of a frame running no
    /// code yet, for the caller to store with its other constants. Clobbers
    /// `temp`.
    fn emit_stub_call_site_fields(
        &mut self,
        new_frame: crate::asm::Gpr,
        temp: crate::asm::Gpr,
        ecmascript: bool,
    ) -> [super::call::ConstantField; 7] {
        let offsets = self.runtime.offsets;
        let field = |offset: u32| Address::new(new_frame, offset as i32);
        let argument_count_local = self.stub_local(local::ARGUMENT_COUNT);
        self.masm.load32(temp, &argument_count_local);
        self.masm
            .store32(&field(offsets.execution_context_passed_argument_count), temp);
        let pcs_local = Address::new(M::STACK_POINTER, self.stub_local(local::PCS).displacement + 4);
        self.masm.load32(temp, &pcs_local);
        self.masm
            .store32(&field(offsets.execution_context_caller_return_pc), temp);
        let dst_local = self.stub_local(local::DST);
        self.masm.load32(temp, &dst_local);
        self.masm
            .store32(&field(offsets.execution_context_caller_dst_raw), temp);
        self.new_frame_constants(ecmascript)
    }

    /// Copies the `count` arguments of the stub into `destination` (an address
    /// in the new frame). Clobbers `index`, `value_register`, and `source_base`.
    fn emit_stub_copy_arguments(
        &mut self,
        kind: StubKind,
        destination: crate::asm::Gpr,
        count: crate::asm::Gpr,
        index: crate::asm::Gpr,
        value_register: crate::asm::Gpr,
        source_base: crate::asm::Gpr,
    ) {
        if kind == StubKind::AccessorGet {
            return;
        }
        if kind == StubKind::PutByValueSetter {
            let source_local = self.stub_local(local::ARG_SOURCE);
            self.masm.load64(value_register, &source_local);
            self.masm.store64(&Address::new(destination, 0), value_register);
            return;
        }
        let copied = self.masm.new_label();
        self.masm.branch32_imm(Condition::Equal, count, 0, copied);
        match kind {
            StubKind::Call => {
                let arg_source = self.stub_local(local::ARG_SOURCE);
                let from_stack = self.masm.new_label();
                let have_source = self.masm.new_label();
                self.masm.load64(source_base, &arg_source);
                self.masm.branch64_imm(Condition::Equal, source_base, 0, from_stack);
                self.emit_unbox_cell(source_base, source_base);
                self.masm.load64(
                    source_base,
                    &Address::new(source_base, self.runtime.layout.object_indexed_elements as i32),
                );
                self.masm.jump(have_source);
                self.masm.bind(from_stack);
                self.masm.load_effective_address(
                    source_base,
                    &Address::new(M::STACK_POINTER, self.frame.caller_stack_offset as i32),
                );
                self.masm.bind(have_source);
            }
            StubKind::Construct => {
                self.masm.load_effective_address(
                    source_base,
                    &Address::new(M::STACK_POINTER, self.frame.caller_stack_offset as i32),
                );
            }
            StubKind::CallForwarding => {
                let offsets = self.runtime.offsets;
                self.masm.load32(
                    source_base,
                    &Address::new(self.pinned.frame, offsets.execution_context_slot_count as i32),
                );
                self.masm.load32(
                    index,
                    &Address::new(self.pinned.frame, offsets.execution_context_argument_count as i32),
                );
                self.masm.sub32(source_base, source_base, index);
                self.masm.load_effective_address(
                    source_base,
                    &Address {
                        base: self.pinned.frame,
                        index: Some((source_base, Scale::Eight)),
                        displacement: offsets.execution_context_slots as i32,
                    },
                );
            }
            StubKind::AccessorGet | StubKind::PutByValueSetter => unreachable!(),
        }
        self.masm.move_imm32(index, 0);
        let copy = self.masm.new_label();
        self.masm.bind(copy);
        self.masm.load64(
            value_register,
            &Address {
                base: source_base,
                index: Some((index, Scale::Eight)),
                displacement: 0,
            },
        );
        self.masm.store64(
            &Address {
                base: destination,
                index: Some((index, Scale::Eight)),
                displacement: 0,
            },
            value_register,
        );
        self.masm.add32_imm(index, index, 1);
        self.masm.branch32(Condition::NotEqual, index, count, copy);
        self.masm.bind(copied);
    }

    /// Probes the site cache (`registers[1]`) and VM keyed lookup cache
    /// (`local::ARG_SOURCE` name) for an accessor on `registers[0]` (`base`).
    /// On hit with a non-null getter, stores boxed getter in `local::CALLEE`,
    /// puts unboxed getter in `registers[4]` and its `object_flags` in
    /// `registers[2]`. On null getter, stores `undefined`, sets continuation,
    /// and jumps to `leave`. Otherwise jumps to `slow`.
    fn emit_accessor_get_probe(&mut self, slow: Label, leave: Label) {
        let offsets = self.runtime.offsets;
        let layout = self.runtime.layout;
        let registers = M::ARGUMENT_GPRS;
        let object = registers[0];
        let shape = registers[1];
        let entry = registers[2];
        let raw = registers[3];
        let function_register = registers[4];
        let scratch = self.pinned.scratch;

        let found_site_entry = self.masm.new_label();
        let try_vm_cache = self.masm.new_label();
        let have_accessor = self.masm.new_label();
        let null_getter = self.masm.new_label();

        self.branch_on_tag(Condition::NotEqual, object, value::OBJECT_TAG, scratch, slow);
        self.emit_unbox_cell(object, object);
        self.masm.load64(entry, &Address::new(shape, 0));
        self.emit_load_compressed_cell(shape, &Address::new(object, offsets.object_shape as i32));
        self.masm.branch_test64(Condition::Zero, entry, u64::MAX, slow);
        self.masm.move64(raw, entry);
        self.masm
            .and64_imm(entry, entry, layout.property_lookup_cache_data_pointer_mask);
        self.branch_if_entry_shape(entry, shape, found_site_entry);

        self.masm
            .and64_imm(raw, raw, !layout.property_lookup_cache_data_pointer_mask);
        let not_polymorphic = self.masm.new_label();
        self.masm.branch64_imm(
            Condition::NotEqual,
            raw,
            layout.property_lookup_cache_polymorphic_tag as i64,
            not_polymorphic,
        );
        for _ in 1..layout.property_lookup_cache_polymorphic_entry_count {
            self.masm
                .add64_imm(entry, entry, i64::from(layout.property_lookup_cache_entry_size));
            self.branch_if_entry_shape(entry, shape, found_site_entry);
        }
        self.masm.jump(slow);

        self.masm.bind(not_polymorphic);
        self.masm.branch64_imm(
            Condition::NotEqual,
            raw,
            layout.property_lookup_cache_megamorphic_tag as i64,
            slow,
        );
        self.emit_megamorphic_cache_lookup(shape, entry, MegamorphicKey::Named, found_site_entry, try_vm_cache);

        // Matched an entry in the site cache.
        self.masm.bind(found_site_entry);
        let expected_dict = Address::new(entry, layout.property_lookup_cache_entry_dictionary_generation as i32);
        self.branch_unless_dictionary_generation_is(shape, expected_dict, [shape, scratch], slow);
        let site_type_ok = self.masm.new_label();
        self.masm.load32(
            scratch,
            &Address::new(entry, layout.property_lookup_cache_entry_type as i32),
        );
        self.masm.branch32_imm(
            Condition::Equal,
            scratch,
            layout.property_lookup_cache_entry_type_get_own_property as i32,
            site_type_ok,
        );
        self.masm.branch32_imm(
            Condition::NotEqual,
            scratch,
            layout.property_lookup_cache_entry_type_get_property_in_prototype_chain as i32,
            slow,
        );
        self.masm.bind(site_type_ok);
        let site_own = self.masm.new_label();
        self.masm.load64(
            raw,
            &Address::new(entry, layout.property_lookup_cache_entry_prototype as i32),
        );
        self.masm.branch_test64(Condition::Zero, raw, u64::MAX, site_own);
        self.masm.load64(
            shape,
            &Address::new(
                entry,
                layout.property_lookup_cache_entry_prototype_chain_validity as i32,
            ),
        );
        self.masm.branch_test64(Condition::Zero, shape, u64::MAX, slow);
        self.branch_unless_prototype_chain_valid(shape, slow);
        self.masm.move64(object, raw);
        self.masm.bind(site_own);
        self.masm.load32(
            scratch,
            &Address::new(entry, layout.property_lookup_cache_entry_property_offset as i32),
        );
        self.masm
            .load64(shape, &Address::new(object, offsets.object_named_properties as i32));
        self.masm
            .load64(entry, &Address::indexed(shape, scratch, Scale::Eight, 0));
        self.branch_on_tag(Condition::NotEqual, entry, value::ACCESSOR_TAG, scratch, slow);
        self.masm.jump(have_accessor);

        // Megamorphic site miss: probe the VM keyed property lookup cache.
        self.masm.bind(try_vm_cache);
        if layout.keyed_lookup_cache_entries == 0
            || layout.keyed_lookup_cache_prototype == 0
            || layout.keyed_lookup_cache_prototype_chain_validity == 0
        {
            self.masm.jump(slow);
        } else {
            let name_local = self.stub_local(local::ARG_SOURCE);
            self.masm.load64(raw, &name_local);
            self.masm.branch_test64(Condition::Zero, raw, u64::MAX, slow);
            let entry_size_log2 = layout.keyed_lookup_cache_entry_size.trailing_zeros();
            let bits = layout.keyed_lookup_cache_index_bits;
            self.masm.xor32(scratch, shape, raw);
            self.masm.mul32_imm(
                scratch,
                scratch,
                layout.property_lookup_cache_megamorphic_hash_multiplier,
            );
            self.masm
                .shr32_imm(scratch, scratch, (32 - bits - entry_size_log2) as u8);
            self.masm
                .and32_imm(scratch, scratch, ((1u32 << bits) - 1) << entry_size_log2);
            self.masm.move_imm64(entry, layout.keyed_lookup_cache_entries);
            self.masm.add64(entry, entry, scratch);
            self.masm.branch64_memory(
                Condition::NotEqual,
                &Address::new(entry, layout.keyed_lookup_cache_name as i32),
                raw,
                slow,
            );
            self.masm.branch64_memory(
                Condition::NotEqual,
                &Address::new(entry, layout.keyed_lookup_cache_shape as i32),
                shape,
                slow,
            );
            let expected = Address::new(entry, layout.keyed_lookup_cache_dictionary_generation as i32);
            self.branch_unless_dictionary_generation_is(shape, expected, [shape, scratch], slow);
            let vm_own = self.masm.new_label();
            self.masm.load32(
                scratch,
                &Address::new(entry, layout.keyed_lookup_cache_entry_type as i32),
            );
            self.masm.branch32_imm(
                Condition::Equal,
                scratch,
                layout.property_lookup_cache_entry_type_get_own_property as i32,
                vm_own,
            );
            self.masm.branch32_imm(
                Condition::NotEqual,
                scratch,
                layout.property_lookup_cache_entry_type_get_property_in_prototype_chain as i32,
                slow,
            );
            self.masm.load64(
                shape,
                &Address::new(entry, layout.keyed_lookup_cache_prototype_chain_validity as i32),
            );
            self.masm.branch_test64(Condition::Zero, shape, u64::MAX, slow);
            self.branch_unless_prototype_chain_valid(shape, slow);
            self.masm
                .load64(object, &Address::new(entry, layout.keyed_lookup_cache_prototype as i32));
            self.masm.branch_test64(Condition::Zero, object, u64::MAX, slow);
            self.masm.bind(vm_own);
            self.masm.load32(
                scratch,
                &Address::new(entry, layout.keyed_lookup_cache_property_offset as i32),
            );
            self.masm
                .load64(shape, &Address::new(object, offsets.object_named_properties as i32));
            self.masm
                .load64(entry, &Address::indexed(shape, scratch, Scale::Eight, 0));
            self.branch_on_tag(Condition::NotEqual, entry, value::ACCESSOR_TAG, scratch, slow);
        }

        self.masm.bind(have_accessor);
        self.emit_unbox_cell(entry, entry);
        self.emit_load_compressed_cell_or_branch(
            function_register,
            &Address::new(entry, offsets.accessor_getter as i32),
            null_getter,
        );
        self.box_cell_with_tag(entry, function_register, value::OBJECT_TAG);
        let callee_local = self.stub_local(local::CALLEE);
        self.masm.store64(&callee_local, entry);
        self.masm
            .load16(entry, &Address::new(function_register, offsets.object_flags as i32));
        let dispatch = self.masm.new_label();
        self.masm.jump(dispatch);

        self.masm.bind(null_getter);
        self.masm.move_imm64(entry, value::UNDEFINED);
        self.emit_stub_store_result(StubKind::AccessorGet, entry, raw);
        self.emit_stub_continuation();
        self.masm.jump(leave);

        self.masm.bind(dispatch);
    }

    /// Probes the VM keyed store cache read-only for a
    /// `ChangePropertyInPrototypeChain` accessor on (`base` in `registers[0]`,
    /// `key` in `registers[1]`). On hit with a non-null setter, stores boxed
    /// setter in `local::CALLEE`, puts unboxed setter in `registers[4]` and its
    /// `object_flags` in `registers[2]`. Otherwise jumps to `slow`.
    fn emit_put_by_value_setter_probe(&mut self, slow: Label) {
        let offsets = self.runtime.offsets;
        let layout = self.runtime.layout;
        let registers = M::ARGUMENT_GPRS;
        let object = registers[0];
        let shape = registers[1];
        let entry = registers[2];
        let name = registers[3];
        let function_register = registers[4];
        let scratch = self.pinned.scratch;

        self.branch_on_tag(Condition::NotEqual, object, value::OBJECT_TAG, scratch, slow);
        self.branch_on_tag(Condition::NotEqual, shape, value::STRING_TAG, scratch, slow);
        self.emit_unbox_cell(name, shape);
        self.masm
            .load64(name, &Address::new(name, layout.primitive_string_storage as i32));
        self.masm.branch_test64(Condition::Zero, name, u64::MAX, slow);

        self.emit_unbox_cell(object, object);
        self.emit_load_compressed_cell(shape, &Address::new(object, offsets.object_shape as i32));

        let entry_size_log2 = layout.keyed_lookup_cache_entry_size.trailing_zeros();
        let bits = layout.keyed_lookup_cache_index_bits;
        self.masm.xor32(scratch, shape, name);
        self.masm.mul32_imm(
            scratch,
            scratch,
            layout.property_lookup_cache_megamorphic_hash_multiplier,
        );
        self.masm
            .shr32_imm(scratch, scratch, (32 - bits - entry_size_log2) as u8);
        self.masm
            .and32_imm(scratch, scratch, ((1u32 << bits) - 1) << entry_size_log2);
        self.masm.move_imm64(entry, layout.keyed_store_cache_entries);
        self.masm.add64(entry, entry, scratch);
        self.masm.branch64_memory(
            Condition::NotEqual,
            &Address::new(entry, layout.keyed_lookup_cache_name as i32),
            name,
            slow,
        );
        self.masm.branch64_memory(
            Condition::NotEqual,
            &Address::new(entry, layout.keyed_lookup_cache_shape as i32),
            shape,
            slow,
        );
        let expected = Address::new(entry, layout.keyed_lookup_cache_dictionary_generation as i32);
        self.branch_unless_dictionary_generation_is(shape, expected, [shape, scratch], slow);
        self.masm.branch32_memory_imm(
            Condition::NotEqual,
            &Address::new(entry, layout.keyed_lookup_cache_entry_type as i32),
            layout.property_lookup_cache_entry_type_change_property_in_prototype_chain as i32,
            slow,
        );
        self.masm
            .load64(object, &Address::new(entry, layout.keyed_lookup_cache_prototype as i32));
        self.masm.branch_test64(Condition::Zero, object, u64::MAX, slow);
        self.masm.load64(
            shape,
            &Address::new(entry, layout.keyed_lookup_cache_prototype_chain_validity as i32),
        );
        self.masm.branch_test64(Condition::Zero, shape, u64::MAX, slow);
        self.branch_unless_prototype_chain_valid(shape, slow);
        self.masm.load32(
            scratch,
            &Address::new(entry, layout.keyed_lookup_cache_property_offset as i32),
        );
        self.masm
            .load64(shape, &Address::new(object, offsets.object_named_properties as i32));
        self.masm
            .load64(entry, &Address::indexed(shape, scratch, Scale::Eight, 0));
        self.branch_on_tag(Condition::NotEqual, entry, value::ACCESSOR_TAG, scratch, slow);
        self.emit_unbox_cell(entry, entry);
        self.emit_load_compressed_cell_or_branch(
            function_register,
            &Address::new(entry, offsets.accessor_setter as i32),
            slow,
        );
        self.box_cell_with_tag(entry, function_register, value::OBJECT_TAG);
        let callee_local = self.stub_local(local::CALLEE);
        self.masm.store64(&callee_local, entry);
        self.masm
            .load16(entry, &Address::new(function_register, offsets.object_flags as i32));
    }

    /// Stores the call's result into the calling frame's destination slot
    /// (unless `kind == StubKind::PutByValueSetter`). Clobbers `temp`.
    fn emit_stub_store_result(&mut self, kind: StubKind, result: crate::asm::Gpr, temp: crate::asm::Gpr) {
        if kind != StubKind::PutByValueSetter {
            let dst_local = self.stub_local(local::DST);
            self.masm.load32(temp, &dst_local);
            self.masm.store64(
                &Address {
                    base: self.pinned.frame,
                    index: Some((temp, Scale::Eight)),
                    displacement: self.runtime.offsets.execution_context_slots as i32,
                },
                result,
            );
        }
        // Stack traces show the caller at its next instruction from now on.
        let next_pc = Address::new(M::STACK_POINTER, self.stub_local(local::PCS).displacement + 4);
        self.masm.load32(temp, &next_pc);
        self.masm.store32(
            &Address::new(
                self.pinned.frame,
                self.runtime.offsets.execution_context_program_counter as i32,
            ),
            temp,
        );
    }

    /// Puts a continuation at the call's next pc in the first return
    /// register.
    fn emit_stub_continuation(&mut self) {
        let control = M::RETURN_GPRS[0];
        let next_pc = Address::new(M::STACK_POINTER, self.stub_local(local::PCS).displacement + 4);
        self.masm.load32(control, &next_pc);
        self.masm.or64_imm(control, control, super::CONTINUATION_BIT);
    }

    /// Calls the raw native function in the function register (registers[4])
    /// in its lightweight frame, like `emit_native_frame_call()` does for the
    /// stub's dynamic argument count. Jumps to `slow` before anything
    /// observable happened if the stacks are too full, and to `leave` with a
    /// control word in the first return register otherwise.
    fn emit_stub_native_call(&mut self, kind: StubKind, slow: Label, leave: Label) -> Result<(), CompileFailure> {
        let runtime = self.runtime;
        let offsets = runtime.offsets;
        let layout = runtime.dynamic_calls;
        let registers = M::ARGUMENT_GPRS;
        let native_frame = registers[1];
        let temp = registers[2];
        let other = registers[3];
        let function_register = registers[4];
        let count = registers[5];
        let scratch = self.pinned.scratch;
        let vm = self.pinned.vm;
        let frame = self.pinned.frame;
        let vm_field = |offset: u32| Address::new(vm, offset as i32);
        let frame_field = |offset: u32| Address::new(frame, offset as i32);
        let field = |offset: u32| Address::new(native_frame, offset as i32);
        let field_of = |base: Gpr, offset: u32| Address::new(base, offset as i32);

        self.emit_native_stack_check(slow);

        // Allocate the frame on the interpreter stack: the header and one
        // slot per argument.
        let argument_count_local = self.stub_local(local::ARGUMENT_COUNT);
        self.masm.load64(count, &argument_count_local);
        self.masm
            .load64(native_frame, &vm_field(offsets.vm_interpreter_stack_top));
        self.masm.load_effective_address(
            temp,
            &Address {
                base: native_frame,
                index: Some((count, Scale::Eight)),
                displacement: offsets.execution_context_slots as i32,
            },
        );
        self.masm.load64(other, &vm_field(offsets.vm_interpreter_stack_limit));
        self.masm.branch64(Condition::Above, temp, other, slow);
        self.masm.store64(&vm_field(offsets.vm_interpreter_stack_top), temp);

        // Fill in the frame like the interpreter's call fast path.
        self.masm
            .load64(temp, &frame_field(offsets.execution_context_lexical_environment));
        self.masm
            .store64(&field(offsets.execution_context_lexical_environment), temp);
        let this_local = self.stub_local(local::THIS);
        self.masm.load64(temp, &this_local);
        self.masm.store64(&field(offsets.execution_context_this_value), temp);
        // NB: Frames get their id when the debugger first needs it.
        self.masm.store_imm64(&field(offsets.execution_context_frame_id), 0);
        self.masm.store64(&field(offsets.execution_context_caller_frame), frame);
        self.masm
            .store64(&field(offsets.execution_context_function), function_register);
        self.masm.store32(&field(offsets.execution_context_slot_count), count);
        self.masm
            .store32(&field(offsets.execution_context_argument_count), count);
        let mut constants = self.spare_fields.take();
        constants.clear();
        constants.extend(self.emit_stub_call_site_fields(native_frame, temp, false));
        constants.extend([(offsets.execution_context_executable, 8, Field::Cell(CellId::NULL))]);
        self.emit_constant_fields_with_vector(native_frame, temp, &mut constants, Some(M::ARGUMENT_FPRS[0]), &[])?;
        self.spare_fields = constants;
        self.masm.load_effective_address(
            other,
            &Address::new(native_frame, offsets.execution_context_slots as i32),
        );
        self.emit_stub_copy_arguments(kind, other, count, registers[0], temp, scratch);
        self.masm
            .store64(&vm_field(offsets.vm_running_execution_context), native_frame);
        // The function runs in its own realm, which may be another one. The
        // frame keeps the caller's realm, which the return makes current
        // again, as does the runtime when it unwinds the frame of a function
        // that threw.
        self.masm.load64(temp, &vm_field(offsets.vm_current_realm));
        self.masm
            .store64(&field(offsets.execution_context_realm_without_function), temp);
        self.emit_load_compressed_cell(temp, &Address::new(function_register, offsets.object_shape as i32));
        self.emit_load_compressed_cell(temp, &Address::new(temp, layout.shape_realm as i32));
        self.masm.store64(&vm_field(offsets.vm_current_realm), temp);

        // Call the native function: it returns a ThrowCompletionOr<Value>,
        // the value or exception in the first return register and whether it
        // is an exception in the low byte of the second.
        self.masm.load32(
            temp,
            &Address::new(function_register, layout.raw_native_function_index as i32),
        );
        self.masm.and32_imm(temp, temp, layout.native_function_table_index_mask);
        self.masm.shl64_imm(
            temp,
            temp,
            u8::try_from(layout.native_function_table_entry_size.trailing_zeros()).unwrap_or(0),
        );
        self.masm.load64(scratch, &vm_field(layout.vm_native_function_table));
        self.masm.load64(
            scratch,
            &Address {
                base: scratch,
                index: Some((temp, Scale::One)),
                displacement: layout.native_function_table_entry_function as i32,
            },
        );
        self.masm.move64(registers[0], vm);
        self.masm.call_register(scratch);
        let [value_register, variant] = [M::RETURN_GPRS[0], M::RETURN_GPRS[1]];
        let threw = self.masm.new_label();
        self.masm.branch_test32(Condition::NonZero, variant, 0xFF, threw);
        // Pop the frame, which is the running one, like the interpreter does.
        self.masm.load64(temp, &vm_field(offsets.vm_running_execution_context));
        self.masm.load64(
            scratch,
            &field_of(temp, offsets.execution_context_realm_without_function),
        );
        self.masm.store64(&vm_field(offsets.vm_current_realm), scratch);
        self.masm.store64(&vm_field(offsets.vm_interpreter_stack_top), temp);
        self.masm
            .store64(&vm_field(offsets.vm_running_execution_context), frame);
        self.emit_stub_store_result(kind, value_register, temp);
        self.emit_stub_continuation();
        self.masm.jump(leave);

        // The runtime unwinds the frame of a native function that threw.
        self.masm.bind(threw);
        self.masm.move64(registers[1], value_register);
        self.masm.move64(registers[0], self.pinned.vm);
        self.masm.call_absolute(runtime.raw_native_exception);
        self.masm.jump(leave);
        Ok(())
    }
}
