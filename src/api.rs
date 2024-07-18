//! Task APIs for multi-task configuration.

use alloc::{string::String, sync::Arc};
#[cfg(not(feature = "async"))]
#[cfg(feature = "monolithic")]
use axhal::KERNEL_PROCESS_ID;

use crate::task::{ScheduleTask, TaskState};

use crate::schedule::get_wait_for_exit_queue;
#[doc(cfg(feature = "multitask"))]
pub use crate::task::{new_task, CurrentTask, TaskId};
#[doc(cfg(feature = "multitask"))]
pub use crate::wait_queue::WaitQueue;

pub use crate::processor::{current_processor, Processor};

pub use crate::schedule::schedule;

#[cfg(feature = "irq")]
pub use crate::schedule::schedule_timeout;

/// The reference type of a task.
pub type AxTaskRef = Arc<AxTask>;

cfg_if::cfg_if! {
    if #[cfg(feature = "sched_rr")] {
        const MAX_TIME_SLICE: usize = 5;
        pub(crate) type AxTask = scheduler::RRTask<ScheduleTask, MAX_TIME_SLICE>;
        pub(crate) type Scheduler = scheduler::RRScheduler<ScheduleTask, MAX_TIME_SLICE>;
    } else if #[cfg(feature = "sched_cfs")] {
        pub(crate) type AxTask = scheduler::CFSTask<ScheduleTask>;
        pub(crate) type Scheduler = scheduler::CFScheduler<ScheduleTask>;
    } else {
        // If no scheduler features are set, use FIFO as the default.
        pub(crate) type AxTask = scheduler::FifoTask<ScheduleTask>;
        pub(crate) type Scheduler = scheduler::FifoScheduler<ScheduleTask>;
    }
}

/// Gets the current task, or returns [`None`] if the current task is not
/// initialized.
pub fn current_may_uninit() -> Option<CurrentTask> {
    CurrentTask::try_get()
}

/// Gets the current task.
///
/// # Panics
///
/// Panics if the current task is not initialized.
pub fn current() -> CurrentTask {
    CurrentTask::get()
}

/// Initializes the task scheduler (for the primary CPU).
pub fn init_scheduler() {
    info!("Initialize scheduling...");

    crate::processor::init();
    #[cfg(feature = "irq")]
    crate::timers::init();

    info!("  use {} scheduler.", Scheduler::scheduler_name());
}

/// Initializes the task scheduler for secondary CPUs.
pub fn init_scheduler_secondary() {
    crate::processor::init_secondary();
}

/// Handles periodic timer ticks for the task manager.
///
/// For example, advance scheduler states, checks timed events, etc.
#[cfg(feature = "irq")]
#[doc(cfg(feature = "irq"))]
pub fn on_timer_tick() {
    crate::timers::check_events();
    crate::schedule::scheduler_timer_tick();
}

#[cfg(feature = "preempt")]
/// Checks if the current task should be preempted.
/// This api called after handle irq,it may be on a
/// disable_preempt ctx
pub fn current_check_preempt_pending() {
    let curr = crate::current();
    // if task is already exited or blocking,
    // no need preempt, they are rescheduling
    if curr.get_preempt_pending() && curr.can_preempt() && !curr.is_exited() && !curr.is_blocking()
    {
        debug!(
            "current {} is to be preempted , allow {}",
            curr.id_name(),
            curr.can_preempt()
        );
        #[cfg(not(feature = "async"))]
        crate::schedule::schedule();
        #[cfg(feature = "async")]
        crate::task_switch::preempt_switch_entry();
    }
}

#[cfg(not(feature = "async"))]
/// Spawns a new task with the given parameters.
///
/// Returns the task reference.
pub fn spawn_raw<F>(f: F, name: String, stack_size: usize) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    let task = new_task(
        f,
        name,
        stack_size,
        #[cfg(feature = "monolithic")]
        KERNEL_PROCESS_ID,
        #[cfg(feature = "monolithic")]
        0,
    );
    Processor::first_add_task(task.clone());
    task
}

#[cfg(not(feature = "async"))]
/// Spawns a new task with the default parameters.
///
/// The default task name is an empty string. The default task stack size is
/// [`axconfig::TASK_STACK_SIZE`].
///
/// Returns the task reference.
pub fn spawn<F>(f: F) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_raw(f, "".into(), axconfig::TASK_STACK_SIZE)
}

/// Set the priority for current task.
///
/// The range of the priority is dependent on the underlying scheduler. For
/// example, in the [CFS] scheduler, the priority is the nice value, ranging from
/// -20 to 19.
///
/// Returns `true` if the priority is set successfully.
///
/// [CFS]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub fn set_priority(prio: isize) -> bool {
    crate::schedule::set_current_priority(prio)
}

#[cfg(not(feature = "async"))]
/// Current task gives up the CPU time voluntarily, and switches to another
/// ready task.
pub fn yield_now() {
    crate::schedule::yield_current();
}

#[cfg(not(feature = "async"))]
/// Current task is going to sleep for the given duration.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub fn sleep(dur: core::time::Duration) {
    sleep_until(axhal::time::current_time() + dur);
}

#[cfg(not(feature = "async"))]
/// Current task is going to sleep, it will be woken up at the given deadline.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub fn sleep_until(deadline: axhal::time::TimeValue) {
    #[cfg(feature = "irq")]
    crate::schedule::schedule_timeout(deadline);
    #[cfg(not(feature = "irq"))]
    axhal::time::busy_wait_until(deadline);
}

/// wake up task
pub fn wakeup_task(task: AxTaskRef) {
    log::debug!("wakeup {}", task.id_name());
    crate::schedule::wakeup_task(task)
}

#[cfg(not(feature = "async"))]
/// Current task is going to sleep, it will be woken up when the given task exits.
///
/// If the given task is already exited, it will return immediately.
pub fn join(task: &AxTaskRef) -> Option<i32> {
    get_wait_for_exit_queue(task)
        .map(|wait_queue| wait_queue.wait_until(|| task.state() == TaskState::Exited));
    Some(task.get_exit_code())
}

#[cfg(not(feature = "async"))]
#[cfg(feature = "monolithic")]
/// Current task is going to sleep. It will be woken up when the given task does exec syscall or exit.
pub fn vfork_suspend(task: &AxTaskRef) {
    get_wait_for_exit_queue(task).map(|wait_queue| {
        wait_queue.wait_until(|| {
            // If the given task does the exec syscall, it will be the leader of the new process.
            task.is_leader() || task.state() == TaskState::Exited
        });
    });
}

#[cfg(feature = "monolithic")]
/// To wake up the task that is blocked because vfork out of current task
pub fn wake_vfork_process(task: &AxTaskRef) {
    get_wait_for_exit_queue(task).map(|wait_queue| wait_queue.notify_all());
}

/// Exits the current task.
pub fn exit(exit_code: i32) -> ! {
    crate::schedule::exit_current(exit_code)
}

/// The idle task routine.
///
/// It runs an infinite loop that keeps calling [`yield_now()`].
pub fn run_idle() -> ! {
    loop {
        #[cfg(not(feature = "async"))]
        yield_now();
        #[cfg(feature = "async")]
        schedule();
        //debug!("idle task: waiting for IRQs...");
        #[cfg(feature = "irq")]
        axhal::arch::wait_for_irqs();
    }
}

#[cfg(feature = "async")]
/// Current task gives up the CPU time voluntarily, and switches to another
/// ready task.
pub async fn yield_now() {
    crate::schedule::yield_current().await;
}

#[cfg(feature = "async")]
/// Current task is going to sleep for the given duration.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub async fn sleep(dur: core::time::Duration) {
    sleep_until(axhal::time::current_time() + dur).await;
}

#[cfg(feature = "async")]
/// Current task is going to sleep, it will be woken up at the given deadline.
///
/// If the feature `irq` is not enabled, it uses busy-wait instead.
pub async fn sleep_until(deadline: axhal::time::TimeValue) {
    #[cfg(feature = "irq")]
    crate::schedule::schedule_timeout(deadline).await;
    #[cfg(not(feature = "irq"))]
    axhal::time::busy_wait_until(deadline);
}

#[cfg(feature = "async")]
use core::future::Future;

#[cfg(feature = "async")]
/// Spawns a new task with the given parameters.
///
/// Returns the task reference.
pub fn spawn_raw<F, T>(f: F, name: String) -> AxTaskRef
where
    F: FnOnce() -> T,
    T: core::future::Future<Output = i32> + Send + 'static,
{
    let task = new_task(
        f,
        name,
    );
    Processor::first_add_task(task.clone());
    task
}

#[cfg(feature = "async")]
/// Spawns a new task with the given parameters.
///
/// Returns the task reference.
pub fn spawn<F, T>(f: F) -> AxTaskRef
where
    F: FnOnce() -> T,
    T: core::future::Future<Output = i32> + Send + 'static,
{
    spawn_raw(f, "".into())
}

#[cfg(feature = "async")]
/// Current task is going to sleep, it will be woken up when the given task exits.
///
/// If the given task is already exited, it will return immediately.
pub async fn join(task: &AxTaskRef) -> Option<i32> {
    if let Some(wait_queue) = get_wait_for_exit_queue(task) {
        wait_queue.wait_until(|| task.state() == TaskState::Exited).await;
    }
    Some(task.get_exit_code())
}

#[cfg(feature = "async")]
#[cfg(feature = "monolithic")]
/// Current task is going to sleep. It will be woken up when the given task does exec syscall or exit.
pub async fn vfork_suspend(task: &AxTaskRef) {
    if let Some(wait_queue) = get_wait_for_exit_queue(task) {
        wait_queue.wait_until(|| {
            // If the given task does the exec syscall, it will be the leader of the new process.
            task.is_leader() || task.state() == TaskState::Exited
        }).await;   
    }
}

#[cfg(feature = "async")]
/// The idle task routine.
///
/// It runs an infinite loop that keeps calling [`yield_now()`].
pub async fn async_run_idle() -> ! {
    loop {
        yield_now().await;
        //debug!("idle task: waiting for IRQs...");
        #[cfg(feature = "irq")]
        axhal::arch::wait_for_irqs();
    }
}

#[cfg(feature = "async")]
/// Run a task to completion
pub fn block_on<F, T>(f: F, name: alloc::string::String) -> T::Output
where
    F: FnOnce() -> T,
    T: Future<Output = i32> + Send + 'static,
{
    spawn_raw(f, name);
    loop {
        schedule();
    }
}
