use alloc::collections::VecDeque;
 use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_init::LazyInit;
use scheduler::BaseScheduler;
use spinlock::{SpinNoIrq, SpinNoIrqOnly,SpinRaw};
use kernel_guard::BaseGuard;
use kernel_guard::NoOp;
use axhal::cpu::this_cpu_id;

#[cfg(feature = "monolithic")]
use axhal::KERNEL_PROCESS_ID;

use crate::task::{new_init_task, new_task, CurrentTask, TaskState};
use crate::{AxTaskRef, Scheduler, WaitQueue, CpuMask};

const ARRAY_REPEAT_VALUE: MaybeUninit<&'static mut Processor> = MaybeUninit::uninit();

static mut PROCESSORS: [MaybeUninit<&'static mut Processor>; axconfig::SMP] = [ARRAY_REPEAT_VALUE; axconfig::SMP];

#[percpu::def_percpu]
static PROCESSOR: LazyInit<Processor> = LazyInit::new();

/// A map to store tasks' wait queues, which stores tasks that are waiting for this task to exit.
pub(crate) static WAIT_FOR_TASK_EXITS: SpinNoIrq<BTreeMap<u64, Arc<WaitQueue>>> =
    SpinNoIrq::new(BTreeMap::new());

pub(crate) fn add_wait_for_exit_queue(task: &AxTaskRef) {
    WAIT_FOR_TASK_EXITS
        .lock()
        .insert(task.id().as_u64(), Arc::new(WaitQueue::new()));
}

pub(crate) fn get_wait_for_exit_queue(task: &AxTaskRef) -> Option<Arc<WaitQueue>> {
    WAIT_FOR_TASK_EXITS.lock().get(&task.id().as_u64()).cloned()
}


/// When the task exits, notify all tasks that are waiting for this task to exit, and
/// then remove the wait queue of the exited task.
pub(crate) fn notify_wait_for_exit(task: &AxTaskRef) {
    if let Some(wait_queue) = WAIT_FOR_TASK_EXITS.lock().remove(&task.id().as_u64()) {
        wait_queue.notify_all();
    }
}
pub struct Processor {
    /// Processor SCHEDULER
    /// spinraw: migrate may be happend on different cpu
    /// cpu1 add task to cpu2, so percpu and  no preempt are not enough 
    /// to protect scheduler
    scheduler: SpinRaw<Scheduler>,
    /// The exited task-queue of the current processor
    exited_tasks: SpinRaw<VecDeque<AxTaskRef>>,
    /// Pre save ctx when processor switch ctx
    prev_ctx_save: PrevCtxSave,
    /// GC wait or notify use
    gc_wait: WaitQueue,
    /// The idle task of the processor
    idle_task: AxTaskRef,
    /// The gc task of the processor
    gc_task: AxTaskRef,
}

unsafe impl Sync for Processor {}
unsafe impl Send for Processor {}

impl Processor {
    pub fn new(idle_task: AxTaskRef) -> Self {
        let gc_task = new_task(
            gc_entry,
            "gc".into(),
            axconfig::TASK_STACK_SIZE,
            #[cfg(feature = "monolithic")]
            KERNEL_PROCESS_ID,
            #[cfg(feature = "monolithic")]
            0,
        );

        Processor {
            scheduler: SpinRaw::new(Scheduler::new()),
            idle_task,
            prev_ctx_save: PrevCtxSave::new_empty(),
            exited_tasks: SpinRaw::new(VecDeque::new()),
            gc_wait: WaitQueue::new(),
            gc_task: gc_task,
        }
    }

    pub fn idle_task(&self) -> &AxTaskRef {
        &self.idle_task
    }

    #[inline]
    /// Pick one task from processor
    pub(crate) fn pick_next_task(&mut self) -> AxTaskRef {
        self.scheduler.lock().pick_next_task()
            .unwrap_or_else(|| self.idle_task.clone())
    }

    #[inline]
    /// Add curr task to Processor, it ususally add to back
    pub(crate) fn put_prev_task(&mut self, task: AxTaskRef, front: bool) {
        self.scheduler.lock().put_prev_task(task, front);
    }


    #[inline]
    /// Processor Clean
    pub(crate) fn task_tick(&mut self, task: &AxTaskRef) -> bool {
        self.scheduler.lock().task_tick(task)
    }

    #[inline]
    /// Processor Clean
    pub(crate) fn set_priority(&mut self, task: &AxTaskRef, prio: isize) -> bool {
        self.scheduler.lock().set_priority(task, prio)
    }

    #[inline]
    /// update prev_ctx_save when ctx_switch
    pub(crate) fn set_prev_ctx_save(&mut self, prev_save: PrevCtxSave) {
        self.prev_ctx_save = prev_save;
    }


    #[inline]
    /// Processor Clean
    fn clean(&mut self) {
        self.exited_tasks.lock().clear()
    }

    #[inline]
    /// Processor Clean all
    pub fn clean_all() {
    }

}

fn get_processor(index: usize) -> &'static mut Processor{
    unsafe { PROCESSORS[index].assume_init_mut() }
}

fn select_processor_index(cpumask: CpuMask) -> usize {
    static PROCESSOR_INDEX: AtomicUsize = AtomicUsize::new(0);
    assert!(!cpumask.is_empty(), "No available CPU for task execution");

    // Round-robin selection of the processor index.
    loop {
        let index = PROCESSOR_INDEX.fetch_add(1, Ordering::SeqCst) % axconfig::SMP;
        if cpumask.get(index) {
            return index;
        }
    }
}

pub(crate) fn select_processor<G: BaseGuard>(task: &AxTaskRef) -> AxProcessorRef<'static, G> {
    let irq_state = G::acquire();
    let index = select_processor_index(task.cpumask());
    AxProcessorRef {
            inner: get_processor(index),
            state: irq_state,
            _phantom: core::marker::PhantomData,
    }
}

/// `AxProcessorRef`
pub struct AxProcessorRef<'a, G: BaseGuard> {
    inner: &'a mut Processor,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<'a, G: BaseGuard> Drop for AxProcessorRef<'a, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// The interfaces here are all called through current_processor
/// to ensure that CPU preemption does not occur during the call
impl<'a, G: BaseGuard> AxProcessorRef<'a, G> {
    pub(crate) fn clean_task_wait(&mut self) {
        loop {
            // Drop all exited tasks and recycle resources.
            let mut exit_tasks = self.inner.exited_tasks.lock();
            let n = exit_tasks.len();
            for _ in 0..n {
                // Do not do the slow drops in the critical section.
                let task = exit_tasks.pop_front();
                if let Some(task) = task {
                    if Arc::strong_count(&task) == 1 {
                        // If I'm the last holder of the task, drop it immediately.
                        debug!("clean task :{} ", task.id().as_u64());
                        drop(task);
                    } else {
                        // Otherwise (e.g, `switch_to` is not compeleted, held by the
                        // joiner, etc), push it back and wait for them to drop first.
                        exit_tasks.push_back(task);
                    }
                }
            }
            drop(exit_tasks);
            // gc wait other task exit
            self.inner.gc_wait.wait();
        }
    }

    fn kick_exited_task(&mut self, task: &AxTaskRef) {
        self.inner.exited_tasks.lock().push_back(task.clone());
        self.inner.gc_wait.notify_one();
    }

    #[inline]
    /// gc init
    fn gc_init(&mut self) {
        self.add_task(self.inner.gc_task.clone());
    }

    #[inline]
    /// post process prev_ctx_save
    pub(crate) fn switch_post(&mut self) {
        if let Some(prev_task) = self.inner.prev_ctx_save.take_prev_task() {
            prev_task.set_on_cpu(false);
        } else {
            panic!("no prev task");
        }
    }

    /// Add task to processor
    pub(crate) fn add_task(&mut self, task: AxTaskRef) {
        self.inner.scheduler.lock().add_task(task);
    }

    #[cfg(feature = "irq")]
    pub fn scheduler_timer_tick(&mut self) {
        let curr = crate::current();
        if !curr.is_idle() && self.inner.task_tick(curr.as_task_ref()) {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
        }
    }

    pub fn set_current_priority(&mut self, prio: isize) -> bool {
        self.inner.set_priority(crate::current().as_task_ref(), prio)
    }

    pub fn wakeup_task(&mut self, task: AxTaskRef) {
        if task.transition_state(TaskState::Blocked, TaskState::Runable) {
            debug!("task unblock: {}", task.id_name());
            while task.on_cpu() {
                // Wait for the task to finish its scheduling process.
                core::hint::spin_loop();
            }
            self.add_task(task.clone());
        } else {
            debug!("try to wakeup {:?} unexpect state {:?}",
            task.id(), task.state());
        }
    }


    #[cfg(feature = "irq")]
    pub fn schedule_timeout(&mut self, deadline: axhal::time::TimeValue) -> bool {
        let curr = crate::current();
        debug!("task sleep: {}, deadline={:?}", curr.id_name(), deadline);
        assert!(!curr.is_idle());
        crate::timers::set_alarm_wakeup(deadline, curr.clone());
        self.reschedule();
        let timeout = axhal::time::current_time() >= deadline;
        // may wake up by others
        crate::timers::cancel_alarm(curr.as_task_ref());
        timeout
    }

    pub(crate) fn yield_current(&mut self) {
        let curr = crate::current();
        assert!(curr.is_runable());
        trace!("task yield: {}", curr.id_name());
        self.reschedule();
    }

    pub(crate) fn exit_current(&mut self, exit_code: i32) -> ! {
        let curr = crate::current();
        debug!("task exit: {}, exit_code={}", curr.id_name(), exit_code);
        curr.set_state(TaskState::Exited);
        // maybe others join on this thread
        // must set state before notify wait_exit
        notify_wait_for_exit(curr.as_task_ref());

        self.kick_exited_task(curr.as_task_ref());
        if curr.is_init() {
            Processor::clean_all();
            axhal::misc::terminate();
        } else {
            curr.set_exit_code(exit_code);
            self.reschedule();
        }
        unreachable!("task exited!");
    }

    pub fn reschedule(&mut self) {
        let curr = crate::current();
        if curr.is_runable() {
            if !curr.is_idle() {
                #[cfg(feature = "preempt")]
                self.inner.put_prev_task(curr.clone(), curr.get_preempt_pending());
                #[cfg(not(feature = "preempt"))]
                self.inner.put_prev_task(curr.clone(), false);
            }
        }

        let next_task = self.inner.pick_next_task();
        self.switch_to(curr, next_task);
    }


    fn switch_to(&mut self, prev_task: CurrentTask, next_task: AxTaskRef) {
        // task in a disable_preempt context? it not allowed ctx switch
        #[cfg(feature = "preempt")]
        assert!(
            prev_task.can_preempt(),
            "task can_preempt failed {}",
            prev_task.id_name()
        );

    
        #[cfg(feature = "preempt")]
        //reset preempt pending
        next_task.set_preempt_pending(false);
    
        if prev_task.ptr_eq(&next_task) {
            return;
        }
    
        if prev_task.is_blocked() {
            debug!("task block: {}", prev_task.id_name());
        }
        // 当任务进行切换时，更新两个任务的时间统计信息
        #[cfg(feature = "monolithic")]
        {
            let current_timestamp = axhal::time::current_time_nanos() as usize;
            next_task.time_stat_when_switch_to(current_timestamp);
            prev_task.time_stat_when_switch_from(current_timestamp);
        }
    
        // Claim the task as running, we do this before switching to it
        // such that any running task will have this set.
        #[cfg(feature = "smp")]
        next_task.set_on_cpu(true);
    
        trace!(
            "context switch: {} -> {}",
            prev_task.id_name(),
            next_task.id_name(),
        );
        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();
    
            // The strong reference count of `prev_task` will be decremented by 1,
            // but won't be dropped until `gc_entry()` is called.
            assert!(
                Arc::strong_count(prev_task.as_task_ref()) > 1,
                "task id {} strong count {}",
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
    
            self.inner.set_prev_ctx_save(
                PrevCtxSave::new(prev_task.clone())
            );
    
            CurrentTask::set_current(prev_task, next_task);
    
            axhal::arch::task_context_switch(&mut (*prev_ctx_ptr), &(*next_ctx_ptr));
            self.switch_post();
    
        }
    }
}

/// current processor protect by a irq Guard,
/// protect when it is used,no cpu switch happend
pub fn current_processor<G: BaseGuard>() -> AxProcessorRef<'static, G> {
    let irq_state = G::acquire();
    AxProcessorRef {
        inner: unsafe { PROCESSOR.current_ref_mut_raw() },
        state: irq_state,
        _phantom: core::marker::PhantomData,
    }
}

pub(crate) struct PrevCtxSave(Option<AxTaskRef>);

impl PrevCtxSave {
    pub(crate) fn new(prev_task: AxTaskRef) -> PrevCtxSave {
        Self(Some(prev_task))
    }

    const fn new_empty() -> PrevCtxSave {
        Self(None)
    }

    pub(crate) fn take_prev_task(&mut self) -> Option<AxTaskRef> {
        self.0.take()
    }
}

fn gc_entry() {
    current_processor::<NoOp>().clean_task_wait();
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
    );

    let main_task = new_init_task("main".into());
    #[cfg(feature = "monolithic")]
    main_task.set_process_id(KERNEL_PROCESS_ID);

    let processor = Processor::new(idle_task.clone());
    PROCESSOR.with_current(|i| i.init_by(processor));
    current_processor::<NoOp>().gc_init();

    unsafe {
        PROCESSORS[this_cpu_id()].write(PROCESSOR.current_ref_mut_raw());
    }

    unsafe { CurrentTask::init_current(main_task) }
}

pub(crate) fn init_secondary() {
    // FIXME: name 现已被用作 prctl 使用的程序名，应另选方式判断 idle 进程
    let idle_task = new_init_task("idle".into());
    #[cfg(feature = "monolithic")]
    idle_task.set_process_id(KERNEL_PROCESS_ID);

    let processor = Processor::new(idle_task.clone());
    PROCESSOR.with_current(|i| i.init_by(processor));
    current_processor::<NoOp>().gc_init();
    unsafe {
        PROCESSORS[this_cpu_id()].write(PROCESSOR.current_ref_mut_raw());
    }
    unsafe { CurrentTask::init_current(idle_task) };
}
