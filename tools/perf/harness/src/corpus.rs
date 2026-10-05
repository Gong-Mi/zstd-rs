// Fixed-seed corpus recipes retained from the existing perf_ab example.
use rand::{Rng, SeedableRng};

pub(super) fn gen_text(size: usize) -> Vec<u8> {
    let words: [&[u8]; 16] = [
        b"the ", b"of ", b"and ", b"to ", b"in ", b"that ", b"he ", b"was ", b"it ", b"his ",
        b"with ", b"is ", b"for ", b"as ", b"had ", b"be ",
    ];
    let mut rng = rand::rngs::SmallRng::seed_from_u64(42);
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        buf.extend_from_slice(words[rng.gen_range(0..words.len())]);
    }
    buf.truncate(size);
    buf
}

pub(super) fn gen_random(size: usize) -> Vec<u8> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    buf
}

pub(super) fn gen_medium(size: usize) -> Vec<u8> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(99);
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        let tag: u32 = rng.gen_range(0..64);
        let len: u32 = rng.gen_range(8..256);
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        for _ in 0..len {
            if rng.gen_bool(0.6) {
                buf.push(rng.gen_range(0..16));
            } else {
                buf.push(rng.gen());
            }
        }
    }
    buf.truncate(size);
    buf
}

/// 仿真内核风格源码（pseudo-random，确定种子）：关键字/标识符/缩进/注释 +
/// 重复行，token 频率明显偏斜——比 16 词表的 text 语料接近真实 C 代码的熵结构。
/// 不是真数据，是"结构仿造"，用来替代"必须下载内核源码"这类重依赖语料。
pub(super) fn gen_src_like(size: usize) -> Vec<u8> {
    const KWS: [&[u8]; 22] = [
        b"static ",
        b"int ",
        b"struct ",
        b"const ",
        b"unsigned long ",
        b"void ",
        b"return ",
        b"if (",
        b") {\n",
        b"}\n",
        b"for (",
        b";\n",
        b" = ",
        b"->",
        b"/* ",
        b" */",
        b"\n\t",
        b"EXPORT_SYMBOL(",
        b"__init",
        b"#define ",
        b"#include <linux/",
        b">\n",
    ];
    const IDS: [&[u8]; 18] = [
        b"page",
        b"sk_buff",
        b"alloc",
        b"mutex_lock",
        b"vmalloc",
        b"inode",
        b"task_struct",
        b"list_head",
        b"spinlock",
        b"kmalloc",
        b"rcu_read_lock",
        b"dma_addr_t",
        b"workqueue",
        b"kmem_cache",
        b"jiffies",
        b"refcount",
        b"hlist_node",
        b"bio",
    ];
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xC0FFEE);
    let mut buf = Vec::with_capacity(size + 4096);
    let mut line_start = 0usize;
    while buf.len() < size {
        let r: u32 = rng.gen_range(0..100);
        if r < 45 {
            let k = KWS[rng.gen_range(0..KWS.len())];
            buf.extend_from_slice(k);
        } else if r < 80 {
            let i = IDS[rng.gen_range(0..IDS.len())];
            buf.extend_from_slice(i);
        } else if r < 92 {
            // 重复上一行（真实代码里大量重复的调用/声明）
            let l = buf.len().min(buf.len() - line_start);
            if l > 0 {
                let from = buf.len() - l;
                let take = l.min(120);
                buf.extend_from_within(from..from + take);
            }
        } else {
            buf.push(rng.gen_range(b'a'..=b'z'));
        }
        if buf.len() - line_start > 80 && rng.gen_bool(0.5) {
            buf.push(b'\n');
            line_start = buf.len();
        }
    }
    buf.truncate(size);
    buf
}

/// 仿真 ELF 二进制（pseudo-random，确定种子）：ELF 头 + 代码段字节分布
/// （x86-64 常见 opcode/ModRM 前缀）+ 零填充 + 字符串表 + 指针数组。
pub(super) fn gen_bin_like(size: usize) -> Vec<u8> {
    const OPS: &[u8] =
        b"\x48\x8b\x89\xe8\x0f\x1f\x00\x48\x89\xc7\xeb\x74\x75\x31\xc0\xff\x25\xc3\x55\x5d";
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xBADC0DE);
    let mut buf = Vec::with_capacity(size + 4096);
    buf.extend_from_slice(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    buf.extend_from_slice(&[0x02, 0x00, 0x3e, 0x00]); // ET_EXEC, x86-64
    while buf.len() < size {
        let r: u32 = rng.gen_range(0..100);
        if r < 40 {
            // 代码段：opcode 偏斜
            let n = rng.gen_range(4..24);
            for _ in 0..n {
                buf.push(OPS[rng.gen_range(0..OPS.len())]);
            }
        } else if r < 55 {
            // 对齐/填充：零串
            let n = rng.gen_range(8..64);
            buf.extend_from_slice(&vec![0u8; n]);
        } else if r < 70 {
            // 指针/地址数组：重复的高位字节 + 低熵尾巴
            let n = rng.gen_range(4..16);
            for _ in 0..n {
                buf.extend_from_slice(&[0x00, 0x00, 0x40, 0x00]);
                buf.extend_from_slice(&(rng.gen_range(0u32..0x0040_0000u32)).to_le_bytes());
            }
        } else if r < 88 {
            // 字符串表：短符号名
            let n = rng.gen_range(3..10);
            for _ in 0..n {
                buf.push(rng.gen_range(b'a'..=b'z'));
            }
            buf.push(0);
        } else {
            let n = rng.gen_range(1..40);
            for _ in 0..n {
                buf.push(rng.gen());
            }
        }
    }
    buf.truncate(size);
    buf
}

/// 从目录收集文件拼成语料（按路径排序，确定）。`kind` 决定收哪些文件：
/// `src` = 源码/文本类扩展名；`bin` = ELF 二进制（按魔数判断，不看扩展名）。
/// 用于真实语料：Linux 内核源码/头文件、系统里的真实二进制。
fn collect_from_dir(dir: &str, kind: &str, size: usize) -> Vec<u8> {
    fn walk(dir: &std::path::Path, kind: &str, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name == "target" {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, kind, out);
                } else if kind == "bin" {
                    // ELF 魔数，避免把脚本/文本混进二进制语料
                    if let Ok(mut f) = std::fs::File::open(&p) {
                        let mut magic = [0u8; 4];
                        if std::io::Read::read_exact(&mut f, &mut magic).is_ok()
                            && &magic == b"\x7fELF"
                            && p.metadata().map(|m| m.len() >= 4096).unwrap_or(false)
                        {
                            out.push(p);
                        }
                    }
                } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
                    if matches!(
                        ext,
                        "rs" | "c" | "h" | "md" | "toml" | "json" | "yml" | "yaml" | "py" | "txt"
                    ) {
                        out.push(p);
                    }
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(std::path::Path::new(dir), kind, &mut files);
    files.sort();
    let mut buf = Vec::with_capacity(size + (1 << 16));
    for f in files {
        if buf.len() >= size {
            break;
        }
        if let Ok(data) = std::fs::read(&f) {
            buf.extend_from_slice(&data);
        }
    }
    if buf.is_empty() {
        eprintln!("WARNING: no files collected from {dir} (kind={kind})");
    }
    buf.truncate(size);
    buf
}

/// 真实数据腿：checkout 内按路径排序拼接源文件（确定、无网络）。
pub(super) fn gen_repo_sources(dir: &str, size: usize) -> Vec<u8> {
    collect_from_dir(dir, "src", size)
}
