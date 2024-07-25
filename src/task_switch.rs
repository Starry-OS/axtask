use core::task::Poll;
use crate::{current_processor, TaskState};
use taskctx::load_next_ctx;

#[cfg(feature = "preempt")]
/// This is only used when the preempt feature is enabled.
pub fn preempt_switch_entry(taskctx: &mut taskctx::TaskContext) {
    let prev_task = crate::current();
    prev_task.set_ctx_ref(taskctx as _);
    schedule_with_sp_change();
}

/// This function is the entry of activitily switching.
pub fn yield_switch_entry() {
    let prev_task = crate::current();
    let prev_task_ctx_ref = prev_task.get_ctx_ref();
    unsafe { taskctx::save_prev_ctx(&mut *prev_task_ctx_ref); }
}

/// This function is the entrance of activie switching.
pub fn switch_entry() {
    crate::schedule();
}

#[no_mangle]
/// Pick next task from the scheduler and run it.
fn schedule_with_sp_change() {
    // Dangerous: it will change stack in the rust function, which can cause undefined behavior.
    unsafe {
        let free_stack = current_processor().pick_stack();
        let free_stack_top = free_stack.top().as_usize();
        current_processor().set_curr_stack(Some(free_stack.clone()));
        log::trace!("exchange next_stack {:#X?}", free_stack_top);
        core::arch::asm!("mv sp, {0}", in(reg) free_stack_top);
    }
    loop {
        crate::schedule::schedule();
    }
}

/// Run next task
pub fn run_next() {
    current_processor().switch_post();

    let task = crate::current();
    if task.is_thread() {
        log::debug!("run thread: {}", task.id_name());
        let task_ctx_ref = task.get_ctx_ref();
        // let task_ctx = task.get_ctx();
        // log::warn!("run_next: task_ctx={:#X?}, ctx_type {}", task_ctx as usize, unsafe { (&*task_ctx).ctx_type });
        // Dangerous: the current stack will be recycled. 
        // But it is used until executing the `load_next_ctx` function.
        unsafe {
            current_processor().set_curr_stack(None);
            load_next_ctx(&mut *task_ctx_ref);
        }
    } else {
        let waker = crate::waker_from_task(task.as_task_ref().clone());
        let mut cx = core::task::Context::from_waker(&waker);
        let future = unsafe { &mut *task.get_future() };
        log::debug!("run coroutine: {}", task.id_name());
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
