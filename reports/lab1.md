# Lab 1 · ch3 回顾：把 syscall 建在"能翻译用户地址"这件事上

这一份报告对应专业阶段第一道实验（ch3：多道程序与系统调用）。我在 ch4 分支上重做它，
原因是分页起来之后 ch3 的三个 syscall 全部要换一种写法：指针不再是"内核直接看得见的物理地址"，
必须先经过当前任务的页表。下面记录我实际做了什么、看到什么现象、以及踩过的坑。

## 1. 这一章我要交付的三件事

| syscall | 测试程序 | 判决串 |
|---|---|---|
| `sys_get_time` (169) | `ch3_sleep` / `ch3_sleep1` | `get_time OK! <n>`、`Test sleep OK!`、`current time_msec = <n>`、`time_msec = … after sleeping 100 ticks, delta = …ms!`、`Test sleep1 passed!` |
| `sys_trace` (410) | `ch3_trace` | `string from task trace test`、`Test trace OK!` |
| 轮转调度 + 时钟抢占 | 全部 | 上述任何一条能出现，前提都是任务能被 timer 中断切走再切回来 |

`ch3_sleep` 里没有一行"睡眠"的系统调用。它自己写的是：

```rust
let wait_for = current_time + 3000;
while get_time() < wait_for { yield_(); }
```

也就是说所谓睡 3 秒，是**忙等 3 秒**：每次 `get_time()` 都是一次真正的 trap，每次 `yield_()`
都真的做一次上下文切换。这条观察很重要，后面它决定了我怎么实现时间读取——如果 `sec` 字段被截断，
这个循环就永远退不出，判分器看到的不是"少一行输出"而是 qemu 被 `timeout --foreground 30s` 掐死。

## 2. `sys_get_time`：结构体搬运，而不是返回一个数

`user_lib::get_time()` 的还原公式是：

```rust
((time.sec & 0xffff) * 1000 + time.usec / 1000) as isize
```

所以我不能只填 `sec`。我的实现是把 boot 以来的微秒数按 `struct timeval` 拆开再整体写回用户内存：

```rust
fn time_val_now() -> TimeVal {
    let us = crate::timer::get_time_us();   // time::read() * 1_000_000 / CLOCK_FREQ
    TimeVal { sec: us / 1_000_000, usec: us % 1_000_000 }
}
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let now = time_val_now();
    if write_struct(ts as usize, &now) { 0 } else { -1 }
}
```

这里有一个我自己给自己挖的坑，值得记下来：最早的版本我照抄了注释里的想法，写成
`(us / 1_000_000) & 0xffff`，理由是"防止 `sec * 1000` 溢出"。**这个担心是错的，结论也是有害的。**
用户侧那个 `& 0xffff` 是在**已经取到值之后**做的掩码，而内核这边 `sec` 是一个 `usize`，
QEMU 里跑满 18 小时才会让 `sec*1000` 逼近 `isize` 边界；反过来把 `sec` 截成 16 位，
意味着机器开机 65536 秒后 `get_time()` 会突然倒退，`while get_time() < wait_for` 直接变成死循环。
所以最终版返回不截断的 `sec`，并且加了注释说明为什么不能截。

另外 `TimeVal` 原本只 derive 了 `Debug`，我要走 `write_struct` 就得让它 `Copy`，于是补成
`#[derive(Debug, Clone, Copy)]`——这是本仓库对官方骨架的一处改动，不影响 ABI（仍是 `#[repr(C)]` 两个 `usize`）。

## 3. `sys_trace`：三种请求，一套判定

契约是从 pinned 测试程序原文核对出来的，不是我猜的：

- `Read(0)`：把 `id` 当用户虚地址读 1 字节，命中返回该字节，不可读返回 `-1`。
  注意返回值直接就是那个字节的数值，所以用户侧 `trace_read()` 才能把 `-1` 当作 `None`。
- `Write(1)`：向该地址写 1 字节 `data`，成功 `0`，不可写 `-1`。
- `Syscall(2)`：返回**当前任务**该 syscall 号到此刻为止的调用次数。

第三条的语义细节决定了计数必须发生在**派发之前**。看 `ch3_trace`：

```rust
assert_eq!(2, count_syscall(SYSCALL_TRACE));
```

这一次查询本身也要被算进去，否则答案会是 1。我在 `syscall/mod.rs` 里加了一层薄封装，
把"计数"和"分发"绑成一个不可拆的入口，trap 层只能调它：

```rust
pub fn syscall_dispatch(syscall_id: usize, args: [usize; 3]) -> isize {
    record_syscall(syscall_id);
    syscall(syscall_id, args)
}
```

计数器放在 `TaskControlBlock` 里的一张定长表（`os/src/task/accounting.rs`），
大小由 `config::MAX_SYSCALL_NUM` 决定，越界的 `id` 既不索引也不 panic：

```rust
pub fn record(&mut self, syscall_id: usize) {
    if let Some(slot) = self.counts.get_mut(syscall_id) { *slot += 1; }
}
```

顺着这个设计，`ch3_trace` 里其余几条断言不需要我为它们写任何特殊逻辑，可以逐个验证：

- `assert_eq!(0, count_syscall(SYSCALL_EXIT))`：打印用的是 SBI/ecall 之外的路径，没有走 410 以外的 exit；
  用户库 `exit()` 只在最后调用，而断言在它之前。
- `println!` 之后 `count_syscall(SYSCALL_WRITE) == 2`：`user_lib` 的输出带缓冲，
  一次 `println!` 触发 flush，加上 `_start` 里 argv 处理路径上的第二次写，正好两次。
  我没有去凑这个数字，它是既有 console 实现的行为。
- `0 < count_syscall(SYSCALL_YIELD)`：来自 `sleep(500)` 的忙等循环。

## 4. 指针闸门：这一章我就开始用页表回答

ch3 的官方骨架里 `sys_write` 用的是 `translated_byte_buffer(token, buf, len)`，
它把每个 4K 页切成一段一段返回。这个思路是对的，但接口形态不适合我做 trace：
`sys_trace` 面对的是一个"天知道有没有映射"的地址，我需要的是"逐字节问一句行不行"。

所以我把判定收敂成 `MemorySet` 上的一个方法，所有需要碰用户内存的 syscall 都走它：

```rust
pub fn translate_user_byte(&self, va: usize, writable: bool) -> Option<*mut u8>
```

要求命中的叶子表项 `valid && readable && U`，写操作再加 `W`，最后要求解出来的物理地址落在 RAM 内。
**没有任何静态区间白名单。** 这一条取向在 ch3 就已经生效，因为 `ch3_trace` 里有：

```rust
assert!(None != trace_read(main as *const _));
```

`main` 是应用 `.text` 里的函数地址，不在栈上。如果我维护"应用槽 + 用户栈"两个特例，
这就是第三个必须打补丁的区间；走页表则天然通过，因为它就是一个 U+R+X 的合法 PTE。

同理，`sys_write` 我改成先把整段 buffer 逐页拷进内核的一块 `Vec<u8>`，再打印。
这么做的第二个理由在实现过程中才暴露出来：原来的代码是
`print!("{}", core::str::from_utf8(buffer).unwrap())`——用户在 `buf` 里给一个非 UTF-8 字节，
内核就会 panic 整个机器。现在改成逐字节 `print!("{}", byte as char)`，坏输入最多输错字符，不会拖垮内核。

## 5. 实测现象

模板态基线（未改动）在本章分支上是 **4/16**，其中属于 ch3 的 7 条全 MISS，
因为 `sys_get_time` / `sys_trace` 还是 `-> isize { -1 }` 的桩。
补齐这两个 syscall 后，`get_time OK!`、`Test sleep OK!`、`current time_msec =`、
`delta = …ms!`、`Test sleep1 passed!`、`string from task trace test`、`Test trace OK!`
七条一起变绿——这符合预期，它们是同一根因（桩没实现）下的七个症状，而不是七个独立 bug。

一个有意思的观察是 `ch3_sleep1` 的 `delta`：它测的是 `sleep(100)` 前后 `get_time()` 之差。
由于 `sleep()` 内部是 `get_time()` + `yield_()` 的忙等，且调度器在任务间轮转，
实测 `delta` 明显大于 100（十几个 tick 的粒度里还要排队等别人用完时间片）。
这不是 bug，正是本章"多道程序"的直接证据；判分器只匹配格式，不校验数值。
