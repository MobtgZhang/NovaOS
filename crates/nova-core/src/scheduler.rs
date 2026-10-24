//! Round-robin scheduling, as a pure state machine.
//!
//! # Why the scheduler is separate from the context switch
//!
//! Two things are called "the scheduler" and they are not the same thing:
//!
//! 1. **The policy** — which thread runs next, when a thread's timeslice
//!    expires, when a thread exits and who reaps it. This is a state machine
//!    over a queue and a set of states.
//! 2. **The mechanism** — saving registers, switching stacks, loading a new
//!    CR3. This is architecture-specific assembly.
//!
//! Only the first is here, and it is here in full. The second lives in the
//! kernel's `sched` module, which owns the assembly and calls into this state
//! machine to ask "who runs next?".
//!
//! # Why the state machine is host-testable
//!
//! Round-robin scheduling is exactly the kind of code whose bugs are
//! invisible in a running kernel: a thread that is starved looks like a
//! thread that is slow, and a reaper that misses an exit looks like a thread
//! that is blocked forever. Neither is distinguishable from a hang without a
//! debugger.
//!
//! As a pure state machine with no `unsafe` and no architecture, the whole
//! policy is exercised by the host test suite: queue ordering, timeslice
//! expiry, exit, reaping, and the invariant that a thread is never in the
//! runnable queue twice.

/// Maximum number of threads the scheduler tracks.
///
/// A fixed bound rather than a growable list: the kernel has no allocator. The
/// bound is generous for v0.1.0 — the whole point of the milestone is that
/// *two* processes exist — and a thread that cannot be created fails loudly.
pub const MAX_THREADS: usize = 64;

/// The states a thread can be in.
///
/// The set is deliberately small. "Runnable" and "running" are distinct
/// because the running thread is the one whose timeslice is being consumed, and
/// conflating them makes the expiry path ambiguous: is the current thread in
/// the queue or not?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    /// Ready to run, waiting its turn.
    Runnable,
    /// Currently running on the CPU.
    Running,
    /// Waiting for something — a message, a reply, a timer.
    Blocked,
    /// Finished; awaiting reaping.
    Exited,
}

impl ThreadState {
    /// A stable tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            ThreadState::Runnable => "runnable",
            ThreadState::Running => "running",
            ThreadState::Blocked => "blocked",
            ThreadState::Exited => "exited",
        }
    }
}

/// Why a scheduling operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedError {
    /// The thread table is full.
    ThreadLimit,
    /// No such thread.
    UnknownThread,
    /// The thread is not in a state that permits the operation.
    InvalidState,
    /// No thread is runnable.
    NoRunnableThread,
    /// The thread's timeslice has not expired.
    TimesliceNotExpired,
}

impl SchedError {
    /// A stable, lowercase tag for the serial log.
    pub const fn tag(self) -> &'static str {
        match self {
            SchedError::ThreadLimit => "thread-limit",
            SchedError::UnknownThread => "unknown-thread",
            SchedError::InvalidState => "invalid-state",
            SchedError::NoRunnableThread => "no-runnable-thread",
            SchedError::TimesliceNotExpired => "timeslice-not-expired",
        }
    }
}

impl core::fmt::Display for SchedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.tag())
    }
}

/// A scheduler-visible thread record.
///
/// The register context and the kernel stack live in the kernel's `sched`
/// module; this is the *policy* half — the identity, the state, and the
/// accounting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ThreadRecord {
    /// The thread's identifier.
    pub id: u32,
    /// The state the thread is in.
    pub state: ThreadState,
    /// The process the thread belongs to.
    pub process: u32,
    /// How many ticks the thread has been given.
    pub ticks: u64,
    /// The thread's scheduling priority. Lower runs first.
    pub priority: u8,
}

impl ThreadRecord {
    /// A new, runnable thread record.
    pub const fn new(id: u32, process: u32, priority: u8) -> Self {
        Self {
            id,
            state: ThreadState::Runnable,
            process,
            ticks: 0,
            priority,
        }
    }
}

/// The round-robin scheduler's state.
///
/// # The queue
///
/// The runnable queue is a fixed-size ring of thread identifiers. A ring
/// rather than a linked list because a linked list needs allocation for its
/// nodes, and because the queue's *order* is the whole point of a round-robin
/// policy — a structure that made the order hard to read would make the policy
/// hard to test.
#[derive(Clone)]
pub struct Scheduler {
    /// The thread records, indexed by slot.
    threads: [Option<ThreadRecord>; MAX_THREADS],
    /// How many slots are occupied.
    count: usize,
    /// The runnable queue: a ring of thread identifiers.
    queue: [u32; MAX_THREADS],
    /// The queue's head.
    head: usize,
    /// The queue's length.
    queue_len: usize,
    /// The identifier the next created thread will receive.
    next_id: u32,
    /// The currently running thread, if any.
    current: Option<u32>,
    /// How many ticks the current thread has consumed.
    current_ticks: u64,
    /// How many ticks a thread may consume before it is preempted.
    pub timeslice: u64,
    /// How many threads have exited and not yet been reaped.
    exited: u32,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(4)
    }
}

impl Scheduler {
    /// A scheduler with a given timeslice, in ticks.
    pub fn new(timeslice: u64) -> Self {
        Self {
            threads: [None; MAX_THREADS],
            count: 0,
            queue: [0; MAX_THREADS],
            head: 0,
            queue_len: 0,
            next_id: 1,
            current: None,
            current_ticks: 0,
            timeslice,
            exited: 0,
        }
    }

    /// How many threads the scheduler tracks.
    pub const fn thread_count(&self) -> usize {
        self.count
    }

    /// How many threads are in the runnable queue.
    pub const fn runnable_count(&self) -> usize {
        self.queue_len
    }

    /// The currently running thread's identifier.
    pub const fn current(&self) -> Option<u32> {
        self.current
    }

    /// How many threads have exited and not yet been reaped.
    pub const fn exited_count(&self) -> u32 {
        self.exited
    }

    /// The record for `id`, if it exists.
    pub fn thread(&self, id: u32) -> Option<&ThreadRecord> {
        self.threads.iter().flatten().find(|record| record.id == id)
    }

    /// The identifier of the thread at the front of the runnable queue.
    pub fn next_runnable(&self) -> Option<u32> {
        if self.queue_len == 0 {
            None
        } else {
            Some(self.queue[self.head])
        }
    }

    /// The runnable queue, in order.
    pub fn runnable_order(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.queue_len).map(|offset| self.queue[(self.head + offset) % MAX_THREADS])
    }

    /// Create a runnable thread.
    pub fn create_thread(&mut self, process: u32, priority: u8) -> Result<u32, SchedError> {
        if self.count >= MAX_THREADS {
            return Err(SchedError::ThreadLimit);
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let slot = self
            .threads
            .iter()
            .position(|entry| entry.is_none())
            .ok_or(SchedError::ThreadLimit)?;

        self.threads[slot] = Some(ThreadRecord::new(id, process, priority));
        self.count += 1;

        self.enqueue(id)?;

        Ok(id)
    }

    /// Put `id` at the back of the runnable queue.
    fn enqueue(&mut self, id: u32) -> Result<(), SchedError> {
        if self.queue_len >= MAX_THREADS {
            return Err(SchedError::ThreadLimit);
        }

        let tail = (self.head + self.queue_len) % MAX_THREADS;
        self.queue[tail] = id;
        self.queue_len += 1;

        Ok(())
    }

    /// Take the thread at the front of the runnable queue.
    fn dequeue(&mut self) -> Option<u32> {
        if self.queue_len == 0 {
            return None;
        }

        let id = self.queue[self.head];
        self.head = (self.head + 1) % MAX_THREADS;
        self.queue_len -= 1;

        Some(id)
    }

    /// Remove `id` from the runnable queue, wherever it is.
    fn remove_from_queue(&mut self, id: u32) {
        if self.queue_len == 0 {
            return;
        }

        // The queue is a ring, so removing from the middle means shifting the
        // tail down one slot. The cost is bounded by the queue length, and
        // removals are rare — a thread exits or blocks, which happens far less
        // often than a thread runs.
        let mut position = None;
        for offset in 0..self.queue_len {
            if self.queue[(self.head + offset) % MAX_THREADS] == id {
                position = Some(offset);
                break;
            }
        }

        let Some(offset) = position else {
            return;
        };

        for index in offset..self.queue_len - 1 {
            self.queue[(self.head + index) % MAX_THREADS] =
                self.queue[(self.head + index + 1) % MAX_THREADS];
        }

        self.queue_len -= 1;
    }

    /// Mark `id` as blocked, removing it from the runnable queue.
    pub fn block(&mut self, id: u32) -> Result<(), SchedError> {
        let record = self.thread_mut(id).ok_or(SchedError::UnknownThread)?;

        if record.state == ThreadState::Exited {
            return Err(SchedError::InvalidState);
        }

        record.state = ThreadState::Blocked;
        self.remove_from_queue(id);

        if self.current == Some(id) {
            self.current = None;
        }

        Ok(())
    }

    /// Mark `id` as runnable again, putting it at the back of the queue.
    pub fn unblock(&mut self, id: u32) -> Result<(), SchedError> {
        let record = self.thread_mut(id).ok_or(SchedError::UnknownThread)?;

        if record.state == ThreadState::Exited {
            return Err(SchedError::InvalidState);
        }

        if record.state == ThreadState::Runnable {
            // Already queued. Enqueueing it again would put the same thread in
            // the queue twice, and it would then run twice per round — which
            // looks exactly like a scheduler that loses track of its threads.
            return Ok(());
        }

        record.state = ThreadState::Runnable;
        self.enqueue(id)?;

        Ok(())
    }

    /// Mark `id` as exited.
    ///
    /// An exited thread is not runnable and must not be reaped until the
    /// kernel has collected whatever it owns — which is why the state is
    /// distinct from "gone".
    pub fn exit(&mut self, id: u32) -> Result<(), SchedError> {
        let record = self.thread_mut(id).ok_or(SchedError::UnknownThread)?;

        if record.state == ThreadState::Exited {
            return Err(SchedError::InvalidState);
        }

        record.state = ThreadState::Exited;
        self.remove_from_queue(id);

        if self.current == Some(id) {
            self.current = None;
        }

        self.exited += 1;

        Ok(())
    }

    /// Reap `id`, releasing its slot.
    ///
    /// Reaping a thread that has not exited is refused: the kernel frees the
    /// thread's kernel stack and its address space here, and doing that for a
    /// thread that is still running would free memory out from under it.
    pub fn reap(&mut self, id: u32) -> Result<(), SchedError> {
        let slot = self
            .threads
            .iter()
            .position(|entry| entry.is_some_and(|record| record.id == id))
            .ok_or(SchedError::UnknownThread)?;

        let record = self.threads[slot].expect("the slot was just found to be occupied");

        if record.state != ThreadState::Exited {
            return Err(SchedError::InvalidState);
        }

        self.threads[slot] = None;
        self.count -= 1;
        self.exited -= 1;

        Ok(())
    }

    /// Whether `id` has exited and is awaiting reaping.
    pub fn is_exited(&self, id: u32) -> bool {
        self.thread(id)
            .is_some_and(|record| record.state == ThreadState::Exited)
    }

    /// Borrow a thread record mutably.
    fn thread_mut(&mut self, id: u32) -> Option<&mut ThreadRecord> {
        self.threads
            .iter_mut()
            .flatten()
            .find(|record| record.id == id)
    }

    /// Choose the next thread to run.
    ///
    /// The current thread, if any, is returned to the back of the queue first,
    /// so the result is the thread that has been waiting longest — the
    /// round-robin property.
    ///
    /// Returns `None` when nothing is runnable, which is the scheduler's
    /// "idle" condition: the kernel halts the CPU until an interrupt arrives.
    pub fn schedule(&mut self) -> Option<u32> {
        if let Some(current) = self.current {
            if let Some(record) = self.thread_mut(current) {
                if record.state == ThreadState::Running {
                    record.state = ThreadState::Runnable;
                }
            }
            // The current thread goes back to the queue *unless* it exited or
            // blocked, in which case it is no longer in it.
            if self
                .thread(current)
                .is_some_and(|record| record.state == ThreadState::Runnable)
                && !self.queue_contains(current)
            {
                let _ = self.enqueue(current);
            }
        }

        let next = self.dequeue()?;

        if let Some(record) = self.thread_mut(next) {
            record.state = ThreadState::Running;
        }

        self.current = Some(next);
        self.current_ticks = 0;

        Some(next)
    }

    /// Whether `id` is in the runnable queue.
    fn queue_contains(&self, id: u32) -> bool {
        (0..self.queue_len).any(|offset| self.queue[(self.head + offset) % MAX_THREADS] == id)
    }

    /// Account one tick against the running thread.
    ///
    /// Returns `true` when the timeslice has expired and the thread should be
    /// preempted.
    pub fn tick(&mut self) -> bool {
        self.current_ticks += 1;

        if let Some(current) = self.current
            && let Some(record) = self.thread_mut(current)
        {
            record.ticks += 1;
        }

        self.current_ticks >= self.timeslice
    }

    /// How many ticks the running thread has consumed.
    pub const fn current_ticks(&self) -> u64 {
        self.current_ticks
    }

    /// Yield the remainder of the current thread's timeslice.
    ///
    /// The thread goes to the back of the queue and the next one is chosen
    /// immediately.
    pub fn yield_current(&mut self) -> Option<u32> {
        if self.current.is_none() {
            return None;
        }

        self.current_ticks = 0;
        self.schedule()
    }

    /// How many threads each process has.
    pub fn process_thread_count(&self, process: u32) -> usize {
        self.threads
            .iter()
            .flatten()
            .filter(|record| record.process == process)
            .count()
    }
}
