# Lab 4 · 根因不在检测器：`user_lib::sleep` 是用户态忙等，而 `sys_get_time` 是个返回 -1 的桩

对应改动：`os/src/syscall/process.rs` 的 `sys_get_time`（唯一一行功能性修改）。
判分证据：`logs/c3_ch8_final.log`（25/25）；排查过程：`logs/c3_ch8_probe1_diag90.log` …
`logs/c3_ch8_probe10_diag45.log`。

## 1. 现象

在实现归约检测器之后，ch8 的读数长期停在 `23/25`：`deadlock test mutex 1` 通过，
`deadlock test semaphore 1` 和 `deadlock test semaphore 2` 两条期望串永远不出现。
两条串的缺席方式很特别——不是断言失败、不是错误码不对，而是**整个 usertest 收集停住了**，
后面的输出照常有，只有这两条（以及它们所属进程的最终汇总）不再出现。

`ch8_usertest` 的模型解释了为什么「一个用例不退出」会伪装成「另一个用例没过」：

```rust
for (i, &test) in TESTS.iter().enumerate() {   // 先把 23 个用例全部 fork 出来
    ...
}
for i in 0..TEST_NUM {                          // 再按 pid 顺序逐个 waitpid 收集
    waitpid(pids[i], &mut status);
}
```

先 fork 全部、再按 pid 顺序收，意味着**第一个不退回调用者的用例就冻结整条收集链**。
sem1（pid 15）、sem2（pid 16）在 `forktest2`（pid 11）之后，而 pid 11 也在等一个不会退出的东西。

## 2. 三个被否证的假说

排查期间我按顺序给出过三个假说，每个都被探针**主动否证**（不是等它自己消失）。

### 假说 A：内核唤醒路径有 bug

依据是 `[dl] pid=15` 之后该 pid 再无任何输出，看起来像「睡下去没人叫醒」。
否证：在 `sys_sleep` 和 `check_timer` 里加 `[sl]`/`[tw]` 计数后，同一批日志里其它 42 次睡眠
都完整打出了 `due`/`now`/`awake` 三段，`[tw] ... wake pX tY due=` 也照样出现。
pid 15/16 的特殊之处是它们**从来没有调用过 `sys_sleep`**。

### 假说 B：任务被标成 Ready 但被丢出了就绪队列

依据是 tick 快照里出现 `p15 t0 ready` 却在其后看不到任何该 pid 的输出，很像丢唤醒。
否证：在 `add_task`/`fetch_task` 里对 pid 11/15/16 打 `[qa]`/`[qf]`，
一轮诊断跑完得到 8940 次成对的入队/出队计数。它们没有被丢掉，它们被反复调度，
只是每次醒来第一件事就是 `sys_yield` 回来。

### 假说 C：stdout 的自旋锁被永久持有

依据是「有调度却没有任何打印」。
否证：读 `os/src/fs/stdio.rs`——`Stdout` 里根本没有锁，`write` 直接走 `console_write`。
这条假说连前提都不成立，是我在看日志形态时类比出来的，代价是又一轮诊断。

## 3. 真正的因果链

`user_lib` 是判分副本里的官方代码（我不会去改它），它的 `sleep` **不是** `sys_sleep`：

```rust
// ci-user/user/src/lib.rs:263
pub fn sleep(period_ms: usize) {
    let start = get_time();
    while get_time() < start + period_ms as isize {
        sys_yield();            // ← 用户态忙等，靠 get_time 推进
    }
}

// ci-user/user/src/lib.rs:209
pub fn get_time() -> isize {
    let mut time = TimeVal::new();
    match sys_get_time(&mut time, 0) {
        0 => ((time.sec & 0xffff) * 1000 + time.usec / 1000) as isize,
        _ => -1,
    }
}
```

而 ch8 章节树里的 `sys_get_time` 是本章骨架留给学生的桩，直接返回 `-1` 且不写 `*ts`：

```rust
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize { ... -1 }
```

于是 `get_time()` 恒为 `-1`，`start = -1`，循环条件变成 `-1 < -1 + 500` —— **恒真**。
sem1 的 `main`（`ch8_deadlock_sem1.rs:89` 的 `sleep(500)`）、sem2 的 `main`
（`ch8_deadlock_sem2.rs:61` 的 `sleep(1000)`）以及 `ch5b_forktest2` 的子进程（`ch5b_forktest2.rs:20` 的 `sleep(sleep_length)`，它同时还直接用 `get_time()` 算睡眠长度）
都落进这个永真循环，每圈一次 `sys_yield`。这就是假说 B 里那 8940 次入队/出队的来源。

三个用例里有两个是本次判决的目标，第三个（pid 11）按 pid 顺序挡在它们前面——
「23/25 缺两条 sem」的真实原因是「三个主线程里有两条卡在同一个未实现的系统调用上」。

## 4. 修复

`os/src/syscall/process.rs`：

```rust
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let now_us = get_time_us();
    let now = TimeVal {
        sec: now_us / 1_000_000,
        usec: now_us % 1_000_000,
    };
    let process = current_process();
    let token = process.inner_exclusive_access().memory_set.token();
    drop(process);
    *translated_refmut(token, ts) = now;
    0
}
```

三个决定各有理由，不是风格问题：

1. **同一个计数器**：`sec/usec` 必须由一次 `get_time_us()` 读出并拆分。分两次读的话
   在跨越微秒进位时能拼出 `sec=1, usec=999999 → sec=0, usec=0` 这类倒序值，
   `get_time()` 的毫秒折算就会倒退，忙等条件反而多绕几圈。内核的定时器用的正是 `get_time_us()`
   （`os/src/timer.rs`），两侧同源，`sleep` 的墙钟语义才和 `sys_sleep` 一致。
2. **`sec` 要能进 `& 0xffff`**：用户侧折算写的是 `(sec & 0xffff) * 1000 + usec / 1000`，
   `sec` 超过 65535 会绕回，所以这个字段必须保持小——按微秒计数器直接除 1e6 在 30s 会话内最多到 30，安全。
3. **写回走调用者页表**：`*translated_refmut(token, ts) = now`，和同文件 `sys_waitpid` 写
   `exit_code` 的写法一致；`drop(process)` 之后再翻译，避免把 `RefCell` 的可变借用
   跨过可能触发调度的路径（这条约束在 `MutexSpin` 那处已经踩过一次，见 `os/src/sync/mutex.rs:47-58` 的注释）。

分发不需要动：`syscall/mod.rs:134` 早就是
`SYSCALL_GETTIMEOFDAY => sys_get_time(args[0] as *mut TimeVal, args[1])`。

## 5. 这一条值得留下的教训

`ch8` 的三条缺口我先前归因到检测器算法上，还为此重写过一次归约规则；而真正拦住它的是一行
`_ => -1` 的桩函数。判分集合里**没有任何一条**期望串叫「`sys_get_time` 已实现」——
`ch3_sleep`/`ch3_sleep1` 在 ch3/ch5 的判决集合里，ch8 只借用 `sleep` 作为用例内部的同步手段。
所以「缺字符串」这种失败形态，在多个用例共享同一个未实现桩的时候，不能推出「检测逻辑判错了」。
正确的下一步永远是：先问这个用例的**控制流**是否真的走到了断言，再谈断言的内容。

诊断轮与官方轮的分工也在这里定下来：`probe10` 给出 `Test passed18192: 25/25` 时，内核里还带着全部探针、
会话超时被我放宽到 45s。那个 25/25 只能证明「根因找对了」，不能当交付读数；
撤净探针后的 `logs/c3_ch8_final.log`（`Test passed50353: 25/25`，
`grep -ac "error: could not compile"` = 0）和补报告后的 `logs/c3_ch8_verify.log`
（`Test passed46471: 25/25`，`MAKE_RC=0`）才是。
