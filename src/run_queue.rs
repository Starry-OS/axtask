use alloc::collections::VecDeque;
use alloc::sync::Arc;

#[cfg(feature = "monolithic")]
use axhal::KERNEL_PROCESS_ID;
use lazy_init::LazyInit;
use scheduler::BaseScheduler;
use spinlock::SpinNoIrq;
use taskctx::TaskState;

use crate::task::{new_init_task, new_task, CurrentTask};
use crate::{AxTaskRef, Scheduler, WaitQueue};

// TODO: per-CPU
/// The running task-queue of the kernel.
static RUN_QUEUE: LazyInit<SpinNoIrq<AxRunQueue>> = LazyInit::new();

// TODO: per-CPU
/// The exited task-queue of the kernel.
pub static EXITED_TASKS: SpinNoIrq<VecDeque<AxTaskRef>> = SpinNoIrq::new(VecDeque::new());

static WAIT_FOR_EXIT: WaitQueue = WaitQueue::new();

#[percpu::def_percpu]
/// The idle task of the kernel.
pub static IDLE_TASK: LazyInit<AxTaskRef> = LazyInit::new();

/// The struct to define the running task-queue of the kernel.
pub struct AxRunQueue {
    scheduler: Scheduler,
}

//
// task state: Ready --- Running --- Blocking ----- Blocked ---- EXITED
// Concurrency note: 
//  - Blocked/Exited: Always triggered by task self
//  - Running: always happend when task is scheduled 
//  - Ready: 
//      Blocked to Ready: There may be concurrency
//      Running to Ready: no concurrency
//
// There are several events that trigger State Change:
//  - task_yield: 
//  - task_exit: 
//  - task_block: 
//  - task_wake: Only this event will occur concurrently with other events
// 
//                          task_yield VS task_wake
// --------------------------------------------------------------------
//  CPU0               CPU1                     CPU2     
// task_yield:        task_wake:               task_wake  
//                    Running do noting
// Running -> Ready                             
//                                            Ready do nothing
// reschedule: 
// rq.lock().add()
// --------------------------------------------------------------------
//                          task_exit VS task_wake
// --------------------------------------------------------------------
//  CPU0               CPU1                     CPU2     
// task_exit:         task_wake:               task_wake  
//                    Running do noting         
// Running -> Exit                             
//                                            Exit  do nothing
// --------------------------------------------------------------------
//                          task_block VS task_wake
// NOTE: 
// The situation here is a bit complicated. When the task is ready to block, 
// a timer or waiting queue will be added externally. 
// If the wakeup is skipped directly, it may cause the blocked task to never wake up
//
// By adding new field to the task: wake_cnt:atomic, 
// when waking up and do_nothing increase wake_cnt;
// and reuse Rq.lock protect process
// --------------------------------------------------------------------
//  CPU0                 CPU1                       CPU2
// wake_cnt=0
// task_block:         task_wake:                 task_wake 
//                     rq.lock()                
//                    Running do noting
//                     wake_cnt++ 
//                     rq.unlock()                
//                                                rq.lock()
//                                               Running do_nothing 
//                                                 wake_cnt++ 
//                                                rq.unlock()
// rq.lock()                
// if wake_cnt > 0  
//  wakecnt = 0
//  return
// rq.unlock()
// ------------------------------------------------------------------
//  CPU0                   CPU1                     CPU2
// wake_cnt=0
// task_block:           task_wake:                task_wake 
// 
// rq.lock()
// Running -> Blocked 
// rq.rescuedule()
// rq.unlock() 
//                      rq.lock()
//                      Blocked->Ready 
//                      rq.add()
//                      rq.unlock()
//                                                  rq.lock()
//                                                  ready do_nothing()
//

pub(crate) unsafe fn force_unlock() {
    RUN_QUEUE.force_unlock()
}

pub(crate) fn add_task(task: AxTaskRef) {
    debug!("task spawn: {}", task.id_name());
    assert!(task.is_ready());
    RUN_QUEUE.lock().scheduler.add_task(task);
}

pub(crate) fn exit_current(exit_code: i32) -> ! {
    let curr = crate::current();
    debug!("task exit: {}, exit_code={}", curr.id_name(), exit_code);
    assert!(curr.is_running());
    assert!(!curr.is_idle());

    curr.set_state(TaskState::Exited);
    // maybe others join on this thread
    // must set state before notify wait_exit
    crate::schedule::notify_wait_for_exit(curr.as_task_ref());
    gc_clear(curr.as_task_ref());

    if curr.is_init() {
        EXITED_TASKS.lock().clear();
        axhal::misc::terminate();
    } else {
        curr.set_exit_code(exit_code);
        RUN_QUEUE.lock().resched(false);
    }
    unreachable!("task exited!");
}

pub(crate) fn yield_current() {
    let curr = crate::current();
    trace!("task yield: {}", curr.id_name());
    assert!(curr.is_running());
    curr.set_state(TaskState::Ready);
    RUN_QUEUE.lock().resched(false);
}

/// schedule to next,and try to set current state is blocked
pub fn block_current() {
    let curr = crate::current();
    assert!(!curr.is_idle());
    assert!(curr.is_running());
    debug!("task block: {}", curr.id_name());
    let mut rq = RUN_QUEUE.lock();
    if curr.wakeup_cnt() > 0 {
        return;
    }
    // we must not block current task with preemption disabled.
    #[cfg(feature = "preempt")]
    assert!(curr.can_preempt(1));
    curr.set_state(TaskState::Blocked);
    rq.resched(false);
}

pub fn wake_task(task: AxTaskRef, resched: bool) {
    debug!("task unblock: {}", task.id_name());
    let mut rq = RUN_QUEUE.lock();
    if task.is_blocked() {
        task.set_state(TaskState::Ready);
        rq.scheduler.add_task(task); 
        if resched {
            #[cfg(feature = "preempt")]
            crate::current().set_preempt_pending(true);
        }
    } else if task.is_running() {
        task.inc_wakeup_cnt();
    }
}

#[cfg(feature = "irq")]
pub fn schedule_timeout(deadline: axhal::time::TimeValue) -> bool {
    let curr = crate::current();
    debug!("task sleep: {}, deadline={:?}", curr.id_name(), deadline);
    assert!(curr.is_running());
    assert!(!curr.is_idle());

    curr.reset_wakeup_cnt();

    crate::timers::set_alarm_wakeup(deadline, curr.clone());
    block_current();
    let timeout = axhal::time::current_time() >= deadline;
    // may wake up by others 
    crate::timers::cancel_alarm(curr.as_task_ref());
    timeout 
}

// A hack api
// thread can exit only by it self
#[cfg(feature = "monolithic")]
/// 仅用于exec与exit时清除其他后台线程
pub fn remove_task(task: &AxTaskRef) {
    debug!("task remove: {}", task.id_name());
    // 当前任务不予清除
    assert!(!task.is_running());
    assert!(!task.is_idle());
    if task.is_ready() {
        task.set_state(TaskState::Exited);
        gc_clear(task);
        RUN_QUEUE.lock().scheduler.remove_task(task);
    }
}
#[cfg(feature = "preempt")]
pub fn preempt_resched() {
    let curr = crate::current();
    assert!(curr.is_running());

    // When we get the mutable reference of the run queue, we must
    // have held the `SpinNoIrq` lock with both IRQs and preemption
    // disabled. So we need to set `current_disable_count` to 1 in
    // `can_preempt()` to obtain the preemption permission before
    //  locking the run queue.
    let can_preempt = curr.can_preempt(1);

    debug!(
        "current task is to be preempted: {}, allow={}",
        curr.id_name(),
        can_preempt
    );

    if can_preempt {
        curr.set_state(TaskState::Ready);
        RUN_QUEUE.lock().resched(true);
    } else {
        curr.set_preempt_pending(true);
    }
}

#[cfg(feature = "irq")]
pub fn scheduler_timer_tick() {
    let curr = crate::current();
    if !curr.is_idle() && RUN_QUEUE.lock().scheduler.task_tick(curr.as_task_ref()) {
        #[cfg(feature = "preempt")]
        curr.set_preempt_pending(true);
    }
}

pub fn set_current_priority(prio: isize) -> bool {
    RUN_QUEUE.lock().scheduler
        .set_priority(crate::current().as_task_ref(), prio)
}

impl AxRunQueue {
    pub fn new() -> SpinNoIrq<Self> {
        let gc_task = new_task(
            gc_entry,
            "gc".into(),
            axconfig::TASK_STACK_SIZE,
            #[cfg(feature = "monolithic")]
            KERNEL_PROCESS_ID,
            #[cfg(feature = "monolithic")]
            0,
            #[cfg(feature = "monolithic")]
            false,
        );
        let mut scheduler = Scheduler::new();
        scheduler.add_task(gc_task);
        SpinNoIrq::new(Self { scheduler })
    }
}

impl AxRunQueue {
    /// Common reschedule subroutine. 
    /// If `preempt`, keep current task's time slice, otherwise reset it.
    fn resched(&mut self, preempt: bool) {
        let prev = crate::current();
        if prev.is_ready() {
            if !prev.is_idle() {
                self.scheduler.put_prev_task(prev.clone(), preempt);
            }
        }
        #[cfg(feature = "monolithic")]
        {
            use alloc::collections::BTreeSet;
            use axhal::cpu::this_cpu_id;
            let mut task_set = BTreeSet::new();
            let next = loop {
                let task = self.scheduler.pick_next_task();
                if task.is_none() {
                    break unsafe {
                        // Safety: IRQs must be disabled at this time.
                        IDLE_TASK.current_ref_raw().get_unchecked().clone()
                    };
                }
                let task = task.unwrap();
                // 原先队列有任务，但是全部不满足CPU适配集，则还是返回IDLE
                if task_set.contains(&task.id().as_u64()) {
                    break unsafe {
                        // Safety: IRQs must be disabled at this time.
                        IDLE_TASK.current_ref_raw().get_unchecked().clone()
                    };
                }
                let mask = task.get_cpu_set();
                let curr_cpu = this_cpu_id();
                // 如果当前进程没有被 vfork 阻塞，弹出任务
                if mask & (1 << curr_cpu) != 0 {
                    break task;
                }
                task_set.insert(task.id().as_u64());
                self.scheduler.put_prev_task(task, false);
            };
            self.switch_to(prev, next);
        }
        #[cfg(not(feature = "monolithic"))]
        {
            let next = self.scheduler.pick_next_task().unwrap_or_else(|| unsafe {
                // Safety: IRQs must be disabled at this time.
                IDLE_TASK.current_ref_raw().get_unchecked().clone()
            });
            self.switch_to(prev, next);
        }
    }

    fn switch_to(&mut self, prev_task: CurrentTask, next_task: AxTaskRef) {
        trace!(
            "context switch: {} -> {}",
            prev_task.id_name(),
            next_task.id_name()
        );
        #[cfg(feature = "preempt")]
        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        if prev_task.ptr_eq(&next_task) {
            return;
        }
        // 当任务进行切换时，更新两个任务的时间统计信息
        #[cfg(feature = "monolithic")]
        {
            let current_timestamp = axhal::time::current_time_nanos() as usize;
            next_task.time_stat_when_switch_to(current_timestamp);
            prev_task.time_stat_when_switch_from(current_timestamp);
        }
        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();

            // The strong reference count of `prev_task` will be decremented by 1,
            // but won't be dropped until `gc_entry()` is called.
            assert!(Arc::strong_count(prev_task.as_task_ref()) > 1, 
                "task id {} strong count {}", prev_task.id().as_u64(),Arc::strong_count(prev_task.as_task_ref()));

            assert!(Arc::strong_count(&next_task) >= 1);
            #[cfg(feature = "monolithic")]
            {
                let page_table_token = *next_task.page_table_token.get();
                if page_table_token != 0 {
                    axhal::arch::write_page_table_root0(page_table_token.into());
                }
            }

            CurrentTask::set_current(prev_task, next_task);
            axhal::arch::task_context_switch(&mut (*prev_ctx_ptr), &(*next_ctx_ptr))
        }
    }
}

pub(crate) fn gc_clear(task: &AxTaskRef) {
    EXITED_TASKS.lock().push_back(task.clone());
    error!("task {} enter gc_clear, strong count {}", task.id().as_u64(), Arc::strong_count(task));
    WAIT_FOR_EXIT.notify_one(false);
}

fn gc_entry() {
    loop {
        // Drop all exited tasks and recycle resources.
        let n = EXITED_TASKS.lock().len();
        for _ in 0..n {
            // Do not do the slow drops in the critical section.
            let task = EXITED_TASKS.lock().pop_front();
            if let Some(task) = task {
                error!("enter gc ,get task id  {} ,  {} ", task.id().as_u64(),
                    Arc::strong_count(&task));
                if Arc::strong_count(&task) == 1 {
                    // If I'm the last holder of the task, drop it immediately.
                    error!("task {} is dropped",task.id().as_u64());
                    drop(task);
                } else {
                    // Otherwise (e.g, `switch_to` is not compeleted, held by the
                    // joiner, etc), push it back and wait for them to drop first.
                    EXITED_TASKS.lock().push_back(task);
                }
            }
        }
        // gc wait other task exit
        WAIT_FOR_EXIT.wait();
    }
}

pub(crate) fn init() {
    const IDLE_TASK_STACK_SIZE: usize = 4096;
    let idle_task = new_task(
        || crate::run_idle(),
        "idle".into(), // FIXME: name 现已被用作 prctl 使用的程序名，应另选方式判断 idle 进程
        IDLE_TASK_STACK_SIZE,
        #[cfg(feature = "monolithic")]
        KERNEL_PROCESS_ID,
        #[cfg(feature = "monolithic")]
        0,
        #[cfg(feature = "monolithic")]
        false,
    );
    IDLE_TASK.with_current(|i| i.init_by(idle_task.clone()));

    let main_task = new_init_task("main".into());
    #[cfg(feature = "monolithic")]
    main_task.set_process_id(KERNEL_PROCESS_ID);
    main_task.set_state(TaskState::Running);

    RUN_QUEUE.init_by(AxRunQueue::new());
    unsafe { CurrentTask::init_current(main_task) }
}

pub(crate) fn init_secondary() {
    let idle_task = new_init_task("idle".into()); // FIXME: name 现已被用作 prctl 使用的程序名，应另选方式判断 idle 进程
    #[cfg(feature = "monolithic")]
    idle_task.set_process_id(KERNEL_PROCESS_ID);
    idle_task.set_state(TaskState::Running);
    IDLE_TASK.with_current(|i| i.init_by(idle_task.clone()));
    unsafe { CurrentTask::init_current(idle_task) }
}
