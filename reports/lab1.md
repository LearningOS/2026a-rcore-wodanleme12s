# Lab 1 · ch6 的第一件事不是写文件系统，是确认「读数是不是真的」

分支：`LearningOS/2026a-rcore-wodanleme12s` 的 `ch6`；本地判分副本 `oscamp3/c_run3/ch6`，
判分器 `checker=7d61ec55b58eed6ca7052917846c1b87af34563a`、`tests=a0593662ad55d670ba8c27ce1763347cd0dd552f`。
工具链 `rustc 1.80.0-nightly (c987ad527 2024-05-01)` + `QEMU 11.1.3`，官方 `timeout --foreground 30s` 未放宽。

## 1. 判分口径（直接读判分器源码，不是我推测的）

`check/ch6.py` 取

```
EXPECTED_6 = set(EXPECTED_5 + EXPECTED_6b + [file0, fstat, link, mass open/unlink]) - ["Test set_priority OK!"]
```

分母 **31**（29 条期望串 + 2 条 `NOT_EXPECTED`：`FAIL: T.T`、`Test sbrk failed!`）。
`check/base.py` 里是 `assert count == total`，也就是**部分通过等于整章不通过**；
而且期望串带着每轮随机数（本轮 `Test file0 OK33950!`），所以伪造输出这条路本身就不成立。

一轮里实际被 `ch6_usertest` 拉起来的程序共 27 个（`Usertests: Running ...` 逐行可查）：
ch2 的 power/hello、ch3 的 sleep/yield、ch4 的 mmap/unmap/sbrk、ch5 的 spawn/fork/exit、
ch6 的 file0/file1/file2/file3/filetest_simple。**没有任何 shell 或 pipe 断言**，
这条限制我写在 lab4 里，不假装测过。

## 2. 拿到的第一个读数是假的

接手时该分支已经有一轮别人留下的 1500+ 行改动，两次判分都显示 `2/31`。
`2/31` 配 `MAKE_RC=2` 是关键线索：日志里 os 根本没编过——

```
error: could not compile `os` (bin "os") due to 12 previous errors
make[1]: *** [kernel] Error 101
```

`make build` 失败后 `kernel.bin` 还是**上一轮遗留的旧镜像**，qemu 照跑不误，
于是「2/31」描述的是旧二进制，而不是当时的代码。教训很直接：
**`MAKE_RC != 0` 时，PASS 数没有解释价值**，我把它写进了 lab1 的第一段，以后每轮先看 `MAKE_RC`。

（顺带说明我的仪器局限：`oscamp3/rig3_c_check.sh` 这个「只编译」闸在本仓库不可用——
worktree 里 `user/build/elf/` 是空的，`os/build.rs` 的 `read_dir().unwrap()` 会 panic 并顺手把
未跟踪的生成文件 `os/src/link_app.S` 截成 0 字节。我给脚本加了显式 `ABORT` 守卫，
编译错误一律以官方判分轮的日志为准，不再信第二个仪器。）

**这条口径后来需要精细化，否则会把好读数误判成坏读数。** `MAKE_RC != 0` 有两个来源完全不同的原因：

1. `make build` 失败 ⇒ 跑的是遗留镜像 ⇒ **PASS 数无解释价值**（上面就是这种）；
2. 报告闸失败 ⇒ `Makefile` 的检查循环位于 `check/chN.py` **之后**，`for i in $(seq $(LAB))`
   缺一个文件就 `exit 1`。这时 qemu 会话已经完整跑完、判决已经打印，
   **PASS 数照样有效**，只是整轮 make 退非零。

区别办法不是看 `MAKE_RC`，而是看日志里有没有 `error: could not compile` 与
`Test passed<token>: N/31` 两行；两者共存（前者缺、后者在）就是第 2 种。
我做负控制时两轮都是第 2 种（当时 `reports/lab4.md` 还不存在），
如果只按粗口径把它们当无效读数，两个对照实验就白跑了。

## 3. 12 个编译错误的逐一收口

| # | 位置 | 症状 | 收口 |
|---|---|---|---|
| 1 | `syscall/fs.rs:15` | `no UserStaging in mm` | `mm/mod.rs` 的 `pub use page_table::{...}` 补 `UserStaging` |
| 2/3 | `virtio_blk.rs:3`、`mm/mod.rs:18` | `no kernel_token in mm/memory_set` | `memory_set.rs` 恢复 `pub fn kernel_token()`（`KERNEL_SPACE.exclusive_access().token()`）；进 trap 时 `trap.S` 已把 satp 切成内核表，所以这个 token 就是当下生效的那张表 |
| 4 | `syscall/fs.rs:13` | `link_in_root / unlink_in_root` 不存在 | `fs/inode.rs` 加两个对 `ROOT_INODE.link/unlink` 的薄封装并在 `fs/mod.rs` 导出 |
| 5 | `syscall/fs.rs:66` | `use of undeclared type Vec` | 补 `use alloc::vec::Vec;` |
| 6 | `virtio_blk.rs:70` | `PhysPageNum` 没有 `step()` | `address.rs` 给 `PhysPageNum` 补 `StepByOne`（原本只对 `VirtPageNum` 实现），顺带消掉 `unused import: StepByOne` |
| 7 | `syscall/fs.rs:156` | `&dyn File` 上没有 `downcast_ref` | `File` 加 `Any` 上界 + `fn as_any(&self) -> &dyn Any`，四个实现各自补一行（详见 lab4） |
| 8 | `task/task.rs:108` | `TaskStatus::Zombie` 不存在 | 删掉 `is_zombie()`——全仓库无人调用，留着就会撞 `#![deny(warnings)]` |
| 9 | `trap/mod.rs:73` | 参数数组只有 3 个槽，`syscall` 要 `[usize; 4]` | 恢复 `cx.x[13]`（`sys_mmap`/`sys_linkat` 确实用第 4 个参数） |
| 10/11 | `fs/pipe.rs:71`、`:85` | E0502 / E0596 | 环形缓冲的下标先落局部变量再写；`PipeState::read` 的参数改成 `mut buf` |
| 12 | `syscall/fs.rs:162`（下一轮才暴露） | `Stat: Copy` 不满足 `write_struct<T: Copy>` | `#[derive(Debug, Clone, Copy)]`，字段本来就全是 `Copy` |

外加 easy-fs 三处噪声警告（未用的 `BLOCK_SZ` 导入、`remove_dirent` 里多余的 `mut fs`），
一并清掉，因为它们和 `#![deny(warnings)]` 是同一类风险。

## 4. 修完编译后的真实读数

| 轮次 | 日志 | 读数 | 说明 |
|---|---|---|---|
| `fix1` | `logs/c3_ch6_fix1.log` | 2/31 | 12 编译错误，旧镜像在跑 |
| `fix2` | `logs/c3_ch6_fix2.log` | 2/31 | 只剩 1 个错误（`Stat: Copy`） |
| `fix3` | `logs/c3_ch6_fix3.log` | **28/31** | 编译通了；三条失败都在数据回读 |
| `fix4` | `logs/c3_ch6_fix4.log` | **31/31** | 一个下标偏移修正（见 lab2） |

差集工具 `rig3_c_gaps.py` 在 `fix4` 报 `需补齐=0`、`不该出现却出现了=0`，
即 31 条全绿且不包含 2 条不该出现的串。提交：`da6cdcd`（编译收口）、`9614089`（数据通路）。
