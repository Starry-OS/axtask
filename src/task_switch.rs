use core::task::Poll;
use crate::{current_processor, TaskState};

#[cfg(feature = "preempt")]
#[no_mangle]
#[inline(never)]
/// This is only used when the preempt feature is enabled.
/// 
pub fn preempt_switch_entry(taskctx: &mut taskctx::TaskContext) {
    log::trace!("preempt_switch_entry, ctx_type {}", taskctx.ctx_type);
    let prev_task = crate::current();
    prev_task.set_ctx_ref(taskctx as _);
    let new_ctx = prepare_new_ctx();
    unsafe { taskctx::switch_to_ctx(new_ctx); }
}

#[no_mangle]
#[inline(never)]
/// This function is the entry of activitily switching.
pub fn yield_switch_entry() {
    let prev_task = crate::current();
    #[cfg(feature = "preempt")]
    assert!(
        prev_task.can_preempt(),
        "task can_preempt failed {}",
        prev_task.id_name()
    );
    #[cfg(feature = "preempt")]
    prev_task.set_preempt_pending(false);
    let prev_task_ctx_ref = prev_task.get_ctx_ref();
    // log::warn!("prev_task yield {} ctx_ref {:#X}", prev_task.id_name(), prev_task.get_ctx() as usize);
    let new_ctx = prepare_new_ctx();
    taskctx::yield_to_new_ctx(unsafe { &mut *prev_task_ctx_ref }, new_ctx);
    current_processor().set_curr_stack(None);
}

pub fn prepare_new_ctx() -> &'static mut taskctx::TaskContext {
    let free_stack = current_processor().pick_stack();
    let free_stack_top = free_stack.top().as_usize();
    current_processor().set_curr_stack(Some(free_stack));
    log::trace!("prepare_new_ctx, free_stack_top {:#X}", free_stack_top);
    unsafe {
        let new_ctx_ptr = (free_stack_top - core::mem::size_of::<taskctx::TaskContext>()) as *mut taskctx::TaskContext;
        let new_ctx = &mut *new_ctx_ptr;
        new_ctx.init(switch_entry as usize, (new_ctx_ptr as usize).into(), 0.into());
        new_ctx
    }
}

/// This function is the entrance of activie switching.
pub fn switch_entry() {
    loop {
        crate::schedule();
    }
}

/// Run next task
pub fn run_next() {
    current_processor().switch_post();

    let task = crate::current();
    if task.is_thread() {
        let task_ctx_ref = task.get_ctx_ref();
        unsafe { taskctx::restore_ctx(&mut *task_ctx_ref); }
    } else {
        let waker = crate::waker_from_task(task.as_task_ref().clone());
        let mut cx = core::task::Context::from_waker(&waker);
        let future = unsafe { &mut *task.get_future() };
        log::trace!("run coroutine: {}", task.id_name());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(_ret) => {
                trace!("task exit: {}, exit_code={}", task.id_name(), _ret);
                task.set_state(TaskState::Exited);
                crate::schedule::notify_wait_for_exit(task.as_task_ref());
                crate::current_processor().kick_exited_task(task.as_task_ref());
                if task.name() == "main_coroutine" {
                    crate::Processor::clean_all();
                    axhal::misc::terminate();
                }
            }
            Poll::Pending => {
                trace!("task is pending");
            }
        }
    }
}
