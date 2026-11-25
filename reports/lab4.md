# Lab 4 · 系统调用层：`*at` 约定的接线、`as_any` 的下行转换，以及判决集合之外的 pipe/dup

对应改动：`os/src/syscall/mod.rs`（分发与参数下标）、`os/src/syscall/fs.rs`
（`sys_open/close/read/write/fstat/linkat/unlinkat/dup/pipe`）、`os/src/fs/mod.rs`
（`File: Any` + `as_any`）、`os/src/fs/inode.rs`（`OSInode::stat`）、
`os/src/fs/pipe.rs`、`os/src/fs/stdio.rs`、`os/src/trap/mod.rs`（第 4 个参数槽）。
判分证据：`logs/c3_ch6_fix4.log`/`fix5.log` 的 `Test fstat OK`、`Test link OK`；
ABI 判据来自判分副本里的官方用户态库 `c_run3/ch6/ci-user/user/src/{lib.rs,syscall.rs}`。

## 1. 官方 ch6 的用户态库走的是 `*at` 约定，参数从 a1 起算

这是我读判分副本源码读出来的，不是推测。`user/src/lib.rs:159-186`：

```rust
const AT_FDCWD: isize = -100;
pub fn open(path: &str, flags: OpenFlags) -> isize {
    sys_openat(AT_FDCWD as usize, path, flags.bits, OpenFlags::RDWR.bits)
}
pub fn link(old_path: &str, new_path: &str) -> isize {
    sys_linkat(AT_FDCWD as usize, old_path, AT_FDCWD as usize, new_path, 0)
}
pub fn unlink(path: &str) -> isize { sys_unlinkat(AT_FDCWD as usize, path, 0) }
```

而 `user/src/syscall.rs:48-58` 的 `syscall()` 把 `args[0]` 放进 `x10`、`args[1]`→`x11`……
`x17` 放调用号。两个事实合起来决定了我内核侧的参数下标：

| 调用 | 寄存器里的实际布局 | 我的分发写法 |
|---|---|---|
| `openat` | a0=AT_FDCWD, **a1=path, a2=flags** | `sys_open(args[1], args[2])` |
| `linkat` | a0=dirfd, **a1=old**, a2=dirfd, **a3=new**, a4=flags | `sys_linkat(args[1], args[3])` |
| `unlinkat` | a0=dirfd, **a1=path** | `sys_unlinkat(args[1])` |
| `close/read/write/fstat/dup/pipe` | 没有 dirfd，参数从 **a0** 起 | `args[0]` 起算 |

也就是说同一个分发表里两种下标基准并存，而且**都对**——因为带 `dirfd` 的那三条我按 `*at`
取 a1/a3，不带的取 a0。本章我这棵树「忽略 dirfd、只做根目录平坦命名空间」，
所以 a0 直接丢掉，这是设计选择而不是遗漏，代价是绝对路径/子目录暂时不支持。

调用号逐个对过表，与判分副本一致：`unlinkat=35`、`linkat=37`、`dup=24`、`openat=56`、
`close=57`、`pipe=59`、`read=63`、`write=64`、`fstat=80`。

**顺着这张表我抓到一处真 bug 并修掉**：`sys_pipe` 的指针参数在 a0
（`sys_pipe(pipe)` → `syscall(SYSCALL_PIPE, [pipe.as_mut_ptr(), 0, 0])`），
而分发里写的是 `args[1]`——那是 user_lib 填的 0，于是内核会把 `(读端, 写端)` 往地址 0 上写，
`write_struct` 翻译失败 ⇒ `pipe()` 永远返回 -1，并且回滚掉刚分配的两个 fd。
它不影响本章判决（见第 5 节：ch6 的 31 条里没有任何 pipe 用例），所以我是在 ch7/ch8
之前修它，而不是因为被判分逼着修。

## 2. `fstat`：为什么用 `as_any` 而不是给 `File` 加一个 `fn stat()`

`ch6_file2` 要 `stat.dev`/`stat.ino`/`stat.nlink`，但 `fd_table` 里装的是 `Arc<dyn File>`，
`OSInode`、`Stdin`、`Stdout`、`PipeReader`、`PipeWriter` 都在里面。两种做法：

1. trait 上加 `fn stat(&self) -> Option<Stat>`，每个实现都得到写一遍；
2. trait 上界加 `Any`，只让 `OSInode` 提供 `as_any`，`sys_fstat` 用 `downcast_ref` 判定。

我选 2，理由是语义诚实：`dev/ino/nlink` 只对盘上 inode 成立。控制台和管道没有磁盘身份，
与其编一个假的 `ino`，不如答 `-1`——`sys_fstat` 里 `downcast_ref::<OSInode>()` 失败即 `-1`。
判分用例恰好只对真文件调用 `fstat`（`ch6_file1/2`），所以这个拒绝路径不会被踩到，
但它是我愿意保留的默认行为。

代价是 `as_any` 这一行要在 4 个实现里各写一遍，且 `File` 的上界变成
`Send + Sync + Any`。另一个连带约束：`write_struct<T: Copy>` 要求 `Stat: Copy`，
于是补 `#[derive(Debug, Clone, Copy)]`（Lab 1 错误表里的第 12 项，它单独吃掉一轮判分）。

`stat.ino` 的正确性依赖 Lab 3 的 `inode_id_of`：两个硬链接名必须报同一个号，
而 `DiskInode` 里没有指回自己槽位的字段，所以号是从 `(block_id, block_offset)` 反算的位置属性。

## 3. fd 语义：`dup` 共享对象、`pipe` 失败回滚、`close` 立即析构

- `sys_dup` 交出的是 `file.clone()`——同一个 `Arc`。这不是风格问题：
  测试用 `dup` 把 fd 1 指到管道写端再 `fork`+`exec`，只有共享同一个对象才共享同一个
  偏移量/环形缓冲；「造一个等价对象」会让两侧各自从头读写。
- `alloc_fd`（`task/task.rs:111`）是「找第一个空位，否则 push」，所以 fd 号不保证连续语义，
  但保证 `pipe` 的两个端点是两个不同槽位。
- `sys_pipe` 在 `write_struct` 失败时把两个槽位显式清回 `None`：一次失败的 `pipe()`
  不应该在 fd 表里留下两个永远占用的位——否则用户程序重试几次就把表撑长了。
- `sys_close` 用 `take()` 而不是 `Some(None)` 式的置空，让 `Arc` 的引用计数当场减一；
  `PipeWriter` 的 `Drop`（维护写端计数）依赖这次析构真的发生。

## 4. 入口一律不 panic

`sys_open` 的 flags 以前是 `OpenFlags::from_bits(flags).unwrap()`——未知位直接把内核打死。
判分只有一个 qemu 会话，一次 panic 连带吃掉二十几条断言（这正是 Lab 1 里 `2/31` 那种
假读数的放大器），所以改成 `None => return -1`。同理，字符串指针和缓冲区指针全部走
Lab 2 的统一门控，翻译失败答 `-1`。这里有个容易误判的点：`RDONLY` 的值是 0，
`from_bits(0)` 是**合法**的空位集合，别把它当非法 flags 拒掉——探针轮里
`open(fname3, bits 0)` 确实走到了文件系统层（返回 -1 是因为名字不在，不是 flags 被拒）。

## 5. 判决集合之外：pipe/dup 本章没有任何断言（不假装测过）

一轮 ch6 判分实际拉起 27 个用户程序（`Usertests: Running ...` 逐行可数），
31 条期望串里**没有任何 shell 或 pipe 断言**。所以 `pipe`/`dup` 的证据只有三条，
我按强弱如实排：

1. **ABI 对表**（第 1 节）——这也是我为什么发现 `args[1]` 是错的；
2. **编译**：`#![deny(warnings)]` 下零警告通过；
3. **代码走查**。

明确**没测**的部分也写在这：管道满时我不阻塞而是少写（`free_space() == 0` 直接 break），
空时 `read` 返回 0，于是 `PipeInner.writers` 这个计数只被减、从不被读——
「暂时空」和「写端全关（EOF）」在语义上没有区分开，POSIX 的另一半还欠着。
`Test pipe1`、usershell 属于 ch7/ch8 的集合，要等那两章才有真判决。

## 6. 复现与核对

```
bash rig3_c_run.sh ch6 <后缀>          # 官方 make test，日志 logs/c3_ch6_<后缀>.log
~/.local/bin/python3.12 rig3_c_gaps.py ch6 logs/c3_ch6_<后缀>.log
```

判绿的形状是四件事同时成立：`MAKE_RC=0`、`Test passed<token>: 31/31`、
四行 `Report for lab1..lab4 found.`、差集 `需补齐=0 / 不该出现却出现了=0`
（两条 `NOT_EXPECTED`——`FAIL: T.T`、`Test sbrk failed!`——必须继续不出现，
把它们「顺手修好」同样是作弊）。报告闸在 `check/ch6.py` 之后，所以只有满分那轮
才会暴露报告缺失，这点见 Lab 1 第 2 节。
