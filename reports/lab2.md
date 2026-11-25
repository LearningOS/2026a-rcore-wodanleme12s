# Lab 2 · ch4 分页与 VMA 在 ch5 的第二次落地：区间的开端与闭端

这一份写 ch5 分支上的动态映射（`sys_mmap`/`sys_munmap`）。设计取向沿用我在 ch4 定下来的那套：
**合法性由页表说了算，VMA 用有序区间表管理，支持 split/merge，帧在 munmap 时归还分配器**。
ch4 那一版最后 16/16（提交 `48cb257`，日志 `logs/c3_ch4_round1.log`），所以这一章的工作
不是重新发明，而是把同一套东西搬进 ch5 的文件布局，并把它对 `spawn` 的依赖关系想清楚。

## 1. 区间表长什么样

```
os/src/mm/memory_set.rs
  vmas: BTreeMap<VirtPageNum, Vma { start_vpn, end_vpn, len, perm, frames: BTreeMap<VirtPageNum, FrameTracker> }>
                                    ^ 半开：[start_vpn, end_vpn)，end_vpn 同时是「下一个区间的起点」
```

半开口径必须在三个地方保持一致，否则会像我第一次推 04_5 时那样推出三种互相矛盾的结论：

| 位置 | 口径 |
|---|---|
| `VPNRange` / `SimpleRange` 迭代器 | `if current == end { None }`，即 **`[l, r)`** |
| `vma_owns(vpn)` | `vma.end_vpn > vpn && vma.frames.contains_key(&vpn)` |
| `try_merge_right(start_vpn)` | 右邻居的 key 就是本区间的 `end_vpn` |

⇒ `end_vpn = VirtPageNum(end / PAGE_SIZE)`（`end` 是页对齐的排他末地址）这三处刚好自洽。
我先怀疑过迭代器是闭区间、从而推出「mmap 多映射一页」，读 `mm/address.rs:209-268` 才确认是半开，
于是那部分「修复」根本没有必要——**动手前先把尺子的刻度读对**。

## 2. mmap 的返回契约只有两个值

钉死的测试程序（`ci-user/user/src/bin/ch4_*.rs`）里，`mmap` 的断言全部是

```rust
assert_eq!(mmap(start, len, prot), 0);        // 成功
assert_eq!(mmap(.., .., ..), -1);             // 每一种非法
```

也就是说**成功返回 0、失败返回 -1**，而不是像 POSIX 那样返回映射地址。我在 ch4 曾经为了诊断返回过
`-101..-108` 这类错误码，结果把整族用例打挂；这一版直接在内核边界收口：

- `start % PAGE_SIZE != 0` ⇒ -1
- `len == 0` ⇒ -1
- `prot` 含 RWX 之外的位、或 `prot & PROT_READ == 0`（RISC-V 建不出 W-无-R 的叶子项）⇒ -1
- 越过 `TRAP_CONTEXT_BASE` / `USER_MMAP_LIMIT` ⇒ -1
- 区间与静态段（`static_owns`，按加载时记录的 `areas` 判）或已有 VMA（`vma_owns`）重叠 ⇒ -1
- 帧不够 ⇒ 回滚已映射的项、把 `FrameTracker` drop 掉归还，表里不留痕迹

`04_2`/`04_3` 那两条非法参数必须由内核拒绝，判分器要求 `Should cause error, Test 04_2/04_3 fail!`
**继续不出现**——这句话是应用自己打出来的，所以「把非法请求放过去让应用 assert」是反向操作。

## 3. 长度向上取整，起点严格对齐：04_5 与 04_6 是相反方向的两个约束

`ch4_unmap.rs`（04_5）：

```rust
assert_eq!(mmap(start, len, prot), 0);              // len = 4096
assert_eq!(mmap(start + len, len * 2, prot), 0);    // 8192
assert_eq!(munmap(start, len), 0);
assert_eq!(mmap(start - len, len + 1, prot), 0);    // ← 4097 字节必须成功
// 随后把 (start-len)..(start+len*3) 四个页全部写满
```

`ch4_unmap2.rs`（04_6）反过来：

```rust
assert_eq!(munmap(start, len + 1), -1);
assert_eq!(munmap(start + 1, len - 1), -1);
```

⇒ **mmap 的 len 向上取整到整页（只有 start 要求对齐），munmap 保持严格对齐**。
如果 mmap 也拒绝非整页长度，04_5 的第 4 步直接失败（这正是 `Test 04_5 ummap OK!` 长期 MISS 的根因）；
如果 munmap 放宽，04_6 失败。实现：

```rust
let span = len.checked_add(PAGE_SIZE - 1)? / PAGE_SIZE * PAGE_SIZE;   // 溢出走 -1，不 panic
let end  = start.checked_add(span)?;
```

`checked_add` 不是为了好看：`isize::MAX` 这类探测地址在 trace/sleep 系列里真的会出现。

## 4. 帧回收与 dentry 之外的复用压力

04_6 的最后一步是反复 mmap/munmap 之后**还能再分配**，所以 `Vma.frames` 必须真的把
`FrameTracker` 拥有权拿走、`munmap` 时 drop 掉；`unmap_pages` 里我用的是
`page_table.unmap(vpn)`（对缺失叶子项会 panic），因此调用前先用
`VPNRange::new(start, end).all(|vpn| vma_owns(vpn))` 把整段验一遍——
这样「重复 free」「拆静态段」「拆不存在的洞」三种误用在进入 `unmap` 之前就被拒掉。

跨多个相邻区间的 munmap（例如一次拆掉由两次 mmap 拼出来的范围）在 `unmap_pages` 里按
「从右往左逐个取出与本区间相交的 VMA，必要时 `split_at` 保留头/尾」处理，剩下的部分重新登记。

## 5. 本章实测读数

| 轮次 | 命令 | 读数 | 说明 |
|---|---|---|---|
| `takeov0` | `rig3_c_run.sh ch5 takeov0` | `2/15` | 接手时的工作区，mmap 家族全灭 |
| `probe1` | `rig3_c_run.sh ch5 probe1` | `2/15` | 只为拿探针输出（见 Lab 1 第 3 节） |
| `gate1` | `rig3_c_run.sh ch5 gate1` | 编译失败 | 我改 `translated_str_opt` 时的 E0597 借用错误 |
| `gate2` | `rig3_c_run.sh ch5 gate2` | `11/15` | mmap 家族 `04_1/04_4/04_5/04_6` 全亮 |

`gate2` 之后 `rig3_c_gaps.py` 里剩下的 4 条全部是进程管理层面的，见 Lab 3。
全过程 `不该出现却出现了 = 0`。
