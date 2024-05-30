use alloc::collections::VecDeque;
use spinlock::SpinNoIrq;
use alloc::sync::Arc;
use crate::AxTaskRef;

/// A queue to store sleeping tasks.
///
/// # Examples
///
/// ```
/// use axtask::WaitQueue;
/// use core::sync::atomic::{AtomicU32, Ordering};
///
/// static VALUE: AtomicU32 = AtomicU32::new(0);
/// static WQ: WaitQueue = WaitQueue::new();
///
/// axtask::init_scheduler();
/// // spawn a new task that updates `VALUE` and notifies the main task
/// axtask::spawn(|| {
///     assert_eq!(VALUE.load(Ordering::Relaxed), 0);
///     VALUE.fetch_add(1, Ordering::Relaxed);
///     WQ.notify_one(true); // wake up the main task
/// });
///
/// WQ.wait(); // block until `notify()` is called
/// assert_eq!(VALUE.load(Ordering::Relaxed), 1);
/// ```
pub struct WaitQueue {
    // Support queue lock by external caller,use SpinNoIrq
    // Arceos SpinNoirq current implementation implies irq_save,
    // so it can be nested
    // TODO: use linked list has good performance
    queue: SpinNoIrq<VecDeque<AxTaskRef>>,
}

impl WaitQueue {
    /// Creates an empty wait queue.
    pub const fn new() -> Self {
        Self {
            queue: SpinNoIrq::new(VecDeque::new()),
        }
    }

    /// Creates an empty wait queue with space for at least `capacity` elements.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: SpinNoIrq::new(VecDeque::with_capacity(capacity)),
        }
    }

    // CPU0 wait                CPU1 notify           CPU2 signal 
    // q.lock().push(curr)                     
    //                          q.lock.get()          unblock_task()
    //                          unblock(task)
    // block_current()          
    //                          q.unlock()
    // queue.lock().remove(curr)  
    /// Blocks the current task and put it into the wait queue, until other task
    /// notifies it.
    pub fn wait(&self) {
        let curr = crate::current();
        curr.reset_wakeup_cnt();
        self.queue.lock().push_back(curr.clone());
        crate::run_queue::block_current();
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the condition becomes true.
    pub fn wait_until<F>(&self, condition: F)
    where F: Fn() -> bool 
    {
        let curr = crate::current();
        curr.reset_wakeup_cnt();
        self.queue.lock().push_back(curr.clone());
        loop {
            if condition() {
                break;
            }
            crate::run_queue::block_current();
        }
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
    }

    /// Blocks the current task and put it into the wait queue, until other tasks
    /// notify it, or the given duration has elapsed.
    #[cfg(feature = "irq")]
    pub fn wait_timeout(&self, dur: core::time::Duration) -> bool {
        let curr = crate::current();
        let deadline = axhal::time::current_time() + dur;
        debug!(
            "task wait_timeout: {} deadline={:?}",
            curr.id_name(),
            deadline
        );
        curr.reset_wakeup_cnt();
        self.queue.lock().push_back(curr.clone());
        let timeout = crate::run_queue::schedule_timeout(deadline);
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
        timeout
    }

    /// Blocks the current task and put it into the wait queue, until the given
    /// `condition` becomes true, or the given duration has elapsed.
    ///
    /// Note that even other tasks notify this task, it will not wake up until
    /// the above conditions are met.
    #[cfg(feature = "irq")]
    pub fn wait_timeout_until<F>(&self, dur: core::time::Duration, condition: F) -> bool
    where
        F: Fn() -> bool,
    {
        let curr = crate::current();
        let deadline = axhal::time::current_time() + dur;
        debug!(
            "task wait_timeout: {}, deadline={:?}",
            curr.id_name(),
            deadline
        );

        let mut timeout = false;
        curr.reset_wakeup_cnt();
        self.queue.lock().push_back(curr.clone());
        loop {
            if condition() {
                break;
            }
            timeout = crate::run_queue::schedule_timeout(deadline);
            if timeout {
                break;
            }
        }
        self.queue.lock().retain(|t| !curr.ptr_eq(t));
        timeout
    }

    /// Wake up the given task in the wait queue.
    ///
    /// If `resched` is true, the current task will be preempted when the
    /// preemption is enabled.
    pub fn notify_task(&self, resched: bool, task: &AxTaskRef) -> bool {
        let wq = self.queue.lock();
        if let Some(_) = wq.iter().position(|t| Arc::ptr_eq(t, task)) {
            crate::run_queue::wake_task(task.clone(), resched);
            true
        } else {
            false
        }
    }

    /// Wakes up one task in the wait queue, usually the first one.
    ///
    /// If `resched` is true, the current task will be preempted when the
    /// preemption is enabled.
    pub fn notify_one(&self, resched: bool) -> bool {
        let mut queue = self.queue.lock();
        let task = queue.front_mut();
        match task {
            None => false,
            Some(t) => {
                crate::run_queue::wake_task(t.clone(), resched);
                true
            },
        }
    }

    /// Wakes all tasks in the wait queue.
    ///
    /// If `resched` is true, the current task will be preempted when the
    /// preemption is enabled.
    pub fn notify_all(&self, resched: bool) {
        let mut queue = self.queue.lock();
        for task in queue.iter_mut() {
            crate::run_queue::wake_task(task.clone(), resched);
        }
    }
}
