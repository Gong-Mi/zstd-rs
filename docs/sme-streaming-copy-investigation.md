# SME (streaming SVE) 用于 ruzstd 解码拷贝的实测评估（v2：含一次测量缺陷更正）

设备：小米 17 Pro（pandora / 25098PN5AC / caneo / Oryon 8e5，Android 17）
基线：`integration-green`（PR #46 合并点 `5b2769e`）
状态：**实验性，未合入任何生产路径**（钩子默认惰性）

## 0. 更正声明（先读）

**本文 v1 的性能数字作废。** 原因是内核 ABI 违规，不是硬件问题：

- v1 的内联汇编在 `smstart`/`smstop` 区间里只声明了 `z0..z3`、`p0`,`p1` 作为 clobber。
- Arm ACLE 明确规定：**执行 SMSTART/SMSTOP 会使全部 Z 和 P 寄存器状态失效**，汇编的 clobber 列表必须列出所有可能被改变的寄存器。
  （来源：ARM-software/acle，`main/acle.md`，inline asm 章节）
- 后果是编译器可能把活值（包括计时用的值）留在被静默破坏的寄存器里。v1 观测到的 NaN、恒为 0 的时间差、异常的入口开销，必须优先按测量程序缺陷解释，不能当作硬件特性。

修正后（clobber 覆盖 `z0..z31`、`p0..p15`、`cc`、`memory`）的对比：

| 量 | v1（作废） | v2（修正后） |
|---|---|---|
| `smstart`+`smstop` 入口成本 | p50 ≈ 5 ticks（约 260 ns） | **p50 = 1 tick（约 52 ns），mean 0.68 tick（36 ns）** |
| 裸拷贝带宽 vs memcpy（256 KB–8 MB） | 2.2–2.6x | **1.39–1.54x** |
| 端到端（rep.bin / rep64） | −5% / 方向漂移 | **+9.7% / +8.7%（更慢）** |

定性结论不变（SME 在解码器上不值得），但**任何引用 v1 数字的地方都要换成 v2**。

## 1. 结论

1. **不作为默认路径。** 修正后结论更强：真实解码上开 SME 路径是明确的负收益（拷贝密集语料 −9~10%）。
2. SME 的真实优势比 v1 小：裸拷贝带宽 **1.39–1.54x** libc memcpy（256 KB–8 MB，缓存驻留），DRAM 受限（≥32 MB）**1.00x**。
3. 单向量（1×64B）写法**一律打不过 memcpy**（0.30–0.76x）。展开到 4×64B 才有优势。
4. 入口成本很低（约 1 tick / 数十 ns），**不是** <8 KB 拷贝变慢的理由。
5. **缓存驻留是决定性变量**：源被驱逐时 SME 慢 2.3–3.6x（16/64 KB），驻留时 SME 更快（256 KB 1.4x）。解码器 ring buffer 窗口是 MB 级，必然踩冷数据。
6. in-situ 同调用点对比：SME 在拷贝内耗时是 libc memcpy 的 **2.23x**（rep64：79920 vs 35774 ticks）。

## 2. 设备与工具链（实测）

| 项 | 实测值 | 方法 |
|---|---|---|
| 非 streaming SVE 向量长度 | 16 B（128-bit） | `cntb` |
| SME streaming 向量长度 | 64 B（512-bit） | `smstart; cntb; smstop` |
| SME 可执行 | 是 | `smstart` / `rdsvl` 内联汇编实跑 |
| 周期计数（PMCCNTR_EL0） | EL0 读取 SIGILL；root 下 `perf_event_open` 可开但只能 `read(fd)` 批量 | `pmc_probe2.c` |
| 计时 | CNTVCT_EL0 @19.2 MHz（1 tick = 52.1 ns） | 量化地板 |

工具链：rustc 1.100.0-nightly **无 SME intrinsics**（`stdarch_aarch64_sme` 不存在；`#[target_feature(enable="sme")]` 需 `aarch64_unstable_target_feature`）→ SME 必须写在 C（`clang-23 -march=armv9.2-a+sme2`）并经 C ABI 调用。

## 3. 实验装置

- `experiments/sme-copy/ruzstd_sme_copy.c`
  - `ruzstd_sme_copy`：dst 对齐 64 B（peel 用 `memcpy`），主体 4×64B/迭代，尾部 `whilelt` 谓词块，单个 `smstart/smstop` 区间；**clobber 按 ACLE 完整声明**。
  - `ruzstd_sme_copy_naive`：1×64B/迭代、不对齐（对照）。
  - `ruzstd_sme_copy_pf`：主体加 `prfm pldl1keep`（对照）。
  - 计数器：调用数 / 字节 / 尺寸直方图 / 每调用延迟直方图（析构打印）。
- Rust 钩子 `ruzstd/src/sme_copy.rs`：`RUZSTD_SME_MIN=<bytes>` 门控（不设=惰性），`RUZSTD_SME_STATS=1` 开统计，`RUZSTD_SME_BACKEND=sme|naive|neon|memcpy` 把同一调用点切到不同后端。
- 接线点（全在解码侧）：`decoding/ringbuffer.rs::copy_bytes_overshooting`、`decoding/ringbuffer.rs::extend`、`decoding/decode_buffer.rs::direct_write` / `direct_repeat`。
- 语料：`text.tar`（LLVM 源码 tar，203 MB 输出）、`rand40.bin`（不可压 40 MB）、`rep.bin`（16 MB / 32 KB 周期）、`rep64.bin`（64 MB / 1 MB 周期），C zstd 1.5.7 `-3` 压缩。
- 测量：pinned `cpu7` + `nice -n -20`，同轮轮转、多轮取 min。宿主全程有重负载（load ≈ 12），绝对值不可跨时段比较。

## 4. 结果（v2）

### 4.1 裸拷贝带宽（每样本约 256 MiB；GB/s）

| span | memcpy | neon64 | sme 1×64B | sme 2×64B | sme 4×64B | sme 8×64B | sme 4×64B NT |
|---|---|---|---|---|---|---|---|
| 256 KB | 73.5 | 55.5 | 22.3 | 59.2 | **102.0** | 103.7 | 102.4 |
| 2 MB | 73.8 | 56.1 | 22.5 | 59.5 | **102.3** | 103.0 | 102.1 |
| 8 MB | 58.8 | 42.9 | 19.9 | 59.5 | **90.8** | 91.0 | 90.4 |
| 32 MB | 33.2 | 22.4 | 17.7 | 33.5 | 33.2 | 33.2 | 33.3 |
| 36 MB | 33.3 | 21.5 | 17.8 | 33.4 | 33.4 | 33.3 | 33.1 |

倍率：`sme4x64` = **1.39x / 1.39x / 1.54x / 1.00x / 1.00x**；`sme1x64` = 0.30 / 0.30 / 0.34 / 0.53 / 0.53；NT 存储无额外收益。

### 4.2 单次拷贝延迟分布（隔离；dst 未对齐 +3；ticks，1 tick = 52.1 ns）

| 项 | p50 | 说明 |
|---|---|---|
| 空测量（tick();tick()） | 0–1 | 量化地板 |
| `smstart;smstop` 仅切换 | **1**（mean 0.68 ≈ 36 ns） | 入口成本 |

每尺寸 p50：

| size | memcpy | neon64 | sme 1×64B | sme 4×64B |
|---|---|---|---|---|
| 1 KB | 0 | 0 | 1 | 2 |
| 4 KB | 1 | 1 | 2 | 3 |
| 16 KB | 5 | 8 | 5 | 5 |
| 64 KB | 17 | 30 | 18 | 19 |
| 256 KB | 68 | 120 | 60 | **52** |

所有实现的 `max` 都在数百到 2000+ ticks（几十~上百 µs），与代码无关（抢占噪声）。

### 4.3 缓存驻留矩阵（p50 ticks；同轮轮转）

`resident` = 拷贝前读一遍源；`evicted` = 每次拷贝前 24 MB sweep 冲 cache。

| size | resident memcpy / neon / sme / sme+prfm | evicted memcpy / neon / sme / sme+prfm |
|---|---|---|
| 16 KB | 17 / 3 / **4** / 3 | 27 / 28 / **97** / 102 |
| 64 KB | 21 / 19 / **13** / 39 | 53 / 91 / **120** / 157 |
| 256 KB | 69 / 88 / **49** / 77 | 207 / 266 / **225** / 372 |

- 驻留：SME 最快（256 KB 1.4x 于 memcpy）。
- 驱逐：16 KB 慢 **3.6x**、64 KB 慢 **2.3x**、256 KB 大致持平（1.09x）。
- `prfm pldl1keep` 无效（驱逐档反而更慢）。

### 4.4 真实解码 in-situ（同一调用点，可切后端）

| 语料 | 调用数 / 字节 | sme 拷贝内 ticks | memcpy 拷贝内 ticks | 倍率 |
|---|---|---|---|---|
| rep64.bin.zst | 701 / 66.5 MB | 79920（mean 114） | 35774（mean 51） | **2.23x** |

端到端（同一二进制切门控，同轮 min）：

| 语料 | off | thr=4096 | Δ |
|---|---|---|---|
| rep.bin.zst | 2709 µs | 2972 µs | **+9.7%** |
| rep64.bin.zst | 17978 µs | 19550 µs | **+8.7%** |
| text.tar.zst | 606662 µs | 605171 µs | −0.2%（噪声；覆盖率 <1%） |
| rand40.bin.zst | 10627 µs | 10556 µs | −0.7%（raw 块不走拷贝点，0 命中） |

比特级一致性：四个语料解码输出哈希在 SME 开/关两侧完全相同（`98fbe133ec86eb25` / `34ad964b2a0bbb25` / `eb9ac52e195c45bf` / `4622c5527a6f2048`）。

## 5. 机制：证据边界

**已证（本机可复现）**
1. 源被驱逐时 SME 拷贝显著劣于核心拷贝（16 KB 3.6x、64 KB 2.3x）。
2. 源驻留时 SME 更快（256 KB 1.4x）。
3. 软件预取（`prfm pldl1keep`）不能改善驱逐档。
4. 入口成本极低（≲1 tick），无法解释上述差异。

**假设（尚未证实到微架构层）**
- SME 的 load/store 路径在 cache miss 上的并发/预取能力弱于核心 LSU。外部资料（Apple M4 上 SME 为簇级协处理器、经 L2 与核心交换数据）属**同类实现观察**，不能直接外推成 Oryon 微架构事实。
- 缺少 PMU：本机 EL0 读 PMCCNTR 触发 SIGILL，`perf_event_open` 只给 `read()` 批量计数，因此**无法**直接测量 miss、MLP、驻留周期来闭环证明该假设。

## 6. 建议

1. 保持默认关闭、仅设备侧实验开关。
2. 若将来启用，判据至少是：单次 ≥16 KB **且** 源疑似驻留（如刚写出的倍增段、极小 offset 的匹配）。当前实现无法识别，代价大于收益。
3. 不要期待 CI 验证：仓库 perf 判据在 x86_64 runner；可选 arm64 runner 是 Neoverse（SVE2 256-bit，无 SME）。
4. 与仓库 CI 契约冲突（nightly + 手写 asm + 平台专用）也是不引入默认路径的理由。

## 7. 复现

```sh
clang-23 -O3 -march=armv9.2-a+sme2 -c experiments/sme-copy/ruzstd_sme_copy.c -o ruzstd_sme_copy.o
llvm-ar rcs libruzstd_sme_copy.a ruzstd_sme_copy.o
RUSTFLAGS="-L native=$PWD -l static=ruzstd_sme_copy" cargo build --release

RUZSTD_SME_MIN=4096 RUZSTD_SME_STATS=1 RUZSTD_SME_BACKEND=sme     ./zbench decode <file.zst> 1
RUZSTD_SME_MIN=4096 RUZSTD_SME_STATS=1 RUZSTD_SME_BACKEND=memcpy  ./zbench decode <file.zst> 1
```

隔离微基准：`sme_bw`（带宽）、`sme_lat`（延迟分布）、`sme_cold`（驻留/驱逐）、`sme_cachestate`（缓存状态矩阵）、`sme_align`（对齐）、`pmc_probe2`（时钟可用性）。

## 附：证据文件

| 文件 | 内容 |
|---|---|
| `experiments/sme-copy/ruzstd_sme_copy.c` | C 内核（4 变体 + 计数器 + ACLE 完整 clobber） |
| `experiments/sme-copy/sme_bw.c` | 带宽对比（含 NT 存储） |
| `experiments/sme-copy/sme_lat.c` | 单次拷贝延迟分布 |
| `experiments/sme-copy/sme_cold.c` | 驻留/驱逐轮转对比 |
| `experiments/sme-copy/sme_cachestate.c` | 缓存状态矩阵 |
| `experiments/sme-copy/sme_align.c` | 对齐敏感度 |
| `experiments/sme-copy/pmc_probe2.c` | 周期计数可用性探针 |
| `experiments/sme-copy/ruzstd-sme-hook.patch` | Rust 侧钩子与三处接线 |
