# Lab 3 · 硬链接、空洞目录项与「删了还能再建」——easy-fs 这一层改了什么

对应改动：`easy-fs/src/layout.rs`（`nlink` 字段与三个访问器）、
`easy-fs/src/efs.rs`（`inode_id_of`、`dealloc_inode`）、
`easy-fs/src/vfs.rs`（`add_dirent`/`remove_dirent`/`destroy`/`link`/`unlink`/`resize_to`）、
`os/src/fs/inode.rs`（`link_in_root`/`unlink_in_root`、`OSInode::stat`）。
判分证据：`logs/c3_ch6_fix4.log`、`logs/c3_ch6_fix5.log` 里 `Test link OK` 与
`Test mass open/unlink OK` 两条；`Test fstat OK` 依赖本文件的 `inode_id_of`。
本文件还记录三组对照实验：`logs/c3_ch6_negctl1.log`、`logs/c3_ch6_negctl2.log`（负控制）
与 `logs/c3_ch6_negctl6.log`（机制探针）——第 5、6 节的结论完全来自它们，不是从代码推出来的。

## 1. 动磁盘结构之前先想清楚代价

教程给的 `DiskInode` 是

```
size, direct[30], indirect1, indirect2, type_
```

没有硬链接计数，所以 `link` 必须自己找地方存 `nlink`。我选择**加字段**而不是
「数目录里有几个 dirent 指向它」，理由是可验证性：后者要在 `fstat` 每次调用时扫根目录，
而根目录里既有 fuse 打进去的二十几个应用，也会在本轮判分里累积新名字，
扫描结果还会被下面第 4 节的空洞 dirent 污染。

代价是 `DiskInode` 变大 ⇒ 每个块能装的 inode 数变小 ⇒ inode 区布局整体平移。
这个代价是可接受的，因为**读写两头用的是同一把尺子**：`os/Makefile:54` 的 `fs-img`
目标在每轮 `make build` 里跑 `easy-fs-fuse`，而 fuse 和内核链接的是同一个 easy-fs crate，
`get_disk_inode_pos` 与 `inode_id_of` 都用 `size_of::<DiskInode>()` 反推。
判分日志里 `Compiling easy-fs-fuse` 出现 3 次，就是这条链子在这一轮确实重跑了。

```rust
// layout.rs
pub fn nlink(&self) -> u32 { self.nlink.max(1) }
pub fn inc_nlink(&mut self) { self.nlink = self.nlink() + 1; }
pub fn dec_nlink(&mut self) -> u32 { self.nlink = self.nlink().saturating_sub(1); self.nlink }
```

`max(1)` 是防御，不是当前判分路径的必需品，这点要说清楚：
`initialize` 已经把 `nlink` 置 1，而 `fs.img` 每轮由同一份代码重新打包
（`EasyFileSystem::create` 走根 inode 的 `initialize`），所以正常路径不会读到 0。
留着它是因为 0 和「一个引用」语义不同：万一是没被写过这个字段的旧镜像，
报 `nlink == 0` 会让 `ch6_file1` 第一条断言（期望 1）以一个「文件明明存在」的样子失败，
排查方向会被带到目录项上。

## 2. `stat.ino` 是位置属性，不是内容属性

`ch6_file2` 要求两个硬链接名的 `dev`/`ino` 相等。`Inode` 结构里只有
`(block_id, block_offset)`——盘上的 `DiskInode` 并没有指回自己槽位的字段——
所以 inode 号必须由 superblock 的 `inode_area_start_block` 反算：

```rust
// efs.rs，get_disk_inode_pos 的逆运算
pub fn inode_id_of(&self, block_id: u32, block_offset: usize) -> u32 {
    let inode_size = core::mem::size_of::<DiskInode>();
    let inodes_per_block = (BLOCK_SZ / inode_size) as u32;
    (block_id - self.inode_area_start_block) * inodes_per_block
        + (block_offset / inode_size) as u32
}
```

这里有个容易踩的锁坑，我特意把它留在注释里：`Inode::inode_id()` **不能**顺手
`read_disk_inode`，因为 `read_disk_inode` 走的是块缓存锁，而 superblock 字段在
`fs` 锁后面；两个锁不是一回事，嵌套顺序写错就是自锁。现在的写法是只拿 `fs` 锁做算术。

## 3. `link`：查两次、挂一项、加一次计数

```rust
pub fn link(&self, old: &str, new: &str) -> bool {
    // 1) old 必须在（否则 false）
    // 2) new 必须不在（否则 false，对应 POSIX 的 EEXIST）
    // 3) add_dirent(new, inode_id)
    // 4) 单独打开目标 inode 的块，nlink += 1
}
```

第 4 步刻意放在 `add_dirent` **之后、且不共享它的锁**：`add_dirent` 自己会拿根 inode 的
块缓存锁去扩目录 size，easy-fs 的 `DiskInode` 访问器不是可重入自旋锁，套着写就会死锁。
这一步是「先改目录再改计数」，不是原子事务——判分用例里没有掉电场景，
我把这个不足当作已知限制记下，而不是假装做过 journal。

## 4. `unlink`：空洞 dirent + 归零才真正回收

目录项是定长的（`DIRENT_SZ = 32`），`find_inode_id` 按下标遍历。我没有做「压缩目录」
（把后面的项往前挪），而是把命中的槽重写成 `DirEntry::new("", 0)`：
`DirEntry::name()` 在第一个 NUL 截断，所以空洞的名字读回来是空串，
路径参数不可能等于它，`inode_id` 也是 0。

计数归零才回收：

```rust
let left = ...modify(block_offset, |d| d.dec_nlink());
if left == 0 { inode.destroy(); }
```

`destroy()` 里要**同时**归还数据块和 inode 槽位，顺序上有两个细节：

- `clear_size` 报的是**绝对**块号（它直接读 direct/indirect 槽里的值），
  所以必须喂给 `dealloc_data` 而不是先减 `data_area_start_block`；
- `destroy()` 自己会再拿一次 `fs` 锁，所以调用它时必须手上什么都没有——
  这就是 `unlink` 把 `destroy` 放在所有守卫作用域之外的原因（代码注释同义）。

## 5. 反向对照：我原本以为这条用例证明了回收，它没有

`ch6_file3` 的形态是 10 轮 `create → 50 次 write（每串 58 字节，共 2900 字节）→ close → unlink`，
每轮断言 `unlink` 返回 0，随后 `open(fname, RDONLY)` 必须失败，最后打印
`Test mass open/unlink OK!`。我最初把它当作 inode/数据块回收的证据，理由是
「位图只进不出，第 2 轮起会死在 `alloc_inode().unwrap()`」。**这个归因是错的，是我自己做负控制推翻的：**

| 轮次 | 改法 | 读数 |
|---|---|---|
| `negctl1` | `if left == 0 { inode.destroy(); }` 整块禁用 | 31/31 |
| `negctl2` | 跳过 `self.remove_dirent(name)` | 31/31 |
| `fix4`/`fix5` | 真实实现 | 31/31 |

两轮负控制的读数是有效的：`Compiling easy-fs v0.1.0 (…/c_run3/ch6/easy-fs)` 各打印两次，
说明改过的 easy-fs 确实进了被测二进制（它们 `MAKE_RC=2` 的原因是我当时还没有 `reports/lab4.md`，
**构建失败和报告缺失都会让 make 退非零，但只有前者会让 PASS 数失去解释力**——这条口径见 Lab 1）。
判分器自己的注释也站在负控制这边：`ch6_file2.rs` 在最后一次 `unlink` 之后写着
`// It's Ok if you don't delete the inode and data blocks.`——官方就没要求回收。
所以本章里回收路径的真实覆盖只有 file2 那一次「最后一个名字」的调用，
`mass open/unlink` **不能**算作它的证据，我把它从「已证明」降级为「已实现、覆盖不足」。

## 6. 用探针量出来的机制足迹（以及空洞的真实代价）

`negctl2` 与真实实现都让 `open(RDONLY)` 失败，这两个读数互相矛盾，于是我不再靠推理，
而是在 `unlink_in_root` 前后各读一次根目录，把机制直接打出来（`logs/c3_ch6_negctl6.log`，
该轮判决 31/31，`MAKE_RC=2` 仅因当时缺 lab4）：

```
[nc6] pre  "fname2"    ino=0 nlink=1 hit=true  holes=0  n=60
[nc6] post ok=true nlink=1 hit=false holes=1  n=60
[nc6] pre  "linkname1" ino=0 nlink=1 hit=true  holes=1  n=60
...
[nc6] pre  "fname3"    ino=0 nlink=1 hit=true  holes=4  n=61
[nc6] post ok=true nlink=1 hit=false holes=5  n=61
```

三条结论：

1. **名字确实是 `remove_dirent` 抹掉的**：`hit` 逐次 true→false，`holes` 每次 +1。
   这解释了 fix 轮里 `open(RDONLY)` 为什么失败，也让 `ch6_file2` 的 nlink 序列（1→2→4→3→1）
   成为我在这个实现里唯一被断言死锁住的量化证据。
2. **`destroy()` 没有打到根 inode**：全程 `ino=0`、根的 `nlink` 恒为 1、`n` 不减。
   我一度怀疑「`unlink` 里的回收误伤了目录项所在槽位」，这组数据把它排除了。
3. **空洞不重用的代价被量出来了**：10 轮之后 `n` 从 60 涨到 70。每轮 `create` 都追加新目录项，
   从不回填空洞。这正是 Lab §4 那个「不压缩目录」设计的可观测成本，
   `Inode::ls()` 会把它们全打印成空字符串。

`negctl2` 为什么在没有 `remove_dirent` 的情况下仍然 31/31，我**没有查明**。
它和第 1 条数据不能同时成立，说明两次构建之间还有一处我没控制的变量；
在这查清之前，我不拿 `negctl2` 支持任何结论，也不拿它反对 `remove_dirent` 的必要性。

## 7. 已知的不足（写清楚，不藏）

1. 空洞目录项不回收目录 `size`：`ls()` 打印空串，且如上所述每轮创建都在膨胀目录。
   本章判分集合里没有任何 `ls` 断言，所以这条路没有被测试覆盖，我没有用它证明什么。
2. `find_inode_id("")` 会命中第一个空洞并返回 inode 0。判分用例里没有空路径，
   但严格说 `open_file` 应该在入口拒绝空串——这是「测试没打到」，不是「不存在」。
3. 回收路径（`destroy`）目前只有 `ch6_file2` 最后一次 unlink 路过，且官方明确说不回收也算对。
   要真正压它，需要一条「创建数逼近 inode 位图容量」的用例，本仓库的判分集合里没有，我也没造。

