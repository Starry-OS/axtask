use alloc::{string::String, sync::Arc};

use core::{mem::ManuallyDrop, ops::Deref};

#[cfg(not(feature = "async"))]
use {
    alloc::boxed::Box,
    memory_addr::VirtAddr,
    crate::current_processor,
};

#[cfg(not(feature = "async"))]
#[cfg(feature = "monolithic")]
use axhal::arch::TrapFrame;

use crate::{
    processor::Processor, schedule::add_wait_for_exit_queue, AxTask, AxTaskRef,
};

pub use taskctx::{TaskId, TaskInner};

use spinlock::{SpinNoIrq, SpinNoIrqOnly, SpinNoIrqOnlyGuard};

extern "C" {
    fn _stdata();
    fn _etdata();
    fn _etbss();
}

#[cfg(feature = "tls")]
pub(crate) fn tls_area() -> (usize, usize) {
    (_stdata as usize, _etbss as usize)
}

#[cfg(feature = "async")]
use core::future::Future;

/// The possible states of a task.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum TaskState {
    Runable = 1,
    Blocking = 2,
    Blocked = 3,
    Exited = 4,
}

pub struct ScheduleTask {
    inner: TaskInner,
    /// Task state
    state: SpinNoIrqOnly<TaskState>,
    /// Task own which Processor
    processor: SpinNoIrq<Option<&'static Processor>>,
}

impl ScheduleTask {
    fn new(inner: TaskInner) -> Self {
        Self {
            state: SpinNoIrqOnly::new(TaskState::Runable),
            processor: SpinNoIrq::new(None),
            inner: inner,
        }
    }

    #[inline]
    /// lock the task state and ctx_ptr access
    pub fn state_lock_manual(&self) -> ManuallyDrop<SpinNoIrqOnlyGuard<TaskState>> {
        ManuallyDrop::new(self.state.lock())
    }

    #[inline]
    /// set the state of the task
    pub fn state(&self) -> TaskState {
        *self.state.lock()
    }

    #[inline]
    /// set the state of the task
    pub fn set_state(&self, state: TaskState) {
        *self.state.lock() = state
    }

    /// Whether the task is Exited
    #[inline]
    pub fn is_exited(&self) -> bool {
        matches!(*self.state.lock(), TaskState::Exited)
    }

    /// Whether the task is runnalbe
    #[inline]
    pub fn is_runable(&self) -> bool {
        matches!(*self.state.lock(), TaskState::Runable)
    }

    /// Whether the task is blocking
    #[inline]
    pub fn is_blocking(&self) -> bool {
        matches!(*self.state.lock(), TaskState::Blocking)
    }

    /// Whether the task is blocked
    #[inline]
    pub fn is_blocked(&self) -> bool {
        matches!(*self.state.lock(), TaskState::Blocked)
    }

    /// Whether the task is blocked
    #[inline]
    pub(crate) fn init_processor(&self, p: &'static Processor) {
        *self.processor.lock() = Some(p);
    }

    /// Whether the task is blocked
    #[inline]
    pub(crate) fn get_processor(&self) -> &'static Processor {
        self.processor
            .lock()
            .as_ref()
            .expect("task {} processor not init")
    }
}

impl Deref for ScheduleTask {
    type Target = TaskInner;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(not(feature = "async"))]
#[cfg(feature = "monolithic")]
/// Create a new task.
///
/// # Arguments
/// - `entry`: The entry function of the task.
/// - `name`: The name of the task.
/// - `stack_size`: The size of the stack.
/// - `process_id`: The process ID of the task.
/// - `page_table_token`: The page table token of the task.
/// - `sig_child`: Whether the task will send a signal to its parent when it exits.
pub fn new_task<F>(
    entry: F,
    name: String,
    stack_size: usize,
    process_id: u64,
    page_table_token: usize,
) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    use axhal::time::current_time_nanos;

    use crate::schedule::add_wait_for_exit_queue;

    let mut task = taskctx::TaskInner::new(
        entry,
        name,
        stack_size,
        process_id,
        page_table_token,
        #[cfg(feature = "tls")]
        tls_area(),
    );
    #[cfg(feature = "tls")]
    let tls = VirtAddr::from(task.get_tls_ptr());
    #[cfg(not(feature = "tls"))]
    let tls = VirtAddr::from(0);

    // 当 trap 进内核的时候，内核栈会先存储 trap frame，然后再存储 task context
    task.init_task_ctx(
        task_entry as usize,
        (task.get_kernel_stack_top().unwrap() - core::mem::size_of::<TrapFrame>()).into(),
        tls,
    );

    // 设置 CPU 亲和集
    task.set_cpu_set((1 << axconfig::SMP) - 1, 1, axconfig::SMP);

    task.reset_time_stat(current_time_nanos() as usize);

    let axtask = Arc::new(AxTask::new(ScheduleTask::new(task)));
    add_wait_for_exit_queue(&axtask);
    axtask
}

#[cfg(not(feature = "async"))]
#[cfg(not(feature = "monolithic"))]
/// Create a new task.
///
/// # Arguments
/// - `entry`: The entry function of the task.
/// - `name`: The name of the task.
/// - `stack_size`: The size of the kernel stack.
pub fn new_task<F>(entry: F, name: String, stack_size: usize) -> AxTaskRef
where
    F: FnOnce() + Send + 'static,
{
    let mut task = taskctx::TaskInner::new(
        entry,
        name,
        stack_size,
        #[cfg(feature = "tls")]
        tls_area(),
    );
    #[cfg(feature = "tls")]
    let tls = VirtAddr::from(task.get_tls_ptr());
    #[cfg(not(feature = "tls"))]
    let tls = VirtAddr::from(0);

    task.init_task_ctx(
        task_entry as usize,
        task.get_kernel_stack_top().unwrap().into(),
        tls,
    );
    let axtask = Arc::new(AxTask::new(ScheduleTask::new(task)));
    add_wait_for_exit_queue(&axtask);
    axtask
}

pub(crate) fn new_init_task(name: String) -> AxTaskRef {
    let axtask = Arc::new(AxTask::new(ScheduleTask::new(
        taskctx::TaskInner::new_init(
            name,
            #[cfg(feature = "tls")]
            tls_area(),
        ),
    )));

    #[cfg(feature = "monolithic")]
    // 设置 CPU 亲和集
    axtask.set_cpu_set((1 << axconfig::SMP) - 1, 1, axconfig::SMP);

    add_wait_for_exit_queue(&axtask);
    axtask
}

/// A wrapper of [`AxTaskRef`] as the current task.
pub struct CurrentTask(ManuallyDrop<AxTaskRef>);

impl CurrentTask {
    pub(crate) fn try_get() -> Option<Self> {
        let ptr: *const super::AxTask = taskctx::current_task_ptr();
        if !ptr.is_null() {
            Some(Self(unsafe { ManuallyDrop::new(AxTaskRef::from_raw(ptr)) }))
        } else {
            None
        }
    }

    pub(crate) fn get() -> Self {
        Self::try_get().expect("current task is uninitialized")
    }

    /// Converts [`CurrentTask`] to [`AxTaskRef`].
    pub fn as_task_ref(&self) -> &AxTaskRef {
        &self.0
    }

    pub(crate) fn clone(&self) -> AxTaskRef {
        self.0.deref().clone()
    }

    #[allow(unused)]
    pub(crate) fn ptr_eq(&self, other: &AxTaskRef) -> bool {
        Arc::ptr_eq(&self.0, other)
    }

    pub(crate) unsafe fn init_current(init_task: AxTaskRef) {
        #[cfg(feature = "tls")]
        axhal::arch::write_thread_pointer(init_task.get_tls_ptr());
        let ptr = Arc::into_raw(init_task);
        taskctx::set_current_task_ptr(ptr);
    }

    pub(crate) unsafe fn set_current(prev: Self, next: AxTaskRef) {
        let Self(arc) = prev;
        ManuallyDrop::into_inner(arc); // `call Arc::drop()` to decrease prev task reference count.
        let ptr = Arc::into_raw(next);
        taskctx::set_current_task_ptr(ptr);
    }
}

impl Deref for CurrentTask {
    type Target = ScheduleTask;
    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

#[cfg(not(feature = "async"))]
extern "C" fn task_entry() -> ! {
    // SAFETY: INIT when switch_to
    // First into task entry, manually perform the subsequent work of switch_to

    current_processor().switch_post();

    #[cfg(feature = "irq")]
    axhal::arch::enable_irqs();
    let task = crate::current();
    if let Some(entry) = task.get_entry() {
        cfg_if::cfg_if! {
            if #[cfg(feature = "monolithic")] {
                use axhal::KERNEL_PROCESS_ID;
                if task.get_process_id() == KERNEL_PROCESS_ID {
                    // 是初始调度进程，直接执行即可
                    unsafe { Box::from_raw(entry)() };
                    // 继续执行对应的函数
                } else {
                    // 需要通过切换特权级进入到对应的应用程序
                    let kernel_sp = task.get_kernel_stack_top().unwrap();
                    // 切换页表已经在switch实现了
                    // 记得更新时间
                    task.time_stat_from_kernel_to_user(axhal::time::current_time_nanos() as usize);
                    axhal::arch::first_into_user(kernel_sp);
                }
            }
            else {
                unsafe { Box::from_raw(entry)() };
            }
        }
    }
    // only for kernel task
    crate::exit(0);
}

#[cfg(feature = "async")]
/// Create a new task.
/// 
/// Arguments:
/// - `fut`: The future the task.
/// - `name`: The name of the task.
pub fn new_task<F, T>(
    fut: F,
    name: String,
) -> AxTaskRef 
where 
    F: FnOnce() -> T,
    T: Future<Output = i32> + Send + 'static,
{
    let task = taskctx::TaskInner::new(
        fut, 
        name,
        #[cfg(feature = "tls")]
        tls_area(),
    );
    let axtask = Arc::new(AxTask::new(ScheduleTask::new(task)));
    add_wait_for_exit_queue(&axtask);
    axtask
}
