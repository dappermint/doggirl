/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! A straightforward linear-order register allocator.
//!
//! One forward walk over the blocks in linear order assigns registers to
//! values as nodes need them, using the constraints each node declares. When
//! no register is free, the value whose next use is furthest away is evicted.
//!
//! Spilling is simple: a value that ever needs a stack slot gets one slot for
//! its whole life, and code generation stores it there right after its
//! definition. Values live across a call always get one. Values loaded from a
//! frame slot that nothing writes need none: they are reloaded from the frame
//! (see `frame_home()`).
//! Phis share their slots with the values flowing into them where their
//! lives do not overlap (see `coalescing`), so that values carried around
//! loops in stack slots need no copying.
//!
//! A merge or loop header starts with the register state of its first
//! predecessor in linear order (its "state predecessor"; for a loop header,
//! the block entering the loop), and its phis get registers that are free
//! there. The other predecessors, back edges included, move their values (and
//! their phis' inputs) into place at their end, as parallel moves. The rare
//! merges whose other predecessors do not jump to them start with every value
//! in its stack slot (see `liveness::state_predecessor()`).
//!
//! Loops (see `loops`) keep their registers for what they use: a loop header
//! leaves the values that the loop only carries through in their stack slots
//! where they have them already, and inside a loop, the values it reads again
//! in its next iteration are evicted after those used only after it.
//!
//! Cold blocks (see `Block::is_cold`) come last in linear order and cost the
//! other blocks nothing: they start with the register state of the block
//! they branch off from, and move their values into place where they rejoin
//! the other blocks.
//!
//! There are two classes of registers: general purpose registers hold
//! every value but `Repr::Float64` ones, which floating point registers hold.
//! Floating point registers are numbered from `FPR_BASE` on in `Location`s
//! and masks. Nodes whose slow paths save the general purpose registers
//! around their calls (see `Clobbers::Floats`) clobber the floating point
//! ones, so float64 values live across them get stack slots, like values
//! live across a call.
//!
//! What each node needs (where its inputs and its output are, its temps and
//! what it clobbers) is declared next to its lowering, in
//! `codegen::lowering_info()`.

mod coalescing;
mod liveness;
mod loops;
mod moves;
mod verify;

pub use moves::resolve_parallel_moves;
pub use verify::verify;

use crate::CompileFailure;
use crate::asm::Gpr;
use crate::code::Repr;
use crate::codegen::lowering_info::Clobbers;
use crate::codegen::lowering_info::FixedRegister;
use crate::codegen::lowering_info::InputConstraint;
use crate::codegen::lowering_info::OutputConstraint;
use crate::codegen::lowering_info::lowering_info;
use crate::ir::BlockId;
use crate::ir::Graph;
use crate::ir::NodeId;
use crate::ir::Op;
use crate::tables::Lists;
use crate::tables::Run;
use ak::Zone;
use ak::ZoneVec;
use coalescing::Coalescing;
use core::fmt;
use liveness::ValueLiveness;
use liveness::is_allocated_value;
use liveness::state_predecessor;
use loops::Loops;

/// The number of the first floating point register in `Location`s and
/// `RegisterMask`s: floating point register `n` is register `FPR_BASE + n`.
pub const FPR_BASE: u8 = 32;

/// How many registers of both classes there are at most.
const REGISTER_COUNT: usize = 64;

/// What a value's register must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterClass {
    General,
    Float,
}

impl RegisterClass {
    /// The class of the registers that hold values of representation `repr`.
    pub fn of(repr: Option<Repr>) -> Self {
        match repr {
            Some(Repr::Float64) => RegisterClass::Float,
            _ => RegisterClass::General,
        }
    }

    /// The class of register number `register`.
    pub fn of_register(register: u8) -> Self {
        if register >= FPR_BASE {
            RegisterClass::Float
        } else {
            RegisterClass::General
        }
    }
}

/// A set of registers, as a bit mask of register numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegisterMask(pub u64);

pub struct RegisterMaskIter(u64);

impl Iterator for RegisterMaskIter {
    type Item = u8;

    #[inline(always)]
    fn next(&mut self) -> Option<u8> {
        if self.0 == 0 {
            return None;
        }
        let register = self.0.trailing_zeros() as u8;
        self.0 &= self.0 - 1;
        Some(register)
    }
}

impl RegisterMask {
    #[inline(always)]
    pub fn contains(self, register: u8) -> bool {
        self.0 & (1 << register) != 0
    }

    #[inline(always)]
    pub fn with(self, register: u8) -> Self {
        Self(self.0 | (1 << register))
    }

    #[inline(always)]
    pub fn iter(self) -> RegisterMaskIter {
        RegisterMaskIter(self.0)
    }
}

/// The registers of a target, as far as register allocation cares.
/// Registers are named by their hardware encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSet {
    /// Registers the allocator may assign to values.
    pub allocatable_gprs: RegisterMask,
    pub allocatable_fprs: RegisterMask,
    /// Registers a call clobbers.
    pub caller_saved_gprs: RegisterMask,
    pub caller_saved_fprs: RegisterMask,
    /// Where calls take their arguments, in order.
    pub argument_gprs: &'static [Gpr],
    /// Where calls return their (first) result.
    pub return_gpr: u8,
    /// Where shifts by a register amount take the amount, on targets that shift by one register only.
    pub shift_amount_gpr: Option<u8>,
    /// Never allocated. Moves between stack slots and of constants into stack
    /// slots go through it, and node lowerings may clobber it.
    pub scratch_gpr: u8,
}

impl RegisterSet {
    /// The registers of `class` the allocator may assign to values.
    pub fn allocatable(&self, class: RegisterClass) -> RegisterMask {
        match class {
            RegisterClass::General => self.allocatable_gprs,
            RegisterClass::Float => RegisterMask(self.allocatable_fprs.0 << FPR_BASE),
        }
    }

    /// The number of the calling convention's register `register`.
    pub fn fixed(&self, register: FixedRegister) -> u8 {
        match register {
            FixedRegister::Argument(index) => self.argument_gprs[usize::from(index)].0,
            FixedRegister::Return => self.return_gpr,
            FixedRegister::ShiftAmount => self
                .shift_amount_gpr
                .expect("only targets with a shift amount register have fixed shift amounts"),
        }
    }

    /// Where an input of `constraint` must be on this target, if it is a constant (`is_constant`) or not.
    pub fn resolve(&self, constraint: InputConstraint, is_constant: bool) -> InputConstraint {
        match constraint {
            InputConstraint::ShiftAmount if is_constant || self.shift_amount_gpr.is_none() => {
                InputConstraint::RegisterOrConstant
            }
            InputConstraint::ShiftAmount => InputConstraint::Fixed(FixedRegister::ShiftAmount),
            constraint => constraint,
        }
    }

    /// The registers of both classes a call clobbers.
    pub fn caller_saved(&self) -> RegisterMask {
        RegisterMask(self.caller_saved_gprs.0 | (self.caller_saved_fprs.0 << FPR_BASE))
    }

    /// The floating point registers a call or float-clobbering slow path clobbers.
    pub fn caller_saved_fprs(&self) -> RegisterMask {
        RegisterMask(self.caller_saved_fprs.0 << FPR_BASE)
    }

    /// The registers of `class` a call clobbers.
    pub fn caller_saved_for(&self, class: RegisterClass) -> RegisterMask {
        match class {
            RegisterClass::General => self.caller_saved_gprs,
            RegisterClass::Float => self.caller_saved_fprs(),
        }
    }

    /// The fewest allocatable registers allocation works with: the ones
    /// calls take their arguments and return their results in, the one
    /// shifts take their amount in, and `extra` more.
    pub fn few_registers(&self, extra: u32) -> RegisterMask {
        let mut mask = RegisterMask(0).with(self.return_gpr);
        for register in self.argument_gprs {
            mask = mask.with(register.0);
        }
        if let Some(register) = self.shift_amount_gpr {
            mask = mask.with(register);
        }
        let needed = RegisterMask(mask.0 & self.allocatable_gprs.0);
        let mut mask = needed;
        for register in self
            .allocatable_gprs
            .iter()
            .filter(|register| !needed.contains(*register))
            .take(extra as usize)
        {
            mask = mask.with(register);
        }
        mask
    }

    /// A small made up target for tests: six allocatable registers, all of
    /// them clobbered by calls.
    pub fn test_configuration() -> Self {
        Self {
            allocatable_gprs: RegisterMask(0b0011_1111),
            allocatable_fprs: RegisterMask(0b1111_1111),
            caller_saved_gprs: RegisterMask(0b1111_1111),
            caller_saved_fprs: RegisterMask(0b1111_1111),
            argument_gprs: &[Gpr(1), Gpr(2), Gpr(3)],
            return_gpr: 0,
            shift_amount_gpr: None,
            scratch_gpr: 7,
        }
    }
}

/// Where a value is, or where a move reads or writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Location {
    Register(u8),
    /// A spill slot of the JIT frame.
    Stack(u32),
    /// A constant, the bits of a value of the representation; only ever a move source. A constant of
    /// `Repr::Pointer` is the address of a cell, which code holds as a cell (see `crate::relocation`).
    Constant(u64, Repr),
    /// A slot of the compiled function's frame (an operand), which holds the value loaded from it wherever that value
    /// is live (see `frame_home()`); only ever a move source.
    Frame(u32),
}

/// The representation of the constant `value`.
fn constant_repr(graph: &Graph<'_>, value: NodeId) -> Repr {
    graph.node(graph.unrefined(value)).repr.unwrap_or(Repr::Tagged)
}

impl Default for Location {
    fn default() -> Self {
        Location::Register(0)
    }
}

/// Copies a value. Moves between stack slots and of constants into stack
/// slots go through the scratch register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    pub from: Location,
    pub to: Location,
}

/// The most temps a node has (see `LoweringInfo::temps`).
const MAX_TEMPS: usize = 4;

/// How the allocator placed one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeAllocation<'a> {
    /// Moves to perform, in order, right before the node.
    pub moves_before: &'a [Move],
    /// Where the node finds each input: a register, or a constant for
    /// inputs that may be immediates.
    pub inputs: &'a [Location],
    pub temps: &'a [u8],
    /// Where the node puts its value: a register, or a stack slot for phis.
    pub output: Option<Location>,
    /// If set and the output is a register, the value must be stored to this
    /// spill slot right after its definition.
    pub spill_slot: Option<u32>,
    /// For nodes with a frame state, where the exit finds each of its values,
    /// as (frame slot, location).
    pub exit_values: &'a [(u32, Location)],
}

/// The row of a node in the tables of an allocation (see `NodeAllocation`). Its lists are parts of the columns that
/// hold the lists of all the nodes.
#[derive(Clone, Copy, Default)]
struct NodeRow {
    moves_before: Run,
    inputs: Run,
    exit_values: Run,
    output: Option<Location>,
    spill_slot: Option<u32>,
    temps: [u8; MAX_TEMPS],
    temp_count: u8,
}

/// How large a graph is, in what the tables of the allocator have rows and elements for.
struct Size {
    nodes: usize,
    blocks: usize,
    frame_states: usize,
    /// The inputs of all the nodes.
    inputs: usize,
    /// The edges from the blocks to the blocks they continue at.
    edges: usize,
}

impl Size {
    /// The size of `graph`. A compile whose graph is larger than the allocator takes fails.
    fn of(graph: &Graph) -> Result<Self, CompileFailure> {
        let mut edges = 0;
        for index in 0..graph.blocks.len() {
            graph
                .control(BlockId::from_index(index))
                .op
                .for_each_successor(|_| edges += 1);
        }
        let size = Self {
            nodes: graph.nodes.len(),
            blocks: graph.blocks.len(),
            frame_states: graph.frame_states.len(),
            inputs: graph.nodes.iter().map(|node| node.inputs.len()).sum(),
            edges,
        };
        if size.nodes > Allocation::MAX_NODES
            || size.blocks > Allocation::MAX_BLOCKS
            || size.frame_states > Allocation::MAX_NODES
            || size.inputs > Allocation::MAX_ITEMS
            || size.edges > Allocation::MAX_ITEMS
        {
            return Err(CompileFailure::TooLarge);
        }
        Ok(size)
    }
}

/// Fails the compile if a list of the allocator that grows while it runs, to a length that the size of the graph
/// does not tell, has more than `Allocation::MAX_ITEMS` elements.
fn check_items(len: usize) -> Result<(), CompileFailure> {
    if len > Allocation::MAX_ITEMS {
        return Err(CompileFailure::TooLarge);
    }
    Ok(())
}

/// Where the allocator placed the values of a graph, in tables in the work zone of a compile.
pub struct Allocation<'w> {
    /// Indexed by `NodeId`.
    nodes: ZoneVec<'w, NodeRow>,
    /// The moves before the nodes and at the ends of the blocks.
    moves: ZoneVec<'w, Move>,
    /// The locations of the inputs of the nodes.
    locations: ZoneVec<'w, Location>,
    exit_values: ZoneVec<'w, (u32, Location)>,
    /// The moves at the end of each block, as a part of `moves`.
    block_end_moves: ZoneVec<'w, Run>,
    pub spill_slot_count: u32,
    /// The values live into each block, sorted.
    live_in: Lists<'w, NodeId>,
}

impl<'w> Allocation<'w> {
    // NB: A compile whose graph has more of these fails (see `Size::of()` and `check_items()`).
    const MAX_NODES: usize = 1 << 24;
    const MAX_BLOCKS: usize = 1 << 24;
    const MAX_ITEMS: usize = 1 << 26;

    /// An allocation of no node, which takes no memory before it has some.
    pub fn new_in(work: &'w Zone) -> Self {
        Self {
            nodes: ZoneVec::new_in(work),
            moves: ZoneVec::new_in(work),
            locations: ZoneVec::new_in(work),
            exit_values: ZoneVec::new_in(work),
            block_end_moves: ZoneVec::new_in(work),
            spill_slot_count: 0,
            live_in: Lists::new_in(work),
        }
    }

    /// Tables for the allocation of a graph of `size`.
    fn for_graph(size: &Size, work: &'w Zone) -> Self {
        Self {
            nodes: ZoneVec::from_elem_in(NodeRow::default(), size.nodes, work),
            // NB: Most nodes need fewer moves than they have inputs.
            moves: ZoneVec::with_capacity_in(size.inputs, work),
            locations: ZoneVec::with_capacity_in(size.inputs, work),
            exit_values: ZoneVec::new_in(work),
            block_end_moves: ZoneVec::from_elem_in(Run::default(), size.blocks, work),
            spill_slot_count: 0,
            live_in: Lists::new_in(work),
        }
    }

    /// How many nodes have an allocation.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn node(&self, node: NodeId) -> NodeAllocation<'_> {
        let row = &self.nodes[node.index()];
        NodeAllocation {
            moves_before: &self.moves[row.moves_before.range()],
            inputs: &self.locations[row.inputs.range()],
            temps: &row.temps[..usize::from(row.temp_count)],
            output: row.output,
            spill_slot: row.spill_slot,
            exit_values: &self.exit_values[row.exit_values.range()],
        }
    }

    /// Moves to perform, in order, at the end of `block`, after its body
    /// and before its control node. Only blocks ending in a jump to a merge
    /// or a loop header have any.
    pub fn block_end_moves(&self, block: BlockId) -> &[Move] {
        &self.moves[self.block_end_moves[block.index()].range()]
    }

    /// Whether `value` is live into `block`.
    pub fn is_live_in(&self, block: BlockId, value: NodeId) -> bool {
        self.live_in.get(block.index()).binary_search(&value).is_ok()
    }
}

// NB: The tests of the verifier make wrong allocations with these.
#[cfg(test)]
impl Allocation<'_> {
    /// All the moves of the allocation.
    pub(crate) fn moves(&self) -> &[Move] {
        &self.moves
    }

    pub(crate) fn set_spill_slot(&mut self, node: NodeId, slot: Option<u32>) {
        self.nodes[node.index()].spill_slot = slot;
    }

    pub(crate) fn set_input(&mut self, node: NodeId, index: usize, location: Location) {
        let inputs = self.nodes[node.index()].inputs.range();
        self.locations[inputs][index] = location;
    }

    pub(crate) fn set_move_before(&mut self, node: NodeId, index: usize, new_move: Move) {
        let moves = self.nodes[node.index()].moves_before.range();
        self.moves[moves][index] = new_move;
    }

    /// Sets the move at `index` of moves().
    pub(crate) fn set_move(&mut self, index: usize, new_move: Move) {
        self.moves[index] = new_move;
    }

    pub(crate) fn set_exit_value(&mut self, node: NodeId, index: usize, location: Location) {
        let exit_values = self.nodes[node.index()].exit_values.range();
        self.exit_values[exit_values][index].1 = location;
    }

    /// Adds the allocation of one more node, which has `inputs` and `output` and nothing else.
    pub(crate) fn push_node(&mut self, inputs: &[Location], output: Option<Location>) {
        let start = self.locations.len();
        self.locations.extend_from_slice(inputs);
        self.nodes.push(NodeRow {
            inputs: Run::new(start, self.locations.len()),
            output,
            ..NodeRow::default()
        });
    }
}

/// The value each register holds; slot `r` is valid iff `occupied.contains(r)`.
type Contents = [NodeId; REGISTER_COUNT];

/// The slot of the compiled function's frame that holds `value` wherever it is live, if there is one: a value loaded
/// from one of the slots whose frame memory never changes (`Graph::unwritten_slots`).
///
/// OPTIMIZATION: Such a value needs no spill slot, and no store right after its definition: where no register holds
///               it, it is reloaded from the frame. Exits find it there too (`code::ValueLocation::Frame`).
pub fn frame_home(graph: &Graph, value: NodeId) -> Option<u32> {
    match graph.node(value).op {
        Op::LoadSlot { slot } if graph.is_unwritten_slot(slot) => Some(slot),
        _ => None,
    }
}

/// Assigns locations to the values of `graph`, in tables in `work`. The verifier checks the result where the IR is
/// verified (see `verify()`).
pub fn allocate<'w>(graph: &Graph, registers: &RegisterSet, work: &'w Zone) -> Result<Allocation<'w>, CompileFailure> {
    assert!(registers.allocatable_gprs.0 >> FPR_BASE == 0);
    assert!(!registers.allocatable_gprs.contains(registers.scratch_gpr));
    let size = Size::of(graph)?;
    let liveness = ValueLiveness::compute(graph, &size, work)?;
    let coalescing = Coalescing::compute(graph, &liveness, work);
    let loops = Loops::compute(graph, &liveness, work)?;
    let cold_blocks_are_last = graph.blocks.iter().skip_while(|block| !block.is_cold).all(|block| block.is_cold);
    let allocator = Allocator {
        graph,
        registers,
        liveness: &liveness,
        allocation: Allocation::for_graph(&size, work),
        contents: [NodeId(0); REGISTER_COUNT],
        occupied: RegisterMask::default(),
        value_registers: ZoneVec::from_elem_in(None, size.nodes, work),
        end_entries: ZoneVec::with_capacity_in(size.blocks.saturating_mul(8), work),
        end_runs: ZoneVec::from_elem_in(Run::default(), size.blocks, work),
        use_cold_spills: cold_blocks_are_last,
        current_block_is_cold: false,
        cold_homes: ZoneVec::from_elem_in(None, size.nodes, work),
        cold_home_touched: ZoneVec::new_in(work),
        cold_home_end_entries: ZoneVec::new_in(work),
        cold_home_end_runs: ZoneVec::from_elem_in(Run::default(), size.blocks, work),
        merge_entries: ZoneVec::with_capacity_in(size.blocks.saturating_mul(4), work),
        merge_runs: ZoneVec::from_elem_in(Run::default(), size.blocks, work),
        merge_recorded: ZoneVec::from_elem_in(false, size.blocks, work),
        finished: ZoneVec::from_elem_in(false, size.blocks, work),
        position: 0,
        slot_classes: &coalescing.classes,
        class_slots: ZoneVec::from_elem_in(None, size.nodes, work),
        parallel_moves: ZoneVec::new_in(work),
        loops: &loops,
        work,
    };
    let mut allocation = allocator.run()?;
    // NB: The code generator reads which values are live into a block.
    allocation.live_in = liveness.live_in;
    Ok(allocation)
}

struct Allocator<'a, 'w> {
    graph: &'a Graph<'a>,
    registers: &'a RegisterSet,
    liveness: &'a ValueLiveness<'w>,
    allocation: Allocation<'w>,
    contents: Contents,
    /// Bit mask of registers in `contents` that currently hold a value.
    occupied: RegisterMask,
    /// The register each value is in, if any.
    value_registers: ZoneVec<'w, Option<u8>>,
    /// Compact occupied `(register, value)` entries at the end of finished blocks.
    end_entries: ZoneVec<'w, (u8, NodeId)>,
    /// Range in `end_entries` for each finished block.
    end_runs: ZoneVec<'w, Run>,
    /// Whether all cold blocks come after all hot blocks, allowing cold-only lazy spilling without
    /// inserting eager definition-site stores on the hot path.
    use_cold_spills: bool,
    /// Whether the block currently being allocated is cold.
    current_block_is_cold: bool,
    /// Per-cold-block lazily spilled stack homes for values that have no permanent home.
    cold_homes: ZoneVec<'w, Option<u32>>,
    /// Values whose `cold_homes` entry was set in the current block (for fast O(touched) clearing).
    cold_home_touched: ZoneVec<'w, NodeId>,
    /// Compact `(value, stack_slot)` cold homes live out of finished cold blocks (disjoint from `end_entries`).
    cold_home_end_entries: ZoneVec<'w, (NodeId, u32)>,
    /// Range in `cold_home_end_entries` for each finished block.
    cold_home_end_runs: ZoneVec<'w, Run>,
    /// Frozen non-phi `(register, value)` entries kept in registers on entry to a merge/loop header with a state
    /// predecessor, shared verbatim by `place_merge_phis`, `start_block`, and `add_merge_moves`.
    merge_entries: ZoneVec<'w, (u8, NodeId)>,
    /// Range in `merge_entries` for each block with `merge_recorded[block] == true`.
    merge_runs: ZoneVec<'w, Run>,
    /// Whether `merge_runs[block]` has been frozen for `block`.
    merge_recorded: ZoneVec<'w, bool>,
    /// Whether each block is finished.
    finished: ZoneVec<'w, bool>,
    /// The position of the node being allocated.
    position: u32,
    /// The spill slot class of each value (see `coalescing`), and the
    /// slot of each class that has one.
    slot_classes: &'a [u32],
    class_slots: ZoneVec<'w, Option<u32>>,
    /// Moves that happen in parallel, before they are put in an order.
    parallel_moves: ZoneVec<'w, Move>,
    loops: &'a Loops<'w>,
    work: &'w Zone,
}

impl<'w> Allocator<'_, 'w> {
    fn run(mut self) -> Result<Allocation<'w>, CompileFailure> {
        // Give the phis of blocks without a state predecessor their locations
        // up front, since their predecessors may be allocated after them.
        // Phis of loop headers live in registers, which the header starts
        // with (it starts with every other value in its stack slot); the
        // others live in stack slots.
        for (index, block) in self.graph.blocks.iter().enumerate() {
            // NB: The phis of merges with a state predecessor get their
            //     locations when it ends (see `place_merge_phis()`).
            if state_predecessor(self.graph, BlockId::from_index(index)).is_some() {
                continue;
            }
            let is_loop_header = block
                .predecessors
                .iter()
                .any(|predecessor| predecessor.index() >= index);
            let mut general_free = self.registers.allocatable(RegisterClass::General).0;
            let mut float_free = self.registers.allocatable(RegisterClass::Float).0;
            let general_caller = self.registers.caller_saved_for(RegisterClass::General).0;
            let float_caller = self.registers.caller_saved_for(RegisterClass::Float).0;
            for phi in &block.phis {
                let register = if is_loop_header {
                    let prefer_callee_saved = self.prefers_callee_saved(*phi);
                    let (free, caller) = match RegisterClass::of(self.graph.node(*phi).repr) {
                        RegisterClass::General => (&mut general_free, general_caller),
                        RegisterClass::Float => (&mut float_free, float_caller),
                    };
                    if *free == 0 {
                        None
                    } else {
                        let preferred = if prefer_callee_saved {
                            *free & !caller
                        } else {
                            *free & caller
                        };
                        let picked = if preferred != 0 {
                            preferred.trailing_zeros() as u8
                        } else {
                            free.trailing_zeros() as u8
                        };
                        *free &= !(1u64 << picked);
                        Some(picked)
                    }
                } else {
                    None
                };
                self.place_phi(*phi, register);
            }
        }

        // Values live into blocks reached over back edges may be defined
        // after them in linear order; give them their slots up front too.
        for (index, block) in self.graph.blocks.iter().enumerate() {
            let block_id = BlockId::from_index(index);
            if block
                .predecessors
                .iter()
                .any(|predecessor| predecessor.index() >= index)
                && state_predecessor(self.graph, block_id).is_none()
            {
                let liveness = self.liveness;
                for value in liveness.live_in.get(index) {
                    self.ensure_home(*value);
                }
            }
        }

        let mut cycle_temp = None;
        for block in 0..self.graph.blocks.len() {
            let block = BlockId::from_index(block);
            self.start_block(block);
            let block_data = self.graph.block(block);
            for node in &block_data.body {
                self.allocate_node(*node);
            }
            self.allocation.block_end_moves[block.index()] = self.phi_moves(block, &mut cycle_temp);
            self.allocate_node(self.graph.control_id(block));
            let end_start = self.end_entries.len();
            for register in self.occupied.iter() {
                self.end_entries.push((register, self.contents[register as usize]));
            }
            self.end_runs[block.index()] = Run::new(end_start, self.end_entries.len());
            if self.current_block_is_cold && !self.cold_home_touched.is_empty() {
                let cold_start = self.cold_home_end_entries.len();
                let live_out = self.liveness.live_out.get(block.index());
                for &value in &self.cold_home_touched {
                    if self.value_registers[value.index()].is_none()
                        && let Some(slot) = self.cold_homes[value.index()]
                        && live_out.binary_search(&value).is_ok()
                    {
                        self.cold_home_end_entries.push((value, slot));
                    }
                }
                self.cold_home_end_runs[block.index()] = Run::new(cold_start, self.cold_home_end_entries.len());
            }
            self.finished[block.index()] = true;
            // NB: How many moves and exit values a node gets is known only when it is allocated.
            check_items(self.allocation.moves.len())?;
            check_items(self.allocation.exit_values.len())?;
        }
        Ok(self.allocation)
    }

    /// Whether holding `value` in `register` will be clobbered by a call or float-clobbering slow path it crosses.
    fn clobbered_in(&self, value: NodeId, register: u8) -> bool {
        if self.use_cold_spills {
            (self.liveness.crosses_hot_call(value) && self.registers.caller_saved().contains(register))
                || (self.liveness.crosses_hot_float_clobber(value)
                    && self.registers.caller_saved_fprs().contains(register))
        } else {
            (self.liveness.crosses_call(value) && self.registers.caller_saved().contains(register))
                || (self.liveness.crosses_float_clobber(value)
                    && self.registers.caller_saved_fprs().contains(register))
        }
    }

    /// Whether `value` needs a spill/frame home from its definition when allocated in `register`.
    fn needs_home_in(&self, value: NodeId, register: u8) -> bool {
        self.liveness.needs_spill(value) || self.clobbered_in(value, register)
    }

    /// Whether `value` benefits from a callee-saved register because it lives across a call or float clobber
    /// in a non-cold block.
    fn prefers_callee_saved(&self, value: NodeId) -> bool {
        self.liveness.crosses_hot_call(value)
            || (RegisterClass::of(self.graph.node(value).repr) == RegisterClass::Float
                && self.liveness.crosses_hot_float_clobber(value))
    }

    /// Gives `phi` its location: `register` if it has one, and its spill
    /// slot (which it may have already, if it is live into a block
    /// allocated before its own) if it has no register or needs a slot.
    fn place_phi(&mut self, phi: NodeId, register: Option<u8>) {
        let slot = match register {
            Some(register)
                if !self.needs_home_in(phi, register) && self.allocation.nodes[phi.index()].spill_slot.is_none() =>
            {
                None
            }
            _ => Some(self.ensure_spill_slot(phi)),
        };
        let node = &mut self.allocation.nodes[phi.index()];
        node.output = Some(match (register, slot) {
            (Some(register), _) => Location::Register(register),
            (None, Some(slot)) => Location::Stack(slot),
            (None, None) => unreachable!("phis without a register have a slot"),
        });
        node.spill_slot = slot;
    }

    fn new_spill_slot(&mut self) -> u32 {
        let slot = self.allocation.spill_slot_count;
        self.allocation.spill_slot_count += 1;
        slot
    }

    fn ensure_spill_slot(&mut self, value: NodeId) -> u32 {
        if let Some(slot) = self.allocation.nodes[value.index()].spill_slot {
            return slot;
        }
        let class = self.slot_classes[value.index()] as usize;
        let slot = match self.class_slots[class] {
            Some(slot) => slot,
            None => {
                let slot = self.new_spill_slot();
                self.class_slots[class] = Some(slot);
                slot
            }
        };
        self.allocation.nodes[value.index()].spill_slot = Some(slot);
        slot
    }

    /// Makes sure that `value` is available outside of registers: in its frame slot (see `frame_home()`), or in a
    /// spill slot.
    fn ensure_home(&mut self, value: NodeId) {
        if frame_home(self.graph, value).is_none() {
            self.ensure_spill_slot(value);
        }
    }

    fn set_cold_home(&mut self, value: NodeId, slot: u32) {
        if self.cold_homes[value.index()].is_none() {
            self.cold_home_touched.push(value);
        }
        self.cold_homes[value.index()] = Some(slot);
    }

    /// Spills `value` (currently in `register`) lazily inside a cold block so hot definitions never pay
    /// eager def-site stores for cold-only calls or evictions.
    fn ensure_home_from_register(&mut self, value: NodeId, register: u8) {
        if frame_home(self.graph, value).is_some() || self.allocation.nodes[value.index()].spill_slot.is_some() {
            return;
        }
        if self.use_cold_spills && self.current_block_is_cold {
            if self.cold_homes[value.index()].is_none() {
                let slot = self.new_spill_slot();
                self.set_cold_home(value, slot);
                self.add_move(Location::Register(register), Location::Stack(slot));
            }
            return;
        }
        self.ensure_spill_slot(value);
    }

    fn clear_contents(&mut self) {
        for register in self.occupied.iter() {
            let value = self.contents[register as usize];
            self.value_registers[value.index()] = None;
        }
        self.occupied = RegisterMask::default();
        for value in self.cold_home_touched.drain_all() {
            self.cold_homes[value.index()] = None;
        }
    }

    /// Whether the loop with the header `block`, if it is one, leaves `value` in its stack slot: a value that has one
    /// before the loop starts and that the loop only carries through, so that its register is free for the loop.
    fn loop_leaves_in_stack(&self, block: BlockId, value: NodeId) -> bool {
        self.loops
            .headed_by(block)
            .is_some_and(|loop_| self.loops.carries_through(loop_, value))
            && (self.allocation.nodes[value.index()].spill_slot.is_some() || frame_home(self.graph, value).is_some())
    }

    fn start_block(&mut self, block: BlockId) {
        self.clear_contents();
        let block_data = self.graph.block(block);
        self.current_block_is_cold = block_data.is_cold;
        let live_in = self.liveness.live_in.get(block.index());
        if let Some(predecessor) = state_predecessor(self.graph, block) {
            assert!(
                self.finished[predecessor.index()],
                "blocks come after the predecessor whose state they start with"
            );
            if self.merge_recorded[block.index()] {
                let run = self.merge_runs[block.index()].range();
                for entry_index in run {
                    let (register, value) = self.merge_entries[entry_index];
                    self.assign_register(register, value);
                }
            } else {
                let start = self.merge_entries.len();
                if !live_in.is_empty() {
                    let phis = &block_data.phis;
                    let run = self.end_runs[predecessor.index()].range();
                    for entry_index in run {
                        let (register, value) = self.end_entries[entry_index];
                        if live_in.binary_search(&value).is_ok()
                            && !self.loop_leaves_in_stack(block, value)
                            && !phis.contains(&value)
                        {
                            self.merge_entries.push((register, value));
                            self.assign_register(register, value);
                        }
                    }
                }
                self.merge_runs[block.index()] = Run::new(start, self.merge_entries.len());
                self.merge_recorded[block.index()] = true;
            }
            if !live_in.is_empty() {
                let phis = &block_data.phis;
                let cold_run = self.cold_home_end_runs[predecessor.index()].range();
                for entry_index in cold_run {
                    let (value, slot) = self.cold_home_end_entries[entry_index];
                    if live_in.binary_search(&value).is_ok() && !phis.contains(&value) {
                        self.set_cold_home(value, slot);
                    }
                }
            }
            for phi in &block_data.phis {
                if let Some(Location::Register(register)) = self.allocation.nodes[phi.index()].output {
                    debug_assert!(!self.occupied.contains(register));
                    self.assign_register(register, *phi);
                }
            }
        } else if let [predecessor] = block_data.predecessors.as_slice()
            && predecessor.index() < block.index()
        {
            assert!(
                self.finished[predecessor.index()],
                "blocks come after the predecessor whose state they start with"
            );
            if !live_in.is_empty() {
                let run = self.end_runs[predecessor.index()].range();
                for entry_index in run {
                    let (register, value) = self.end_entries[entry_index];
                    if live_in.binary_search(&value).is_ok() {
                        self.assign_register(register, value);
                    }
                }
                let cold_run = self.cold_home_end_runs[predecessor.index()].range();
                for entry_index in cold_run {
                    let (value, slot) = self.cold_home_end_entries[entry_index];
                    if live_in.binary_search(&value).is_ok() {
                        self.set_cold_home(value, slot);
                    }
                }
            }
        } else {
            // Merges and loop headers start with every value in its stack
            // slot, and phis in their registers.
            for phi in &block_data.phis {
                if let Some(Location::Register(register)) = self.allocation.nodes[phi.index()].output {
                    self.assign_register(register, *phi);
                }
            }
        }
        if let Some(first) = self.graph.block_nodes(block).next() {
            self.position = self.liveness.positions[first.index()];
        }
    }

    #[inline(always)]
    fn free_register(&mut self, register: u8) {
        if self.occupied.contains(register) {
            let value = self.contents[register as usize];
            self.value_registers[value.index()] = None;
            self.occupied.0 &= !(1u64 << register);
        }
    }

    #[inline(always)]
    fn assign_register(&mut self, register: u8, value: NodeId) {
        debug_assert!(!self.occupied.contains(register));
        self.contents[register as usize] = value;
        self.occupied = self.occupied.with(register);
        self.value_registers[value.index()] = Some(register);
    }

    /// Removes the value in `register` from it. The value stays available in
    /// its frame slot or its spill slot, which it gets if it had none.
    fn evict(&mut self, register: u8) {
        if self.occupied.contains(register) {
            let value = self.contents[register as usize];
            self.ensure_home_from_register(value, register);
            self.free_register(register);
        }
    }

    /// How soon `value` is needed again, as a key that orders the values by it (smaller is sooner). Inside a loop,
    /// the uses later in the iteration come first, then the values the next iteration reads (live into the loop and
    /// used in it before here), in the order it reads them, then the values used after the loop.
    fn next_use(&self, value: NodeId) -> (u8, u32) {
        let uses = self.liveness.use_positions(value);
        let index = uses.partition_point(|position| *position <= self.position);
        let next = uses.get(index).copied();
        let Some(loop_) = self.loops.innermost_at(self.position) else {
            return next.map_or((3, 0), |position| (0, position));
        };
        if let Some(position) = self.loops.first_use_in(loop_, uses, index..uses.len()) {
            return (0, position);
        }
        if self.liveness.is_live_in(loop_.header, value)
            && let Some(position) = self.loops.first_use_in(loop_, uses, 0..index)
        {
            return (1, position - loop_.start);
        }
        next.map_or((3, 0), |position| (2, position))
    }

    /// A register of `class` that is not `blocked`, freeing one if necessary
    /// by evicting the value used furthest in the future.
    fn pick_register(&mut self, blocked: RegisterMask, class: RegisterClass, prefer_callee_saved: bool) -> u8 {
        let candidates = self.registers.allocatable(class).0 & !blocked.0;
        let free = candidates & !self.occupied.0;
        if free != 0 {
            let caller = self.registers.caller_saved_for(class).0;
            let preferred = if prefer_callee_saved {
                free & !caller
            } else {
                free & caller
            };
            if preferred != 0 {
                return preferred.trailing_zeros() as u8;
            }
            return free.trailing_zeros() as u8;
        }
        let victim = RegisterMask(candidates)
            .iter()
            .max_by_key(|register| {
                debug_assert!(self.occupied.contains(*register), "no free register");
                let value = self.contents[*register as usize];
                (self.next_use(value), core::cmp::Reverse(*register))
            })
            .expect("a node needs more registers than the target has");
        self.evict(victim);
        victim
    }

    /// Where a value can be read from right now.
    fn current_location(&self, value: NodeId) -> Location {
        if let Some(bits) = self.graph.constant_value(value) {
            return Location::Constant(bits, constant_repr(self.graph, value));
        }
        // NB: Exits find virtual objects through their properties.
        if matches!(self.graph.node(value).op, Op::VirtualObject { .. }) {
            return Location::Constant(crate::ir::value::EMPTY, Repr::Tagged);
        }
        if let Some(register) = self.value_registers[value.index()] {
            return Location::Register(register);
        }
        if let Some(slot) = frame_home(self.graph, value) {
            return Location::Frame(slot);
        }
        if let Some(slot) = self.cold_homes[value.index()] {
            return Location::Stack(slot);
        }
        let slot = self.allocation.nodes[value.index()]
            .spill_slot
            .unwrap_or_else(|| panic!("v{} is neither in a register nor spilled", value.0));
        Location::Stack(slot)
    }

    /// Whether `value`, which `node` uses or which is in a register at
    /// `node`, is still needed after it. Registers only ever hold live values.
    fn is_live_after(&self, value: NodeId, node: NodeId) -> bool {
        !self.liveness.dies_at(node).contains(&value)
    }

    /// Whether `value` is referenced by `frame_state` (including parent frames and virtual object properties).
    fn is_in_frame_state(&self, frame_state: crate::ir::FrameStateId, value: NodeId) -> bool {
        self.graph
            .frame_state_values(frame_state, self.work)
            .into_iter()
            .any(|(_, candidate)| candidate == value)
    }

    /// Adds a move before the node that is being allocated.
    fn add_move(&mut self, from: Location, to: Location) {
        self.allocation.moves.push(Move { from, to });
    }

    fn allocate_node(&mut self, node_id: NodeId) {
        let node = self.graph.node(node_id);
        self.position = self.liveness.positions[node_id.index()];
        let constraints = lowering_info(node);
        // NB: The moves, the input locations and the exit values of the node go to the ends of their columns, where
        //     nothing else is added while the node is allocated.
        let moves_start = self.allocation.moves.len();
        let inputs_start = self.allocation.locations.len();
        self.allocation
            .locations
            .resize(inputs_start + node.inputs.len(), Location::Register(0));
        let constrained_inputs = node.inputs.len().min(constraints.inputs.len());
        let constraint_of = |allocator: &Self, index: usize| {
            allocator.registers.resolve(
                constraints.inputs.get(index),
                allocator.graph.constant_value(node.inputs[index]).is_some(),
            )
        };
        let mut blocked = RegisterMask::default();

        let clobbered = match constraints.clobbers {
            Clobbers::CallerSaved => self.registers.caller_saved(),
            Clobbers::Floats => self.registers.caller_saved_fprs(),
            Clobbers::Nothing => RegisterMask::default(),
        };

        // Fixed register inputs first, so that inputs in any register do not
        // take their registers.
        for index in 0..constrained_inputs {
            let input = node.inputs[index];
            let InputConstraint::Fixed(register) = constraint_of(self, index) else {
                continue;
            };
            let register = self.registers.fixed(register);
            let is_value = is_allocated_value(self.graph, input);
            if !is_value || self.value_registers[input.index()] != Some(register) {
                debug_assert!(!blocked.contains(register), "two inputs need the same register");
                self.evict(register);
                let from = self.current_location(input);
                self.add_move(from, Location::Register(register));
                match from {
                    // The value moves, unless an earlier input of this node
                    // needs it where it is, or moving it from a preserved
                    // callee-saved register into a clobbered register would
                    // lose it across this node while it stays live.
                    Location::Register(previous)
                        if !blocked.contains(previous)
                            && !(clobbered.contains(register)
                                && !clobbered.contains(previous)
                                && (self.is_live_after(input, node_id) || node.frame_state.is_some())) =>
                    {
                        self.free_register(previous);
                        self.assign_register(register, input);
                    }
                    Location::Stack(_) => self.assign_register(register, input),
                    _ => {}
                }
            }
            blocked = blocked.with(register);
            self.allocation.locations[inputs_start + index] = Location::Register(register);
        }
        for index in 0..constrained_inputs {
            let input = node.inputs[index];
            let constraint = constraint_of(self, index);
            if constraint == InputConstraint::RegisterOrConstant
                && let Some(bits) = self.graph.constant_value(input)
            {
                self.allocation.locations[inputs_start + index] =
                    Location::Constant(bits, constant_repr(self.graph, input));
                continue;
            }
            if constraint == InputConstraint::Anywhere {
                self.allocation.locations[inputs_start + index] = self.current_location(input);
                continue;
            }
            if matches!(constraint, InputConstraint::Fixed(_)) {
                continue;
            }
            let register = match self.value_registers[input.index()] {
                Some(register) if is_allocated_value(self.graph, input) => register,
                _ => {
                    let prefer_callee_saved = is_allocated_value(self.graph, input)
                        && self.is_live_after(input, node_id)
                        && self.prefers_callee_saved(input);
                    let register = self.pick_register(
                        blocked,
                        RegisterClass::of(self.graph.node(input).repr),
                        prefer_callee_saved,
                    );
                    let from = self.current_location(input);
                    self.add_move(from, Location::Register(register));
                    if is_allocated_value(self.graph, input) {
                        self.assign_register(register, input);
                    }
                    register
                }
            };
            blocked = blocked.with(register);
            self.allocation.locations[inputs_start + index] = Location::Register(register);
        }

        let temp_count = usize::from(constraints.temps);
        assert!(
            temp_count <= MAX_TEMPS,
            "a node has more temps than an allocation has room for"
        );
        let mut temps = [0; MAX_TEMPS];
        for temp in &mut temps[..temp_count] {
            let register = self.pick_register(blocked, RegisterClass::General, false);
            blocked = blocked.with(register);
            *temp = register;
        }

        // NB: Two-address instructions (like those of x86) write their output
        //     over their first input, which then needs no copy where it dies
        //     here. Nodes that may exit keep their inputs for the exit.
        let output_class = RegisterClass::of(node.repr);
        let output_prefers_callee_saved = self.prefers_callee_saved(node_id);
        let first_input = self.allocation.locations[inputs_start..].first().copied();
        let first_input_register = match first_input {
            Some(Location::Register(register))
                if node.frame_state.is_none()
                    && constraints.reuses_first_input
                    && is_allocated_value(self.graph, node.inputs[0])
                    && self.liveness.dies_at(node_id).contains(&node.inputs[0])
                    && RegisterClass::of_register(register) == output_class
                    && !temps[..temp_count].contains(&register)
                    && (!output_prefers_callee_saved
                        || !self.registers.caller_saved_for(output_class).contains(register)) =>
            {
                Some(register)
            }
            _ => None,
        };

        // Pick the output register before recording where exits find their
        // values: a node may exit after writing its output (a check of a value
        // it loaded), so no exit value may stay in that register.
        let pick_output = |allocator: &mut Self| match constraints.output {
            OutputConstraint::None => None,
            OutputConstraint::Register => Some(first_input_register.unwrap_or_else(|| {
                allocator.pick_register(blocked, output_class, output_prefers_callee_saved)
            })),
            OutputConstraint::Return => {
                let register = allocator.registers.return_gpr;
                debug_assert!(constraints.clobbers == Clobbers::CallerSaved || !blocked.contains(register));
                allocator.evict(register);
                Some(register)
            }
        };
        {
            for register in RegisterMask(clobbered.0 & self.occupied.0).iter() {
                let value = self.contents[register as usize];
                if self.is_live_after(value, node_id)
                    || node
                        .frame_state
                        .is_some_and(|frame_state| self.is_in_frame_state(frame_state, value))
                {
                    self.ensure_home_from_register(value, register);
                }
                self.free_register(register);
            }
        }
        let output = pick_output(self);
        // Lazy exits of calls happen after the call returns, so they find
        // their values after the clobber.
        let exit_values_start = self.allocation.exit_values.len();
        if let Some(frame_state) = node.frame_state {
            for (slot, value) in self.graph.frame_state_values(frame_state, self.work) {
                let location = self.current_location(value);
                self.allocation.exit_values.push((slot, location));
            }
        }

        // Values whose last use is this node no longer need their registers.
        let liveness = self.liveness;
        for value in liveness.dies_at(node_id) {
            if let Some(register) = self.value_registers[value.index()] {
                self.free_register(register);
            }
        }

        if let Some(register) = output {
            self.assign_register(register, node_id);
            if self.needs_home_in(node_id, register) {
                self.ensure_home(node_id);
            }
            if self.liveness.is_dead_output(node_id) {
                self.free_register(register);
            }
        }

        let moves_before = Run::new(moves_start, self.allocation.moves.len());
        let inputs = Run::new(inputs_start, self.allocation.locations.len());
        let exit_values = Run::new(exit_values_start, self.allocation.exit_values.len());
        let allocation = &mut self.allocation.nodes[node_id.index()];
        allocation.moves_before = moves_before;
        allocation.inputs = inputs;
        allocation.temps = temps;
        allocation.temp_count = constraints.temps;
        allocation.output = output.map(Location::Register);
        allocation.exit_values = exit_values;
    }

    /// Gives the phis of the merge `join` their locations, at the end of its
    /// state predecessor: registers that hold no value live into the merge
    /// (preferably the input's own), or stack slots if there are none.
    fn place_merge_phis(&mut self, predecessor: BlockId, join: BlockId) {
        let mut taken = RegisterMask::default();
        let start = self.merge_entries.len();
        let phis = &self.graph.block(join).phis;
        for register in self.occupied.iter() {
            let value = self.contents[register as usize];
            if self.liveness.is_live_in(join, value)
                && !self.loop_leaves_in_stack(join, value)
                && !phis.contains(&value)
            {
                taken = taken.with(register);
                self.merge_entries.push((register, value));
            }
        }
        self.merge_runs[join.index()] = Run::new(start, self.merge_entries.len());
        self.merge_recorded[join.index()] = true;
        for (phi, input) in liveness::phi_inputs_from(self.graph, predecessor, join) {
            let class = RegisterClass::of(self.graph.node(phi).repr);
            let candidates = self.registers.allocatable(class).0 & !taken.0;
            let free = candidates & !self.occupied.0;
            let prefer_callee_saved = self.prefers_callee_saved(phi);
            let caller = self.registers.caller_saved_for(class).0;
            let own = self.value_registers[input.index()].filter(|register| {
                is_allocated_value(self.graph, input)
                    && !taken.contains(*register)
                    && !(prefer_callee_saved && ((caller & (1u64 << register)) != 0) && ((free & !caller) != 0))
            });
            let register = own.or_else(|| {
                if candidates == 0 {
                    return None;
                }
                let pool = if free != 0 { free } else { candidates };
                let preferred = if prefer_callee_saved {
                    pool & !caller
                } else {
                    pool & caller
                };
                Some(if preferred != 0 {
                    preferred.trailing_zeros() as u8
                } else {
                    pool.trailing_zeros() as u8
                })
            });
            self.place_phi(phi, register);
            if let Some(register) = register {
                taken = taken.with(register);
            }
        }
    }

    /// Adds the moves into the phis of `successor` at the end of `block`, as moves that happen in parallel.
    fn add_phi_moves(&mut self, block: BlockId, successor: BlockId) {
        for (phi, input) in liveness::phi_inputs_from(self.graph, block, successor) {
            let parallel_move = Move {
                from: self.current_location(input),
                to: self.allocation.nodes[phi.index()].output.expect("phis have locations"),
            };
            self.parallel_moves.push(parallel_move);
        }
    }

    /// Adds the moves at the end of `block` that put the values live into the
    /// merge `join` (and its phis' inputs) where the merge expects them, as moves that happen in parallel.
    fn add_merge_moves(&mut self, block: BlockId, join: BlockId) {
        let predecessor = state_predecessor(self.graph, join).expect("the merge has a state predecessor");
        assert!(
            self.finished[predecessor.index()],
            "blocks come after the predecessor whose state they start with"
        );
        if self.merge_recorded[join.index()] {
            let run = self.merge_runs[join.index()].range();
            for entry_index in run {
                let (register, value) = self.merge_entries[entry_index];
                let to = Location::Register(register);
                let from = self.current_location(value);
                if from != to {
                    self.parallel_moves.push(Move { from, to });
                }
            }
        } else {
            let live_in = self.liveness.live_in.get(join.index());
            if !live_in.is_empty() {
                let phis = &self.graph.block(join).phis;
                let run = self.end_runs[predecessor.index()].range();
                for entry_index in run {
                    let (register, value) = self.end_entries[entry_index];
                    if live_in.binary_search(&value).is_ok()
                        && !self.loop_leaves_in_stack(join, value)
                        && !phis.contains(&value)
                    {
                        let to = Location::Register(register);
                        let from = self.current_location(value);
                        if from != to {
                            self.parallel_moves.push(Move { from, to });
                        }
                    }
                }
            }
        }
        let cold_run = self.cold_home_end_runs[predecessor.index()].range();
        if !cold_run.is_empty() {
            let live_in = self.liveness.live_in.get(join.index());
            let phis = &self.graph.block(join).phis;
            for entry_index in cold_run {
                let (value, slot) = self.cold_home_end_entries[entry_index];
                if live_in.binary_search(&value).is_ok() && !phis.contains(&value) {
                    let to = Location::Stack(slot);
                    let from = self.current_location(value);
                    if from != to {
                        self.parallel_moves.push(Move { from, to });
                    }
                }
            }
        }
        self.add_phi_moves(block, join);
    }

    /// The moves at the end of `block` that put the inputs of the phis of its
    /// successor in place, as a part of the moves of the allocation.
    fn phi_moves(&mut self, block: BlockId, cycle_temp: &mut Option<u32>) -> Run {
        let liveness = self.liveness;
        let successors = liveness.successors.get(block.index());
        let [successor] = successors else {
            for successor in successors {
                debug_assert!(self.graph.block(*successor).phis.is_empty(), "critical edge into a phi");
            }
            return Run::default();
        };
        self.parallel_moves.clear();
        match state_predecessor(self.graph, *successor) {
            Some(predecessor) if predecessor != block => self.add_merge_moves(block, *successor),
            merge => {
                if merge.is_some() {
                    self.place_merge_phis(block, *successor);
                }
                self.add_phi_moves(block, *successor);
            }
        }
        if self.parallel_moves.is_empty() {
            return Run::default();
        }
        let temp = *cycle_temp.get_or_insert_with(|| {
            let slot = self.allocation.spill_slot_count;
            self.allocation.spill_slot_count += 1;
            slot
        });
        let start = self.allocation.moves.len();
        resolve_parallel_moves(
            &mut self.parallel_moves,
            Location::Stack(temp),
            &mut self.allocation.moves,
        );
        Run::new(start, self.allocation.moves.len())
    }
}

/// The spill slot to store `phi`, which is in a register, to at the start
/// of its block, if it has one that its inputs did not fill already. Inputs
/// sharing the phi's slot (see `coalescing`) are in it from their own
/// definition on.
pub fn phi_spill_store(graph: &Graph, allocation: &Allocation, phi: NodeId) -> Option<u32> {
    let node = allocation.node(phi);
    let (Some(Location::Register(_)), Some(slot)) = (node.output, node.spill_slot) else {
        return None;
    };
    let filled = graph
        .node(phi)
        .inputs
        .iter()
        .all(|input| allocation.node(*input).spill_slot == Some(slot));
    (!filled).then_some(slot)
}

/// Prints the allocation of `graph` as text: every node with the moves
/// before it, the locations of its inputs and output, and its spill slot.
pub fn dump_allocation<'a, 'z, 'w>(graph: &'a Graph<'z>, allocation: &'a Allocation<'w>) -> AllocationDump<'a, 'z, 'w> {
    AllocationDump { graph, allocation }
}

/// The text of `dump_allocation()`.
pub struct AllocationDump<'a, 'z, 'w> {
    graph: &'a Graph<'z>,
    allocation: &'a Allocation<'w>,
}

struct LocationName(Location);

impl fmt::Display for LocationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Location::Register(register) if register >= FPR_BASE => write!(f, "f{}", register - FPR_BASE),
            Location::Register(register) => write!(f, "r{register}"),
            Location::Stack(slot) => write!(f, "s{slot}"),
            Location::Constant(bits, Repr::Pointer) => write!(f, "#pointer:{bits:#x}"),
            Location::Constant(bits, _) => write!(f, "#{}", crate::ir::value::describe(bits)),
            Location::Frame(slot) => write!(f, "frame{slot}"),
        }
    }
}

impl fmt::Display for AllocationDump<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /// Writes the text of each item, with ", " between them.
        fn write_list<T>(
            f: &mut fmt::Formatter<'_>,
            items: &[T],
            write_item: impl Fn(&mut fmt::Formatter<'_>, &T) -> fmt::Result,
        ) -> fmt::Result {
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    f.write_str(", ")?;
                }
                write_item(f, item)?;
            }
            Ok(())
        }
        let write_move =
            |f: &mut fmt::Formatter<'_>, m: &Move| write!(f, "{} -> {}", LocationName(m.from), LocationName(m.to));
        let write_location = |f: &mut fmt::Formatter<'_>, location: &Location| write!(f, "{}", LocationName(*location));
        let (graph, allocation) = (self.graph, self.allocation);
        for index in 0..graph.blocks.len() {
            let block = BlockId::from_index(index);
            writeln!(f, "b{index}:")?;
            for node_id in graph.block_nodes(block) {
                let node = allocation.node(node_id);
                if Some(node_id) == graph.block(block).control && !allocation.block_end_moves(block).is_empty() {
                    f.write_str("  moves ")?;
                    write_list(f, allocation.block_end_moves(block), write_move)?;
                    f.write_str("\n")?;
                }
                if !node.moves_before.is_empty() {
                    f.write_str("  moves ")?;
                    write_list(f, node.moves_before, write_move)?;
                    f.write_str("\n")?;
                }
                write!(f, "  v{} {}", node_id.0, graph.node(node_id).op.name())?;
                if !node.inputs.is_empty() {
                    f.write_str(" (")?;
                    write_list(f, node.inputs, write_location)?;
                    f.write_str(")")?;
                }
                if let Some(output) = &node.output {
                    write!(f, " -> {}", LocationName(*output))?;
                }
                if !node.temps.is_empty() {
                    f.write_str(" temps ")?;
                    write_list(f, node.temps, |f, register| write!(f, "r{register}"))?;
                }
                if let Some(slot) = node.spill_slot
                    && graph.node(node_id).op != Op::Phi
                {
                    write!(f, " spill s{slot}")?;
                }
                if !node.exit_values.is_empty() {
                    f.write_str(" exit {")?;
                    write_list(f, node.exit_values, |f, (slot, location)| {
                        write!(f, "{slot}={}", LocationName(*location))
                    })?;
                    f.write_str("}")?;
                }
                f.write_str("\n")?;
            }
        }
        writeln!(f, "spill slots: {}", allocation.spill_slot_count)
    }
}

#[cfg(test)]
pub(crate) mod tests;
