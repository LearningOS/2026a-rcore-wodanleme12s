# Lab 2 · 检测器的形态：不维护 wait-for 图，改成「按需重建 + 归约」

对应改动：`os/src/sync/deadlock.rs`（本分支的主战场，500 行）、
`os/src/sync/semaphore.rs`、`os/src/sync/mutex.rs`、`os/src/task/mod.rs`。
判分证据：`logs/c3_ch8_final.log`（25/25）。

## 1. 两次形态变更的历史

```
20d6885  ch8: 死锁检测改为「按需重建二部 wait-for 图」，并记下仍未解决的墙钟问题
eab2c61  wip(ch8): queue-depth aware deadlock detection, unverified checkpoint
(本轮)   撤净探针 + 修 sys_get_time（见 lab4），形态沿用 eab2c61 的归约版
```

第一版（`20d6885`）在 `MutexBlocking` 里加了 `owner: Option<Arc<TaskControlBlock>>` 字段、
`is_held_by()`、`remove_waiter()`、`owner_tid()`，再加一套 `take_abort_for()` / `refuse_if_not_me()`
的「把已排队者从 FIFO 队列里拽出来并记下欠它的错误码」协议。第二版把这些**全部删掉**了。
删除本身是本报告的主要内容，因为留下的代码少了一半，行为反而从「sem1 不过」变成「三条全过」。

被删掉的三类状态，以及为什么可以删：

| 第一版的东西 | 为什么是必需品 | 第二版为什么不需要 |
|---|---|---|
| `MutexBlockingInner::owner` | 图要回答「这把锁在谁手里」 | 归约不需要边，只需要「每个对象欠谁几个单位」；持有关系由各任务 TCB 里的 `grants` 提供 |
| `remove_waiter()` / `refuse_if_not_me()` | 受害者已经在队列里，必须把它拉出来 | 拒绝只发生在**入队之前**（`deadlock.rs:210` 只是假设性地把请求者放到队尾评估，不写回对象），受害者永远是调用者自己 |
| `exit_current_and_run_next()` 里的 `forget_task()` | 死掉的任务还留在图里会造出假环 | 图每次现场重建（`build_graph`），已退出任务的 `res` 已是 `None`，`self_key()` 返回 `None` 自然不是节点 |

第一版的三样东西都属于「维护一份和真实状态平行的副本」。副本要跟着 hand-off、队列服务、
线程中途死亡三处事件同步，漏任何一处，图就开始**凭空造环**。删掉副本之后，这三处都不再有可犯的错误。

## 2. 状态的三个来源（没有任何第四个）

`deadlock.rs` 模块头把这件事写成了契约，代码里也照它执行：

- **空闲单位**：来自对象本身。`Semaphore` 给 `count` 夹紧到 0 以上的部分（负的 `count` 不是余量，
  是欠队列的 IOU）；`MutexBlocking` 在未锁时给 `1`。
- **等待队列**：来自对象本身，**按前后顺序**读，因为下面规则 2 用到位置。
- **谁持有几个**：来自每个任务自己 TCB 里的 `grants: Vec<(ResKey, isize)>`，由原语在
  acquire/release/hand-off 三个时刻就地记账（`grant_me` / `release_me` / `transfer`）。

键是 `(pid, index)`：`index` 由 `sys_semaphore_create` / `sys_mutex_create` 分配，在地址空间生命周期内稳定，
所以不需要对象指针，也不需要资源注册表。

## 3. 归约规则本身

`would_deadlock()`（`os/src/sync/deadlock.rs:378`）只做一件事：建状态、问 `served()`。真正的内容在 `served()`
（`os/src/sync/deadlock.rs:191`）：

1. 未 park 的任务假定能跑完，先把它的**全部**持有单位还给对应对象（`:197-204`）。
2. 队列里位置 `p` 的等待者，在该对象**累计归还 ≥ p+1** 时才可完成（`:220`）。
   `+1` 是它自己那份，`p` 是它前面每个人各拿一份——这一条是「计数信号量」和「互斥锁」能共用一套代码的原因：
   mutex 就是 `p+1 == 1` 且池子只有 0/1 的特例。
3. 反复应用规则 2 直到不动点（`:212-229`）；每次有新任务进入可完成集合，它的持有再进池子。
4. 请求者被假设性地插到它所请求对象的**队尾**（`:210`），不动点里没有它 → 拒绝，返回 `-0xdead`。

规则 1 是 `ch8_deadlock_sem2` 能过的原因，也是 `sem1` 里主线程能放行子线程的原因：
sem1 的 `main` 握着 barrier 信号量的 3 个单位并 `sleep(500)`，它没有 park 在任何同步对象上，
于是这 3 个单位一开始就进池子，队列后面的任务才有可能被逐个归约掉。

## 4. 为什么「环」这个概念被丢掉了

教科书式的检查是 resource → holder → holder 的 blocker → … 找环。`sem2` 会把这套方法**证伪**：
两把叉子、四个哲学家、每把叉子的信号量创建时给了 **2** 个单位。`t1` 握着 fork2 的一个单位并请求 fork1，
`t3` 握着 fork1 的一个单位并请求 fork2，于是 `t1 → fork1 → t3 → fork2 → t1` 确实是个环。
但它不是死锁：fork1 的第二个单位在 `t2` 手里、fork2 的第二个单位在 `t4` 手里，而这两个人
`REQUEST` 都是 `None`，没有 park 在任何地方，跑完就会把单位还回来。

环只有在「这一步涉及的对象的**每一个**单位都被冻在环里」时才是致命的。
只跟随「谁持有它」而不问「还 loose 几个」的搜索无法区分这两种情况——`sem2` 要求输出
「未检测到死锁、所有子线程返回 0」，用找环的方法必然给出相反的答案。归约把「还 loose 几个」变成了显式的池子计数，
这一歧义就消失了。这也是我不去给找环加「权重/多单位」补丁的原因：补丁会把同一个问题拆到两处去修。

（`deadlock.rs` 模块头里这段的第一稿把 `t3` 的持有写成了 fork2、把 philosophers 数写成了两个，
本轮按 `ch8_deadlock_sem2.rs` 的 `ALLOC=[2,1,1,2]` / `REQUEST=[Some(1),None,Some(2),None]` 重读后改正；
诊断轮 `logs/c3_ch8_probe10_diag45.log` 里 pid=16 的 `held=` 字段就是这份对账依据。）

## 5. 拒绝即策略（为什么不做「挑一个已排队者牺牲」）

模块头把这点单独写成一节，我同意它值得写下来：当不动点排除请求者时，环里**没有**任何成员能前进，
环只能靠「有人离开」打开，而请求者是唯一一个还没进队列的参与者——它是代价最小、也是唯一不留下状态的出口。
去动已排队者则需要：从 FIFO 中间摘人、把它唤醒到一个已经离开了的系统调用里、并把「欠它的错误码」
存起来等它自己来取。那是第二套协议，`sem1` 不需要它：sem1 里真正闭合成环的请求是 `t3 → fork2`，
而它的发起者正是环内唯一还握着环所需单位之外的任务，拒绝它与拒绝 `t3` 效果相同，且不落状态。

顺带划清两处边界：条件变量不参与归约（它 park 的条件是「有人 signal」而不是「某个资源被授予」），
所以 `test_condvar` / `mpsc_sem` / `sync_sem` 三个用例完全不受本模块影响；
自旋互斥量不 park 任何人，既不是节点也不是边，`race_adder_mutex_spin` 同理。

## 6. 仍未收口的一处

`ENABLED` 是**全局** `static mut bool`（`deadlock.rs:99`），不是按地址空间存的。
`enable_deadlock_detect()` 只有三个 deadlock 用例调用，且全部传 `true`、没有一处传 `false`：
`ch8_usertest` 的 `TESTS` 数组里 mutex1/sem1/sem2 是第 14~16 项（对应 pid 14/15/16），
它们之后的 `mpsc_sem`、`phil_din_mutex`、`sync_sem`、`test_condvar`、`threads`、`threads_arg`
（pid 17~23）都是**在检测已开启的状态下**运行的——因为 23 个用例是先 `fork` 完再按 pid 顺序 `waitpid`，
时间上完全重叠。

本轮 25/25 的含义因此要说准：它证明的是「归约对这批用例恒不给出拒绝」，
而不是「开关对它们是关闭的」。前者是实测，后者从来不成立。
把它改成 `per-process` 开关是结构上的正解，我没有在交付版里顺手做，理由不是省事，而是那次改动会和
`sys_get_time` 的修复落进同一个 diff：两条因果链一旦交叉，下一轮的 PASS 数就无法再归因到某一个原因，
而这一轮的排查已经为「读数无法归因」付过两次代价（见 lab1 的 probe8 与 lab4 的三个假说）。
留作下一步独立变更。

