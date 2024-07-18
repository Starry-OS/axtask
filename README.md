# 对 async 函数的支持

对 async 函数的支持（后续简称为“支持”）依赖于 taskctx 模块中对 TaskInner 的修改。

## taskctx 模块的修改

1. TaskInner 表示一条执行流，既可以是协程也可以是线程，在原本的基础上，去掉了 `kstack`、`ctx` 和 `entry` 等字段，取而代之的是用 `future`、`ctx_ref` 字段。
   - `future` 字段表示协程的实际代码
   - `ctx_ref` 则是表示栈上的上下文指针，当任务正常运行时，访问 `ctx_ref` 指向的上下文指针将会是非法的，只有在产生时钟中断时，才会对这个字段进行操作。
2. 在 `TaskContext` 数据结构中，增加了其他通用寄存器的信息以及特权级的信息，后续需要增加地址空间信息，但这些增加的额外信息在目前的改动中是没有使用的。

## 修改的思路

宗旨：尽可能的将修改限制在这个模块内，因此对于 `arch_boot`、`axhal`、`axruntime` 等模块中的初始化代码需要继续沿用。

### Processor

1. 为 `Processor` 增加了 `stack_pool` 字段。当中断发生时，将当前任务的上下文保存在栈上，并将上下文的指针写到当前任务的 `ctx_ref` 字段上，再从 `Processor` 中取出一个空闲栈，在空闲栈上从调度器中取出下一个任务执行。
2. 初始化 primary cpu 时，`idle_task` 仅仅是用于占位，在实际的调度过程中，如果下一个任务是 `idle_task`，则会令 `next_task = prev_task`。而 `main_task` 初始化时也是被初始化为协程（保证所有的任务在没有中断的情况下都是协程）；
3. 在 `Processor` 初始化 `CurrentTask` 后，初始化当前预留的空闲栈 `CurrentFreeStack`。

### task_switch

“支持” 会将原本的 `schedule` 函数替换成 `task_switch.rs` 文件下实现的任务切换代码。 

1. 提供了 `preempt_switch_entry` 函数，用于在时钟中断进行抢占时，进行任务切换的入口，在其中会调用 `save_prev_ctx` 函数，将上下文保存在栈上，并跳转至 `schedule_with_sp_change` 函数。`schedule_with_sp_change` 函数将会切换到空闲的栈上，并从调度器中取出下一个任务执行，直到再次调度到被打断的任务时，才会回到 `preempt_switch_entry` 函数中，并从中断处理函数中返回（期间是不会发生中断的，原本 starry 中的实现也是如此，只有当任务第一次被执行时会使能中断，一旦执行了第一次之后，则必须等到下一次从中断处理函数中返回时才能）。
2. 提供了 `switch_entry` 函数，用于主动进行的任务切换，被用于 `run_idle` 和 `block_on` 函数中。
3. 通过 `exchange_current` 和 `run_next` 函数，当下一个任务为线程时，将当前使用的栈回收，并且从下一个任务的 `ctx_ref` 字段中取出上下文指针，并从中恢复上下文；若为协程，则直接调用协程的 `poll` 接口运行。
4. 增加 `block_on` 接口，标定了 no async 和 async 的界限，使得应用程序一开始就处于 async 的环境中。并且通过 `async_main` 宏来提供便利。

通过 task_switch 中的实现，现在系统中的任务都被统一了，既可以支持线程也可以支持协程，并且支持四种任务切换:

1. 协程 -> 协程
2. 协程 -> 线程
3. 线程 -> 协程
4. 线程 -> 线程

## await 与调度

1. 由于 future 需要通过 await 来驱动，且当 future await 返回 pending 时，会自动回到调度函数中，因此修改过程中利用了 await 来实现 yield、sleep、wait 等操作。而在 async 包裹的函数中，也不应该直接调用 exit 接口，因为当 async 执行完成后，它也会自动返回到调度函数中。

## 修改过程中出现的问题

1. 复杂的上下文切换机制导致了中间增加了额外的代码，既不属于上一个任务也不属于下一个任务
   具体是指：当上一个任务的上下文被保存后，下一个任务的上下文还没有恢复，在这期间，原本 starry 中的逻辑是若 `next_task == idle`，则会令 `next_task == prev_task` 从而直接返回（starry 原本的实现中，调度代码是从属于每个任务的），但在修改后，若直接返回，则会导致上下文没有被正常恢复（这一段代码并不在 `prev_task` 的上下文中），从而导致程序出错。
   解决方案：即使 `next_task == prev_task` 也不返回，而是继续执行，恢复上下文。

2. 上下文嵌套
   当尝试最大程度的利用已有的实现，在 init primary cpu 时，直接将 `main_task` 定义成线程，尽管在发生时钟中断，进行抢占时，任务切换能够正常工作（因为不允许中断嵌套），但一旦从中断处理中退出时，`main_task` 主动切换到其他的任务时，还需要使用 `ctx_ref` 字段来记录当前的上下文，此时再发生时钟中断后（还未切换到下一个任务），此时 `main_task` 任务的 `ctx_ref` 将会被覆盖，导致下一次恢复上下文时，不能恢复到最上层的上下文，导致错误。
   解决方案：将 `main_task` 也定义成协程，但使用一个不会占用空间的 async 闭包来填充 `TaskInner` 的 `future` 字段，这个 main_task 只是为了利用原本的 starry 接口才不得不这样做，实际执行时，不会产生任何影响。当 block_on 接口创建的任务（即 `main_coroutine`）退出时，系统关机。
