# Lab 3 · 三个 deadlock 用例各自在测什么：一份逐案读数的对账

用例源码（判分副本内，未修改）：`ci-user/user/src/bin/ch8_deadlock_mutex1.rs`、
`.../ch8_deadlock_sem1.rs`、`.../ch8_deadlock_sem2.rs`。
驱动：`ch8_usertest` 的 `TESTS` 数组第 14/15/16 项，即 pid 14/15/16。
逐案读数取自诊断轮 `logs/c3_ch8_probe10_diag45.log`（该轮带 `[dl]` 判定探针，见 lab1 §3 关于诊断态的限定；
判决本身以官方轮 `logs/c3_ch8_final.log` 的 25/25 为准）。

## 1. mutex1：一次自锁，测的是「拒绝不能等到排队之后」

```rust
let mid = mutex_blocking_create() as usize;
assert_eq!(mutex_lock(mid), 0);
assert_eq!(mutex_lock(mid), -0xdead);   // ← 第二次必须拿到 -0xdead
mutex_unlock(mid);
```

第二次 `mutex_lock` 时，锁的唯一个单位在**调用者自己**手里，队列里也没有别人。一个只会「排队，等人醒我」
的实现（`wait_queue.push_back(self)` 然后 park）会永久阻塞，永远不会返回 `-0xdead`；一个「先排队再检测」的
实现则要把自己从队列里摘出来才能补救。所以这个用例真正测的是**拒绝发生在入队之前**。

它同时是归约规则 1 的一处反作用力测试：规则 1 说「未 park 的任务假定能跑完并归还持有」，而调用者此刻
恰恰是未 park 的——如果照字面执行，它会把自己的单位还给自己，于是判定为可完成，用例失败。
`served()` 的初始化循环因此带一个 `task != me` 的排除（`os/src/sync/deadlock.rs:198`）：

```rust
for task in graph.tasks() {
    if task != me && graph.parked_on(task).is_none() {
        done.push(task);
    }
}
```

诊断轮里该用例只有一次判定，结论为拒绝：

```
0.25 [dl] pid=14 me=0 ask=-1 free=[(-1, 0)] blocked=[] held=[(0, [(-1, 1)])] queues=[] -> REFUSE
```

`ask=-1` 是探针给 mutex 编的号（`-(index+1)`），`held=[(0, [(-1, 1)])]` 表示 0 号任务握着这把锁，
`free=0`、`queues=[]`：没有任何外部来源能归还这唯一单位。整个 ch8 轮里 pid=14 只出现这一次判定，
没有第二次——拒绝之后测试自己 `unlock` 了。

## 2. sem1：真死锁，测的是「至少一个人必须被拒」

参数：`THREAD_N=3`、`RES_NUM=[1,2,1]` → sem1=S1(1 单位)、sem2=S2(2 单位)、sem3=S3(1 单位)；
barrier 是 S0，创建时 3 个单位。分配与请求：

| 线程 | 持有（`sem_alloc`） | 再请求（`REQUEST`） |
|---|---|---|
| t1 | S2 | S1 |
| t2 | S1、S2 | S3 |
| t3 | S3 | S2 |

三个单位级的观察，决定了这是真死锁而不是 sem2 那种假环：

- S1 只有 1 个单位，被 t2 握着 → t1 的等待没有别的来源。
- S2 有 2 个单位，恰好被 t1、t2 分完 → t3 的等待也没有别的来源。
- S3 只有 1 个单位，被 t3 握着 → t2 的等待同样封闭。

也就是说每个对象的**全部**单位都冻在环内，环里无人能前进。测试要求的是
`assert!(failed > 0)`——三个线程里至少有一个 `exit(-1)`，也就是至少有一次判定给出 REFUSE。

barrier 这一路是 sem1 里最容易误伤的地方：`main` 先 `down(S0)` 三次把 3 个单位全拿走，
`sleep(500)` 之后才 `up(S0)` 三次放行子线程。子线程在 `sem_alloc` 末尾也要 `down(S0)`，
即三个线程排在 S0 的队列里、而 S0 的 3 个单位全在一个**没有 park** 的 `main` 手里。
如果检测器按「谁握着」直接判，S0 上排队的人全不可完成，第一个被拒的就会是 barrier 而不是资源。
归约的规则 1 正是为此存在：`main` 未 park → 它那 3 个单位立刻进池子 → S0 队列按位置 p 逐个满足（p+1 ≤ 3）。

诊断轮读数（pid=15 共 6 次判定，其中 1 次拒绝）：

```
1.07 [dl] pid=15 me=2 ask=3 free=[(0, 0), (1, 0), (2, 0), (3, 0)]
     blocked=[(1, 1), (3, 2)]
     held=[(1, [(2,1),(0,1)]), (2, [(1,1),(2,1),(0,1)]), (3, [(3,1),(0,1)])]
     queues=[(1, [1]), (2, [3])] -> REFUSE
1.17 Deadlock detected. Test 08_sem1 failed!
1.17 thread 1 exited
1.25 thread 3 exited
1.25 deadlock test semaphore 1 OK18192!
```

被拒的是 `me=2`（t2）请求 S3 那一次：`free` 四个对象全为 0，S3 的单位在 t3 手里，t3 排在 S2 队列第 3 位
（`queues=[(2,[3])]` 与 `blocked=[(3,2)]`），而 S2 的两个单位已被 t1、t2 拿走——t2 拿的那一份正是环需要的。
被拒线程的 `try_sem_down` 会先 `sem_dealloc(gettid())`（归还它的 S1、S2 并 `up` barrier）再 `exit(-1)`，
所以它不会把 barrier 卡在半途；剩下的 t1、t3 分别在 1.17、1.25 正常退出。
`failed == 1`，断言成立，期望串出现。

## 3. sem2：有环但没有死锁，测的是「不许把环当成结论」

参数：`RES_NUM=[2,2]` → S1、S2 各 2 单位；`ALLOC=[2,1,1,2]`、`REQUEST=[Some(1),None,Some(2),None]`：

| 线程 | 持有（`ALLOC`） | 再请求（`REQUEST`） |
|---|---|---|
| t1 | S2 | S1 |
| t2 | S1 | — |
| t3 | S1 | S2 |
| t4 | S2 | — |

于是 S1 的两个单位在 t2、t3 手里，S2 的两个单位在 t1、t4 手里。
`t1 → S1 → t3 → S2 → t1`：把「谁握着」画成图，这里确实存在一条闭合回路（t1 握着 S2 请求 S1，
t3 握着 S1 请求 S2）。找环的实现必然在这里误报，而用例要求的输出恰恰是**没有**检测到死锁：
`assert_eq!(waittid(tid), 0)` 对四个线程全部成立，没有任何线程走 `exit(-1)` 分支。

判定的分水岭是「环上每个对象的单位都冻在环里吗」。这里两个对象各有一份单位握在 t2、t4 手里，
而它们**不再请求任何东西**（`REQUEST` 为 `None`，跑完就 `up` 归还）。归约的规则 1 直接给出答案：
t2、t4 未 park → 它们的 S1、S2 单位先回池子 → `pool(S1) = 1`、`pool(S2) = 1`，
恰好满足队尾请求者的 `p + 1 == 1`。t1、t3 都进入可完成集合，判定 `ok`。

诊断轮里 pid=16 共 6 次判定、**0 次拒绝**，例如：

```
0.34 [dl] pid=16 me=1 ask=0 free=[(0, 0), (1, 2), (2, 1)] blocked=[] held=[(0,[(0,4)]), (1,[(2,1)])] queues=[] -> ok
0.35 [dl] pid=16 me=3 ask=0 free=[(0, 0), (1, 0), (2, 1)] blocked=[(1,0),(2,0)] held=[(0,[(0,4)]), ...] queues=[(0,[1,2])] -> ok
0.35 [dl] pid=16 me=4 ask=0 free=[(0, 0), (1, 0), (2, 0)] blocked=[(1,0),(2,0),(3,0)] held=[(0,[(0,4)]), ...] queues=[(0,[1,2,3])] -> ok
1.64 deadlock test semaphore 2 OK18192!
```

`held=[(0,[(0,4)])]` 是 main 握着 barrier 的 4 个单位（`semaphore_create(THREAD_N)`），同样是规则 1 把它放行；
四次资源请求全部 `ok`，四个线程正常退出。

## 4. 三个用例合起来约束了什么

- mutex1 不许「排队之后再检测」；
- sem1 不许「谁都不拒」（它要求至少一次拒绝，并且拒绝必须发生在 `down` 的返回值里而不是内核 panic）；
- sem2 不许「看到环就拒」。

这三条同时成立，才排除了「一律拒绝计数信号量的第二次 down」「一律拒绝成环请求」这两类能骗过单个用例的实现。
我把它们当作 ch8 的正/负对照来读，而不是当作三个要逐个哄过去的字符串——lab1 §4 提到的 substring 误读，
在这三条上最容易发生：`Deadlock detected. Test 08_sem1 failed!` 这行**出现**才是对的（它是 sem1 的内部打印），
而 `Test 08_sem1 failed` 这个字样在判决集合里根本不存在。
