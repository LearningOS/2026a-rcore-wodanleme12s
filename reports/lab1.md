# Lab 1 · ch3 的三个 syscall 在 ch5 分支上重写：谁是合法的内存

这份报告记录我在 **ch5 分支**（`LearningOS/2026a-rcore-wodanleme12s` 的 `ch5`）重新落地导学章 ch3
那三个 syscall 的过程。之所以要在 ch5 重做，是因为上游每章的骨架都会重排文件（ch5 把
`task/mod.rs` 拆成 `task/{mod,manager,processor,task,id}.rs`），把 ch4 分支 merge 进来只会大面积冲突，
所以只能把**设计**在这一章自己的目录结构里重新写一遍。

## 1. 这一章的判分口径（不是我猜的，是判分器源码里的集合）

学生侧 `passwd` 为空 ⇒ `BASE=2`、`BASE_CHAR` 为空 ⇒ 实际执行 `check/ch5.py`，它取
`EXPECTED_5 = EXPECTED_4 + [spawn0 / wait / waitpid / set_priority]`，并且显式减掉了
`string from task trace test`、`Test trace OK!`、`Test trace_1 OK!` 三条。分母 15。

⇒ 结论：**ch5 不考 trace 那三条判决串**，但 `ch3_sleep`/`ch3_sleep1`/`ch4_mmap*` 全部在分母里，
因为它们是被 `ch5_usertest` 用 `spawn` 拉起来跑的。也就是说 ch5 的 15 条里有 11 条挂在
「子进程能不能正常起、能不能打印」这一件事上。这一点后来直接决定了我先修哪个 bug。

## 2. 用户指针：一次差点写错的取舍

`sys_get_time(ts, tz)` 要把 `TimeVal` 写回用户**栈上的局部变量**，`sys_write` 要把用户
`.rodata` 里的字符串搬出来。两条路最省事的写法是「算个区间白名单」：应用槽 + 用户栈各记一段，
落在里面就放行。我没有这么做，而是让**当前任务的页表**当唯一权威：

```
MemorySet::translate_user_byte(va, writable) -> Option<*mut u8>
    ├─ page_table.translate(va.floor())     取 PTE
    ├─ present && readable && U 位          （写还要 W 位）
    ├─ ppn != 0                             （空槽的 ppn 读出来是 0，不能误当成物理页 0）
    └─ pa ∈ [BASE_ADDRESS, MEMORY_END)      （内核线性映射的就是整段 RAM，越界的一律不碰）
```

这样栈变量和 `.rodata` 常量天然都覆盖，不需要维护两个特例。代价是每次翻译要查 SV39 三级表，
但判分器给的 30 秒墙钟里根本看不出来（整轮实测在 10 秒量级）。

## 3. 实测踩到的第一个坑：拿 satp 当用户地址空间

在 ch5 工作区第一次拿读数（`bash rig3_c_run.sh ch5 takeov0`）是 **2/15**，和模板态一模一样——
我改的分页、VMA、mmap 家族一条都没亮。日志里每一行都是

```
Usertests: Test ch4_mmap0 in Process -1 exited with code 0
```

`pid = -1` 说明 `spawn()` 全部失败，而 `assert_eq!(pid, wait_pid)` 因为 `-1 == -1` 恰好没炸，
于是 18 个子应用一个都没跑起来，只是「安静地失败」。我在 `sys_spawn` 的两个失败分支各加一行临时
`println!`，跑 `rig3_c_run.sh ch5 probe1`，探针一次就命中：

```
[probe] spawn: path ptr 3310 unreadable
[probe] spawn: path ptr 31e4 unreadable
...（18 条，地址全在 0x31xx~0x33xx）
```

`0x31e4` 是 `ch5_usertest` 里 `TESTS` 那张字符串表的位置，也就是**应用镜像自己的 `.rodata`**。
它可读性毫无疑问——同一个应用的 `println!("Usertests: Running {}", test)` 打得出来，因为
`println!` 是先把格式化结果写到**栈**上再 `sys_write`，栈上的缓冲走的是「当前任务 MemorySet」这条路；
而 `translated_str_opt` 走的是另一条：

```rust
let token = riscv::register::satp::read().bits();   // ← 错
PageTable::from_token(token).read_byte(va)
```

 trap 期间 `satp` 里装的是**内核那张表**，用户镜像的 rodata 在内核表里根本没有映射，于是全部翻译失败。
这个函数原本是写给「内核态也可能调 syscall」的场景，注释里声称 rCore 的测试台会传内核 `&'static str`；
我去核对启动路径（`main.rs` 用的是 `task::add_initproc()`，全仓库没有任何内核态调用 `sys_spawn`/`sys_exec`
的地方），确认那句话是凭空假设，没有任何调用者满足它。

修法是把这条「看 satp 脸色」的后端整个删掉，字符串读取退回当前任务的页表：

```
translated_str_opt(ptr) = current_task()?.inner_exclusive_access().memory_set.read_user_str(ptr)
MemorySet::read_user_str = 逐字节 read_user_byte，撞上映射空洞返回 None（而不是解引用越界）
```

撤掉探针、修掉一个借用生命周期错误（`RefMut` 掉在块尾）之后跑 `rig3_c_run.sh ch5 gate2`：**11/15**。
一条指针闸门点亮了时钟族和 mmap 族共 9 条，和第一节从判分器源码推出来的结论一致。

## 4. 教训

- 「本地能跑」不算数，`cargo build` 过了也不算数：这一轮的 `takeov0` 就是编译干净、读数零变化。
  判分器一轮 2~4 分钟，比任何自建快速通道都便宜，先拿读数再改码。
- 注释里的假设要去源码里对账。那句「内核也会传 `&'static str`」要是真的，我就得保留 satp 分支；
  核对 `main.rs` 之后发现它不存在，才敢把整条后路删干净。
- 差集尺子（`rig3_c_gaps.py`）比总分有用：`不该出现却出现了=0` 这一项证明我没有为了点亮用例
  把 `FAIL: T.T`、`Should cause error, Test 04_2/04_3 fail!` 这些本该继续失败的串「顺手修好」。
