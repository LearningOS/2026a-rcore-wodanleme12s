# Lab 3 · ch5 进程管理：spawn 的孩子归谁，以及 set_priority 的合法区间

这一章剩下的 4 条全是进程语义：`Test spawn0 OK!`、`Test wait OK!`、`Test waitpid OK!`、
`Test set_priority OK!`。前两条在 Lab 1 修完指针闸门后仍然不亮，说明它们不是「跑不起来」，
而是「跑起来了但记账记错了地方」。这份报告记录我怎么从钉死的测试程序倒推出契约，以及两处修正。

## 1. 先把 `user_lib` 的返回码口径钉住（不是内核想返回什么，是库会怎么解读）

`ci-user/user/src/lib.rs`：

```rust
pub fn wait(exit_code: &mut i32) -> isize {
    loop { match sys_waitpid(-1, exit_code as *mut _) {
        -2 => { sys_yield(); }   // 孩子还活着：让出 CPU 再来
        n  => { return n; }      // 其余值一律直接返回
    }}
}
pub fn waitpid(pid: usize, exit_code: &mut i32) -> isize { /* 同上，只是第一个参数是具体 pid */ }
```

⇒ 内核的 `sys_waitpid` 必须区分**三种**情况，而不是两种：

| 情况 | 返回值 | 库的行为 |
|---|---|---|
| 找到一个 `Exited` 的孩子 | 它的 pid（>0），并把 `exit_code` 写回用户指针 | 返回 pid |
| 孩子存在但还没退出 | `-2` | `yield` 后重试 |
| 根本没有这个孩子 | `-1` | 直接返回 `-1` |

`ch5_spawn0.rs` 最后一行 `assert!(wait(&mut exit_code) <= 0, "wait got too many")` 正好用到了第三行：
40 个孩子都收完以后再 `wait` 一次，必须拿到非正值。所以 `-2` 不能当「没有孩子」用，否则
`user_lib::wait` 会在 `sys_yield` 里无限自旋，那一轮就会以 `Error 124` 结束（30 秒墙钟被 timeout 截掉）。

## 2. 缺陷一：孩子登记在 initproc 名下，于是 `wait` 收不到

`gate2` 的日志里两个应用各自炸在自己的断言上：

```
Panicked at src/bin/ch5_spawn0.rs:23, wait stopped early
Panicked at src/bin/ch5_spawn1.rs:23, assertion `left == right` failed: error exit pid
```

`ch5_spawn0` 生成 40 个 `ch5_getpid` 孩子，然后连收 40 次；`ch5_spawn1` 更直接：

```rust
let cpid = spawn("ch5_exit0\0");
let exit_pid = wait(&mut exit_code);
assert_eq!(exit_pid, cpid, "error exit pid");   // 第 23 行
assert_eq!(exit_code, 66778, "error exit code");
```

两个断言都要求**调用者自己能收到自己 spawn 的孩子**。而 `sys_spawn` 里写的是

```rust
crate::task::INITPROC.inner_exclusive_access().children.push(new_task.clone());
```

原注释理由是「这样它的退出状态还能被 `wait` 收到」——但 `sys_waitpid` 只查**当前任务**的
`children`，initproc 名下的孩子对普通任务不可见，于是每次 `wait` 都走 `-1` 分支：
`spawn0` 第一次 `assert!(wait(..) > 0` 就炸（"wait stopped early"），`spawn1` 拿到 `exit_pid = -1`
和 `cpid` 比较失败（"error exit pid"）。这条注释把「谁收养」和「谁能收」混成了一件事。

改成登记在 spawn 的调用者名下，并保留退出时的孤儿处理（`task/mod.rs:129` 在 `exit_current_and_run_next`
里把死掉任务的孩子重新挂到 INITPROC，父进程不会因此漏收）：

```rust
let task = current_task().unwrap();
task.inner_exclusive_access().children.push(new_task.clone());
add_task(new_task);
```

另外 `sys_spawn` 不能走 `fork + exec`：fork 会克隆父进程地址空间再扔掉，更要紧的是 fork 出来的孩子
会进父进程的 `children`，让 `wait` 报出「父进程从没 spawn 过的程序」。`ch5_spawn0`/`ch5_spawn1`
考的正是这一点，所以我直接用 ELF 构造新 TCB。

## 3. 缺陷二：`set_priority(1)` 必须算非法

`ch5_setprio.rs` 的四条断言把合法区间钉死了：

```rust
assert_eq!(set_priority(10), 10);
assert_eq!(set_priority(isize::MAX), isize::MAX);
assert_eq!(set_priority(0), -1);
assert_eq!(set_priority(1), -1);     // ← 第 16 行，当时正炸在这里
assert_eq!(set_priority(-10), -1);
```

我原来只拒 `prio <= 0`，认为「权重为正就有意义」，于是 `set_priority(1)` 回显 `1`，断言失败。
`1` 不是被数学排除的（stride 里 `charge = STRIDE_WEIGHT / priority`，`STRIDE_WEIGHT = 1 << 20`，`priority == 1` 时电荷正好是整份权重，数学上没问题），
它是被**接口契约**排除的：这一章的 `isize::MAX` 要接受、`1` 要拒绝，能同时满足的写法只有 `prio <= 1` 拒。
教训是别拿「我的实现自洽」去替代「测试怎么写」，判绿只认判分器。

## 4. stride 调度：最小堆，且只碰自己那份 pass

就绪队列是 `BinaryHeap<QueueEntry>`，键为 `(pass, seq)`；因为 `BinaryHeap` 是大顶堆，`Ord` 实现里
把比较反过来，让 pass 最小的先出队。三条规则：

- 出队时给该任务记 `stride_pass`，按 `STRIDE_WEIGHT / priority`（`STRIDE_WEIGHT = 1 << 20`）折算成这一轮要加的 pass，用
  `saturating_add` 保证不回绕；
- 新建/唤醒任务**只**改它自己的 pass，绝不把整队 pass 抬一遍（那是会 isize 回绕并让某个任务独占 CPU 的写法）；
- `sys_set_priority` 改权重后重新入堆，权重变化不需要扫描队列。

`ch5_stride0..5` 与 `ch5_setprio` 组合起来考的就是这两条：改优先级要立刻反映到 CPU 分配比例上，
而改一个任务不能伤到别人的计数。

## 5. 读数与差集

| 轮次 | 读数 | 关键变化 |
|---|---|---|
| `takeov0` | `2/15` | 接手原样 |
| `probe1` | `2/15` | 加两行临时探针，拿到 `path ptr 31e4 unreadable` |
| `gate1` | 编译失败（E0597） | 指针闸门改造时的借用错误，非判分结论 |
| `gate2` | `11/15` | 页表翻译后端接管字符串读取；点亮时钟族 + mmap 族 |
| `gate3` | **`15/15`** | spawn 归属改到调用者 + `set_priority` 拒绝 `1` |

`rig3_c_gaps.py ch5 logs/c3_ch5_gate3.log`：`需补齐=0`、`不该出现却出现了=0`。
临时探针（两行 `println!`）在 `gate3` 之前已经撤掉，判分用的仍是官方 `timeout --foreground 30s`，
`make test` 的 `MAKE_RC` 在补齐本报告（`reports/lab1..lab3.md` 纳入 git 跟踪）之后才是最终口径。
