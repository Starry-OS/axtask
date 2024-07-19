use alloc::sync::Arc;
use spinlock::SpinNoIrqOnlyGuard;
use core::{mem::ManuallyDrop, ops::Deref, task::Poll};
use crate::{current_processor, processor::PrevCtxSave, stack_pool::TaskStack, AxTaskRef, CurrentTask, TaskState};
use taskctx::{save_prev_ctx, load_next_ctx};

#[cfg(feature = "preempt")]
/// This is only used when the preempt feature is enabled.
pub fn preempt_switch_entry() {
    let prev_task = crate::current();
    let prev_task_ctx_ref = prev_task.get_ctx_ref();
    unsafe { save_prev_ctx(&mut *prev_task_ctx_ref) };
    unsafe { *prev_task_ctx_ref = core::ptr::NonNull::dangling() };
}

/// This function is the entrance of activie switching.
pub fn switch_entry() {
    // The current task may have not run yet. So 
    let prev_task = crate::current();
    if prev_task.is_thread() {
        let prev_task_ctx_ref = prev_task.get_ctx_ref();
        unsafe { save_prev_ctx(&mut *prev_task_ctx_ref) };
    } else {
        schedule_without_sp_change();
    }
}

#[no_mangle]
/// Pick next task from the scheduler and run it.
fn schedule_with_sp_change() {
    // Dangerous: it will change stack in the rust function, which can cause undefined behavior.
    unsafe {
        let curr_free_stack_top = CurrentFreeStack::get().top().as_usize();
        log::trace!("exchange next_stack {:#X?}", curr_free_stack_top);
        core::arch::asm!("mv sp, {0}", in(reg) curr_free_stack_top);
    }
    let prev_stack = CurrentFreeStack::get();
    let next_free_stack = current_processor().pick_stack();
    unsafe { 
        let prev_free_stack = CurrentFreeStack::set_current_free(prev_stack, next_free_stack); 
        current_processor().set_curr_stack(Some(prev_free_stack));
    }
    loop {
        schedule_without_sp_change();
    }
}

/// Pick next task from the scheduler and run it.
/// The prev task is a coroutine and the current stack will be reused.
fn schedule_without_sp_change() {
    let next_task = current_processor().pick_next_task();
    exchange_current(next_task);
}

/// Change the current status
/// 
/// Include the Processor and current task
pub fn exchange_current(mut next_task: AxTaskRef) {
    let prev_task = crate::current();
    // task in a disable_preempt context? it not allowed ctx switch
    #[cfg(feature = "preempt")]
    assert!(
        prev_task.can_preempt(),
        "task can_preempt failed {}",
        prev_task.id_name()
    );
    // Here must lock curr state, and no one can change curr state
    // when excuting ctx_switch
    let mut prev_state_lock = prev_task.state_lock_manual();
    loop {
        match **prev_state_lock {
            TaskState::Runable => {
                if next_task.is_idle() {
                    next_task = prev_task.clone();
                    break;
                }
                if !prev_task.is_idle() {
                    #[cfg(feature = "preempt")]
                    current_processor()
                        .put_prev_task(prev_task.clone(), prev_task.get_preempt_pending());
                    #[cfg(not(feature = "preempt"))]
                    current_processor().put_prev_task(prev_task.clone(), false);
                }
                break;
            }
            TaskState::Blocking => {
                debug!("task block: {}", prev_task.id_name());
                **prev_state_lock = TaskState::Blocked;
                break;
            }
            TaskState::Exited => {
                break;
            }
            _ => {
                panic!("unexpect state when switch_to happend ");
            }
        }
    }
    #[cfg(feature = "preempt")]
    //reset preempt pending
    next_task.set_preempt_pending(false);
    // // This will cause bug, because the current execution doesn't belong to any task.
    // // If it return directly, the current exection will be lost.
    // if prev_task.ptr_eq(&next_task) {
    //     log::info!("prev {} is equal to {}", prev_task.id_name(), next_task.id_name());
    //     ManuallyDrop::into_inner(prev_state_lock);
    //     return;
    // }
    // 当任务进行切换时，更新两个任务的时间统计信息
    #[cfg(feature = "monolithic")]
    {
        let current_timestamp = axhal::time::current_time_nanos() as usize;
        next_task.time_stat_when_switch_to(current_timestamp);
        prev_task.time_stat_when_switch_from(current_timestamp);
    }
    trace!("context switch: {} -> {}", prev_task.id_name(), next_task.id_name());

    unsafe {
        // The strong reference count of `prev_task` will be decremented by 1,
        // but won't be dropped until `gc_entry()` is called.
        assert!(
            Arc::strong_count(prev_task.as_task_ref()) > 1,
            "task {} id {} strong count {}", prev_task.id_name(),
            prev_task.id().as_u64(),
            Arc::strong_count(prev_task.as_task_ref())
        );
        assert!(Arc::strong_count(&next_task) >= 1);
        #[cfg(feature = "monolithic")]
        {
            let page_table_token = *next_task.page_table_token.get();
            if page_table_token != 0 {
                axhal::arch::write_page_table_root0(page_table_token.into());
            }
        }
        let prev_ctx = PrevCtxSave::new(core::mem::transmute::<
            ManuallyDrop<SpinNoIrqOnlyGuard<'_, TaskState>>,
            ManuallyDrop<SpinNoIrqOnlyGuard<'static, TaskState>>,
        >(prev_state_lock));
        current_processor().set_prev_ctx_save(prev_ctx);
        CurrentTask::set_current(prev_task, next_task);
    }
    run_next();
}

/// Run next task
pub fn run_next() {
    // SAFETY: INIT when switch_to
    // First into task entry, manually perform the subsequent work of switch_to

    current_processor().switch_post();

    let task = crate::current();
    if task.is_thread() {
        let task_ctx_ref = task.get_ctx_ref();
        // Dangerous: the current stack will be recycled. 
        // But it is used until executing the `load_next_ctx` function.
        // The current_free_stack don't need to be updated.
        unsafe {
            log::trace!("{} load context from stack, curr_free_sp {:#X?}", task.id_name(), CurrentFreeStack::get().top().as_usize());
            current_processor().set_curr_stack(None);
            load_next_ctx(&mut *task_ctx_ref);
        }
    } else {
        let waker = crate::waker_from_task(task.as_task_ref().clone());
        let mut cx = core::task::Context::from_waker(&waker);
        let future = unsafe { &mut *task.get_future() };
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(_ret) => {
                trace!("task exit: {}, exit_code={}", task.id_name(), _ret);
                crate::schedule::notify_wait_for_exit(task.as_task_ref());
                task.set_state(TaskState::Exited);
                crate::current_processor().kick_exited_task(task.as_task_ref());
                if task.name() == "main_coroutine" {
                    crate::Processor::clean_all();
                    axhal::misc::terminate();
                }
            }
            Poll::Pending => {
                log::trace!("task is pending");
            }
        }
    }
}

#[percpu::def_percpu]
/// it is used when a task is interrupted.
static CURRENT_FREE_STACK: usize = 0;

/// A wrapper of [`Arc<TaskStack>`] as the current free stack.
pub struct CurrentFreeStack(ManuallyDrop<Arc<TaskStack>>);

impl CurrentFreeStack {
    pub(crate) fn try_get() -> Option<Self> {
        let ptr: *const TaskStack = CURRENT_FREE_STACK.read_current() as _;
        if !ptr.is_null() {
            Some(Self(unsafe { ManuallyDrop::new(Arc::from_raw(ptr)) }))
        } else {
            None
        }
    }

    pub(crate) fn get() -> Self {
        Self::try_get().expect("current free stack is uninitialized")
    }

    #[allow(unused)]
    /// Converts [`CurrentFreeStack`] to [`AxTaskRef`].
    pub fn as_stack_ref(&self) -> &Arc<TaskStack> {
        &self.0
    }

    pub(crate) unsafe fn init_current_free(free_stack: Arc<TaskStack>) {
        let ptr = Arc::into_raw(free_stack);
        CURRENT_FREE_STACK.write_current(ptr as _);
    }

    pub(crate) unsafe fn set_current_free(prev: Self, next: Arc<TaskStack>) -> Arc<TaskStack> {
        let Self(arc) = prev;
        let prev_stack = ManuallyDrop::into_inner(arc);
        // Not automic drop the stack node.
        let ptr = Arc::into_raw(next);
        CURRENT_FREE_STACK.write_current(ptr as _);
        prev_stack
    }
}

impl Deref for CurrentFreeStack {
    type Target = Arc<TaskStack>;
    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}