/*
 * Copyright (c) 2026-present, the Ladybird developers.
 *
 * SPDX-License-Identifier: BSD-2-Clause
 */

//! A VM's share of the compile threads of the process. Only snapshots go to them and only compile results come back: the
//! compiler never sees the heap. Snapshots and results keep their memory, which serves one compile after the other
//! (see `Spare`).

use core::ops::Deref;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use crate::bytecode::executable_data::ExecutableData;
use crate::jit_compile_pool::{CompilePool, CompilePoolClient};
use libjs_jit::CompileFailure;
use libjs_jit::code::CompiledCode;
use libjs_jit::scratch::Scratch;
use libjs_jit::snapshot::Snapshot;

pub type CompileResult = Result<Compiled, CompileFailure>;

/// How many results of compiles that nothing uses now keep their memory for the next compiles.
const SPARE_RESULTS: usize = 32;

/// How many snapshots that nothing uses now keep their memory for the next compiles. Nearly always, fewer snapshots
/// than this wait for a compile at one time, so the memory of a snapshot seldom comes from the system or goes back
/// there while compiles run.
const SPARE_SNAPSHOTS: usize = 16;

/// How many bytes of code and words of tables such a result keeps the memory for when a compile takes it: the first
/// step a column grows by. The thread that compiles gives the rest back to the system, so that the thread of a VM,
/// which brings the results back, does not pay for that. A result that keeps more than this keeps pages that the
/// usual compile does not write.
const SPARE_CODE_BYTES: usize = 64 * 1024;
const SPARE_TABLE_WORDS: usize = 8 * 1024;

/// The snapshots and the results that nothing uses now. The thread of a VM writes the input of a compile into a
/// snapshot, and the compile thread that read it brings it back. A compile thread writes the code and the tables of
/// a compile into a result, and the thread of the VM that asked for the compile brings it back.
struct Spare {
    snapshots: [Option<Box<Snapshot>>; SPARE_SNAPSHOTS],
    results: [Option<CompiledCode>; SPARE_RESULTS],
    /// How many compile threads had a task since they last were idle. The last of them to go idle gives the memory
    /// of the snapshots and of the results back to the system.
    active_thread_count: usize,
}

static SPARE: Mutex<Spare> = Mutex::new(Spare {
    snapshots: [const { None }; SPARE_SNAPSHOTS],
    results: [const { None }; SPARE_RESULTS],
    active_thread_count: 0,
});

fn spare() -> MutexGuard<'static, Spare> {
    // NB: Nothing leaves the results half changed, so they are good to use after a thread panicked with the lock.
    SPARE.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    /// What this thread compiles with (see `compile()`).
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::new());
    /// Whether this thread is one of `Spare::active_thread_count`.
    static THREAD_IS_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Notes that this compile thread has a job.
fn note_thread_is_active() {
    if !THREAD_IS_ACTIVE.replace(true) {
        spare().active_thread_count += 1;
    }
}

/// The task of a compile thread for spare memory that came back while all of them were idle (see
/// `release_spare_memory_later()`), which counted the thread as active already.
fn stay_active_for_late_memory() {
    if THREAD_IS_ACTIVE.replace(true) {
        spare().active_thread_count -= 1;
    }
}

/// Makes sure that a compile thread gives the spare memory back to the system, after something came back into
/// `spare`. If the compile threads are idle, none of them is going to do that. This makes one of them go idle again,
/// so that the thread of a VM does not pay for it. Without compile threads, the memory stays for the next compile.
fn release_spare_memory_later(mut spare: MutexGuard<'static, Spare>) {
    if spare.active_thread_count > 0 {
        return;
    }
    let Some(pool) = POOL.get() else {
        return;
    };
    spare.active_thread_count += 1;
    drop(spare);
    pool.run_unowned(Box::new(stay_active_for_late_memory));
}

/// Notes that this compile thread went idle.
fn note_thread_is_idle() {
    if !THREAD_IS_ACTIVE.replace(false) {
        return;
    }
    let mut spare = spare();
    spare.active_thread_count -= 1;
    if spare.active_thread_count > 0 {
        return;
    }
    let snapshots = core::mem::replace(&mut spare.snapshots, [const { None }; SPARE_SNAPSHOTS]);
    let results = core::mem::replace(&mut spare.results, [const { None }; SPARE_RESULTS]);
    drop(spare);
    // NB: The memory is given back here, with the lock released.
    drop(snapshots);
    drop(results);
}

/// A snapshot of nothing, to capture into. It has the memory of an earlier snapshot if one is spare.
pub fn spare_snapshot() -> Box<Snapshot> {
    let snapshot = spare().snapshots.iter_mut().find_map(Option::take);
    snapshot.unwrap_or_default()
}

/// Takes back a snapshot that nothing reads anymore, for a later capture.
pub fn give_back_snapshot(mut snapshot: Box<Snapshot>) {
    snapshot.clear();
    snapshot.trim();
    let mut spare = spare();
    // NB: With no place left, the memory of the snapshot goes back to the system here.
    let Some(place) = spare.snapshots.iter_mut().find(|place| place.is_none()) else {
        return;
    };
    *place = Some(snapshot);
    release_spare_memory_later(spare);
}

/// The code of a finished compile, on its way to where it is installed. The code and its tables are in memory that is
/// no block of the heap (see `CompiledCode`), which serves a later compile once this is dropped.
pub struct Compiled {
    // NB: Only drop() takes the code.
    code: Option<CompiledCode>,
}

impl Compiled {
    fn new() -> Self {
        let result = spare().results.iter_mut().find_map(Option::take);
        let code = match result {
            Some(mut code) => {
                code.trim(SPARE_CODE_BYTES, SPARE_TABLE_WORDS);
                code
            }
            None => CompiledCode::default(),
        };
        Self { code: Some(code) }
    }
}

impl Deref for Compiled {
    type Target = CompiledCode;

    fn deref(&self) -> &CompiledCode {
        self.code.as_ref().expect("the code is there until it is dropped")
    }
}

impl Drop for Compiled {
    fn drop(&mut self) {
        let Some(mut code) = self.code.take() else {
            return;
        };
        // NB: This keeps all the memory of the result. The compile that takes it gives back what is too much.
        code.clear();
        let mut spare = spare();
        // NB: With no place left, the memory goes back to the system here.
        let Some(place) = spare.results.iter_mut().find(|place| place.is_none()) else {
            return;
        };
        *place = Some(code);
        release_spare_memory_later(spare);
    }
}

/// How long a compile thread waits for another job before it gives the memory its jobs freed back to the system.
const IDLE_MEMORY_RELEASE_DELAY: Duration = Duration::from_secs(1);

/// Compiles `snapshot` on this thread, with the memory the thread compiles with.
pub fn compile(snapshot: &Snapshot) -> CompileResult {
    let mut compiled = Compiled::new();
    let code = compiled.code.as_mut().expect("the code is there until it is dropped");
    SCRATCH.with_borrow_mut(|scratch| libjs_jit::compile(snapshot, scratch, code))?;
    Ok(compiled)
}

/// The compile threads of the process, once a VM has started them.
static POOL: OnceLock<Arc<CompilePool>> = OnceLock::new();

/// The compile threads of the process. The first VM to compile decides how many there can be.
fn compile_pool(thread_count: usize) -> &'static Arc<CompilePool> {
    POOL.get_or_init(|| {
        CompilePool::new(thread_count, "LibJS JIT", IDLE_MEMORY_RELEASE_DELAY, || {
            SCRATCH.with_borrow_mut(Scratch::release_memory);
            note_thread_is_idle();
            // NB: A compile can allocate a lot for a while, and the allocator keeps the memory a thread freed until
            //     the thread allocates again, which an idle thread may never do.
            ak::release_unused_allocator_memory();
        })
    })
}

/// A VM's queue of compile jobs, which jobs of other VMs take turns with. Jobs finish in any order. Dropping the queue
/// drops the jobs that are still in it. The result of a job that is running by then is dropped once it finishes.
pub struct CompileQueue {
    client: CompilePoolClient,
    result_sender: Sender<(u64, CompileResult)>,
    results: Receiver<(u64, CompileResult)>,
    /// Incremented by a compile thread after it sends a result, and cleared before the results are taken. The thread
    /// that owns the queue checks for finished jobs very often, and the counter is cheaper to check than the channel
    /// and lets callers batch small installs to amortize page-protection toggles and icache invalidations.
    finished_count: Arc<AtomicUsize>,
    /// Bitmask of `(job_id & 63)` for jobs sent since the last drain, so a queued executable can install as soon as
    /// its own job finishes without draining single unrelated jobs one at a time.
    finished_job_bits: Arc<AtomicU64>,
}

impl CompileQueue {
    pub fn start(thread_count: usize) -> Self {
        let (result_sender, results) = channel();
        Self {
            client: compile_pool(thread_count).add_client(),
            result_sender,
            results,
            finished_count: Arc::new(AtomicUsize::new(0)),
            finished_job_bits: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn submit(&self, id: u64, snapshot: Box<Snapshot>, bytecode_owners: Vec<Arc<ExecutableData>>) {
        // NB: Field order ensures `snapshot` is dropped before `_bytecode_owners` even if a queued task is
        //     cancelled when the client drops before it runs.
        struct OwnedSnapshot {
            snapshot: Box<Snapshot>,
            _bytecode_owners: Vec<Arc<ExecutableData>>,
        }
        let owned = OwnedSnapshot {
            snapshot,
            _bytecode_owners: bytecode_owners,
        };
        let result_sender = self.result_sender.clone();
        let finished_count = Arc::clone(&self.finished_count);
        let finished_job_bits = Arc::clone(&self.finished_job_bits);
        self.client.submit(Box::new(move || {
            note_thread_is_active();
            let OwnedSnapshot {
                snapshot,
                _bytecode_owners,
            } = owned;
            let result = compile(&snapshot);
            give_back_snapshot(snapshot);
            drop(_bytecode_owners);
            // NB: A VM that went away has no use for the result, and the thread goes on with other VMs' jobs.
            if result_sender.send((id, result)).is_ok() {
                finished_count.fetch_add(1, Ordering::Release);
                finished_job_bits.fetch_or(1u64 << (id & 63), Ordering::Release);
            }
        }));
    }

    pub fn finished_count(&self) -> usize {
        self.finished_count.load(Ordering::Relaxed)
    }

    pub fn is_job_ready(&self, job_id: u64) -> bool {
        self.finished_job_bits.load(Ordering::Relaxed) & (1u64 << (job_id & 63)) != 0
    }

    /// The results of the jobs that finished since the last call, waiting for one to finish if none has. The queue must
    /// have a job.
    pub fn wait_for_finished(&self) -> Vec<(u64, CompileResult)> {
        let mut finished = vec![self.results.recv().expect("the queue keeps a sender")];
        self.finished_job_bits.store(0, Ordering::Relaxed);
        self.finished_count.store(0, Ordering::Relaxed);
        finished.extend(self.results.try_iter());
        finished
    }

    /// The results of the jobs that finished since the last call when either at least `min_batch` results are ready
    /// or `ready_job_id` has likely finished (matching its 64-bit bloom slot).
    pub fn take_finished_if_ready(&self, min_batch: usize, ready_job_id: Option<u64>) -> Vec<(u64, CompileResult)> {
        let batch_ready = self.finished_count.load(Ordering::Relaxed) >= min_batch;
        let job_ready =
            ready_job_id.is_some_and(|id| self.finished_job_bits.load(Ordering::Relaxed) & (1u64 << (id & 63)) != 0);
        if !batch_ready && !job_ready {
            return Vec::new();
        }
        // NB: Clear bits before count (the reverse of worker publication order), and drain unconditionally once
        //     triggered so no concurrent completion can lose both its bit and its count.
        self.finished_job_bits.swap(0, Ordering::Acquire);
        self.finished_count.swap(0, Ordering::Acquire);
        self.results.try_iter().collect()
    }

    /// The results of the jobs that finished since the last call.
    pub fn take_finished(&self) -> Vec<(u64, CompileResult)> {
        self.take_finished_if_ready(1, None)
    }
}
