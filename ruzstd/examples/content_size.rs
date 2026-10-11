//! 已知源大小时的一趟压缩出口（对齐 C `ZSTD_compress`/`ZSTD_compress2` 的 `pledgedSrcSize`）。
//!
//! 用法：`content_size <outdir> <input>...`
//!
//! 对每个输入写出两份帧：
//! - `<stem>.pledged.zst`：pledge 了源大小 → 帧头带 `Frame_Content_Size`
//!   （窗口装得下时按 C 判为 single-segment，不写窗口描述符）
//! - `<stem>.unpledged.zst`：不 pledge → 帧头无 `Frame_Content_Size`（旧行为，逐字节不变）
//!
//! 每份帧都用本库解回原文并校验帧头字段，输出可直接喂给 C `zstd -d` 做互操作取证。
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ruzstd::decoding::{read_frame_header_info, StreamingDecoder};
use ruzstd::encoding::{compress_to_vec, compress_to_vec_sized, CompressionLevel};

fn decode(frame: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    let mut source = frame;
    StreamingDecoder::new(&mut source)
        .expect("decoder init")
        .read_to_end(&mut decoded)
        .expect("decode");
    decoded
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    assert!(args.len() >= 2, "content_size <outdir> <input>...");
    let outdir = PathBuf::from(&args[0]);
    fs::create_dir_all(&outdir).expect("create outdir");

    let mut total = 0usize;
    for input in &args[1..] {
        let data = fs::read(input).expect("read input");
        let stem = Path::new(input)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "input".to_string());

        for (tag, pledged) in [("pledged", true), ("unpledged", false)] {
            let frame = if pledged {
                compress_to_vec_sized(
                    data.as_slice(),
                    data.len() as u64,
                    CompressionLevel::Fastest,
                )
            } else {
                compress_to_vec(data.as_slice(), CompressionLevel::Fastest)
            };
            let header = read_frame_header_info(frame.as_slice()).expect("parse own frame header");
            let header_len = header.header_size;
            let path = outdir.join(format!("{stem}.{tag}.zst"));
            fs::write(&path, &frame).expect("write frame");
            assert_eq!(decode(&frame), data, "{stem}/{tag} 自解码与原文不一致");
            println!(
                "OK {stem}/{tag} input={} encoded={} header_len={} fcs={} single_segment={}",
                data.len(),
                frame.len(),
                header_len,
                header.frame_content_size.unwrap_or(0),
                header.single_segment
            );
            total += 1;
        }
    }
    println!("TOTAL {total}");
}
