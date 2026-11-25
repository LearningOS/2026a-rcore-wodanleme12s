# Lab 2 · 用户内存的进出口：为什么是 UserStaging，以及一个下标怎么值 3 分

对应改动：`os/src/mm/page_table.rs`（`UserStaging`、`read_byte`/`write_byte`、`write_struct`）、
`os/src/syscall/fs.rs`（`sys_read`/`sys_write`）、`os/src/fs/inode.rs`（`File::write` 的扩块）。
判分证据：`logs/c3_ch6_fix3.log`（28/31）→ `logs/c3_ch6_fix4.log`（31/31）。

## 1. 为什么不能把用户指针直接交给 `File`

本章和前几章不一样的地方在于：`read`/`write` 的目标不再由内核自己解引用一次就完事，
它要穿过 `File` trait 落到 **virtio-blk 的 DMA 缓冲**、**管道环形缓冲**、**控制台输出**上。
而这几条路径的执行环境是内核页表：`os/src/trap/trap.S` 的 `__alltraps` 在保存完上下文后
显式做了

```
ld   t0, 34*8(sp)     # kernel_satp
csrw satp, t0
sfence.vma
jr   t1               # trap_handler
```

也就是说 syscall 期间 satp 是内核表，用户 VA 在块设备驱动眼里毫无意义。
我在 ch5 已经吃过同一件事的反向亏：当时把 `translated_str_opt` 接到「此刻 satp」上，
探针打出 `[probe] spawn: path ptr 31e4 unreadable`，15 条里只有 2 条能过——
`.rodata` 字符串全部翻译失败。所以本仓库自 ch4 起定下的口径是：

> 用户指针一律用**调用者 MemorySet 的页表**翻译，绝不用「当前 satp」。

`UserStaging` 是这条口径在 ch6 的延伸：既坚持用调用者页表，又不把用户页「暴露」给驱动，
而是**每命中一个用户页就分配一个内核帧**，把字节搬进来；`File` 只看到内核帧。

## 2. `UserStaging` 的契约

```rust
pub struct UserStaging {
    frames: Vec<FrameTracker>,          // 每个被触碰的用户页一个内核帧
    segments: Vec<(usize, usize)>,      // (user_va, 本段字节数)，按地址序
    writable: bool,
    token: usize,                       // 调用者页表
}
```

- 逐字节翻译（`PageTable::read_byte` / `write_byte`），检查 present、readable、U 位，
  写方向还要求 W，且 `ppn != 0`、物理地址落在 `[BASE_ADDRESS, MEMORY_END)`；
- 任何一段失败**整体放弃**（`default_unstaged()`），于是 `sys_read` 答 `-1`，
  不会像模板那样 unwrap 出 panic 把整轮 qemu 会话打死——判分器只有一个会话，
  一次 panic 会连带吃掉二十几条断言；
- `user_buffer()` 交出的是 `&mut arr[..len]`，`len` 是**请求内偏移**，
  即「第 i 个请求字节 = 帧的第 i 个字节」；
- `Drop::drop` 里 `flush()` 只在 `writable == true`（也就是 `sys_read` 方向）把帧内容
  按 `va_start + i` 写回用户页。

`sys_write`/`sys_read` 因此长这样：写方向 `stage_for_read` → `file.write(staging.user_buffer())`；
读方向 `stage_for_write` → `file.read(...)` → staging 在臂尾 drop 时回写。

## 3. 三条失败共用一个根因

`fix3` 轮 28/31，缺的正是 `file_test passed`、`Test file0 OK`、`Test link OK`。
日志里的断言文本已经把范围缩死了：

```
Panicked at src/bin/ch6_file0.rs:28, assertion `left == right` failed
  left:  "Hello, world!"
 right: "\0\0\0\0\0\0\0\0\0\0\0\0\0"
```

推理链：
1. `right` 是 `core::str::from_utf8(&buffer[..read_len])`，长度 13 ⇒ **`read_len == 13`**，
   说明文件的 `size` 是对的、`read_at` 也确实读出了 13 个字节；
2. 读出的 13 个字节全为 0 ⇒ 盘上数据是 0 ⇒ 问题在**写侧**，不在回写侧；
3. `ch6_file2` 里 `assert_eq!(stat.nlink, 2/4/3/1)` 都过了才撞到 line 32，
   说明 link/unlink/nlink 逻辑无关，剩下的只有「字节怎么进帧」。

第 3 点很关键：它排除了文件系统层，把我直接推到 `UserStaging::new` 的搬运代码。
`copy-in` 写的是 `frame[i + page_off]`，而 `user_buffer()`/`flush()` 读的都是 `frame[i]`。
`"Hello, world!"` 是应用 `.rodata` 里一个**不在页边界上**的字符串，于是前
`page_off` 个字节被写到帧的深处，驱动从帧首读到的自然是全 0（新帧）。
size 之所以正确，是因为扩块走的是另一条路（`OSInode::write` 里先
`inner.inode.resize_to(offset + slice.len())`，见下一节）。

修正是单点：copy-in 改成 `frame.ppn.get_bytes_array()[i] = b;`，
让「请求偏移」和「帧内偏移」在整个结构里只有一个定义。
`fix4` 轮 31/31，`rig3_c_gaps.py` 报 `需补齐=0`、`不该出现却出现了=0`。

## 4. 顺带定下的一条：新建文件必须先扩再写

easy-fs 的 `DiskInode::write_at` 会先把写长度夹到现有 `size`，
而 `create` 出来的 inode `size == 0`——照抄教程里 `open(CREATE|WRONLY)` + `write` 的顺序，
一个字节都存不进去。所以 `OSInode::write` 显式两步走：

```rust
inner.inode.resize_to(inner.offset + slice.len());
let write_size = inner.inode.write_at(inner.offset, *slice);
assert_eq!(write_size, slice.len());
```

`assert_eq!` 保留是有意的：这一章如果写少了，症状是「读回来全 0」这种最难查的形态
（上一节就栽过一次），宁可当场 panic 也不留到断言文本里让人猜。
