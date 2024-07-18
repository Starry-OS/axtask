use alloc::sync::Arc;
pub use taskctx::TaskStack;
use alloc::vec::Vec;

/// A simple stack pool
pub struct StackPool {
    curr_stack: Option<Arc<TaskStack>>,
    free_stacks: Vec<Arc<TaskStack>>,
}

impl StackPool {
    /// Creates a new empty stack pool.
    pub const fn new() -> Self {
        Self {
            curr_stack: None,
            free_stacks: Vec::new(),
        }
    }

    /// Fetch a free stack from the pool.
    pub fn fetch(&mut self) -> Arc<TaskStack> {
        self.free_stacks.pop().unwrap_or_else(|| Arc::new(TaskStack::alloc(axconfig::TASK_STACK_SIZE)))
    }

    /// Set current stack.
    pub fn set_curr_stack(&mut self, stack: Option<Arc<TaskStack>>) {
        if let Some(old_stack) = self.curr_stack.take() {
            self.free_stacks.push(old_stack);
        }
        if let Some(stack) = stack {
            self.curr_stack.replace(stack);
        }
    }
}
