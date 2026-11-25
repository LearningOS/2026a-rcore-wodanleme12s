# lab1（对应 ch3：多道程序与系统调用）实验报告

- 身份：GitHub `wodanleme12s`，仓库 `LearningOS/2026a-rcore-wodanleme12s`，分支 `ch3`
- 起点：`4df96dc Initialize ch3`（模板态）
- 判分口径：官方 checker（`ci-user/check/ch3.py`，分母 7），仪器 `oscamp3/rig3_c_run.sh ch3 <后缀>`

## 1. 结论先给

模板态基线是 **5/7**，两条 MISS 是 `string from task trace test` 与 `Test trace OK<N>!`，
病灶就是 `os/src/syscall/process.rs` 里直接返回 `-1` 的 `sys_trace` 桩。本轮改动后，
报告入库之后的最终两轮判分（连跑，非单次离群读数）：

```
$ bash rig3_c_run.sh ch3 final1
Test passed34834: 7/7
MAKE_RC=0

$ bash rig3_c_run.sh ch3 final2
Test passed21698: 7/7
MAKE_RC=0
```

两轮都带 `Report for lab1 found.`，`MAKE_RC=0`；逐条清单
（`rig3_c_gaps.py ch3 logs/c3_ch3_final2.log`）给出 `需补齐=0`、`不该出现却出现了=0`。

日志绝对路径：

- `/Users/eieieiei/Documents/Qoder/2026-11-20/91507e43/oscamp3/logs/c3_ch3_final1.log`（`7/7` + `MAKE_RC=0`）
- `/Users/eieieiei/Documents/Qoder/2026-11-20/91507e43/oscamp3/logs/c3_ch3_final2.log`（同上，第二轮）
- `/Users/eieieiei/Documents/Qoder/2026-11-20/91507e43/oscamp3/logs/c3_ch3_wip4.log`（代码侧第一次 7/7，当时报告还没写，故 `MAKE_RC=2`）
- `/Users/eieieiei/Documents/Qoder/2026-11-20/91507e43/oscamp3/logs/c3_ch3_probe2.log`、`c3_ch3_probe3.log`（负路径探针，见第 5 节）
- `/Users/eieieiei/Documents/Qoder/2026-11-20/91507e43/oscamp3/logs/c3_ch3_hand0.log`（模板态 5/7 基线）

中间过程的几轮也如实留在日志目录里：`c3_ch3_wip1/wip2/wip3` 是编译失败（`0/7`），
`c3_ch3_clean1..clean3` 是清理阶段的复测。内核最终编译零 error / 零 warning
（`#![deny(warnings)]` 在 `os/src/main.rs` 里是整章归零的闸门）。

## 2. `sys_trace` 的契约与实现

契约是从 pinned 测试程序原文核对的，不是猜的：
`oscamp/checker/rCore-Tutorial-Test/src/bin/ch3_trace.rs`、`src/lib.rs`（`TraceRequest`/`trace_read`/`trace_write`/`count_syscall`/`sleep`）、
`src/syscall.rs`（`SYSCALL_TRACE = 410`，参数 `[request, id, data]`）。

| request | 语义 | 我的实现 |
|---|---|---|
| `Read = 0` | 把 `id` 当用户虚地址读 1 字节；不可读 ⇒ `-1`（`trace_read` 视作 `None`） | `read_u8(id)`，走用户指针闸门 |
| `Write = 1` | 向该地址写 1 字节 `data`，成功 `0`，不可写 ⇒ `-1` | `write_u8(id, data as u8)`，同一闸门 |
| `Syscall = 2` | 返回**当前任务**对 `id` 号 syscall 到此刻为止的调用次数 | 每任务计数器 `current_syscall_count(id)` |

关键一条：**计数发生在派发之前**。`os/src/trap/mod.rs` 的 `UserEnvCall` 分支里，
在 `syscall(cx.x[17], ...)` 之前先 `record_syscall(cx.x[17])`。测试的两处断言完全依赖这条规则：

- `assert_eq!(2, count_syscall(SYSCALL_TRACE))` —— 查 GETTIMEOFDAY 那次 + 这一次自身；
- 第二段之后 `assert_eq!(7, count_syscall(SYSCALL_TRACE))`。

如果计数放在 `sys_trace` 内部，这两条都会差 1，测试直接 panic。
同理 `user_lib::sleep()` 是 `get_time()`+`yield_()` 忙等，会同时抬 GETTIMEOFDAY 与 YIELD，
所以 `assert!(3 <= ...)` / `assert!(0 < count_syscall(SYSCALL_YIELD))` 才成立；我没有动 timer 行为。

计数器放在任务控制块里（`os/src/task/accounting.rs` 的 `SyscallCounter`，定长 512 槽，按 syscall 号索引），
而不是全局表：随任务消亡而消失，也无法通过别的任务的窗口伪造。
未跟踪的 syscall 号返回 `-1` 而不是 panic —— syscall 号是用户输入。

## 3. 用户指针闸门的设计

取向写在 BRIEF3 第 4 节第 1 条：syscall 侧只见一个抽象，不把区间比较散写到每个 syscall 里。
落在 `os/src/mm/user_ptr.rs`（新建 `os/src/mm/` 模块，为 ch4 的分页后端留好位置）：

- `UserRequest { addr, len, writable }`：一次访问意图；
- `UserAccess` trait：后端唯一需要实现的方法 `resolve(req) -> Option<*mut u8>`；
- `UserPtr<T>`：构造即校验（`new` / `new_mut` / `with_request`），`kern()` 是全内核唯一把用户地址变成可解引用指针的地方；
- `copy_to_user` / `copy_from_user` / `read_u8` / `write_u8`：批量与单字节的两个入口，都只做「先闸门、后搬运」。

ch3 无分页，后端是 `FlatWindowAccess`：应用槽和用户栈在**加载时**就登记进窗口表
（`os/src/loader.rs` 里 `register_app_slot(APP_BASE_ADDRESS, num_app * APP_SIZE_LIMIT)`、
每个 app 建初始上下文时 `register_user_stack(app_id, base, size)`）。
布局知识只留在 `loader.rs`，syscall 一个字都不提 `0x80400000` 这类常量。

三条刻意做的设计决定：

1. **窗口是 per-task 的**，不是全局白名单。`register_user_stack` 以 app 槽位号为键，
   `current_user_access()` 只取当前任务那份，因此 `trace` 不能用来窥探别的任务的栈。
   （BRIEF3 第 4 节点名要避开的正是「静态区间白名单」那条路。）
2. **ch4 换后端时 syscall 代码不动**：把 `current_user_access()` 的函数体换成
   「当前任务页表的视图」、`resolve` 换成逐页翻译并要求 `present && U` 加对应 R/W/X 位即可，
   `UserPtr`/`copy_*` 的签名和所有调用点保持不变。这也是我把 `mm/` 单独建出来的原因。
   接口层面已经能对上 ch4 的期望表（局部变量可读可写、`isize::MAX` 与 `0x80200000` 拒绝、
   `mmap(PROT_READ)` 后读成功而写必须 `-1`、`munmap` 后两者都失败）——权限位判定放在后端，
   请求里带的 `writable` 标志就是为了这个。
3. **对齐一律拒绝而不是猜**：RV64 上编译器对引用和 `ptr::read` 假定期望对齐，
   未对齐的用户指针若放过去就是 trap 或未定义行为，所以 `well_formed` 直接把
   null、回绕、非整元素长度、未对齐四类请求判死；字节级操作没有对齐要求，故 `read_u8`/`write_u8` 走 `read_volatile`/`write_volatile`，
   批量拷贝按字节搬，用户缓冲区因而不必满足 `T` 的对齐。

顺带把两处裸指针访问收进闸门：

- `sys_get_time`：原来无条件 `*ts = TimeVal{..}`。现在一次 `copy_to_user(ts, from_ref(&timeval))`，
  坏地址返回 `-1`，不再可能在内核里 fault 或改写调用者不拥有的内存。
- `sys_write`：原来 `from_raw_parts(buf, len).unwrap()` 直接打印，`len` 也来自用户。
  现在限长 4 KiB、一次性经闸门拷进内核暂存区再打印，非法地址整体返回 `-1`（要么全有效要么全不打印，
  不会输出半截）；UTF-8 解码失败也不再 `unwrap()` panic，而是不打印。
  不支持的 `fd` 从 `panic!` 改为返回 `-1`：用户传错 fd 不该打死内核。

## 4. 改了哪些文件，各对应哪条 EXPECTED

| 文件（相对 worktree 根） | 改动 | 支撑的 EXPECTED |
|---|---|---|
| `os/src/syscall/process.rs` | 实现 `sys_trace` 三个分支；`sys_get_time` 走闸门；`TimeVal` 补 `Copy` | `string from task trace test`、`Test trace OK<N>!`（主因）；`get_time OK<N>! (\d+)`、`current time_msec = (\d+)`、`time_msec = (\d+) after sleeping (\d+) ticks, delta = (\d+)ms!`（`get_time` 仍须对栈上局部变量返回 0，否则这三条全塌） |
| `os/src/mm/user_ptr.rs`（新增） | 用户指针闸门 + ch3 平坦窗口后端 | 同上两条 trace 断言的地址合法性判定；`Test sleep*` 三条依赖它放行 `&mut TimeVal` |
| `os/src/mm/mod.rs`（新增） | 模块出口，为 ch4 分页后端留位 | 结构性，无直接对应 |
| `os/src/loader.rs` | 加载时登记应用槽与各任务用户栈 | `string from task trace test` / `Test trace OK<N>!`（提供窗口来源） |
| `os/src/task/accounting.rs`（新增） | `SyscallCounter`（定长 512，未跟踪号返回 None） | `Test trace OK<N>!`（`count_syscall` 全部断言） |
| `os/src/task/task.rs` | TCB 增加 `syscall_counter`，去掉 `Copy`（改 `Clone`） | `Test trace OK<N>!` |
| `os/src/task/mod.rs` | `array::from_fn` 初始化 TCB 表；`record_syscall` / `current_syscall_count` | `Test trace OK<N>!` |
| `os/src/trap/mod.rs` | 派发前 `record_syscall(cx.x[17])` | `Test trace OK<N>!`（`assert_eq!(2/7, count_syscall(SYSCALL_TRACE))` 的关键） |
| `os/src/syscall/fs.rs` | `sys_write` 走闸门、限长、去掉两处 panic | `string from task trace test`（`println!` 路径本身）；不改 ch2b 的 bad_* 行为 |
| `os/src/main.rs` | 注册 `pub mod mm;` | 编译通过（`#![deny(warnings)]` 闸门） |

ch2 的三条（`bad_address` / `bad_register` / `bad_instructions`）没有退化：
日志里仍是 `[kernel] PageFault in application, bad addr = 0x0, ... kernel killed it.` 与两行
`IllegalInstruction ... kernel killed it.`，且 `FAIL: T.T` 一类串没有出现在输出中。
我没有重构 trap 的既有杀任务逻辑，只在 `UserEnvCall` 分支加了一行计数。

## 5. 怎么验证的

1. **官方判分器**（唯一算数的口径）：`bash rig3_c_run.sh ch3 <后缀>`，只认
   `Test passed<N>: 7/7` + `MAKE_RC`。逐条清单用 `~/.local/bin/python3.12 rig3_c_gaps.py ch3`，
   最新一轮给出 `需补齐=0`、`不该出现却出现了=0`。
2. **随机盐对齐**：checker 的 `randomize` 会把测试里的 `OK`/`passed` 改成 `OK<RAND>`/`passed<RAND>`，
   我一开始误按上游文档去找 `Test trace OK!` 而对不上；确认判分期望串以
   `check/ch3.py` 为准（`string from task trace test`、`Test trace OK<N>!`），并据此核对输出。
3. **负路径探针（临时补丁，已删除）**：光看 7/7 不足以说明闸门真的在拦东西，
   所以我另跑两轮，在 `sys_trace` 里插一段只在 Read 分支执行的自检并打印一行 verdict：

   ```
   PROBE kread_bad=true top_bad=true wrap_bad=true kwrite_bad=true misaligned_bad=true
   ```

   五项分别是：读内核物理地址 `0x80200000`、读 `isize::MAX`、读会回绕的长度、
   写 `0x80200000`、以及把用户栈地址偏移 1 后当 `usize` 读（未对齐）。
   两轮（`logs/c3_ch3_probe2.log`、`logs/c3_ch3_probe3.log`）都是全 true，
   且**同轮仍然 7/7**（`Test passed57236: 7/7`、`Test passed9696: 7/7`）——
   说明拦截生效的同时没有误伤合法访问。第一版探针我还试过更粗暴的做法（直接拒掉某类地址并打印标记），
   结果分数掉到 6/7，正好暴露出「`id` 参数在 Syscall 查询里不是地址」这件事，于是改成现在的形态。
   探针代码已从工作树移除，最终提交里没有 `PROBE` 字样。
4. **跨任务隔离**：由结构与日志共同支持 —— 窗口表按 app 槽位号登记、
   `current_user_access()` 只取当前任务的槽位；实测里 13 个 app 并发轮转，
   `ch3_trace` 的 `assert_eq!(Some(111), trace_read(&var))` 与写回断言都成立，
   没有出现某个任务读到另一任务栈的情况。**这一条我没有单独写压力测试去主动跨槽读取**，
   属于设计层面的保证，不做超出证据的声明。
5. **回归意识**：中途一次重构（把 `sys_get_time` 改成 `copy_to_user` 但漏了给 `TimeVal` 加 `Copy`）
   导致编译失败，判分器如实给 `Test passed33797: 0/7`（`logs/c3_ch3_clean2.log`）。
   这轮日志我保留了，作为「分数来自真跑、不是缓存读数」的反证。

## 6. 待办与不足

- `sys_trace` 的 Read/Write 目前只覆盖 1 字节，够用但不成体系；ch4 需要按页表 R/W 位判权限，
  届时 `FlatWindowAccess` 会被页表后端替换（syscall 侧不动）。
- ch3 无法区分同一任务内「可读窗口」与「可执行窗口」，`trace_read(main as *const _)` 是靠应用槽整体可读通过的。
- `SyscallCounter` 用定长数组，512 之外的 syscall 号统一报「未跟踪」；ch5 之后若要按任务动态分配再改。
- 每轮改动后都重跑判分器而不是只看 `cargo build`；最终满分口径见第 1 节的 `final1`/`final2`。
