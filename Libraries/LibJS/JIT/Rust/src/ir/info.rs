/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! What the compiler knows about each op, apart from how code generation lowers it (see
//! `codegen::lowering_info`): its effects, how it exits, which input it refines, whether it ends a block and which
//! inputs it takes. `meta_for_kind()`, `dynamic_meta()`, and `Op::inputs()` decide all of them exhaustively for every
//! op, so that a new op cannot be added without deciding each, and the passes and the verifier read them from there.

use super::BranchCondition;
use super::ElementsKind;
use super::Locations;
use super::Op;
use super::OpKind;
use crate::code::ExitKind;
use crate::code::Repr;
use crate::snapshot::HelperResult;
use crate::snapshot::Intrinsic;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeProperties {
    /// May need to exit to the interpreter after it returns, resuming after
    /// the current bytecode.
    pub can_lazy_exit: bool,
    /// Calls out of JIT code; every caller-saved register is clobbered.
    pub is_call: bool,
    /// The locations it may read and write, beyond its inputs and outputs.
    pub reads: Locations,
    pub writes: Locations,
    /// May run other code (which may write any location), or invalidate
    /// the compiled code (see `Dependency`).
    pub runs_code: bool,
    pub allocates: bool,
}

impl NodeProperties {
    pub(super) const PURE: Self = Self {
        can_lazy_exit: false,
        is_call: false,
        reads: Locations::NONE,
        writes: Locations::NONE,
        runs_code: false,
        allocates: false,
    };

    /// Reads `reads` and has no other effect.
    const fn reading(reads: Locations) -> Self {
        Self { reads, ..Self::PURE }
    }

    /// Writes `writes` and has no other effect.
    const fn writing(writes: Locations) -> Self {
        Self { writes, ..Self::PURE }
    }

    /// Calls out of JIT code and has no other effect: it only computes its value.
    const CALL: Self = Self {
        is_call: true,
        ..Self::PURE
    };

    /// Calls out of JIT code, which may allocate.
    const ALLOCATING_CALL: Self = Self {
        is_call: true,
        allocates: true,
        ..Self::PURE
    };

    /// Stores or adds a named property through a cache.
    const STORE_PROBE: Self = Self {
        reads: Locations::NAMED,
        writes: Locations::NAMED,
        allocates: true,
        ..Self::PURE
    };

    /// Runs code of the program: a call that may do anything and exit after it returns.
    const RUNS_ANYTHING: Self = Self {
        can_lazy_exit: true,
        is_call: true,
        reads: Locations::ALL,
        writes: Locations::ALL,
        runs_code: true,
        allocates: true,
    };
}

/// The inputs an op takes, which the verifier checks every node against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inputs {
    /// Exactly these, in this order.
    Exactly(&'static [Repr]),
    /// These, then any number of `rest`.
    AtLeast(&'static [Repr], Repr),
    /// The first ones, then either none or all of the others.
    WithOptional(&'static [Repr], &'static [Repr]),
    /// The receiver and the arguments of the intrinsic's `HelperCall`.
    HelperCall(Intrinsic),
    /// Not checked by their representations: phis take one input per predecessor, of their own representation (which
    /// the verifier checks with the predecessors), constants take none, and `StoreSlot` stores any value.
    Unchecked,
}

impl Inputs {
    const NONE: Self = Self::Exactly(&[]);
    const TAGGED: Self = Self::Exactly(&[Repr::Tagged]);
    const TAGGED_2: Self = Self::Exactly(&[Repr::Tagged, Repr::Tagged]);
    const TAGGED_3: Self = Self::Exactly(&[Repr::Tagged, Repr::Tagged, Repr::Tagged]);
    const INT32: Self = Self::Exactly(&[Repr::Int32]);
    const INT32_2: Self = Self::Exactly(&[Repr::Int32, Repr::Int32]);
    const FLOAT64: Self = Self::Exactly(&[Repr::Float64]);
    const FLOAT64_2: Self = Self::Exactly(&[Repr::Float64, Repr::Float64]);
    const BOOL: Self = Self::Exactly(&[Repr::Bool]);
    const POINTER: Self = Self::Exactly(&[Repr::Pointer]);
    const POINTER_2: Self = Self::Exactly(&[Repr::Pointer, Repr::Pointer]);
    const POINTER_INT32: Self = Self::Exactly(&[Repr::Pointer, Repr::Int32]);
    /// An object and its `CellAddress`.
    const OBJECT: Self = Self::Exactly(&[Repr::Tagged, Repr::Pointer]);
    /// An object, a value and the object's `CellAddress`.
    const OBJECT_VALUE: Self = Self::Exactly(&[Repr::Tagged, Repr::Tagged, Repr::Pointer]);
    /// Any number of tagged values.
    const TAGGED_ANY: Self = Self::AtLeast(&[], Repr::Tagged);
    /// An object and a key, and if the key is the key of a for-in loop's property iterator cache at an index, that
    /// cache and the index.
    const OBJECT_KEY: Self = Self::WithOptional(&[Repr::Tagged, Repr::Tagged], &[Repr::Tagged, Repr::Int32]);

    /// The representations of `count` inputs, if they are of this signature, or `None` if `count` is wrong. A
    /// `HelperCall` of an intrinsic without one, and `Unchecked`, accept anything.
    pub fn reprs(self, count: usize) -> Result<Option<impl Iterator<Item = Repr>>, usize> {
        let takes = match self {
            Inputs::Exactly(reprs) if reprs.len() == count => true,
            Inputs::Exactly(reprs) => return Err(reprs.len()),
            Inputs::AtLeast(first, _) if count >= first.len() => true,
            Inputs::AtLeast(first, _) => return Err(first.len()),
            Inputs::WithOptional(first, optional) => {
                if count != first.len() && count != first.len() + optional.len() {
                    return Err(first.len());
                }
                true
            }
            Inputs::HelperCall(intrinsic) => {
                let Some(call) = intrinsic.helper_call() else {
                    return Ok(None);
                };
                if call.arguments.len() + 1 != count {
                    return Err(call.arguments.len() + 1);
                }
                true
            }
            Inputs::Unchecked => false,
        };
        Ok(takes.then(|| (0..count).map(move |index| self.repr(index))))
    }

    /// The representation of input `index`, of a count of inputs that `reprs()` accepts.
    fn repr(self, index: usize) -> Repr {
        match self {
            Inputs::Exactly(reprs) => reprs[index],
            Inputs::AtLeast(first, rest) => first.get(index).copied().unwrap_or(rest),
            Inputs::WithOptional(first, optional) => first
                .get(index)
                .copied()
                .unwrap_or_else(|| optional[index - first.len()]),
            Inputs::HelperCall(intrinsic) => {
                let call = intrinsic.helper_call().expect("the intrinsic has a helper call");
                match index {
                    0 => call.receiver.repr(),
                    _ => call.arguments[index - 1].repr(),
                }
            }
            Inputs::Unchecked => unreachable!("unchecked inputs have no representations"),
        }
    }
}

/// What the compiler knows about an op (see `Op::info()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpInfo {
    pub properties: NodeProperties,
    /// Why the node exits to the interpreter, if it can exit eagerly: before its side effects, resuming at the
    /// current bytecode.
    pub exit_kind: Option<ExitKind>,
    /// For checks and refinements, whose value is an input known to pass them: which input that is (see
    /// "Refinements" in `Op`).
    pub refined_input: Option<usize>,
    /// Whether it ends its block (see `Op::successors()`).
    pub is_control: bool,
    /// Whether its value is never the empty value (see `Graph::is_known_non_empty()`, which decides constants, slots
    /// and phis itself). False is always safe.
    pub never_empty: bool,
    pub inputs: Inputs,
}

/// Compact 8-byte metadata for an `Op`, returned in a single register without constructing `Inputs`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpMeta {
    pub(crate) reads: Locations,
    pub(crate) writes: Locations,
    pub(crate) exit_kind: Option<ExitKind>,
    pub(crate) flags: u8,
}

const FLAG_CAN_LAZY_EXIT: u8 = 1 << 0;
const FLAG_IS_CALL: u8 = 1 << 1;
const FLAG_RUNS_CODE: u8 = 1 << 2;
const FLAG_ALLOCATES: u8 = 1 << 3;
const FLAG_REFINED_INPUT_0: u8 = 1 << 4;
const FLAG_IS_CONTROL: u8 = 1 << 5;
const FLAG_NEVER_EMPTY: u8 = 1 << 6;
/// Sentinel bit in `META_TABLE` indicating that metadata depends on the op's payload.
const FLAG_DYNAMIC: u8 = 1 << 7;

const _: () = assert!(core::mem::size_of::<OpMeta>() <= 8);

impl OpMeta {
    const PURE: Self = Self {
        reads: Locations::NONE,
        writes: Locations::NONE,
        exit_kind: None,
        flags: 0,
    };

    const DYNAMIC: Self = Self {
        flags: FLAG_DYNAMIC,
        ..Self::PURE
    };

    const fn check(kind: ExitKind) -> Self {
        Self {
            exit_kind: Some(kind),
            flags: FLAG_REFINED_INPUT_0,
            ..Self::PURE
        }
    }

    const fn control() -> Self {
        Self {
            flags: FLAG_IS_CONTROL,
            ..Self::PURE
        }
    }

    const fn with_properties(self, properties: NodeProperties) -> Self {
        let mut flags = self.flags & !(FLAG_CAN_LAZY_EXIT | FLAG_IS_CALL | FLAG_RUNS_CODE | FLAG_ALLOCATES);
        if properties.can_lazy_exit {
            flags |= FLAG_CAN_LAZY_EXIT;
        }
        if properties.is_call {
            flags |= FLAG_IS_CALL;
        }
        if properties.runs_code {
            flags |= FLAG_RUNS_CODE;
        }
        if properties.allocates {
            flags |= FLAG_ALLOCATES;
        }
        Self {
            reads: properties.reads,
            writes: properties.writes,
            flags,
            ..self
        }
    }

    const fn exiting(self, kind: ExitKind) -> Self {
        Self {
            exit_kind: Some(kind),
            ..self
        }
    }

    const fn never_empty(self) -> Self {
        Self {
            flags: self.flags | FLAG_NEVER_EMPTY,
            ..self
        }
    }

    const fn refines_input_0(self) -> Self {
        Self {
            flags: self.flags | FLAG_REFINED_INPUT_0,
            ..self
        }
    }

    #[inline(always)]
    pub(crate) const fn properties(self) -> NodeProperties {
        NodeProperties {
            can_lazy_exit: self.flags & FLAG_CAN_LAZY_EXIT != 0,
            is_call: self.flags & FLAG_IS_CALL != 0,
            reads: self.reads,
            writes: self.writes,
            runs_code: self.flags & FLAG_RUNS_CODE != 0,
            allocates: self.flags & FLAG_ALLOCATES != 0,
        }
    }

    #[inline(always)]
    pub(crate) const fn refined_input(self) -> Option<usize> {
        if self.flags & FLAG_REFINED_INPUT_0 != 0 {
            Some(0)
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) const fn is_control(self) -> bool {
        self.flags & FLAG_IS_CONTROL != 0
    }

    #[inline(always)]
    pub(crate) const fn never_empty_flag(self) -> bool {
        self.flags & FLAG_NEVER_EMPTY != 0
    }

    #[inline(always)]
    pub(crate) const fn is_dce_removable(self) -> bool {
        const BLOCKING_FLAGS: u8 = FLAG_IS_CONTROL | FLAG_CAN_LAZY_EXIT | FLAG_ALLOCATES | FLAG_RUNS_CODE;
        self.exit_kind.is_none() && (self.flags & BLOCKING_FLAGS) == 0 && self.writes.is_empty()
    }

    #[inline(always)]
    pub(crate) const fn is_gvn_pure(self) -> bool {
        const BLOCKING_FLAGS: u8 = FLAG_IS_CONTROL | FLAG_CAN_LAZY_EXIT | FLAG_ALLOCATES | FLAG_RUNS_CODE;
        (self.flags & BLOCKING_FLAGS) == 0 && self.reads.is_empty() && self.writes.is_empty()
    }
}

const fn meta_for_kind(kind: OpKind) -> OpMeta {
    use NodeProperties as P;
    match kind {
        OpKind::Constant | OpKind::Phi => OpMeta::PURE,
        OpKind::LoadSlot => OpMeta::PURE.with_properties(P::reading(Locations::FRAME)),
        OpKind::StoreSlot => OpMeta::PURE.with_properties(P::writing(Locations::FRAME)),
        OpKind::InitializeFrame | OpKind::EnsureFrameInitialized | OpKind::PublishFrame => {
            OpMeta::PURE.with_properties(P::writing(Locations::FRAME))
        }
        OpKind::Suspend => OpMeta::PURE.with_properties(P {
            reads: Locations::FRAME,
            writes: Locations::FRAME,
            ..P::PURE
        }),
        OpKind::LoadOuterEnvironment | OpKind::BoxCell => OpMeta::PURE,
        OpKind::LoadEnvironmentBinding => {
            OpMeta::PURE.with_properties(P::reading(Locations::BINDINGS.union(Locations::GLOBAL_BINDINGS)))
        }
        OpKind::StoreEnvironmentBinding => {
            OpMeta::PURE.with_properties(P::writing(Locations::BINDINGS.union(Locations::GLOBAL_BINDINGS)))
        }
        OpKind::AppendEnvironmentBinding => {
            OpMeta::PURE.with_properties(P::writing(Locations::BINDINGS.union(Locations::GLOBAL_BINDINGS)))
        }
        OpKind::SetLexicalEnvironment | OpKind::LeavePrivateEnvironment | OpKind::KeepOwnEnvironments => OpMeta::PURE
            .with_properties(P {
                reads: Locations::FRAME,
                writes: Locations::FRAME,
                ..P::PURE
            }),
        OpKind::IsCallable => OpMeta::PURE.never_empty(),
        OpKind::ToBoolean => OpMeta::PURE.with_properties(P::CALL),
        OpKind::PrimitiveToString => OpMeta::PURE.with_properties(P::ALLOCATING_CALL),
        OpKind::CallArraySplice => OpMeta::PURE.with_properties(P {
            is_call: true,
            allocates: true,
            reads: Locations::NAMED.union(Locations::ELEMENTS_AND_COUNTS),
            writes: Locations::ELEMENTS_AND_COUNTS,
            ..P::PURE
        }),
        OpKind::ToObject | OpKind::ArrayCreate => OpMeta::PURE.with_properties(P::ALLOCATING_CALL),
        OpKind::Generic => OpMeta::PURE.with_properties(P::RUNS_ANYTHING),
        OpKind::CallDirect | OpKind::CallNative => OpMeta::PURE.with_properties(P::RUNS_ANYTHING).never_empty(),
        OpKind::CallNativeAccessor => OpMeta::PURE.with_properties(P::RUNS_ANYTHING),
        OpKind::ArgumentCount => OpMeta::PURE.with_properties(P::reading(Locations::FRAME)).never_empty(),
        OpKind::LoadFrameField => OpMeta::PURE.with_properties(P::reading(Locations::FRAME)),
        OpKind::LoadArgument => OpMeta::PURE
            .with_properties(P::reading(Locations::FRAME))
            .exiting(ExitKind::ArgumentsIndex)
            .never_empty(),
        OpKind::CallForwardingArguments => OpMeta::PURE.with_properties(P::RUNS_ANYTHING),
        OpKind::CheckInt32 => OpMeta::PURE.exiting(ExitKind::NotInt32),
        OpKind::CheckNumber => OpMeta::PURE.exiting(ExitKind::BadType),
        OpKind::BoxInt32 | OpKind::BoxBool => OpMeta::PURE.never_empty(),
        OpKind::Int32WrappingBinary | OpKind::Int32Compare | OpKind::TaggedEquals => OpMeta::PURE,
        OpKind::CheckTypedArray => OpMeta::check(ExitKind::BadElements),
        OpKind::CheckArray => OpMeta::check(ExitKind::BadType),
        OpKind::CheckPropertyAbsenceCacheable => OpMeta::check(ExitKind::BadShape),
        OpKind::CheckNonNegativeInt32 => OpMeta::check(ExitKind::Overflow),
        OpKind::LoadTypedArrayLength | OpKind::LoadTypedArrayDataOffset => {
            OpMeta::PURE.with_properties(P::reading(Locations::ELEMENT_COUNTS))
        }
        OpKind::TypedArrayDataAddress => OpMeta::PURE,
        OpKind::CheckTypedArrayStorage | OpKind::CheckBounds => OpMeta::check(ExitKind::OutOfBounds),
        OpKind::LoadElementAt => OpMeta::PURE.with_properties(P::reading(Locations::ELEMENTS)),
        OpKind::StoreElementAt => OpMeta::PURE.with_properties(P::writing(Locations::ELEMENTS)),
        OpKind::CheckNotHole => OpMeta::check(ExitKind::OutOfBounds),
        OpKind::CheckInitialized => OpMeta::check(ExitKind::Uninitialized),
        OpKind::CellAddress => OpMeta::PURE,
        OpKind::AllocateObject | OpKind::AllocateArray | OpKind::AllocateFunction | OpKind::AllocateEnvironment => {
            OpMeta::PURE
                .with_properties(P {
                    allocates: true,
                    ..P::PURE
                })
                .never_empty()
        }
        OpKind::InitializeNamed => OpMeta::PURE.with_properties(P::writing(Locations::NAMED_SLOTS)),
        OpKind::VirtualObject => OpMeta::PURE,
        OpKind::InitializeElement => OpMeta::PURE.with_properties(P::writing(Locations::ELEMENTS)),
        OpKind::SliceArguments => OpMeta::PURE
            .with_properties(P {
                is_call: true,
                reads: Locations::FRAME,
                allocates: true,
                ..P::PURE
            })
            .exiting(ExitKind::ArgumentsIndex)
            .never_empty(),
        OpKind::CheckIdentityComparable => OpMeta::check(ExitKind::BadType),
        OpKind::CheckObject => OpMeta::check(ExitKind::NotObject),
        OpKind::CheckClosure => OpMeta::check(ExitKind::BadCallTarget),
        OpKind::LoadFunctionEnvironment => OpMeta::PURE,
        OpKind::EmptyToUndefined | OpKind::Typeof => OpMeta::PURE.never_empty(),
        OpKind::TypeofIs => OpMeta::PURE,
        OpKind::CheckShape => OpMeta::check(ExitKind::BadShape).with_properties(P::reading(Locations::SHAPES)),
        OpKind::CheckPrototypeChainValid => OpMeta::PURE
            .with_properties(P::reading(Locations::SHAPES))
            .exiting(ExitKind::BadShape),
        OpKind::HasInPrototypeChain => OpMeta::PURE
            .with_properties(P::reading(Locations::SHAPES))
            .exiting(ExitKind::BadType),
        OpKind::AssumeValid | OpKind::AssumeLegacyArgumentsUnread => OpMeta::PURE
            .with_properties(P::reading(Locations::ALL))
            .exiting(ExitKind::Invalidated),
        OpKind::CheckInterrupt => OpMeta::PURE
            .with_properties(P::writing(Locations::INTERRUPT))
            .exiting(ExitKind::Interrupted),
        OpKind::LoadGlobalBinding => OpMeta::PURE
            .with_properties(P::reading(Locations::GLOBAL_BINDINGS))
            .exiting(ExitKind::BadShape),
        OpKind::StoreGlobalBinding => OpMeta::PURE
            .with_properties(P {
                reads: Locations::GLOBAL_BINDINGS,
                writes: Locations::GLOBAL_BINDINGS,
                ..P::PURE
            })
            .exiting(ExitKind::BadShape),
        OpKind::CheckAccessorFunction => {
            OpMeta::check(ExitKind::UnexpectedValue).with_properties(P::reading(Locations::NAMED_SLOTS))
        }
        OpKind::LoadAccessorFunction => OpMeta::PURE
            .with_properties(P::reading(Locations::NAMED_SLOTS))
            .exiting(ExitKind::BadCallTarget),
        OpKind::AddNamed => OpMeta::PURE.with_properties(P {
            reads: Locations::NAMED,
            writes: Locations::NAMED,
            ..P::PURE
        }),
        OpKind::Jump
        | OpKind::Unreachable
        | OpKind::Branch
        | OpKind::BranchTruthy
        | OpKind::Return
        | OpKind::BranchOnPc => OpMeta::control(),
        OpKind::ShapeSwitch => OpMeta::control()
            .with_properties(P::reading(Locations::SHAPES))
            .exiting(ExitKind::BadShape),
        OpKind::Refine => OpMeta::PURE.refines_input_0(),
        OpKind::UnboxInt32
        | OpKind::UnboxDouble
        | OpKind::Int32Abs
        | OpKind::Int32ToFloat64
        | OpKind::Int32Min
        | OpKind::Int32Max
        | OpKind::Uint32Min
        | OpKind::Uint32ShiftRight
        | OpKind::BoolToInt32
        | OpKind::Float64Unary
        | OpKind::Float64ToInt32
        | OpKind::Float64ToUint8Clamped
        | OpKind::Float64Binary
        | OpKind::Float64Min
        | OpKind::Float64Max
        | OpKind::Float64Compare
        | OpKind::Uint32ToFloat64
        | OpKind::StringAddress
        | OpKind::StringLength
        | OpKind::LoadStringCodeUnit
        | OpKind::SingleCharacterString
        | OpKind::StringsEqual
        | OpKind::IntegerToString
        | OpKind::LoadPropertyIteratorKeyCount
        | OpKind::LoadPropertyIteratorKey
        | OpKind::SlowPathOutput => OpMeta::PURE,
        OpKind::BoxFloat64 => OpMeta::PURE.never_empty(),
        OpKind::CallFloat64Function => OpMeta::PURE.with_properties(P::CALL),
        OpKind::CheckFloat64Int32 => OpMeta::PURE.exiting(ExitKind::NotInt32),
        OpKind::ConcatenateStrings => OpMeta::PURE.with_properties(P {
            allocates: true,
            ..P::PURE
        }),
        OpKind::CheckAppendableArray => OpMeta::check(ExitKind::SlowPath).with_properties(P::reading(Locations::ALL)),
        OpKind::LoadElementsLength | OpKind::LoadElementsCapacity => {
            OpMeta::PURE.with_properties(P::reading(Locations::ELEMENT_COUNTS))
        }
        OpKind::AppendElement => OpMeta::PURE.with_properties(P {
            reads: Locations::ELEMENTS_AND_COUNTS,
            writes: Locations::ELEMENTS_AND_COUNTS,
            ..P::PURE
        }),
        OpKind::CallArrayPush | OpKind::CallStoreHoleyElement => OpMeta::PURE.with_properties(P {
            reads: Locations::ELEMENTS_AND_COUNTS,
            writes: Locations::ELEMENTS_AND_COUNTS,
            allocates: true,
            ..P::PURE
        }),
        OpKind::OpenArrayIteration => OpMeta::PURE.with_properties(P {
            reads: Locations::ALL,
            allocates: true,
            ..P::PURE
        }),
        OpKind::LoadArrayIteratorArray
        | OpKind::LoadArrayIteratorIndex
        | OpKind::LoadGeneratorCompletionType
        | OpKind::LoadGeneratorCompletionValue => OpMeta::PURE.with_properties(P::reading(Locations::ITERATORS)),
        OpKind::StoreArrayIteratorIndex | OpKind::FinishArrayIterator => {
            OpMeta::PURE.with_properties(P::writing(Locations::ITERATORS))
        }
        OpKind::ProbePropertyCache | OpKind::ProbeKeyedCache => {
            OpMeta::PURE.with_properties(P::reading(Locations::NAMED))
        }
        OpKind::ProbePropertyStore | OpKind::ProbeKeyedStore => OpMeta::PURE.with_properties(P::STORE_PROBE),
        OpKind::ProbeGlobalCache | OpKind::ProbeHasProperty => OpMeta::PURE.with_properties(P::reading(Locations::ALL)),
        OpKind::ProbeGlobalStore => OpMeta::PURE.with_properties(P {
            reads: Locations::ALL,
            writes: Locations::ALL,
            runs_code: true,
            ..P::PURE
        }),
        // Payload-dependent ops resolved by `Op::dynamic_meta()`.
        OpKind::CallIntrinsic
        | OpKind::Int32Binary
        | OpKind::CheckElements
        | OpKind::CheckValue
        | OpKind::LoadNamed
        | OpKind::StoreNamed
        | OpKind::Exit
        | OpKind::CallSlowPath => OpMeta::DYNAMIC,
    }
}

const META_TABLE: [OpMeta; OpKind::COUNT] = {
    let mut table = [OpMeta::PURE; OpKind::COUNT];
    let mut index = 0usize;
    while index < OpKind::COUNT {
        table[index] = meta_for_kind(OpKind::ALL[index]);
        index += 1;
    }
    table
};

impl Op<'_> {
    /// Compact metadata for the op, without `Inputs` (which only the verifier inspects).
    #[inline(always)]
    pub(crate) fn meta(&self) -> OpMeta {
        let meta = META_TABLE[self.kind() as usize];
        if meta.flags & FLAG_DYNAMIC == 0 {
            return meta;
        }
        self.dynamic_meta()
    }

    #[cold]
    #[inline(never)]
    fn dynamic_meta(&self) -> OpMeta {
        use NodeProperties as P;
        match self {
            Op::CallIntrinsic { intrinsic } => {
                let call = intrinsic
                    .helper_call()
                    .expect("intrinsics called through a helper have one");
                // NB: Helpers that answer with a number or a boolean make no cells.
                OpMeta::PURE.with_properties(P {
                    is_call: true,
                    allocates: matches!(call.result, HelperResult::String | HelperResult::Value),
                    reads: call.reads,
                    writes: call.writes,
                    ..P::PURE
                })
            }
            Op::Int32Binary { op } => {
                if op.int32_can_overflow() {
                    OpMeta::PURE.exiting(ExitKind::Overflow)
                } else {
                    OpMeta::PURE
                }
            }
            // NB: The elements kind of a typed array never changes.
            Op::CheckElements {
                kind: ElementsKind::TypedArray(_),
            } => OpMeta::check(ExitKind::BadElements),
            Op::CheckElements { .. } => {
                OpMeta::check(ExitKind::BadElements).with_properties(P::reading(Locations::ELEMENT_COUNTS))
            }
            Op::CheckValue { kind, .. } => OpMeta::check(*kind),
            Op::LoadNamed { holds_data, .. } => {
                let meta = OpMeta::PURE.with_properties(P::reading(Locations::NAMED_SLOTS));
                if *holds_data {
                    meta
                } else {
                    meta.exiting(ExitKind::BadShape)
                }
            }
            // NB: Stores to a property an object has change no shape.
            Op::StoreNamed { holds_data, .. } => {
                let meta = OpMeta::PURE.with_properties(P {
                    reads: Locations::NAMED_SLOTS,
                    writes: Locations::NAMED_SLOTS,
                    ..P::PURE
                });
                if *holds_data {
                    meta
                } else {
                    meta.exiting(ExitKind::BadShape)
                }
            }
            Op::Exit { kind } => OpMeta::control().exiting(*kind),
            Op::CallSlowPath {
                saves_registers: false, ..
            } => OpMeta::PURE.with_properties(P::RUNS_ANYTHING),
            // NB: The slow path saves the registers it clobbers, so the node is not a call for the register allocator.
            Op::CallSlowPath { .. } => OpMeta::PURE.with_properties(P {
                reads: Locations::ALL,
                writes: Locations::ALL,
                runs_code: true,
                allocates: true,
                ..P::PURE
            }),
            _ => unreachable!("only dynamic ops reach dynamic_meta()"),
        }
    }

    fn inputs(&self) -> Inputs {
        use Inputs as I;
        match self {
            Op::Constant(_) | Op::Phi | Op::StoreSlot { .. } | Op::VirtualObject { .. } => I::Unchecked,
            Op::LoadSlot { .. }
            | Op::InitializeFrame
            | Op::EnsureFrameInitialized
            | Op::PublishFrame
            | Op::Suspend { .. }
            | Op::LeavePrivateEnvironment
            | Op::KeepOwnEnvironments
            | Op::Generic { .. }
            | Op::ArgumentCount
            | Op::LoadFrameField { .. }
            | Op::CallForwardingArguments { .. }
            | Op::AllocateObject { .. }
            | Op::AllocateArray { .. }
            | Op::CheckPrototypeChainValid { .. }
            | Op::AssumeValid
            | Op::AssumeLegacyArgumentsUnread
            | Op::CheckInterrupt
            | Op::LoadGlobalBinding { .. }
            | Op::Jump { .. }
            | Op::Unreachable
            | Op::Exit { .. } => I::NONE,
            Op::LoadOuterEnvironment
            | Op::BoxCell
            | Op::LoadEnvironmentBinding { .. }
            | Op::AppendEnvironmentBinding
            | Op::LoadTypedArrayLength
            | Op::LoadTypedArrayDataOffset
            | Op::TypedArrayDataAddress
            | Op::CheckTypedArrayStorage
            | Op::LoadFunctionEnvironment { .. }
            | Op::StringLength
            | Op::LoadElementsLength
            | Op::LoadElementsCapacity
            | Op::LoadArrayIteratorArray
            | Op::LoadArrayIteratorIndex
            | Op::LoadGeneratorCompletionType
            | Op::LoadGeneratorCompletionValue
            | Op::FinishArrayIterator => I::POINTER,
            Op::AllocateFunction { .. } | Op::ProbeGlobalCache { .. } => I::POINTER_2,
            Op::LoadElementAt { .. } | Op::LoadStringCodeUnit | Op::StoreArrayIteratorIndex => I::POINTER_INT32,
            Op::StoreEnvironmentBinding { .. } => I::Exactly(&[Repr::Pointer, Repr::Tagged]),
            Op::SetLexicalEnvironment
            | Op::IsCallable
            | Op::ToBoolean
            | Op::PrimitiveToString
            | Op::LoadArgument { .. }
            | Op::CheckInt32
            | Op::CheckNumber
            | Op::CheckElements { .. }
            | Op::CheckTypedArray
            | Op::CheckArray
            | Op::CheckPropertyAbsenceCacheable
            | Op::CheckNotHole
            | Op::CheckInitialized
            | Op::CellAddress
            | Op::AllocateEnvironment { .. }
            | Op::SliceArguments
            | Op::CheckObject
            | Op::CheckClosure { .. }
            | Op::CheckValue { .. }
            | Op::EmptyToUndefined
            | Op::Typeof
            | Op::TypeofIs { .. }
            | Op::StoreGlobalBinding { .. }
            | Op::BranchTruthy { .. }
            | Op::Return
            | Op::UnboxInt32
            | Op::UnboxDouble
            | Op::StringAddress
            | Op::CheckAppendableArray
            | Op::OpenArrayIteration
            | Op::LoadPropertyIteratorKeyCount
            | Op::SlowPathOutput { .. }
            | Op::ProbePropertyCache { .. } => I::TAGGED,
            Op::ToObject
            | Op::TaggedEquals { .. }
            | Op::InitializeNamed { .. }
            | Op::InitializeElement { .. }
            | Op::CheckIdentityComparable
            | Op::HasInPrototypeChain
            | Op::StringsEqual
            | Op::ConcatenateStrings { .. }
            | Op::CallArrayPush
            | Op::ProbePropertyStore { .. } => I::TAGGED_2,
            Op::ProbeKeyedStore { .. } => I::TAGGED_3,
            Op::CallDirect { .. } | Op::CallNative { .. } | Op::CallSlowPath { .. } => I::TAGGED_ANY,
            Op::CallIntrinsic { intrinsic } => I::HelperCall(*intrinsic),
            Op::CallArraySplice => I::AtLeast(&[Repr::Tagged, Repr::Int32, Repr::Int32], Repr::Tagged),
            Op::ArrayCreate => I::Exactly(&[Repr::Int32, Repr::Tagged]),
            Op::CallNativeAccessor { part, .. } => match part {
                super::AccessorPart::Getter => I::TAGGED,
                super::AccessorPart::Setter => I::TAGGED_2,
            },
            Op::BoxInt32
            | Op::CheckNonNegativeInt32
            | Op::BranchOnPc { .. }
            | Op::Int32Abs
            | Op::Int32ToFloat64
            | Op::Uint32ToFloat64
            | Op::SingleCharacterString
            | Op::IntegerToString => I::INT32,
            Op::Int32Binary { .. }
            | Op::Int32WrappingBinary { .. }
            | Op::Int32Compare { .. }
            | Op::CheckBounds
            | Op::Int32Min
            | Op::Int32Max
            | Op::Uint32Min
            | Op::Uint32ShiftRight => I::INT32_2,
            Op::BoxBool | Op::BoolToInt32 => I::BOOL,
            Op::BoxFloat64
            | Op::Float64Unary { .. }
            | Op::Float64ToInt32
            | Op::Float64ToUint8Clamped
            | Op::CheckFloat64Int32 => I::FLOAT64,
            Op::Float64Binary { .. } | Op::Float64Min | Op::Float64Max | Op::Float64Compare { .. } => I::FLOAT64_2,
            Op::CallFloat64Function { function } => match function.argument_count() {
                1 => I::FLOAT64,
                _ => I::FLOAT64_2,
            },
            Op::StoreElementAt { kind } => match kind.stored_repr() {
                Repr::Float64 => I::Exactly(&[Repr::Pointer, Repr::Int32, Repr::Float64]),
                Repr::Int32 => I::Exactly(&[Repr::Pointer, Repr::Int32, Repr::Int32]),
                _ => I::Exactly(&[Repr::Pointer, Repr::Int32, Repr::Tagged]),
            },
            Op::CheckShape { .. }
            | Op::CheckAccessorFunction { .. }
            | Op::LoadAccessorFunction { .. }
            | Op::LoadNamed { .. }
            | Op::ShapeSwitch { .. } => I::OBJECT,
            Op::AddNamed { .. } | Op::StoreNamed { .. } => I::OBJECT_VALUE,
            Op::Branch { condition, .. } | Op::Refine { condition, .. } => I::Exactly(condition.input_reprs()),
            Op::AppendElement | Op::CallStoreHoleyElement => I::Exactly(&[Repr::Pointer, Repr::Int32, Repr::Tagged]),
            Op::LoadPropertyIteratorKey => I::Exactly(&[Repr::Tagged, Repr::Int32]),
            Op::ProbeKeyedCache { .. } | Op::ProbeHasProperty { .. } => I::OBJECT_KEY,
            Op::ProbeGlobalStore { .. } => I::Exactly(&[Repr::Tagged, Repr::Pointer, Repr::Pointer]),
        }
    }

    /// What the compiler knows about the op.
    pub fn info(&self) -> OpInfo {
        let meta = self.meta();
        OpInfo {
            properties: meta.properties(),
            exit_kind: meta.exit_kind,
            refined_input: meta.refined_input(),
            is_control: meta.is_control(),
            never_empty: meta.never_empty_flag(),
            inputs: self.inputs(),
        }
    }

    #[inline]
    pub fn properties(&self) -> NodeProperties {
        self.meta().properties()
    }

    /// Why this node exits to the interpreter, if it can exit eagerly.
    #[inline]
    pub fn exit_kind(&self) -> Option<ExitKind> {
        self.meta().exit_kind
    }

    /// Whether the node may exit to the interpreter before its side effects,
    /// resuming at the current bytecode: whether it has an exit kind.
    #[inline]
    pub fn can_eager_exit(&self) -> bool {
        self.exit_kind().is_some()
    }

    /// For checks and refinements, whose value is an input known to pass
    /// them: which input that is (see "Refinements" in `Op`).
    #[inline(always)]
    pub fn refined_input(&self) -> Option<usize> {
        self.meta().refined_input()
    }

    #[inline(always)]
    pub(crate) fn refines_input_0(&self) -> bool {
        self.meta().flags & FLAG_REFINED_INPUT_0 != 0
    }

    #[inline(always)]
    pub fn is_control(&self) -> bool {
        self.meta().is_control()
    }

    #[inline(always)]
    pub fn never_empty(&self) -> bool {
        self.meta().never_empty_flag()
    }
}

impl BranchCondition {
    /// The representations of the inputs a branch on the condition (and a `Refine` of it) takes.
    pub fn input_reprs(self) -> &'static [Repr] {
        match self {
            BranchCondition::Bool => &[Repr::Bool],
            BranchCondition::Nullish
            | BranchCondition::Undefined
            | BranchCondition::Object
            | BranchCondition::NonNegativeInt32
            | BranchCondition::ElementsKind(_)
            | BranchCondition::Int32Value
            | BranchCondition::Int32OrBoolean
            | BranchCondition::Double
            | BranchCondition::String
            | BranchCondition::Builtin(_) => &[Repr::Tagged],
            BranchCondition::IndexInBounds | BranchCondition::Int32(_) => &[Repr::Int32, Repr::Int32],
            BranchCondition::MagicalLength
            | BranchCondition::Extensible
            | BranchCondition::HoleFillableArray
            | BranchCondition::ArrayIterator
            | BranchCondition::GeneratorObject
            | BranchCondition::PlainArrayPrototypeChain
            | BranchCondition::ArrayWithRealmArrayPrototypeAndNoNamedProperties
            | BranchCondition::HoldsValidWatchpointSet
            | BranchCondition::ResolvedString
            | BranchCondition::BindingMutable { .. }
            | BranchCondition::NextBindingOfShape { .. } => &[Repr::Pointer],
            BranchCondition::Shape(_) => &[Repr::Tagged, Repr::Pointer],
            BranchCondition::Float64(_) => &[Repr::Float64, Repr::Float64],
            BranchCondition::TaggedEquals { .. } | BranchCondition::PropertyIteratorCacheValid => {
                &[Repr::Tagged, Repr::Tagged]
            }
        }
    }
}
