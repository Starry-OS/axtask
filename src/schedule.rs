use alloc::{
    collections::{BTreeMap},
    sync::Arc,
};
use spinlock::SpinNoIrq;

use crate::{AxTaskRef, WaitQueue};

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
        wait_queue.notify_all(true);
    }
}
