# Lab 1 · 先确认尺子：ch8 的判决集合、MAKE_RC≠0 的两种成因、诊断态与判分态的边界

分支：`LearningOS/2026a-rcore-wodanleme12s` 的 `ch8`；本地判分副本 `oscamp3/c_run3/ch8`，
判分器 `checker=7d61ec55b58eed6ca7052917846c1b87af34563a`、`tests=a0593662ad55d670ba8c27ce1763347cd0dd552f`。
工具链 `rustc 1.80.0-nightly (c987ad527 2024-05-01)` + `QEMU 11.1.3`，官方 `timeout --foreground 30s` 未放宽。
本报告记录的是**仪器本身**，因为 ch8 的排查周期里，几乎所有误判都来自把「读数的形状」当成了「读数的含义」。

## 1. 判决集合长什么样

`make test CHAPTER=8` 跑 `check/ch8.py`，期望串由 `ci-user/user/src/bin/*.rs` 里的 `OK`/`passed`
字符串生成，每轮再打一个随机后缀（`check` 与 `user/src/bin` 下全部 `sed -i 's/OK/OK<n>/g'`）。
本轮后缀是 `50353`，上一轮是 `18192`——**任何跨轮引用期望串的报告都必须带上后缀**，否则等于没证据。

- 分母 25（`Test passed50353: 25/25`）。
- 其中三条是本分支新增的：`deadlock test mutex 1 OK50353!`、`deadlock test semaphore 1 OK50353!`、
  `deadlock test semaphore 2 OK50353!`。这三条就是 ch8 的全部增量难度所在。
- `base.py` 用 `assert count == total` 收尾：**25 里缺 1 条 = 整章 0 分**。部分通过没有任何中间价值，
  所以「先修好 mutex1，sem 两条下轮再说」这种排期在这把尺子下是不成立的。
- 负向集合 `NOT_EXPECTED = ['FAIL: T.T', 'Test sbrk failed!']` 必须**继续缺席**——它们分别对应
  ch2 的坏地址用例和未实现的 `sys_sbrk`。`final` 轮日志第 771、772 行：
  ```
  [PASS] not found <FAIL: T.T>
  [PASS] not found <Test sbrk failed!>
  ```
  任何「顺手把 sbrk 也实现了」的改动都会把这两条之一放出来，反而丢分。

## 2. MAKE_RC≠0 有两种，含义完全相反

| 成因 | 判据 | PASS 读数是否可用 |
|---|---|---|
| A. `make build` 编译失败 | 日志里有 `error: could not compile` | **不可用**。qemu 跑的是上一轮遗留镜像，`2/25` 这类数字是幻象 |
| B. 编译通过、报告闸失败 | `Test passed<n>: 25/25` 已打印，随后 `Report for lab<i> needed.` | **可用**。会话完整跑完，失败在判决之后 |

判据一行：`grep -ac "error: could not compile" $LOG`。

这条区分是被 `probe8` 那一轮教出来的：那一轮我给 `probe_suspend` 补文档注释之前就想跑，
crate 顶部 `deny(missing_docs)` 直接拒绝编译，读数 `2/25`。如果把它当成「用例还是不过」继续调检测器，
方向就全错了。同理，`final` 轮的 `MAKE_RC=2` 属于第 B 类：

```
$ grep -ac "error: could not compile" logs/c3_ch8_final.log
0
$ grep -a "Report for lab" logs/c3_ch8_final.log
Report for lab1 needed. Add your report to reports/lab1.pdf or reports/lab1.md
```

报告闸在 `checker/rCore-Tutorial-Checker/Makefile:130-139`，位于 `check/ch8.py` **之后**，
且 `CHAPTER=8` 时 `LAB=5`。也就是说：只有满分轮才会暴露报告缺失。这是刻意的设计——
不满分的人永远看不到这条错误信息。

补上 `reports/lab1..lab5.md` 之后的确认轮 `logs/c3_ch8_verify.log`（后缀 `46471`）：

```
Test passed46471: 25/25
Report for lab1 found.   ... Report for lab5 found.
MAKE_RC=0
```

这才是可交付的读数；`logs/c3_ch8_final.log` 的 `25/25` 是它前一次同样判决、只差闸口的复现。

## 3. 诊断模式不是判分模式

`DIAG_SECONDS=N bash rig3_c_run.sh ch8 <后缀>` 做两件事：把 `timeout` 换成
`shim3diag/timeout` 以放宽会话墙钟，并给每行 qemu 输出打 host 相对时间戳。
它**不改动判决集合、不改动内核**，但两点让它不能充当判决：

1. 放宽后的 30s→Ns 意味着「官方 30s 内跑不完」这种失败会被掩盖。
2. 诊断轮的内核里通常还带着探针 `println!`，本身就在改变调度时序。

所以本轮流程固定为：诊断轮定位根因 → 撤净探针 → 官方轮复跑取数。
`logs/c3_ch8_probe10_diag45.log`（诊断态）与 `logs/c3_ch8_final.log`（官方态）都给出 25/25，
但**只有后者是交付依据**；前者只用来证明「25/25 不是探针带来的假象」，以及提供时间戳证据：

```
   0.33 deadlock test mutex 1 OK18192!
   1.25 deadlock test semaphore 1 OK18192!
   1.64 deadlock test semaphore 2 OK18192!
```

三个用例在 1.64s 内全部通过——远小于官方 30s 预算。这行读数直接否掉了「用例太慢所以超时」的假说，
也说明修复后没有留下任何时序上的运气成分。

## 4. 读日志的姿势

判分日志含二进制字节，`grep` 一律加 `-a`；要人读就先洗一遍：

```bash
tr -d '\0' < logs/c3_ch8_final.log | sed -e 's/\x1b\[[0-9;]*m//g' > /tmp/c8final.txt
```

差集用 `~/.local/bin/python3.12 rig3_c_gaps.py ch8 logs/c3_ch8_final.log` 直接算「缺哪条/多了哪条」，
不靠肉眼看交织输出——ch8 的用例是先 fork 再收集，日志是交错的，
substring 匹配很容易把「A 的输出出现在 B 的行号附近」读成「B 断言失败」。
