/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! Global value numbering of pure nodes: a node that computes the same
//! operation on the same inputs as a node dominating it is replaced by that
//! node. Nodes that read memory are left to check elimination, which knows
//! when memory changes.
//!
//! Nodes that may exit qualify too: the dominating node made the same check
//! on the same values, so this one cannot fail.

use ak::Zone;
use ak::ZoneBitSet;
use ak::ZoneVec;
use core::hash::Hash;
use core::hash::Hasher;
use core::mem::discriminant;

use super::dominators::CachedDominators;
use super::edit;
use super::edit::Replacements;
use crate::ir::BlockId;
use crate::ir::Graph;
use crate::ir::Node;
use crate::ir::NodeId;
use crate::ir::Op;
use ak::FastHasher;

const EMPTY_NODE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Bucket {
    hash: u64,
    head: u32,
}

#[inline(always)]
fn is_numbered(op: &Op) -> bool {
    !matches!(op, Op::Constant(_) | Op::Phi) && op.meta().is_gvn_pure()
}

#[inline(always)]
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^ (x >> 32)
}

#[inline(always)]
fn node_hash(node: &Node, replacements: &Replacements) -> u64 {
    let mut hasher = FastHasher::default();
    discriminant(&node.op).hash(&mut hasher);
    let mut h = hasher.finish() ^ (node.inputs.len() as u64);
    if replacements.is_empty() {
        for input in &node.inputs {
            h = mix64(h ^ u64::from(input.0));
        }
    } else {
        for input in &node.inputs {
            h = mix64(h ^ u64::from(replacements.resolve(*input).0));
        }
    }
    h
}

pub fn run_with_dominators(graph: &mut Graph, work: &Zone, dominators: &mut CachedDominators) {
    run_with_hash(graph, work, dominators, node_hash);
}

fn run_with_hash(
    graph: &mut Graph,
    work: &Zone,
    dominators: &mut CachedDominators,
    hash: impl Fn(&Node, &Replacements) -> u64,
) {
    let mut numbered_count = 0usize;
    for block in &graph.blocks {
        for &node in &block.body {
            if is_numbered(&graph.node(node).op) {
                numbered_count += 1;
            }
        }
    }
    if numbered_count < 2 {
        return;
    }
    let dominators = dominators.get_or_compute(graph);
    let mut replacements = Replacements::new(graph, work);
    let mut removed = ZoneBitSet::new_in(graph.nodes.len(), work);

    // Pre-sized non-growing open-addressed table keyed on the node hash.
    // Leaving a dominator subtree unwinds insertions in strict LIFO order, so
    // restoring each bucket's previous head preserves linear-probing invariants
    // without tombstones, rehashing, or re-hashing on removal.
    let bucket_count = (numbered_count * 2).next_power_of_two().max(16);
    let mask = bucket_count - 1;
    let mut buckets = ZoneVec::from_elem_in(
        Bucket {
            hash: 0,
            head: EMPTY_NODE,
        },
        bucket_count,
        work,
    );
    let mut previous = ZoneVec::from_elem_in(EMPTY_NODE, graph.nodes.len(), work);
    let mut added: ZoneVec<(u32, u32)> = ZoneVec::with_capacity_in(numbered_count, work);

    // Iterative preorder walk of the dominator tree; leaving a block drops
    // what its nodes made available.
    enum Step {
        Enter(BlockId),
        Leave(usize),
    }
    let mut steps = ZoneVec::with_capacity_in(graph.blocks.len() * 2, work);
    for block in (0..graph.blocks.len()).rev().map(BlockId::from_index) {
        if dominators.is_reachable(block) && dominators.idom(block).is_none() {
            steps.push(Step::Enter(block));
        }
    }
    while let Some(step) = steps.pop() {
        let block = match step {
            Step::Enter(block) => block,
            Step::Leave(checkpoint) => {
                while added.len() > checkpoint {
                    let (slot, prev_head) = added.pop().expect("the scope added a node");
                    buckets[slot as usize].head = prev_head;
                }
                continue;
            }
        };
        let children = dominators.children(block);
        let is_leaf = children.is_empty();
        let checkpoint = added.len();
        let body = &graph.block(block).body;
        let last_numbered_in_leaf = if is_leaf {
            body.iter().rposition(|node_id| is_numbered(&graph.node(*node_id).op))
        } else {
            None
        };
        for (body_index, &node_id) in body.iter().enumerate() {
            let node = graph.node(node_id);
            if !is_numbered(&node.op) {
                continue;
            }
            let key = hash(node, &replacements);
            let mut slot = (key as usize) & mask;
            loop {
                let bucket = buckets[slot];
                if bucket.head == EMPTY_NODE || bucket.hash == key {
                    break;
                }
                slot = (slot + 1) & mask;
            }
            let mut candidate = buckets[slot].head;
            let mut existing = None;
            while candidate != EMPTY_NODE {
                let id = NodeId(candidate);
                let other = graph.node(id);
                let same_inputs = if replacements.is_empty() {
                    other.inputs.as_slice() == node.inputs.as_slice()
                } else {
                    other.inputs.len() == node.inputs.len()
                        && other
                            .inputs
                            .iter()
                            .zip(&node.inputs)
                            .all(|(a, b)| replacements.resolve(*a) == replacements.resolve(*b))
                };
                if same_inputs && other.op == node.op {
                    existing = Some(id);
                    break;
                }
                candidate = previous[id.index()];
            }
            if let Some(existing) = existing {
                if node.repr.is_some() {
                    replacements.replace(node_id, existing);
                }
                removed.insert(node_id.index());
                continue;
            }
            // In a dominator-tree leaf, the final numbered node can never dominate another numbered node.
            if last_numbered_in_leaf == Some(body_index) {
                continue;
            }
            let old_head = buckets[slot].head;
            if old_head == EMPTY_NODE {
                buckets[slot].hash = key;
            }
            buckets[slot].head = node_id.0;
            previous[node_id.index()] = old_head;
            added.push((slot as u32, old_head));
        }
        if added.len() > checkpoint {
            steps.push(Step::Leave(checkpoint));
        }
        for child in children.iter().rev() {
            steps.push(Step::Enter(*child));
        }
    }
    if !removed.is_empty() {
        replacements.apply(graph);
        edit::remove_nodes(graph, &removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::Repr;
    use crate::ir::Block;

    #[test]
    fn hash_collisions_do_not_make_different_nodes_equivalent() {
        let mut graph = Graph::default();
        graph.add_block(Block::default());
        let a = graph.add(Op::Constant(1), &[], Some(Repr::Int32), 0);
        let b = graph.add(Op::Constant(2), &[], Some(Repr::Int32), 0);
        let first = graph.add(Op::BoxInt32, &[a], Some(Repr::Tagged), 0);
        let different_input = graph.add(Op::BoxInt32, &[b], Some(Repr::Tagged), 0);
        let different_operation = graph.add(Op::Int32ToFloat64, &[a], Some(Repr::Float64), 0);
        let duplicate = graph.add(Op::BoxInt32, &[a], Some(Repr::Tagged), 0);
        let returned = graph.add(Op::Return, &[duplicate], None, 0);
        graph.blocks[0]
            .body
            .assign(&[first, different_input, different_operation, duplicate]);
        graph.blocks[0].control = Some(returned);
        let mut dominators = CachedDominators::new_in(crate::test_zone());
        run_with_hash(&mut graph, crate::test_zone(), &mut dominators, |_, _| 0);
        assert_eq!(graph.blocks[0].body, [first, different_input, different_operation]);
        assert_eq!(graph.node(returned).inputs, [first]);
    }
}
