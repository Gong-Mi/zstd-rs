# SME (streaming SVE) 用于 ruzstd 解码拷贝的实测评估

设备：小米 17 Pro（pandora / 25098PN5AC / caneo / Oryon 8e5，Android 17）
分支：本评估在 `integration-green`（PR #46 合并点 `5b2769e`）之上做，改动仅为实验性，未合入。

## 一、结论

1. **不作为默认路径。** 端到端在真实语料上没有稳定收益；缓存不驻留时是明确的负收益。
2. **唯一有正收益的窗口很窄**：单次拷贝 ≥16KB **且源数据驻留 L1/L2**。此时裸拷贝带宽是 libc memcpy 的 1.3–2.6 倍。
3. **固定成本不可忽略**：每次进入/退出 streaming 模式约 260–280 ns（=5 ticks @19.2MHz）。因此 <8KB 的拷贝一律亏损（1KB 慢 2–4 倍）。
4. **隔离微基准会系统性高估。** 同一个内核在"小工作集 + 8 槽轮转"的热数据上测出 2.2–2.6x，在真实解码里变成四种后端里最慢的（比 memcpy 慢 1.2–2.1 倍）。
5. 因此若要保留，必须：默认关闭（环境变量门控）、阈值 ≥16KB、并且只在"源疑似驻留"时启用。

## 二、设备能力与工具链（实测，非假设）

| 项 | 实测值 | 方法 |
|---|---|---|
| 非 streaming SVE 向量长度 | 16 B（128-bit） | `cntb`（c=16） |
| SME streaming 向量长度 | 64 B（512-bit） | `smstart; cntb; smstop` |
| SME 指令可执行 | 是 | `smstart/rdsvl` 内联汇编实跑 |
| CPU 特性 | sme, sve, sve2, i8mm, bf16, smei8i32, smef16f32, smeb16f32, smef32f32 … | `/proc/cpuinfo` |
| PMCCNTR_EL0（周期计数） | EL0 读取 SIGILL；root 下 `perf_event_open` 成功但只能 `read(fd)` 批量 | 探针 `pmc_probe2.c` |
| 可用时钟 | CNTVCT_EL0 @19.2 MHz → 1 tick = 52.1 ns | 量化地板，<8KB 不可分辨 |

工具链限制：

- rustc 1.100.0-nightly **没有 SME intrinsics**：`#![feature(stdarch_aarch64_sme)]` 不存在（编译器提示只有 `stdarch_aarch64_sve`，且 SVE intrinsics 仍未稳定）；`#[target_feature(enable = "sme")]` 需要 `aarch64_unstable_target_feature`。
- 结论：SME 代码只能放 C（`clang-23 -march=armv9.2-a+sme2`）+ C ABI 调用，或 nightly inline asm。这与本仓库 stable / MSRV 1.87 / `cargo hack check` 的 CI 契约冲突。

## 三、实验装置

- C 内核：`experiments/sme-copy/ruzstd_sme_copy.c`
  - `ruzstd_sme_copy`：dst 先对齐 64B（peel 用 `__builtin_memcpy`），主体 4×64B/迭代，尾部 `whilelt` 谓词块，全程一个 `smstart/smstop` 区间。
  - `ruzstd_sme_copy_naive`：1×64B/迭代、不对齐（对照组）。
  - `ruzstd_sme_copy_pf`：在主体里加 `prfm pldl1keep`（对照组）。
  - 计数器：调用次数 / 字节数 / 尺寸直方图 / 每调用延迟直方图（析构时打印）。
- Rust 钩子：`ruzstd/src/sme_copy.rs`（环境变量 `RUZSTD_SME_MIN` 门控，不设=完全不进入；`RUZSTD_SME_STATS=1` 开统计；`RUZSTD_SME_BACKEND=sme|naive|neon|memcpy` 可把同一调用点切到不同后端做同条件对比）。
- 接线点（3 处，全部在解码侧）：`decoding/ringbuffer.rs::copy_bytes_overshooting`、`decoding/ringbuffer.rs::extend`、`decoding/decode_buffer.rs::direct_write` / `direct_repeat`。
- 语料：`text.tar`（LLVM 源码 tar，203 MB 输出）、`rand40.bin`（不可压 40 MB）、`rep.bin`（16 MB，32 KB 周期重复）、`rep64.bin`（64 MB，1 MB 周期重复），全部用 C zstd 1.5.7 `-3` 压缩。
- 测量：pinned `cpu7`(prime) + `nice -n -20`，同轮轮转，多次取 min；统计用 CNTVCT。

## 四、结果

### 4.1 裸拷贝带宽（每样本约 256 MiB，GB/s，同轮 min）

| span | memcpy | neon64 | sme 1×64B | sme 2×64B | sme 4×64B | sme 8×64B |
|---|---|---|---|---|---|---|
| 256 KB | 20.0 | 20.6 | 14.5 | 30.3 | **51.6** | 49.3 |
| 2 MB | 17.4 | 17.5 | 13.0 | 25.7 | **40.5** | 41.5 |
| 8 MB | 17.1 | 14.7 | 11.1 | 25.3 | **37.0** | 38.7 |
| 32 MB | 17.7 | 14.2 | 10.6 | 23.7 | 19.7 | 20.0 |
| 36 MB | 16.5 | 14.4 | 9.7 | 23.5 | 20.0 | 20.6 |

- 相对 memcpy：4×64B 展开 = 2.58x / 2.33x / 2.16x / 1.11x / 1.21x。
- **单向量（1×64B）全面输给 memcpy**（0.59–0.75x）——展开度是关键，不是"用了 SME 就快"。
- NT 存储（`stnt1b`）无额外收益。
- ≥32 MB 受 DRAM 带宽限制，退化到 ~1.1x。

### 4.2 单次拷贝延迟分布（隔离；dst 未对齐 +3；ticks，1 tick = 52.1 ns）

固定成本：

| 项 | min | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| 空测量（tick();tick()） | 0 | 0 | 0 | 1 | 1 |
| `smstart;smstop`（进 C 调用，不搬数据） | 4 | **5** | 6 | 6 | 686 |

per-copy：

| size | memcpy p50 | neon p50 | sme 1×64B p50 | sme 4×64B p50 |
|---|---|---|---|---|
| 1 KB | 4 | 3 | 7 | 17 |
| 4 KB | 11 | 8 | 14 | 21 |
| 16 KB | 21–37 | 25 | 13 | 15 |
| 64 KB | 73 | 102 | 50 | **44** |
| 256 KB | 290 | 405 | 195 | **160** |

- ≥16KB 时 SME 的分布是**整体左移且很紧**（p50≈p90≈p99±1 tick），不是靠长尾取胜。
- 所有实现的 `max` 都在 700–2500 ticks（36–130 µs）——抢占噪声，与代码无关。

### 4.3 缓存驻留矩阵（同轮轮转，四种后端）

resident = 拷贝前先读一遍源；evicted = 每次拷贝前用 24 MB sweep 冲掉 cache。

| size | 状态 | memcpy p50 | neon64 p50 | sme 4×64B p50 | sme+PRFM p50 |
|---|---|---|---|---|---|
| 16 KB | resident | ~25 | ~25 | **17** | 19 |
| 16 KB | evicted | 28 | 28 | **99** | 102 |
| 64 KB | resident | 17 | 19 | **13** | 18 |
| 64 KB | evicted | 60 | 91 | **122** | 155 |
| 256 KB | resident | 69 | 89 | **47** | 64 |
| 256 KB | evicted | 233 | 311 | 253 | 366 |

- **驻留时 SME 赢（1.3–1.5x，带宽口径 2.2–2.6x）；源不在 cache 时 SME 崩（16KB 慢 3.5x，64KB 慢 2x）。**
- 256KB 冷数据打平：DRAM 带宽见顶，谁快都一样。
- **软件预取无效**：`prfm pldl1keep` 加入 SME 循环后反而略慢（102 vs 99、155 vs 122）——SME 的 load 流不吃 L1 预取提示，miss 即 64B 停等，缺少有效 MLP。

### 4.4 真实解码 in-situ（同一调用点，四后端可切）

| 语料 | 调用数 | sme 总 ticks | memcpy 总 ticks | sme p50/p90 | memcpy p50/p90 |
|---|---|---|---|---|---|
| rep.bin.zst（16 MB，32KB 周期） | 393 | 22122 | 18059 | 27 / 105 | 34 / 50 |
| rep64.bin.zst（64 MB，1MB 周期） | 701 | 67124 | 32132 | 114 / 141 | 40 / 82 |
| text.tar.zst（真实文本） | 37 | 1648 | 264 | 5 / 99 | 6 / 13 |

- 拷贝占解码时间：`rep.bin` ≈ 20–35%（所以这里拷贝值得优化）；`rep64` ≈ 3–8%；`text.tar` <0.5%。
- in-situ 里 SME 的分布出现**右肩**（30% 的调用落在 3.3–6.6 µs = 64–127 ticks），正好对应 4.3 里 "evicted" 档的 99–122 ticks。
- 端到端（同一二进制切换门控，同轮 min）：`rep.bin` −5%（4/4 轮一致）；`rep64` 方向随负载漂移；`text.tar` ±1%；`rand40`（raw 块）0 命中——raw 块走 `extend_from_reader` 直读，不经过任何拷贝点。

## 五、机制

SME streaming 模式的 load/store 走的不是核心的 L1 快路径（与 Apple M4 上"SME 是簇级协处理器、只经 L2 交换数据"的公开结论一致）。表现：

- 数据驻留 → 64B/指令的宽度优势兑现；
- 数据不驻留 → 每次 64B 加载停等一次 miss 延迟，且 `prfm` 无效、MLP 有限 → 比核心的 16B 流式拷贝差 2–3.5 倍。

解码器的匹配源分布在整个 ring buffer 窗口（MB 级），不可能全驻留；熵解码/查表又持续挤占 cache。所以"冷拷贝"必然占相当比例，SME 的分布因此变双峰、p90 爆炸。

## 六、建议

1. 默认关闭，只作为设备侧实验开关（`RUZSTD_SME_MIN` 不设即惰性，零行为/零性能影响）。
2. 若将来要启用，判据至少是：单次 ≥16KB **且** 源疑似驻留（例如倍增拷贝里刚写出的段、或 offset 很小的匹配）。
3. 不要在 x86 runner 上期待验证：本项目 perf 判据在 CI（`perf.yml`，x86_64）；可选 arm64 runner 是 Neoverse（SVE2 256-bit，无 SME）。SME 路径只能设备本地实测。
4. 与本仓库 CI 契约的冲突（nightly + 手写 asm + 平台专用）也是不引入默认路径的理由。

## 七、复现

```sh
# 1) 编 C 静态库（aarch64）
clang-23 -O3 -march=armv9.2-a+sme2 -c sme/ruzstd_sme_copy.c -o ruzstd_sme_copy.o
llvm-ar rcs libruzstd_sme_copy.a ruzstd_sme_copy.o

# 2) 带钩子编 Rust（钩子默认惰性）
RUSTFLAGS="-L native=$PWD/sme -l static=ruzstd_sme_copy" cargo build --release

# 3) 门控 / 统计 / 后端切换
RUZSTD_SME_MIN=4096 RUZSTD_SME_STATS=1 RUZSTD_SME_BACKEND=memcpy  ./zbench decode <file.zst> 1
RUZSTD_SME_MIN=4096 RUZSTD_SME_STATS=1 RUZSTD_SME_BACKEND=sme     ./zbench decode <file.zst> 1
```

隔离微基准（`experiments/sme-copy/`）：`sme_bw`（带宽）、`sme_lat`（延迟分布）、`sme_align`（对齐敏感度）、`sme_cachestate` / `sme_cold`（驻留矩阵）、`pmc_probe2`（时钟可用性）。

## 附：证据文件

| 文件 | 内容 |
|---|---|
| `experiments/sme-copy/ruzstd_sme_copy.c` | C 内核（4 种变体 + 计数器） |
| `experiments/sme-copy/sme_bw.c` | 带宽对比（含 NT 存储） |
| `experiments/sme-copy/sme_lat.c` | 单次拷贝延迟分布 |
| `experiments/sme-copy/sme_cachestate.c` | 缓存状态矩阵 |
| `experiments/sme-copy/sme_cold.c` | 驻留/驱逐轮转对比 |
| `experiments/sme-copy/sme_align.c` | 对齐敏感度 |
| `experiments/sme-copy/pmc_probe2.c` | 周期计数可用性探针 |
| `experiments/sme-copy/ruzstd-sme-hook.patch` | Rust 侧钩子与三处接线 |
