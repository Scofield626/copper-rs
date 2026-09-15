//! Parallel runtime scheduler state for concurrent CopperList execution.
//!
//! The proc macro emits one ordered process-stage entry per generated runtime
//! plan node. The feature-enabled runtime executes those stages as a FIFO
//! pipeline: each stage worker drains CopperLists in ascending `clid` order and
//! forwards them to the next stage. Determinism therefore comes from queue
//! order, while commit/log handoff is still protected by an explicit ordered
//! cursor.

use crate::config::NodeId;
use crate::copperlist::{CopperList, CuListZeroedInit};
pub use crate::curuntime::{ProcessStepOutcome, ProcessStepResult};
use crate::monitoring::ComponentId;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter, Result as FmtResult};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, Ordering};
use cu29_clock::CuTime;
use cu29_traits::CopperListTuple;

/// Scheduler-facing category for one process-stage checkpoint.
///
/// A stage maps to one node in the generated execution plan. Future worker
/// threads will use this to pick the correct shared mutable lane:
/// - `Task`: a normal Copper task instance
/// - `BridgeRx`: a bridge receive channel
/// - `BridgeTx`: a bridge send channel
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParallelRtStageKind {
    Task,
    BridgeRx,
    BridgeTx,
}

/// Static metadata describing one ordered process stage in the generated plan.
///
/// Field meanings:
/// - `label`: stable human-readable identifier used in diagnostics and tests.
/// - `kind`: whether the stage targets a task, bridge receive lane, or bridge
///   send lane.
/// - `plan_node_id`: node identifier inside the build-time execution plan.
/// - `component_id`: monitor component id attached to this stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParallelRtStageMetadata {
    pub label: &'static str,
    pub kind: ParallelRtStageKind,
    pub plan_node_id: NodeId,
    pub component_id: ComponentId,
}

impl ParallelRtStageMetadata {
    pub const fn new(
        label: &'static str,
        kind: ParallelRtStageKind,
        plan_node_id: NodeId,
        component_id: ComponentId,
    ) -> Self {
        Self {
            label,
            kind,
            plan_node_id,
            component_id,
        }
    }
}

/// Immutable scheduler layout shared by every runtime instance of a mission.
///
/// `stages` is in the exact order emitted by the proc macro for the per-CL
/// process path. The stage-affine executor spawns one FIFO worker lane per
/// entry and hands ownership of each in-flight CopperList from one lane to the
/// next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParallelRtMetadata {
    pub stages: &'static [ParallelRtStageMetadata],
}

impl ParallelRtMetadata {
    pub const fn new(stages: &'static [ParallelRtStageMetadata]) -> Self {
        Self { stages }
    }

    #[inline]
    pub const fn process_stage_count(self) -> usize {
        self.stages.len()
    }
}

/// Empty metadata used by tests and by code paths that do not generate any
/// process-stage parallel layout.
pub const DISABLED_PARALLEL_RT_METADATA: ParallelRtMetadata = ParallelRtMetadata::new(&[]);

/// Minimal cache-line padding wrapper used for hot scheduler cursors.
#[repr(align(64))]
pub struct CachePadded<T>(pub T);

impl<T> CachePadded<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T: Debug> Debug for CachePadded<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        self.0.fmt(f)
    }
}

/// Monotonic authorization cursor used by ordered commit.
///
/// `next_clid` is the smallest CopperList id the serial commit path may accept.
#[derive(Debug)]
pub struct CausalityCheckpoint {
    pub next_clid: AtomicU64,
}

impl CausalityCheckpoint {
    pub const fn new(initial_clid: u64) -> Self {
        Self {
            next_clid: AtomicU64::new(initial_clid),
        }
    }

    #[inline]
    pub fn current_clid(&self) -> u64 {
        self.next_clid.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_authorized_for(&self, clid: u64) -> bool {
        self.current_clid() == clid
    }

    #[inline]
    pub fn authorize_next(&self, next_clid: u64) {
        self.next_clid.store(next_clid, Ordering::Release);
    }
}

/// Per-CopperList scratch state carried while a list is in flight.
///
/// The current parallel executor still serializes keyframe capture at commit
/// time, but the ownership model is explicit so future work can move keyframe
/// accumulation fully into the in-flight ticket.
#[derive(Debug, Clone, Default)]
pub struct ParallelKeyFrameScratch {
    pub culistid: u64,
    pub timestamp: CuTime,
    pub serialized_tasks: Vec<u8>,
}

/// Ownership container for one in-flight CopperList.
///
/// Field meanings:
/// - `clid`: globally ordered CopperList id.
/// - `culist`: the boxed CopperList buffer currently being filled or committed.
/// - `keyframe`: optional per-CL snapshot scratch space.
/// - `raw_payload_bytes`: payload bytes observed before serialization.
/// - `handle_bytes`: handle-backed payload accounting for monitor/log I/O stats.
#[derive(Debug)]
pub struct IterationTicket<P: CopperListTuple> {
    pub clid: u64,
    pub culist: Box<CopperList<P>>,
    pub keyframe: Option<Box<ParallelKeyFrameScratch>>,
    pub raw_payload_bytes: u64,
    pub handle_bytes: u64,
}

impl<P> IterationTicket<P>
where
    P: CopperListTuple + CuListZeroedInit,
{
    pub fn new(clid: u64, mut culist: Box<CopperList<P>>) -> Self {
        culist.reset_for_runtime_use(clid);
        Self {
            clid,
            culist,
            keyframe: None,
            raw_payload_bytes: 0,
            handle_bytes: 0,
        }
    }
}

#[cfg(all(feature = "std", feature = "parallel-rt"))]
mod imp {
    use super::{CachePadded, CausalityCheckpoint, ParallelRtMetadata};
    use cu29_traits::CuResult;

    /// Feature-enabled runtime state shared by the generated stage pipeline.
    pub struct ParallelRt<const NBCL: usize> {
        /// Static process-stage layout emitted by the proc macro.
        metadata: &'static ParallelRtMetadata,
        /// Ordered commit cursor for monitor/keyframe/log handoff.
        commit_checkpoint: CachePadded<CausalityCheckpoint>,
        /// Maximum number of CopperLists intended to be in flight at once.
        in_flight_limit: usize,
    }

    impl<const NBCL: usize> ParallelRt<NBCL> {
        pub fn new(metadata: &'static ParallelRtMetadata) -> CuResult<Self> {
            Ok(Self {
                metadata,
                commit_checkpoint: CachePadded::new(CausalityCheckpoint::new(0)),
                in_flight_limit: NBCL,
            })
        }

        #[inline]
        pub const fn enabled(&self) -> bool {
            true
        }

        #[inline]
        pub const fn metadata(&self) -> &'static ParallelRtMetadata {
            self.metadata
        }

        #[inline]
        pub const fn commit_checkpoint(&self) -> &CachePadded<CausalityCheckpoint> {
            &self.commit_checkpoint
        }

        #[inline]
        pub const fn in_flight_limit(&self) -> usize {
            self.in_flight_limit
        }

        /// Reinitializes the ordered commit cursor to the next CopperList id
        /// that will be dispatched by a fresh parallel run loop.
        pub fn reset_cursors(&self, next_clid: u64) {
            self.commit_checkpoint.authorize_next(next_clid);
        }

        #[inline]
        pub fn current_commit_clid(&self) -> u64 {
            self.commit_checkpoint.current_clid()
        }

        #[inline]
        pub fn release_commit(&self, next_clid: u64) {
            self.commit_checkpoint.authorize_next(next_clid);
        }
    }
}

#[cfg(all(feature = "std", feature = "parallel-rt"))]
mod lanes {
    use super::CachePadded;
    use alloc::collections::VecDeque;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};

    /// Spins this many times on a condition before parking on the condvar.
    const SPIN_ROUNDS: u32 = 256;

    /// The lanes' results to the dispatcher. `std::sync::mpsc` spins with
    /// `sched_yield` on a slot a preempted sender has not finished writing,
    /// which never runs a lower-priority sender on the receiver's CPU; this
    /// queue blocks on a mutex and a condvar instead.
    struct ResultQueue<T> {
        queue: Mutex<VecDeque<T>>,
        condvar: Condvar,
        senders: AtomicUsize,
    }

    pub struct ResultSender<T>(Arc<ResultQueue<T>>);
    pub struct ResultReceiver<T>(Arc<ResultQueue<T>>);

    pub fn result_channel<T>() -> (ResultSender<T>, ResultReceiver<T>) {
        let queue = Arc::new(ResultQueue {
            queue: Mutex::new(VecDeque::new()),
            condvar: Condvar::new(),
            senders: AtomicUsize::new(1),
        });
        (ResultSender(queue.clone()), ResultReceiver(queue))
    }

    impl<T> Clone for ResultSender<T> {
        fn clone(&self) -> Self {
            self.0.senders.fetch_add(1, Ordering::AcqRel);
            Self(self.0.clone())
        }
    }

    impl<T> Drop for ResultSender<T> {
        fn drop(&mut self) {
            self.0.senders.fetch_sub(1, Ordering::AcqRel);
            let _guard = self.0.queue.lock().expect("result queue poisoned");
            self.0.condvar.notify_all();
        }
    }

    impl<T> ResultSender<T> {
        pub fn send(&self, value: T) -> Result<(), T> {
            self.0
                .queue
                .lock()
                .expect("result queue poisoned")
                .push_back(value);
            self.0.condvar.notify_one();
            Ok(())
        }
    }

    impl<T> ResultReceiver<T> {
        pub fn recv(&self) -> Result<T, std::sync::mpsc::RecvError> {
            let mut queue = self.0.queue.lock().expect("result queue poisoned");
            loop {
                if let Some(value) = queue.pop_front() {
                    return Ok(value);
                }
                if self.0.senders.load(Ordering::Acquire) == 0 {
                    return Err(std::sync::mpsc::RecvError);
                }
                queue = self.0.condvar.wait(queue).expect("result queue poisoned");
            }
        }

        pub fn recv_timeout(
            &self,
            timeout: std::time::Duration,
        ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
            let deadline = std::time::Instant::now() + timeout;
            let mut queue = self.0.queue.lock().expect("result queue poisoned");
            loop {
                if let Some(value) = queue.pop_front() {
                    return Ok(value);
                }
                if self.0.senders.load(Ordering::Acquire) == 0 {
                    return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(std::sync::mpsc::RecvTimeoutError::Timeout);
                }
                queue = self
                    .0
                    .condvar
                    .wait_timeout(queue, deadline - now)
                    .expect("result queue poisoned")
                    .0;
            }
        }
    }

    struct LaneSlot {
        culist: AtomicPtr<u8>,
        done: AtomicU32,
        aborted: AtomicBool,
    }

    /// Shared state of the lane executor: which CopperLists are admitted, which
    /// occurrences completed which cycle, and how many occurrences of each
    /// in-flight CopperList are still outstanding.
    ///
    /// Every counter is preallocated at construction; the hot path only reads
    /// and writes atomics, spins briefly, and parks on one condvar when a
    /// wait is long. Waiters re-check their condition after every wake-up.
    pub struct LaneExecutor {
        /// The id of the first CopperList this run admits; cycle `c` covers
        /// ids `first_clid + c * copperlists_per_cycle ..`.
        first_clid: u64,
        /// CopperLists with an id below this value are admitted.
        admitted: CachePadded<AtomicU64>,
        /// No further cycle will be admitted; lanes finish admitted cycles.
        stopping: AtomicBool,
        /// A worker failed; every lane stops as soon as it observes this.
        shutdown: AtomicBool,
        /// Per occurrence: the number of cycles it has completed.
        completed: Vec<CachePadded<AtomicU64>>,
        /// Per in-flight slot (`clid % slots.len()`).
        slots: Vec<LaneSlot>,
        occurrences_per_copperlist: u32,
        copperlists_per_cycle: u64,
        generation: CachePadded<AtomicU64>,
        lock: Mutex<()>,
        condvar: Condvar,
    }

    impl LaneExecutor {
        pub fn new(
            first_clid: u64,
            occurrences: usize,
            max_in_flight: usize,
            occurrences_per_copperlist: u32,
            copperlists_per_cycle: u32,
        ) -> Self {
            Self {
                first_clid,
                admitted: CachePadded::new(AtomicU64::new(first_clid)),
                stopping: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                completed: (0..occurrences)
                    .map(|_| CachePadded::new(AtomicU64::new(0)))
                    .collect(),
                slots: (0..max_in_flight.max(1))
                    .map(|_| LaneSlot {
                        culist: AtomicPtr::new(core::ptr::null_mut()),
                        done: AtomicU32::new(0),
                        aborted: AtomicBool::new(false),
                    })
                    .collect(),
                occurrences_per_copperlist,
                copperlists_per_cycle: u64::from(copperlists_per_cycle.max(1)),
                generation: CachePadded::new(AtomicU64::new(0)),
                lock: Mutex::new(()),
                condvar: Condvar::new(),
            }
        }

        #[inline]
        fn slot(&self, clid: u64) -> &LaneSlot {
            &self.slots[(clid % self.slots.len() as u64) as usize]
        }

        #[inline]
        fn notify(&self) {
            self.generation.fetch_add(1, Ordering::AcqRel);
            let _guard = self.lock.lock().expect("lane executor lock poisoned");
            self.condvar.notify_all();
        }

        /// Blocks until `condition` holds. Returns `false` when the executor is
        /// shutting down before it holds.
        fn wait_until(&self, mut condition: impl FnMut() -> bool) -> bool {
            loop {
                for _ in 0..SPIN_ROUNDS {
                    if condition() {
                        return true;
                    }
                    if self.shutdown.load(Ordering::Acquire) {
                        return false;
                    }
                    core::hint::spin_loop();
                }
                let generation = self.generation.load(Ordering::Acquire);
                if condition() {
                    return true;
                }
                if self.shutdown.load(Ordering::Acquire) {
                    return false;
                }
                let guard = self.lock.lock().expect("lane executor lock poisoned");
                if self.generation.load(Ordering::Acquire) == generation {
                    let _guard = self
                        .condvar
                        .wait(guard)
                        .expect("lane executor lock poisoned");
                }
            }
        }

        #[inline]
        pub fn first_clid(&self) -> u64 {
            self.first_clid
        }

        /// The cycle containing `clid`.
        #[inline]
        pub fn cycle_of(&self, clid: u64) -> u64 {
            (clid - self.first_clid) / self.copperlists_per_cycle
        }

        /// Publishes CopperList `clid` to the lanes. `clid` must be the next
        /// unadmitted id and its slot must be free.
        pub fn admit(&self, clid: u64, culist: *mut u8) {
            debug_assert_eq!(self.admitted.load(Ordering::Acquire), clid);
            let slot = self.slot(clid);
            slot.done.store(0, Ordering::Relaxed);
            slot.aborted.store(false, Ordering::Relaxed);
            slot.culist.store(culist, Ordering::Release);
            self.admitted.store(clid + 1, Ordering::Release);
            self.notify();
        }

        /// The next CopperList id to admit.
        #[inline]
        pub fn next_admission(&self) -> u64 {
            self.admitted.load(Ordering::Acquire)
        }

        /// Announces that no further cycle will be admitted. Lanes waiting for
        /// a CopperList that will never come return from their wait.
        pub fn stop_admitting(&self) {
            self.stopping.store(true, Ordering::Release);
            self.notify();
        }

        #[inline]
        pub fn is_stopping(&self) -> bool {
            self.stopping.load(Ordering::Acquire)
        }

        /// Makes every lane stop at its next wait or completion.
        pub fn request_shutdown(&self) {
            self.shutdown.store(true, Ordering::Release);
            self.notify();
        }

        #[inline]
        pub fn is_shut_down(&self) -> bool {
            self.shutdown.load(Ordering::Acquire)
        }

        /// Waits until CopperList `clid` is admitted and returns its buffer.
        /// `None` means the lane must exit: the executor stopped before that
        /// CopperList existed, or it is shutting down.
        pub fn wait_admitted(&self, clid: u64) -> Option<*mut u8> {
            let ready = self.wait_until(|| {
                self.admitted.load(Ordering::Acquire) > clid
                    || self.stopping.load(Ordering::Acquire)
            });
            if !ready || self.admitted.load(Ordering::Acquire) <= clid {
                return None;
            }
            Some(self.slot(clid).culist.load(Ordering::Acquire))
        }

        /// Waits until occurrence `occurrence` has completed cycle `cycle`.
        /// Returns `false` on shutdown.
        pub fn wait_completed(&self, occurrence: usize, cycle: u64) -> bool {
            let counter = &self.completed[occurrence];
            self.wait_until(|| counter.load(Ordering::Acquire) > cycle)
        }

        /// Records that occurrence `occurrence` completed cycle `cycle`.
        pub fn complete(&self, occurrence: usize, cycle: u64) {
            self.completed[occurrence].store(cycle + 1, Ordering::Release);
            self.notify();
        }

        /// Counts one finished occurrence of CopperList `clid`; `true` when it
        /// was the last one, so the CopperList can be committed.
        pub fn finish_occurrence(&self, clid: u64) -> bool {
            self.slot(clid).done.fetch_add(1, Ordering::AcqRel) + 1
                == self.occurrences_per_copperlist
        }

        /// Marks CopperList `clid` aborted: its remaining occurrences skip their
        /// call and only count themselves as finished.
        pub fn abort(&self, clid: u64) {
            self.slot(clid).aborted.store(true, Ordering::Release);
        }

        #[inline]
        pub fn is_aborted(&self, clid: u64) -> bool {
            self.slot(clid).aborted.load(Ordering::Acquire)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;
        use std::thread;

        #[test]
        fn lanes_wait_for_admission_and_dependencies() {
            // Two occurrences per CL: 0 on lane A, 1 on lane B, edge 0 -> 1.
            let lanes = Arc::new(LaneExecutor::new(0, 2, 2, 2, 1));
            let mut buffers = [0u64; 2];
            let ptrs: Vec<*mut u8> = buffers
                .iter_mut()
                .map(|b| b as *mut u64 as *mut u8)
                .collect();
            let order = Arc::new(Mutex::new(Vec::new()));
            let a = {
                let lanes = Arc::clone(&lanes);
                let order = Arc::clone(&order);
                thread::spawn(move || {
                    let mut cycle = 0;
                    while let Some(ptr) = lanes.wait_admitted(cycle) {
                        unsafe { *(ptr as *mut u64) = cycle * 10 };
                        order.lock().unwrap().push((0, cycle));
                        lanes.complete(0, cycle);
                        assert!(!lanes.finish_occurrence(cycle));
                        cycle += 1;
                    }
                    cycle
                })
            };
            let done = Arc::new(Mutex::new(Vec::new()));
            let b = {
                let lanes = Arc::clone(&lanes);
                let order = Arc::clone(&order);
                let done = Arc::clone(&done);
                thread::spawn(move || {
                    let mut cycle = 0;
                    while let Some(ptr) = lanes.wait_admitted(cycle) {
                        assert!(lanes.wait_completed(0, cycle));
                        assert_eq!(unsafe { *(ptr as *mut u64) }, cycle * 10);
                        order.lock().unwrap().push((1, cycle));
                        lanes.complete(1, cycle);
                        if lanes.finish_occurrence(cycle) {
                            done.lock().unwrap().push(cycle);
                        }
                        cycle += 1;
                    }
                    cycle
                })
            };
            for clid in 0..4u64 {
                lanes.admit(clid, ptrs[(clid % 2) as usize]);
                // Wait for the slot to free before reusing it.
                let lanes = Arc::clone(&lanes);
                assert!(lanes.wait_until(|| done.lock().unwrap().len() as u64 > clid));
            }
            lanes.stop_admitting();
            assert_eq!(a.join().unwrap(), 4);
            assert_eq!(b.join().unwrap(), 4);
            let order = order.lock().unwrap();
            for cycle in 0..4 {
                let a_at = order.iter().position(|&e| e == (0, cycle)).unwrap();
                let b_at = order.iter().position(|&e| e == (1, cycle)).unwrap();
                assert!(a_at < b_at);
            }
            assert_eq!(*done.lock().unwrap(), vec![0, 1, 2, 3]);
        }

        #[test]
        fn shutdown_releases_waiters_and_aborts_are_per_slot() {
            let lanes = Arc::new(LaneExecutor::new(0, 1, 1, 1, 1));
            let waiter = {
                let lanes = Arc::clone(&lanes);
                thread::spawn(move || {
                    (lanes.wait_admitted(0).is_none(), lanes.wait_completed(0, 0))
                })
            };
            lanes.request_shutdown();
            assert_eq!(waiter.join().unwrap(), (true, false));
            let lanes = LaneExecutor::new(0, 1, 2, 1, 2);
            let mut buffer = 0u8;
            lanes.admit(0, &mut buffer as *mut u8);
            lanes.abort(0);
            assert!(lanes.is_aborted(0));
            lanes.admit(1, &mut buffer as *mut u8);
            assert!(!lanes.is_aborted(1));
            assert_eq!(lanes.cycle_of(1), 0);
            assert_eq!(lanes.cycle_of(2), 1);
        }
    }
}

#[cfg(all(feature = "std", feature = "parallel-rt"))]
pub use lanes::{LaneExecutor, ResultReceiver, ResultSender, result_channel};

#[cfg(not(all(feature = "std", feature = "parallel-rt")))]
mod imp {
    use super::{CachePadded, CausalityCheckpoint, ParallelRtMetadata};
    use cu29_traits::CuResult;

    /// Feature-disabled placeholder.
    ///
    /// Keeping the type available lets the rest of the runtime compose against a
    /// single API while the actual executor remains behind the `parallel-rt`
    /// feature.
    pub struct ParallelRt<const NBCL: usize> {
        metadata: &'static ParallelRtMetadata,
        commit_checkpoint: CachePadded<CausalityCheckpoint>,
    }

    impl<const NBCL: usize> ParallelRt<NBCL> {
        pub fn new(metadata: &'static ParallelRtMetadata) -> CuResult<Self> {
            Ok(Self {
                metadata,
                commit_checkpoint: CachePadded::new(CausalityCheckpoint::new(0)),
            })
        }

        #[inline]
        pub const fn enabled(&self) -> bool {
            false
        }

        #[inline]
        pub const fn metadata(&self) -> &'static ParallelRtMetadata {
            self.metadata
        }

        #[inline]
        pub const fn commit_checkpoint(&self) -> &CachePadded<CausalityCheckpoint> {
            &self.commit_checkpoint
        }

        #[inline]
        pub const fn in_flight_limit(&self) -> usize {
            NBCL
        }

        #[inline]
        pub fn reset_cursors(&self, next_clid: u64) {
            self.commit_checkpoint.authorize_next(next_clid);
        }

        #[inline]
        pub fn current_commit_clid(&self) -> u64 {
            self.commit_checkpoint.current_clid()
        }

        #[inline]
        pub fn release_commit(&self, next_clid: u64) {
            self.commit_checkpoint.authorize_next(next_clid);
        }
    }
}

pub use imp::ParallelRt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitoring::ComponentId;

    #[test]
    fn checkpoint_advances_monotonically() {
        let checkpoint = CausalityCheckpoint::new(0);
        assert!(checkpoint.is_authorized_for(0));
        checkpoint.authorize_next(1);
        assert!(!checkpoint.is_authorized_for(0));
        assert!(checkpoint.is_authorized_for(1));
    }

    #[test]
    fn disabled_metadata_is_empty() {
        assert_eq!(DISABLED_PARALLEL_RT_METADATA.process_stage_count(), 0);
    }

    #[test]
    fn parallel_rt_stage_metadata_is_const_constructible() {
        const STAGES: &[ParallelRtStageMetadata] = &[ParallelRtStageMetadata::new(
            "demo",
            ParallelRtStageKind::Task,
            7,
            ComponentId::new(3),
        )];
        const METADATA: ParallelRtMetadata = ParallelRtMetadata::new(STAGES);
        assert_eq!(METADATA.process_stage_count(), 1);
        assert_eq!(METADATA.stages[0].label, "demo");
    }

    #[cfg(all(feature = "std", feature = "parallel-rt"))]
    #[test]
    fn enabled_parallel_rt_tracks_metadata_and_limit() {
        const STAGES: &[ParallelRtStageMetadata] = &[
            ParallelRtStageMetadata::new("a", ParallelRtStageKind::Task, 0, ComponentId::new(0)),
            ParallelRtStageMetadata::new("b", ParallelRtStageKind::Task, 1, ComponentId::new(1)),
        ];
        const METADATA: ParallelRtMetadata = ParallelRtMetadata::new(STAGES);

        let rt = ParallelRt::<4>::new(&METADATA).expect("parallel rt should build");
        assert!(rt.enabled());
        assert_eq!(rt.metadata().process_stage_count(), 2);
        assert_eq!(rt.in_flight_limit(), 4);
    }

    #[cfg(not(all(feature = "std", feature = "parallel-rt")))]
    #[test]
    fn disabled_parallel_rt_preserves_metadata() {
        const STAGES: &[ParallelRtStageMetadata] = &[ParallelRtStageMetadata::new(
            "a",
            ParallelRtStageKind::Task,
            0,
            ComponentId::new(0),
        )];
        const METADATA: ParallelRtMetadata = ParallelRtMetadata::new(STAGES);

        let rt = ParallelRt::<4>::new(&METADATA).expect("parallel rt placeholder should build");
        assert!(!rt.enabled());
        assert_eq!(rt.metadata().process_stage_count(), 1);
    }
}
