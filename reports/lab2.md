# Lab 2 · ch4 虚拟内存管理：用页表回答"这个地址能不能碰"

本章题目给的是分页（SV39）。我的实现取向从一开始就定死了：**合法性由当前任务的页表说了算，
不维护任何静态区间白名单。** 这份报告记录我为了守住这条取向付出的工程代价，以及两个
只有真跑判分器才会暴露的 bug——它们都不是"代码写错了"，而是"我对地址空间的假设错了"。

## 1. 分层：谁负责翻译，谁负责记账

```
os/src/mm/user_ptr.rs       copy_from_user / copy_to_user / read_u8 / write_u8 / read_struct / write_struct
        │                   （唯一允许解引用用户地址的地方，全部返回 Option/bool，绝不 panic）
os/src/mm/memory_set.rs     MemorySet::translate_user_byte(va, writable) -> Option<*mut u8>
                            MemorySet::mmap / munmap、Vma 区间表（BTreeMap<VirtPageNum, Vma>）
os/src/mm/page_table.rs     PageTable::{translate, map_or_replace_invalid}、PTE 的 U/R/W/X 位查询
os/src/task/accounting.rs   SyscallAccounting：sys_trace(Syscall) 的计数表
```

闸门的核心只有一个函数，它回答的问题和硬件会回答的是同一个问题：

```rust
let pte = self.page_table.translate(vpn)?;
if !pte.is_valid() || !pte.readable() || !pte.user_accessible() { return None; }
if writable && !pte.writable() { return None; }
let pa = usize::from(PhysAddr::from(pte.ppn())) + va % PAGE_SIZE;
if !(BASE_ADDRESS..MEMORY_END).contains(&pa) { return None; }   // 必须落在 RAM 内
```

`ch4_trace1` 的四类地址因此都不需要特例：

| 测试里的地址 | 判定结果 | 为什么 |
|---|---|---|
| 栈上局部变量 `var` | 可读、可写 | 用户栈是 `R|W|U` |
| `isize::MAX as usize` | 读 `None`、写 `-1` | SV39 符号扩展后无叶子表项 |
| `0x8020_0000`（内核物理基址） | 读 `None`、写 `-1` | **没有任何 U 位置起的 PTE 指向它**，不是我把它拉黑了 |
| `mmap(.., PROT_READ)` 之后 | 读 Some、写必须 `-1` | 同一张表里 R=1、W=0 |
| `munmap` 之后 | 读 `None`、写 `-1` | 表项被清成 empty，帧已归还 |

第三行是这套设计最省事的地方。内核线性映射在 *内核* 空间里当然看得见 `0x8020_0000`，
但 syscall 是在当前任务的 satp 下做翻译的，那个地址在这个页表里根本不存在 U 映射，
于是自然被拒。我不需要维护一张"内核禁区"清单。

还有一个隐蔽的正确性点：空 PTE 的 ppn 字段是 0，如果只判 `is_valid()` 之外的条件而漏掉它，
未映射地址会被翻译成物理页 0（`0x8000_0000`，CLINT 那一片）。我在两处都做了显式防御——
`is_valid()` 先短路，另外 `ppn.0 == 0` 再兜一次，并在注释里写明原因。

## 2. mmap / munmap：有序区间表 + split/merge

`Vma` 记录 `[start_vpn, end_vpn)`、权限、以及**逐页的 `FrameTracker`**。帧的所有权放在
`BTreeMap<VirtPageNum, FrameTracker>` 里，所以归还就是让这段 map 走出作用域：

```rust
fn unmap_pages(&mut self, start_vpn, end_vpn) {
    for vpn in VPNRange::new(start_vpn, end_vpn) { self.page_table.unmap(vpn); }  // ① 先撤 PTE
    … split_at 把头/尾切回 vmas …
    drop(vma.frames);                                                              // ② 后还帧
}
```

顺序是有意的：**先把 PTE 抹掉，再把帧还给分配器**。反过来的话，中间存在一个窗口，
别的任务可能拿到这块帧，而本任务的页表还指着它——`sys_trace(Write)` 就能隔着这张旧表写进
别人的内存。`Test 04_5 ummap` 要求 `munmap` 之后地址立刻不可访问，靠的就是①。

`split_at` 用 `BTreeMap::split_off` 把帧表一分为二，两侧各自维持
`len == frames.len()` 的不变式（`debug_assert` 盯着）。`try_merge_right` 在相邻且权限相同
时合并区间，避免反复 mmap/munmap 把表拉成一串碎片。

参数校验放在 `MemorySet::mmap`，非法请求一律返回 `false` ⇒ syscall 层给 `-1`，
绝不让应用自己去踩：

- `start` 不页对齐、`len == 0`、`len` 不页对齐；
- `prot & !(READ|WRITE|EXEC) != 0`（覆盖 `prot | 8`）；
- `prot & READ == 0`（覆盖 `prot == 0`，也覆盖 RISC-V 上无法表达的"只写"PTE）。

这里有个细节值得记：`ch4_mmap2` 用 `prot = 2`（只写），断言 `mmap` **成功**返回 0，
然后自己读它并期待被杀。也就是说 RISC-V 上 W=1/R=0 的 PTE 根本无法构造，
内核只能退化成"至少可读"。所以我的规则是拒绝 `READ == 0`，与 `ch4_mmap2` 期望的
`Should cause error, Test 04_3 fail!` 不出现这一条并不冲突——那条测试程序是被
异常处理杀掉的，不是被返回值骗过的。

## 3. 踩到的第一个坑：mmap 的地址下限

最初的版本我给 mmap 加了一条地板：`start < USER_MMAP_BASE → 拒绝`，理由写在注释里是
"不能盖到别的 app 的槽位上"。听起来很安全。

实际跑判分器，`Test 04_1` 直接 MISS。原因是所有测试程序硬编码的是：

```rust
let start: usize = 0x1000_0000;
```

而我以为 mmap 应该发生在内核镜像之上（`0x80a0_0000` 起）。**这个前提整个错了**：
`user/src/linker.ld` 写的是 `BASE_ADDRESS = 0x0`，官方构建链产出的应用 ELF 是
position-independent 的，加载后 `.text` 落在 `0x1xxxx` 这种极低的地址上。
`0x1000_0000` 对它们来说就是一块普通的高位空洞。

修法不是把地板挪个位置继续当白名单用——那还是同一个思路。我把方向换成"只设天花板"：

```rust
pub const USER_MMAP_LIMIT: usize = 0x4000_0000;   // config.rs
if start >= TRAP_CONTEXT_BASE || end > USER_MMAP_LIMIT { return false; }
```

顶侧由 `TRAP_CONTEXT_BASE`（共享 trampoline/trap frame 所在处）封住，底侧完全开放，
是否重叠交给区间表判断。同时删掉了 `from_elf` 里那段"把 user_stack_top 到
USER_MMAP_BASE 之间每一页都 reserve 出一个空叶子表项"的代码——那是上一个思路的配套，
既慢又没必要。

## 4. 踩到的第二个坑：拿"页表里有没有叶子项"当所有权判据

删掉 reserve 之后 `Test 04_5 ummap` 仍然 MISS，日志里是精确的一行：

```
Panicked at src/bin/ch4_unmap.rs:19, assertion `left == right` failed
  left: -1
 right: 0
```

第 19 行是 `assert_eq!(mmap(start + len, len * 2, prot), 0)`——紧挨着第一块的第二段 mmap 被拒了。
我的重叠检查当时写成：

```rust
fn page_reserved(&self, vpn) -> bool {
    self.page_table.has_entry(vpn) || …vmas…
}
```

看起来天衣无缝：**已经映射的页当然有叶子表项**。但 SV39 的叶子表项是**惰性创建**的，
只有真正 `map` 过的位置才存在。ELF 的 segment 描述的是虚拟范围，加载时按 program header
建映射；一旦某段范围压根没被 `map` 调用过，`find_pte_ref` 走到第二级发现无效就直接返回 `None`，
`has_entry` 于是答"没人拥有"——反过来看，`0x1000_0000` 这块空洞在第一段 mmap 之后
确实有了叶子项，第二段紧邻而来时命中的却是**第一段自己的 Vma**……

真正的错因在这里：`page_reserved` 用页表答"静态区"的问题，就会把"页表尚未展开"误报成"空闲"，
也会把"刚被 munmap 清空但仍留有表项壳子"的状态搅进来。静态区间的归属其实早就准确地记在
`areas: Vec<MapArea>` 里了，我应该去问它而不是猜页表：

```rust
fn static_owns(&self, vpn) -> bool {
    self.areas.iter().any(|a| a.vpn_range.get_start() <= vpn && vpn < a.vpn_range.get_end())
}
```

于是判据拆成两个语义清晰的问题：
`static_owns`（镜像/栈/trampoline/trap frame 归不归它）与
`vma_owns`（是不是某个动态区间**当前持有帧**）。

- `mmap` 的重叠检查：`static_owns(vpn) || vma_owns(vpn)`；
- `munmap` 的范围检查：**只用** `vma_owns`。

第二条是关键的安全改进。之前用 `page_reserved` 判 munmap，理论上允许把用户栈或 trap frame
的页"unmap"掉（它们在页表里有表项），帧却永远不会归还（不在 `frames` 里），
等于把一个静态区域从页表上抠掉。现在 `munmap` 只能撤销自己发出去的东西。

顺带解决另一个必然冲突：`from_elf` 会给用户栈**上方**额外 push 一个零长度 `MapArea`
（sbrk 的增长区），它的 `vpn_range` 两端相等、不占任何页，所以新的 `static_owns` 不会因为它
而误拒 mmap——这也说明为什么必须用区间而不是数表项。

## 5. 帧回收与"重复 mmap/munmap 仍可分配"

`Test 04_6 ummap2` 考察两件事：非页对齐的 `munmap` 必须返回 `-1`（不能部分生效），
以及一轮 mmap/munmap 之后系统还能继续分配。前者由 `munmap` 开头的对齐检查保证；
后者靠两条：

1. `unmap_pages` 里 `drop(vma.frames)` 让 `FrameTracker` 析构归还帧；
2. `map_pages` 分配失败时**回滚**：把已建的表项 unmap、丢掉已拿到的 tracker，
   并且不往 `vmas` 里留任何记录，否则区间表会声称拥有一堆自己没有的页。

`FrameTracker::new` 在建 tracker 时就把整页清零，这正好兑现 `mmap` 的"匿名零页"承诺，
所以 `ch4_unmap` 里第二轮写入前不需要内核再做任何 memset。

## 6. 实测读数

| 轮次 | 读数 | 该轮做的事 |
|---|---|---|
| `hand0` | 4/16 | 模板基线（前任留下，未改动） |
| `wip1` | 4/16 | 原样提交 WIP —— 编译不过（`reserve()` 重复定义、build.rs 指向不存在的路径），判分器停在 build 阶段 |
| `fix1` | **15/16** | 修编译 + mmap 去掉地址地板 + `sec` 不截断；只剩 `Test 04_5 ummap` |
| `fix2` | 见文末 | 所有权判据改成 `static_owns`/`vma_owns` |

`wip1` 那一轮最值得说：分数和基线一模一样，看上去"什么都没改"，实际上差距是
`#![deny(warnings)]` 让内核压根没链接出来，16 条判决里有 12 条来自同一份旧产物。
**只看分数会误判进度，必须先看判分器有没有跑到 check 阶段**——这也是 `rig3_c_gaps.py`
要校验"分母 == 逐条判决行数"的原因。

NOT_EXPECTED 四条（`FAIL: T.T`、`Test sbrk failed!`、`Should cause error, Test 04_2/04_3 fail!`）
从 `fix1` 起保持不出现。其中 `04_2`/`04_3` 是靠真实机制通过的：`prot=1` 时写只读页触发
StoreAmoPageFault，内核打印 "kernel killed it." 并杀掉任务；`prot=2` 时读同样命中保护位不符。
这两条不是"输出恰好没匹配上"。
