# Lab 5 · 收尾对账：交付读数、探针撤净清单，以及两处**没有**被 25/25 覆盖的问题

## 1. 交付读数

交付读数是 `logs/c3_ch8_verify.log`（未放宽超时、内核内无任何探针、`reports/` 齐备）：

```
Test passed46471: 25/25
Report for lab1 found.   ... Report for lab5 found.
MAKE_RC=0
[PASS] not found <FAIL: T.T>
[PASS] not found <Test sbrk failed!>
```

它的前一次 `logs/c3_ch8_final.log`（后缀 `50353`）同样给出 `25/25`，唯一区别是当时
`reports/` 还没有内容，`MAKE_RC=2` 来自判决之后的报告闸。两轮判决完全一致，
所以报告缺失没有影响用例本身。

`~/.local/bin/python3.12 rig3_c_gaps.py ch8 logs/c3_ch8_final.log` 的差集：

```
verdict_lines=[('25', '25')]  逐条判决行数=25
需补齐=0
不该出现却出现了=0
```

`grep -ac "error: could not compile"` 在两轮都是 0。`CHAPTER=8` 需要 `lab1..lab5` 五个文件齐备，
闸口才闭合，这一点由 `verify` 轮的 `MAKE_RC=0` 直接证实。

同一分支此前两次的提交记录：

```
20d6885  ch8: 死锁检测改为「按需重建二部 wait-for 图」，并记下仍未解决的墙钟问题
eab2c61  wip(ch8): queue-depth aware deadlock detection, unverified checkpoint
```

`eab2c61` 的提交信息里我自己标了 unverified；本轮把它变成 verified 的内容是两处：
归约规则的最终形态（lab2）和 `sys_get_time`（lab4）。

## 2. 探针撤净的清单与验证方式

诊断期间往内核里塞过七组探针，全部为临时仪器，交付版不留：

| 位置 | 标签 | 撤除方式 |
|---|---|---|
| `os/src/syscall/sync.rs` | `[sl]` | `git checkout --`（该文件只有探针改动） |
| `os/src/timer.rs` | `[tw]`、`DUMP_TICK`、`[sq]` 快照 | 同上 |
| `os/src/trap/mod.rs` | `probe_suspend(1)` | 同上 |
| `os/src/fs/pipe.rs`、`os/src/fs/stdio.rs` | `probe_suspend(4/5/6)` | 同上 |
| `os/src/task/manager.rs` | `[qa]/[qf]`、`len()`、`snapshot()`、`probe_id()` | 同上 |
| `os/src/task/mod.rs` | `probe_suspend`、`SUSPEND_COUNT`、`ready_queue_len/snapshot` | 手工删（同文件保留了 `forget_task` 调用的移除） |
| `os/src/sync/mutex.rs` | `probe_suspend(2)` | 手工删（同文件是归约版的实体改动） |
| `os/src/sync/semaphore.rs` | `[su]/[sd]`、`deadlock::whoami/short` 调用 | 手工删，`up()` 的 `match` 收回 `if let` |
| `os/src/sync/deadlock.rs` | `[dl]` 判定行、`dump_state()`、`whoami()`、`short()` | 手工删，`would_deadlock` 收回两行 |

撤净后的验证是一行 grep，而不是「看着差不多」：

```
$ grep -rn "probe\|TEMPORARY\|\[dl\]\|\[sd\]\|\[su\]\|\[sl\]\|\[tw\]\|\[sq\]\|\[qa\]\|\[qf\]\|\[sc\]\|dump_state" os/src
（无输出）
```

保留的 `git diff` 只剩五个文件：`sync/deadlock.rs`、`sync/mutex.rs`、`sync/semaphore.rs`、
`syscall/process.rs`、`task/mod.rs`。撤完必须重跑官方轮——诊断态的 25/25 与官方态的 25/25
是两次独立读数（lab1 §3），这一步不能省。

## 3. 没被 25/25 覆盖的问题之一：检测开关是全局的，而 `mpsc_sem` 的记账会漂

`ENABLED`（`os/src/sync/deadlock.rs:99`）是 `static mut bool`。三个 deadlock 用例把它打开后再没有人关掉，
而 `ch8_usertest` 的 23 个用例并发运行，所以后面的用例是在检测开启的状态下跑的。这在 `probe10` 的日志里
留下了明确痕迹——按 `[dl]` 出现次数统计地址空间分布：

```
185 [dl] pid=55      34 次 REFUSE
 19 [dl] pid=56       0 次 REFUSE
  6 [dl] pid=16       0 次 REFUSE   ← sem2
  6 [dl] pid=15       1 次 REFUSE   ← sem1
  1 [dl] pid=58       1 次 REFUSE
  1 [dl] pid=14       1 次 REFUSE   ← mutex1
```

pid 55 是 `ch8b_mpsc_sem`。它的形状暴露了归约的适用边界：

```
0.45 [dl] pid=55 me=5 ask=2 free=[(0, 1), (1, 6), (2, 0)] held=[..., (5, [(2, 8)])] queues=[] -> REFUSE
0.51 [dl] pid=55 me=3 ask=1 free=[(0, 1), (1, 0), (2, 8)] blocked=[(1, 1), (2, 1)] held=[..., (5, [(2, 8)])] queues=[(1, [1, 2])] -> REFUSE
```

`held` 里 `t5` 对 S2 的持有数从 8 一路涨到 11。S2 是 `SEM_EXISTED`，`ch8b_mpsc_sem.rs:39-42` 的消费者循环
`for _ in 0..400 { semaphore_down(SEM_EXISTED); ... semaphore_up(SEM_EMPTY); }` ——
它 down 一个对象、up **另一个**对象，且 `semaphore_down` 的返回值没有被接收。
我的记账把每次 `down` 记成该任务的一次持有、只有对**同一对象** `up` 才销账，
于是管道型用法（单位从生产者流向消费者）会让消费者一侧的持有数单调增长，
最终 detector 认为「EXISTED 的全部单位都冻在消费者自己手里」，拒绝它自己的下一次 `down`。

这不是「检测器算错了一个真死锁」，而是**检测器的语义只在 down/up 配对的用法下成立**。
同一批日志里还有一条更能说明边界的拒绝，来自 pid 58（`ch8b_sync_sem`）：

```
0.47 [dl] pid=58 me=2 ask=0 free=[(0, 0)] blocked=[] held=[] queues=[] -> REFUSE
```

`free=0`、`held=[]`、`queues=[]`——被请求的对象此刻一个单位都没发出去，也没有任何人握着它。
归约因此断言「不会有单位回来」并拒绝。但这个形状恰恰是**等待未来生产者**的正常用法：
`sync_sem` 用 `semaphore_create(0)` 建同步量，先跑的一方本就该 park 到另一方 `up`。
归约的规则 1 只能归还「已经发出去的单位」，对「还没发生的生产」一无所知，
所以它把这类合法阻塞也判成了死锁。这是算法的适用边界，不是本轮的笔误。

本轮这些拒绝都没有扣分，因为 `mpsc_sem` 和 `sync_sem` 都不接收 `semaphore_down` 的返回值，
`assert` 只查创建时的 id 顺序，测试照样打出 `mpsc_sem passed50353!` / `sync_sem passed50353!`。
但这是**通过得比设计更宽松**，不是无害。收口方向两步，都不该和这次的修复混在一个 diff 里：

1. 开关改为按地址空间存（`build_graph(me.0)` 本来就是按进程重建的，把 `enabled()` 也变成按进程读即可），
   未调用 `enable_deadlock_detect` 的进程永远走原路径——上面两类误拒随之消失，
   因为三个 deadlock 用例以外的进程根本不再被询问。
2. 持有记账改成「按对象净持有」：把 `grants` 的语义限定为「该任务从该对象取得且尚未归还的单位」，
   对 `down(A)/up(B)` 这种流动型用法，A 侧的计数应由对象自身的 `count` 表达，而不应长期挂在任务上。

## 4. 没被 25/25 覆盖的问题之二：一次内核 panic，本轮无法复现

更早的两轮里出现过：

```
logs/c3_ch8_clean_diag120.log:
   1.13 ... race adder using spin mutex test passed17847!
   1.13 [kernel] Panicked at src/trap/mod.rs:146 a trap Exception(InstructionPageFault) from kernel!

logs/c3_ch8_diag240b.log:
   0.60 I am child 39
   0.60 forktest pass.
   0.60 [kernel] Panicked at src/trap/mod.rs:146 a trap Exception(InstructionPageFault) from kernel!
```

`src/trap/mod.rs:146` 是 `trap_from_kernel()` 里的 `panic!("a trap {:?} from kernel!")`——
即内核态自己取指缺页。两次现场分别在 `race_adder_mutex_spin` 之后和 `forktest` 之后，
共同点是都发生在大量进程/线程密集建立与销毁的窗口。
`probe1`~`probe10` 与 `final` 六轮里 `grep -ac "from kernel!"` 全为 0，包括放宽到 45s/120s 的诊断轮。

我的判断是：**没有证据支持它已被 `sys_get_time` 的修复顺带解决**，只能说它的触发条件依赖时序，
而这几轮的时序里没有命中。这条不做处理也不声称已修；如果后续任何一轮复现，
最小可查动作是在 `trap_from_kernel` 里把 `sepc`/`stval` 连同当时的 `pid/tid` 一起打出来
（`trace!` 那行现在在 release 配置下不输出， panic 之后什么都拿不到）。

## 5. 交付外的两件事

- CI 复核：推送只能本地快进，`gh` 在本机未登录，Actions 的判分结果需要用户账号或浏览器通道确认，
  我不代替它声称「CI 已过」。
- 与 ego1/ego2 的独立性：本分支的检测器不是「维护一张 wait-for 图并找环」那条常见路线
  （lab2 §3、§4 给了它被 `sem2` 证伪的具体数据），`sys_get_time` 的修法也不同于按 `do_gettimeofday`
  抄写的一版：这里坚持 `sec/usec` 单次读数并与内核定时器同源，理由写在 lab4 §4。
