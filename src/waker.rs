//! This mod specific the waker related with coroutine
//!

use crate::{AxTaskRef, wakeup_task, AxTask};
use alloc::sync::Arc;

use core::task::{RawWaker, RawWakerVTable, Waker};

const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);

unsafe fn clone(p: *const ()) -> RawWaker {
    RawWaker::new(p, &VTABLE)
}

/// nop
unsafe fn wake(p: *const ()) { 
    wakeup_task(Arc::from_raw(p as *const AxTask))
}

unsafe fn drop(p: *const ()) {
    // nop
    Arc::from_raw(p as *const AxTask);
}

/// 
pub(crate) fn waker_from_task(task_ref: AxTaskRef) -> Waker {
    unsafe {
        Waker::from_raw(RawWaker::new(Arc::into_raw(task_ref) as _, &VTABLE))
    }
}