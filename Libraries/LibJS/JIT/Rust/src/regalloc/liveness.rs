/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! Liveness of SSA values, the pre-pass of the register allocator.
//!
//! Constants are never live: they are rematerialized at every use. A value
//! used by a frame state is live at the node carrying it; for calls, frame
//! state uses happen after the call returns.

use ak::Zone;
use ak::ZoneVec;

use super::Size;
use super::check_items;
use crate::CompileFailure;
use crate::codegen::lowering_info::Clobbers as LoweringClobbers;
use crate::codegen::lowering_info::lowering_clobbers;
use crate::ir::BlockId;
use crate::ir::FrameStateId;
use crate::ir::Graph;
use crate::ir::NodeId;
use crate::ir::Op;
use crate::tables::Lists;
use crate::tables::Run;

/// A value that nobody uses.
const DEAD_OUTPUT: u8 = 1 << 0;
/// A value that must have a stack slot that holds it from its definition on.
const NEEDS_SPILL: u8 = 1 << 1;
/// A node that is an allocated SSA value (`is_allocated_value`).
const ALLOCATED_VALUE: u8 = 1 << 2;
/// A node whose representation is `Repr::Float64`.
const IS_FLOAT64: u8 = 1 << 3;
/// A value live across at least one `Clobbers::CallerSaved` node (or in its non-`SiteKind::Call` frame state).
const CROSSES_CALL: u8 = 1 << 4;
/// A Float64 value live across at least one `Clobbers::Floats` (or call) node.
const CROSSES_FLOAT_CLOBBER: u8 = 1 << 5;
/// A value live across at least one `Clobbers::CallerSaved` node in a non-cold block.
const CROSSES_HOT_CALL: u8 = 1 << 6;
/// A Float64 value live across at least one `Clobbers::Floats` (or call) node in a non-cold block.
const CROSSES_HOT_FLOAT_CLOBBER: u8 = 1 << 7;

/// Whether `op` emits a `SiteKind::Call` site whose inlined caller frame values are read from stack slots while the
/// callee runs (`codegen/sites.rs`).
#[inline(always)]
fn is_inlined_call_site(op: &Op) -> bool {
    matches!(
        op,
        Op::CallDirect {
            inlined_frame_bytes,
            ..
        } | Op::CallNative {
            inlined_frame_bytes,
            ..
        } | Op::CallNativeAccessor {
            inlined_frame_bytes,
            ..
        } if *inlined_frame_bytes != 0
    )
}

/// In place of the frame state uses of a frame state: they are not listed yet.
const NOT_LISTED: Run = Run::NOT_WRITTEN;

/// The liveness of the values of a graph, in tables in the work zone of a compile.
///
/// No table here is a set that grows. A graph in SSA form makes that possible: a value is live into exactly the blocks
/// that a walk up from its uses passes before it reaches the block of its definition, so one walk for each value
/// gives all the blocks at one time. The values are walked in the order of their indices, so the list of the values
/// that are live into a block is sorted as it is written.
pub(super) struct ValueLiveness<'w> {
    /// The values live into each block and out of each block, sorted.
    pub live_in: Lists<'w, NodeId>,
    live_out: Lists<'w, NodeId>,
    /// For each node, the values whose last use (on this path) it is, as a part of `dying`.
    deaths: ZoneVec<'w, Run>,
    dying: ZoneVec<'w, NodeId>,
    /// For each value, `DEAD_OUTPUT` and `NEEDS_SPILL`.
    flags: ZoneVec<'w, u8>,
    /// The position of each node in linear order.
    pub positions: ZoneVec<'w, u32>,
    /// For each value, the positions of the nodes that use it as an input, in order, as a part of `uses`. A phi input
    /// is used at the control node of its predecessor, where the moves into the phis are.
    use_runs: ZoneVec<'w, Run>,
    uses: ZoneVec<'w, u32>,
    /// The blocks each block continues at.
    pub successors: Lists<'w, BlockId>,

    // NB: Only fill() reads what is below.
    /// The block of each node.
    blocks: ZoneVec<'w, u32>,
    /// The uses of values as inputs, as the value in the high half and the position in the low half.
    use_pairs: ZoneVec<'w, u64>,
    /// Where cross-block liveness starts: the value in the high half, then the block, then whether the value is only
    /// known to flow out of the block (to a phi) in the lowest bit.
    seeds: ZoneVec<'w, u64>,
    /// What the dataflow found: the block in the high half and the value in the low half, in ascending value order.
    in_pairs: ZoneVec<'w, u64>,
    out_pairs: ZoneVec<'w, u64>,
    /// For each frame state, the allocated values that a node uses through it, as a part of `frame_uses`.
    frame_runs: ZoneVec<'w, Run>,
    frame_uses: ZoneVec<'w, NodeId>,
    /// For each frame state, the block (plus one) whose scan last expanded it.
    frame_seen_block: ZoneVec<'w, u32>,
    /// Per-value mark reused across disjoint phases: `last_seed_block` in the forward pass, `live` in the backward walk.
    node_mark: ZoneVec<'w, u32>,
    /// Per-value use counts / write cursors for linear-time use-list scattering.
    use_counts: ZoneVec<'w, u32>,
    /// Seeds bucketed by value index, avoiding comparison sort on `seeds`.
    seed_lists: Lists<'w, u32>,
    /// The values that were live at some place of the block that is walked.
    touched: ZoneVec<'w, NodeId>,
    live_since: ZoneVec<'w, Clobbers>,
}

/// Whether a node is a value the register allocator tracks.
pub(super) fn is_allocated_value(graph: &Graph, node: NodeId) -> bool {
    let node = graph.node(node);
    node.repr.is_some() && !matches!(node.op, Op::Constant(_) | Op::VirtualObject { .. })
}

/// For a merge or a loop header: the predecessor whose register state the
/// block starts with, the first one in linear order, which comes before it.
/// Every other predecessor (another one before it, a cold one or a back
/// edge after it) jumps to it, and moves its values to match that state at
/// its end. `None` for blocks with one predecessor, and for merges that
/// start with every value in its stack slot.
pub(super) fn state_predecessor(graph: &Graph, block: BlockId) -> Option<BlockId> {
    let data = graph.block(block);
    if data.predecessors.len() < 2 {
        return None;
    }
    let first = *data.predecessors.iter().min()?;
    if first >= block {
        return None;
    }
    let others_conform = data
        .predecessors
        .iter()
        .filter(|predecessor| **predecessor != first)
        .all(|predecessor| matches!(graph.control(*predecessor).op, Op::Jump { .. }));
    others_conform.then_some(first)
}

/// The input of each phi of `successor` that flows in from `block`.
pub(super) fn phi_inputs_from<'g>(
    graph: &'g Graph<'g>,
    block: BlockId,
    successor: BlockId,
) -> impl Iterator<Item = (NodeId, NodeId)> + 'g {
    let successor_block = graph.block(successor);
    let index = if successor_block.phis.is_empty() {
        0
    } else {
        successor_block
            .predecessors
            .iter()
            .position(|predecessor| *predecessor == block)
            .expect("successors list their predecessors")
    };
    successor_block
        .phis
        .iter()
        .map(move |phi| (*phi, graph.node(*phi).inputs[index]))
}

/// How many calls, and how many nodes that clobber the floating point registers (calls among them), the backwards walk
/// of the liveness computation passed.
#[derive(Clone, Copy, Default)]
struct Clobbers {
    calls: u32,
    float_clobbers: u32,
}

fn pair(high: u32, low: u32) -> u64 {
    u64::from(high) << 32 | u64::from(low)
}

fn high(pair: u64) -> u32 {
    (pair >> 32) as u32
}

fn low(pair: u64) -> u32 {
    pair as u32
}

impl<'w> ValueLiveness<'w> {
    /// The liveness of the values of `graph`, which has `size`. A compile fails if the lists of the liveness grow
    /// past their limit (see `check_items()`).
    pub fn compute(graph: &Graph, size: &Size, work: &'w Zone) -> Result<Self, CompileFailure> {
        let single_block = graph.blocks.len() <= 1;
        let mut liveness = Self {
            live_in: Lists::new_in(work),
            live_out: Lists::new_in(work),
            deaths: ZoneVec::from_elem_in(Run::default(), size.nodes, work),
            // NB: Most values die once for each use.
            dying: ZoneVec::with_capacity_in(size.inputs, work),
            flags: ZoneVec::from_elem_in(0, size.nodes, work),
            positions: ZoneVec::from_elem_in(0, size.nodes, work),
            use_runs: ZoneVec::from_elem_in(Run::default(), size.nodes, work),
            uses: ZoneVec::with_capacity_in(size.inputs, work),
            successors: Lists::with_capacity_in(size.blocks, size.edges, work),
            blocks: ZoneVec::from_elem_in(0, size.nodes, work),
            use_pairs: ZoneVec::with_capacity_in(size.inputs, work),
            seeds: if single_block {
                ZoneVec::new_in(work)
            } else {
                ZoneVec::with_capacity_in(size.inputs, work)
            },
            in_pairs: if single_block {
                ZoneVec::new_in(work)
            } else {
                ZoneVec::with_capacity_in(size.inputs.max(size.blocks), work)
            },
            out_pairs: if single_block {
                ZoneVec::new_in(work)
            } else {
                ZoneVec::with_capacity_in(size.inputs.max(size.blocks), work)
            },
            frame_runs: ZoneVec::from_elem_in(NOT_LISTED, size.frame_states, work),
            frame_uses: ZoneVec::with_capacity_in(size.frame_states.saturating_mul(4), work),
            frame_seen_block: ZoneVec::from_elem_in(0, size.frame_states, work),
            node_mark: ZoneVec::from_elem_in(0, size.nodes, work),
            use_counts: ZoneVec::from_elem_in(0, size.nodes, work),
            seed_lists: Lists::new_in(work),
            touched: ZoneVec::with_capacity_in(size.nodes.min(256), work),
            live_since: ZoneVec::from_elem_in(Clobbers::default(), size.nodes, work),
        };
        liveness.fill(graph, work)?;
        Ok(liveness)
    }

    pub fn is_live_in(&self, block: BlockId, value: NodeId) -> bool {
        self.live_in.get(block.index()).binary_search(&value).is_ok()
    }

    pub fn is_live_out(&self, block: BlockId, value: NodeId) -> bool {
        self.live_out.get(block.index()).binary_search(&value).is_ok()
    }

    /// The values whose last use (on this path) `node` is.
    pub fn dies_at(&self, node: NodeId) -> &[NodeId] {
        &self.dying[self.deaths[node.index()].range()]
    }

    /// Whether nobody uses `value`.
    pub fn is_dead_output(&self, value: NodeId) -> bool {
        self.flags[value.index()] & DEAD_OUTPUT != 0
    }

    /// Whether `value` must have a stack slot that holds it from its definition on regardless of register class:
    /// a value live into a merge without a state predecessor, or referenced by a `SiteKind::Call` frame state.
    pub fn needs_spill(&self, value: NodeId) -> bool {
        self.flags[value.index()] & NEEDS_SPILL != 0
    }

    /// Whether `value` is live across a `Clobbers::CallerSaved` node.
    pub fn crosses_call(&self, value: NodeId) -> bool {
        self.flags[value.index()] & CROSSES_CALL != 0
    }

    /// Whether `value` is live across a `Clobbers::CallerSaved` node in a non-cold block.
    pub fn crosses_hot_call(&self, value: NodeId) -> bool {
        self.flags[value.index()] & CROSSES_HOT_CALL != 0
    }

    /// Whether `value` (a Float64) is live across a `Clobbers::Floats` (or call) node.
    pub fn crosses_float_clobber(&self, value: NodeId) -> bool {
        self.flags[value.index()] & CROSSES_FLOAT_CLOBBER != 0
    }

    /// Whether `value` (a Float64) is live across a `Clobbers::Floats` (or call) node in a non-cold block.
    pub fn crosses_hot_float_clobber(&self, value: NodeId) -> bool {
        self.flags[value.index()] & CROSSES_HOT_FLOAT_CLOBBER != 0
    }

    /// The positions of the nodes that use `value` as an input, in order.
    pub fn use_positions(&self, value: NodeId) -> &[u32] {
        &self.uses[self.use_runs[value.index()].range()]
    }

    /// Lists the allocated values of the frame state `frame`, if they are not listed yet.
    /// When `!has_virtual_objects`, only `frame`'s own values are listed (parents are walked on demand
    /// and deduplicated per block so shared inlined caller frames are neither copied nor re-scanned).
    fn list_frame_uses(
        &mut self,
        graph: &Graph,
        frame: FrameStateId,
        has_virtual_objects: bool,
        work: &Zone,
    ) -> Result<(), CompileFailure> {
        if self.frame_runs[frame.index()] != NOT_LISTED {
            return Ok(());
        }
        if !has_virtual_objects {
            let own_values = &graph.frame_state(frame).values;
            let start = self.frame_uses.len();
            self.frame_uses.reserve(own_values.len());
            for &(_, value) in own_values {
                if self.flags[value.index()] & ALLOCATED_VALUE != 0 {
                    self.frame_uses.push(value);
                }
            }
            self.frame_runs[frame.index()] = Run::new(start, self.frame_uses.len());
            return check_items(self.frame_uses.len());
        }
        let start = self.frame_uses.len();
        for (_, value) in graph.frame_state_values(frame, work) {
            if self.flags[value.index()] & ALLOCATED_VALUE != 0 {
                self.frame_uses.push(value);
            }
        }
        self.frame_runs[frame.index()] = Run::new(start, self.frame_uses.len());
        // NB: How many values the frame states of a graph have in all is known only when they are listed.
        check_items(self.frame_uses.len())
    }

    /// Notes that `value` is used in `block`, at a node or (`flows_out`) by a phi of a block that `block` continues
    /// at. Fails the compile if the seeds would be past their limit (see `check_items()`).
    #[inline]
    fn add_seed(&mut self, value: NodeId, block: usize, flows_out: bool) -> Result<(), CompileFailure> {
        let value_index = value.index();
        if !flows_out {
            // NB: A use in the block of the definition makes the value live into no block.
            if self.blocks[value_index] as usize == block {
                return Ok(());
            }
            let block_mark = block as u32 + 1;
            if self.node_mark[value_index] == block_mark {
                return Ok(());
            }
            self.node_mark[value_index] = block_mark;
        }
        // NB: The frame states of the nodes give a block seeds that its inputs do not tell, so the seeds of one
        //     block alone can be past the limit.
        check_items(self.seeds.len() + 1)?;
        self.seeds
            .push(pair(value.0, (block as u32) << 1 | u32::from(flows_out)));
        Ok(())
    }

    /// Fills the tables, which `compute()` made for `graph`.
    fn fill(&mut self, graph: &Graph, work: &Zone) -> Result<(), CompileFailure> {
        let block_count = graph.blocks.len();
        let node_count = graph.nodes.len();
        let mut has_virtual_objects = false;
        for (node_index, node) in graph.nodes.iter().enumerate() {
            match &node.op {
                Op::VirtualObject { .. } => {
                    has_virtual_objects = true;
                }
                Op::Constant(_) => {}
                _ => {
                    if let Some(repr) = node.repr {
                        let mut flag = ALLOCATED_VALUE;
                        if repr == crate::code::Repr::Float64 {
                            flag |= IS_FLOAT64;
                        }
                        self.flags[node_index] = flag;
                    }
                }
            }
        }

        self.successors.begin();
        let mut position = 0;
        for (index, block) in graph.blocks.iter().enumerate() {
            for node in block.nodes() {
                self.positions[node.index()] = position;
                self.blocks[node.index()] = index as u32;
                position += 1;
            }
            graph
                .control(BlockId::from_index(index))
                .op
                .for_each_successor(|successor| self.successors.push(successor));
            self.successors.end_list();
        }

        // The uses of each value: where it is an input, and the blocks it is used in that are not its own.
        let single_block = block_count <= 1;
        for (index, block) in graph.blocks.iter().enumerate() {
            let block_mark = index as u32 + 1;
            for node_id in block.body.iter().chain(&block.control) {
                let node = graph.node(*node_id);
                let position = self.positions[node_id.index()];
                for input in &node.inputs {
                    if self.flags[input.index()] & ALLOCATED_VALUE != 0 {
                        self.use_pairs.push(pair(input.0, position));
                        self.use_counts[input.index()] += 1;
                        if !single_block {
                            self.add_seed(*input, index, false)?;
                        }
                    }
                }
                if let Some(frame) = node.frame_state {
                    if has_virtual_objects {
                        self.list_frame_uses(graph, frame, true, work)?;
                        if !single_block && self.frame_seen_block[frame.index()] != block_mark {
                            self.frame_seen_block[frame.index()] = block_mark;
                            for use_index in self.frame_runs[frame.index()].range() {
                                self.add_seed(self.frame_uses[use_index], index, false)?;
                            }
                        }
                    } else {
                        let mut current = Some(frame);
                        while let Some(id) = current {
                            if self.frame_seen_block[id.index()] == block_mark {
                                break;
                            }
                            self.frame_seen_block[id.index()] = block_mark;
                            self.list_frame_uses(graph, id, false, work)?;
                            if !single_block {
                                for use_index in self.frame_runs[id.index()].range() {
                                    self.add_seed(self.frame_uses[use_index], index, false)?;
                                }
                            }
                            current = graph.frame_state(id).parent;
                        }
                    }
                }
            }
            if !single_block && !block.phis.is_empty() {
                // Invert the phi edge scan: iterate each predecessor once in O(1) instead of searching
                // `predecessors.iter().position(...)` per `(block, successor)` pair.
                for (pred_index, &predecessor) in block.predecessors.iter().enumerate() {
                    let pred_idx = predecessor.index();
                    let end = self.positions[graph.control_id(predecessor).index()];
                    for &phi in &block.phis {
                        let input = graph.node(phi).inputs[pred_index];
                        if self.flags[input.index()] & ALLOCATED_VALUE != 0 {
                            self.use_pairs.push(pair(input.0, end));
                            self.use_counts[input.index()] += 1;
                            self.add_seed(input, pred_idx, true)?;
                        }
                    }
                }
            }
        }

        // Linear-time counting scatter: `use_pairs` is appended in ascending position order for regular inputs;
        // phi uses come from predecessors whose `end` may differ from the successor's block order, so sort each
        // value's small slice only if out of order (almost never).
        let mut offset = 0usize;
        for value in 0..node_count {
            let count = self.use_counts[value] as usize;
            if count != 0 {
                self.use_runs[value] = Run::new(offset, offset + count);
                self.use_counts[value] = offset as u32;
                offset += count;
            }
        }
        self.uses.resize(offset, 0);
        for &use_pair in &self.use_pairs {
            let value = high(use_pair) as usize;
            let slot = self.use_counts[value] as usize;
            self.use_counts[value] = slot as u32 + 1;
            self.uses[slot] = low(use_pair);
        }
        if !single_block {
            for value in 0..node_count {
                let range = self.use_runs[value].range();
                if range.len() > 1 {
                    let slice = &mut self.uses[range];
                    if slice.windows(2).any(|w| w[0] > w[1]) {
                        slice.sort_unstable();
                    }
                }
            }
        }

        if single_block {
            self.live_in.begin_counts(block_count);
            self.live_in.end_counts(NodeId(0));
            self.live_in.end_fill();
            self.live_out.begin_counts(block_count);
            self.live_out.end_counts(NodeId(0));
            self.live_out.end_fill();
        } else {
            // Bucket seeds by value index in linear time, then solve cross-block liveness in 64-value bitwise RPO chunks.
            self.seed_lists.begin_counts(node_count);
            for &seed_pair in &self.seeds {
                self.seed_lists.count(high(seed_pair) as usize);
            }
            self.seed_lists.end_counts(0);
            for &seed_pair in &self.seeds {
                self.seed_lists.fill(high(seed_pair) as usize, low(seed_pair));
            }
            self.seed_lists.end_fill();

            let mut cross_values = ZoneVec::with_capacity_in(node_count.min(self.seeds.len()), work);
            for value_index in 0..node_count {
                if !self.seed_lists.get(value_index).is_empty() {
                    cross_values.push(value_index as u32);
                }
            }
            if !cross_values.is_empty() {
                let mut has_loops = false;
                let mut last_loop_block = 0usize;
                for (b, block) in graph.blocks.iter().enumerate() {
                    for pred in &block.predecessors {
                        if pred.index() >= b {
                            has_loops = true;
                            last_loop_block = last_loop_block.max(pred.index());
                        }
                    }
                }
                let mut use_mask = ZoneVec::from_elem_in(0u64, block_count, work);
                let mut phi_out_mask = ZoneVec::from_elem_in(0u64, block_count, work);
                let mut def_mask = ZoneVec::from_elem_in(0u64, block_count, work);
                let mut live_in_mask = ZoneVec::from_elem_in(0u64, block_count, work);
                let mut live_out_mask = ZoneVec::from_elem_in(0u64, block_count, work);

                let mut chunk_start = 0usize;
                while chunk_start < cross_values.len() {
                    let chunk_end = (chunk_start + 64).min(cross_values.len());
                    let chunk = &cross_values[chunk_start..chunk_end];
                    if chunk_start != 0 {
                        use_mask.fill(0);
                        phi_out_mask.fill(0);
                        def_mask.fill(0);
                        live_in_mask.fill(0);
                        live_out_mask.fill(0);
                    }
                    for (bit, &value) in chunk.iter().enumerate() {
                        let mask = 1u64 << bit;
                        def_mask[self.blocks[value as usize] as usize] |= mask;
                        for &seed in self.seed_lists.get(value as usize) {
                            let block = (seed >> 1) as usize;
                            if seed & 1 != 0 {
                                phi_out_mask[block] |= mask;
                            } else {
                                use_mask[block] |= mask;
                            }
                        }
                    }
                    for b in (0..block_count).rev() {
                        let mut out = phi_out_mask[b];
                        for &succ in self.successors.get(b) {
                            out |= live_in_mask[succ.index()];
                        }
                        live_out_mask[b] = out;
                        live_in_mask[b] = (use_mask[b] | out) & !def_mask[b];
                    }
                    if has_loops {
                        loop {
                            let mut changed = false;
                            for b in (0..=last_loop_block).rev() {
                                let mut out = phi_out_mask[b];
                                for &succ in self.successors.get(b) {
                                    out |= live_in_mask[succ.index()];
                                }
                                let new_in = (use_mask[b] | out) & !def_mask[b];
                                if out != live_out_mask[b] || new_in != live_in_mask[b] {
                                    live_out_mask[b] = out;
                                    live_in_mask[b] = new_in;
                                    changed = true;
                                }
                            }
                            if !changed {
                                break;
                            }
                        }
                    }
                    for b in 0..block_count {
                        let mut m = live_in_mask[b];
                        while m != 0 {
                            let bit = m.trailing_zeros() as usize;
                            m &= m - 1;
                            self.in_pairs.push(pair(b as u32, chunk[bit]));
                        }
                        let mut m = live_out_mask[b];
                        while m != 0 {
                            let bit = m.trailing_zeros() as usize;
                            m &= m - 1;
                            self.out_pairs.push(pair(b as u32, chunk[bit]));
                        }
                    }
                    check_items(self.in_pairs.len())?;
                    check_items(self.out_pairs.len())?;
                    chunk_start = chunk_end;
                }
            }
            // NB: The pairs are emitted in ascending value order across chunks, so each block's list comes out sorted.
            fn lists_of_blocks(lists: &mut Lists<NodeId>, pairs: &[u64], block_count: usize) {
                lists.begin_counts(block_count);
                for pair in pairs {
                    lists.count(high(*pair) as usize);
                }
                lists.end_counts(NodeId(0));
                for pair in pairs {
                    lists.fill(high(*pair) as usize, NodeId(low(*pair)));
                }
                lists.end_fill();
            }
            lists_of_blocks(&mut self.live_in, &self.in_pairs, block_count);
            lists_of_blocks(&mut self.live_out, &self.out_pairs, block_count);
            // Reset `node_mark` before reusing it as `live` in the backwards walk.
            self.node_mark.fill(0);
        }

        // OPTIMIZATION: Rather than marking every value live across each call or float clobbering slow path there,
        //               which takes time quadratic in the size of code that keeps many values live past many of them
        //               (like a large array literal of arrays), the walk counts those nodes, notes the counts where
        //               each value becomes live, and compares them where the value is defined or the block starts.
        let mut clobbers = Clobbers::default();
        let back_mark_base = block_count as u32;
        for (index, block) in graph.blocks.iter().enumerate() {
            let hot_block = !block.is_cold;
            let block_id = BlockId::from_index(index);
            let block_start = clobbers;
            let check_spill = |flags: &mut [u8], since: Clobbers, clobbers: Clobbers, value: usize| {
                let is_float = flags[value] & IS_FLOAT64 != 0;
                if clobbers.calls != since.calls {
                    flags[value] |= CROSSES_CALL | if hot_block { CROSSES_HOT_CALL } else { 0 };
                }
                if is_float && clobbers.float_clobbers != since.float_clobbers {
                    flags[value] |= CROSSES_FLOAT_CLOBBER | if hot_block { CROSSES_HOT_FLOAT_CLOBBER } else { 0 };
                }
            };
            // Walk the block backwards from the values live at its end. A value is live where the walk is if `node_mark`
            // has the mark of the block for it.
            let mark = index as u32 + 1;
            let parent_back_mark = back_mark_base + mark;
            self.touched.clear();
            for value in self.live_out.get(index) {
                self.node_mark[value.index()] = mark;
                self.live_since[value.index()] = block_start;
                self.touched.push(*value);
            }
            let mut prev_frame: Option<FrameStateId> = None;
            for node_id in block.body.iter().chain(&block.control).rev() {
                let node = graph.node(*node_id);
                let frame_uses = match node.frame_state {
                    Some(frame) => self.frame_runs[frame.index()].range(),
                    None => 0..0,
                };

                let node_was_live = self.node_mark[node_id.index()] == mark;
                if node_was_live {
                    check_spill(
                        &mut self.flags,
                        self.live_since[node_id.index()],
                        clobbers,
                        node_id.index(),
                    );
                } else if self.flags[node_id.index()] & ALLOCATED_VALUE != 0 {
                    self.flags[node_id.index()] |= DEAD_OUTPUT;
                }
                let (is_call, clobbers_floats) = match lowering_clobbers(node) {
                    LoweringClobbers::CallerSaved => (true, false),
                    // NB: Their slow paths clobber the floating point registers.
                    LoweringClobbers::Floats => (false, true),
                    LoweringClobbers::Nothing => (false, false),
                };
                if is_call || clobbers_floats {
                    clobbers.float_clobbers += 1;
                    clobbers.calls += u32::from(is_call);
                    // Values in `SiteKind::Call` frame states are read from stack slots while the callee runs,
                    // so they must always have a stack home (`NEEDS_SPILL`). Other call/float-clobber frame states
                    // read from `RegisterDump` on exit, so callee-saved registers remain valid there.
                    let requires_stack_home = is_inlined_call_site(&node.op);
                    let mark_frame_clobber = |flags: &mut [u8], value: NodeId| {
                        if value == *node_id {
                            return;
                        }
                        let flag = &mut flags[value.index()];
                        if requires_stack_home {
                            *flag |= NEEDS_SPILL;
                        }
                        if is_call {
                            *flag |= CROSSES_CALL | if hot_block { CROSSES_HOT_CALL } else { 0 };
                        }
                        if *flag & IS_FLOAT64 != 0 {
                            *flag |= CROSSES_FLOAT_CLOBBER | if hot_block { CROSSES_HOT_FLOAT_CLOBBER } else { 0 };
                        }
                    };
                    for &value in &self.frame_uses[frame_uses.clone()] {
                        mark_frame_clobber(&mut self.flags, value);
                    }
                    if !has_virtual_objects && let Some(frame) = node.frame_state {
                        let mut parent = graph.frame_state(frame).parent;
                        while let Some(id) = parent {
                            if !requires_stack_home && self.frame_seen_block[id.index()] == parent_back_mark {
                                break;
                            }
                            for &value in &self.frame_uses[self.frame_runs[id.index()].range()] {
                                mark_frame_clobber(&mut self.flags, value);
                            }
                            parent = graph.frame_state(id).parent;
                        }
                    }
                }
                let deaths = self.dying.len();
                self.node_mark[node_id.index()] = 0;
                let mut self_used = false;
                let mut mark_use = |value: NodeId| {
                    if value == *node_id {
                        if !self_used {
                            self_used = true;
                            if !node_was_live {
                                self.dying.push(value);
                                self.live_since[value.index()] = clobbers;
                            }
                        }
                        return;
                    }
                    if self.node_mark[value.index()] != mark {
                        self.node_mark[value.index()] = mark;
                        self.dying.push(value);
                        self.live_since[value.index()] = clobbers;
                        self.touched.push(value);
                    }
                };
                for &input in &node.inputs {
                    if self.flags[input.index()] & ALLOCATED_VALUE != 0 {
                        mark_use(input);
                    }
                }
                // Consecutive nodes sharing a ResumeAt frame state already have every value of that frame state
                // marked live from the immediately following node, and cannot reference their own result.
                let skip_frame_uses = match node.frame_state {
                    Some(frame) if prev_frame == Some(frame) => {
                        !node_was_live
                            || matches!(
                                graph.frame_states[frame.index()].mode,
                                crate::code::ResumeMode::ResumeAt
                            )
                    }
                    _ => false,
                };
                if !skip_frame_uses {
                    for &value in &self.frame_uses[frame_uses] {
                        mark_use(value);
                    }
                    if !has_virtual_objects && let Some(frame) = node.frame_state {
                        let mut parent = graph.frame_state(frame).parent;
                        while let Some(id) = parent {
                            if self.frame_seen_block[id.index()] == parent_back_mark {
                                break;
                            }
                            self.frame_seen_block[id.index()] = parent_back_mark;
                            for &value in &self.frame_uses[self.frame_runs[id.index()].range()] {
                                mark_use(value);
                            }
                            parent = graph.frame_state(id).parent;
                        }
                    }
                }
                prev_frame = node.frame_state;
                if self_used {
                    self.node_mark[node_id.index()] = mark;
                    self.touched.push(*node_id);
                }
                self.deaths[node_id.index()] = Run::new(deaths, self.dying.len());
            }
            // NB: The phis of the block are live from its start if anything uses them.
            for phi in &block.phis {
                if self.node_mark[phi.index()] == mark {
                    check_spill(&mut self.flags, self.live_since[phi.index()], clobbers, phi.index());
                    self.node_mark[phi.index()] = 0;
                }
            }
            // NB: `touched` may have a value more than one time, which does no harm here.
            for value in &self.touched {
                if self.node_mark[value.index()] == mark {
                    check_spill(&mut self.flags, self.live_since[value.index()], clobbers, value.index());
                }
            }
            // NB: Most merges start with the register state of one of their
            //     predecessors (see `state_predecessor()`).
            if !single_block {
                let predecessors = &block.predecessors;
                if (predecessors.len() > 1 || predecessors.first().is_some_and(|predecessor| *predecessor >= block_id))
                    && state_predecessor(graph, block_id).is_none()
                {
                    for value in self.live_in.get(index) {
                        self.flags[value.index()] |= NEEDS_SPILL;
                    }
                }
            }
            // NB: The frame states of the nodes give a block deaths that its inputs do not tell.
            check_items(self.dying.len())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::{Repr, ResumeMode};
    use crate::ir::{Block, FrameState};

    #[test]
    fn shared_frames_and_duplicate_uses_have_one_last_use_per_value() {
        let mut graph = Graph::default();
        graph.add_block(Block::default());
        let value = graph.add(Op::LoadSlot { slot: 0 }, &[], Some(Repr::Tagged), 0);
        graph.blocks[0].body.push(value);
        let frame = graph.add_frame_state(FrameState {
            executable: 0,
            pc: 0,
            mode: ResumeMode::ResumeAt,
            values: ak::ZoneVec::from_slice_in(&[(0, value), (1, value)], crate::test_zone()),
            in_frame: ak::ZoneVec::new_in(crate::test_zone()),
            parent: None,
            passed_argument_count: None,
            legacy_arguments: 0..0,
        });
        let mut checks = Vec::new();
        for _ in 0..2 {
            let check = graph.add(Op::CheckObject, &[value], Some(Repr::Tagged), 0);
            graph.nodes[check.index()].frame_state = Some(frame);
            graph.blocks[0].body.push(check);
            checks.push(check);
        }
        let constant = graph.add(Op::Constant(0), &[], Some(Repr::Tagged), 0);
        let returned = graph.add(Op::Return, &[constant], None, 0);
        graph.blocks[0].control = Some(returned);
        let size = Size::of(&graph).unwrap();
        let liveness = ValueLiveness::compute(&graph, &size, crate::test_zone()).unwrap();
        assert!(liveness.dies_at(checks[0]).is_empty());
        assert_eq!(liveness.dies_at(checks[1]), [value]);
        assert!(!liveness.is_dead_output(value));
        assert!(liveness.live_in.get(0).is_empty());
    }

    #[test]
    fn many_blocks_with_short_live_ranges() {
        let mut graph = Graph::default();
        let block_count = 8192;
        let mut values = Vec::new();
        for index in 0..block_count {
            let mut block = Block::default();
            let value = if index == 0 {
                graph.add(Op::LoadSlot { slot: 0 }, &[], Some(Repr::Tagged), 0)
            } else {
                block.predecessors.push(BlockId::from_index(index - 1));
                graph.add(Op::CheckObject, &[values[index - 1]], Some(Repr::Tagged), 0)
            };
            values.push(value);
            block.body.push(value);
            block.control = Some(if index + 1 == block_count {
                graph.add(Op::Return, &[value], None, 0)
            } else {
                graph.add(
                    Op::Jump {
                        target: BlockId::from_index(index + 1),
                    },
                    &[],
                    None,
                    0,
                )
            });
            graph.add_block(block);
        }
        let size = Size::of(&graph).unwrap();
        // NB: The second time, the zone holds what the first time left in it.
        for _ in 0..2 {
            let liveness = ValueLiveness::compute(&graph, &size, crate::test_zone()).unwrap();
            assert!(liveness.live_in.get(0).is_empty());
            assert!(liveness.live_out.get(block_count - 1).is_empty());
            for index in 1..block_count {
                assert_eq!(liveness.live_in.get(index), [values[index - 1]]);
                assert_eq!(liveness.live_out.get(index - 1), [values[index - 1]]);
                assert!(liveness.is_live_in(BlockId::from_index(index), values[index - 1]));
                assert!(!liveness.is_live_out(BlockId::from_index(index), values[index - 1]));
                assert_eq!(liveness.dies_at(values[index]), [values[index - 1]]);
                assert!(!liveness.is_dead_output(values[index - 1]));
            }
            assert_eq!(
                liveness.dies_at(graph.blocks[block_count - 1].control.unwrap()),
                [values[block_count - 1]]
            );
        }
    }
}
