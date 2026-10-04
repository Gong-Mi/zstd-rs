//! 多帧流（= 分块/多进程压缩产物）的消费保证。
//!
//! 分块压缩的形状：每个进程压自己的分块得到**独立的帧**，产物按序拼接成一个流。
//! 这里把四条保证固化成测试：
//!   1. 拼接流能被一次读完，且等于各分块按序拼接；
//!   2. 每一帧可单独解码（随机访问 / 跳帧）；
//!   3. C（zstd crate）与 ruzstd 双向互操作；
//!   4. 同一分块在不同压缩器实例里编出的字节相同（跨进程确定性的进程内等价物）。
//!
//! 外加两条边界：skippable 帧被跳过；截断的尾部报错而不是静默丢数据。

// 需要 std（`read_to_end` 来自 std 的 Read）与 hash（编码侧）；无默认特性组合下整文件门掉，
// 与 cross_validate.rs 同一模式。
#![cfg(all(feature = "std", feature = "hash"))]

use std::io::Read;

use ruzstd::decoding::{FrameDecoder, MultiFrameDecoder, StreamingDecoder};
use ruzstd::encoding::{compress_to_vec, CompressionLevel};

fn gen_data(seed: u64, size: usize, period: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut base = Vec::with_capacity(period);
    for _ in 0..period {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        base.push((state & 0xff) as u8);
    }
    base.iter().copied().cycle().take(size).collect()
}

fn rs_encode(data: &[u8]) -> Vec<u8> {
    compress_to_vec(data, CompressionLevel::Fastest)
}

fn read_all_rs(data: &[u8]) -> Vec<u8> {
    let mut dec = StreamingDecoder::new(data).unwrap();
    let mut out = Vec::new();
    dec.read_to_end(&mut out).unwrap();
    out
}

fn multi_frame_read(data: &[u8]) -> Vec<u8> {
    let mut dec = MultiFrameDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).unwrap();
    out
}

/// 1+2+4：分块独立压缩（模拟 N 个进程）→ 拼接 → 一次读完；每帧可独立解；压缩确定性
#[test]
fn chunked_multi_frame_roundtrip_and_random_access() {
    let chunks: Vec<Vec<u8>> = (0..4)
        .map(|i| gen_data(i as u64 * 7 + 1, 300_000 + i * 997, 512))
        .collect();

    let frames: Vec<Vec<u8>> = chunks.iter().map(|c| rs_encode(c)).collect();

    // 4：同一分块在两个独立压缩器实例里编出的字节必须相同（跨进程确定性的进程内等价物）
    assert_eq!(frames[2], rs_encode(&chunks[2]), "同一分块两次压缩结果不同");

    // 拼接成一个流
    let mut stream = Vec::new();
    for f in &frames {
        stream.extend_from_slice(f);
    }

    // 1：一次读完 == 各分块按序拼接
    let mut expected = Vec::new();
    for c in &chunks {
        expected.extend_from_slice(c);
    }
    assert_eq!(multi_frame_read(&stream), expected);

    // 2：每一帧可单独解码（随机访问）
    for (frame, chunk) in frames.iter().zip(&chunks) {
        assert_eq!(read_all_rs(frame), *chunk);
    }

    // 单帧 API 的既有语义（只吃第一帧）不变 —— 记录现状，别静默改变行为
    assert_eq!(
        read_all_rs(&stream),
        chunks[0],
        "StreamingDecoder 应只解第一帧"
    );
}

/// 3：与 C 的双向互操作
#[test]
fn c_interop_both_directions() {
    let chunks: Vec<Vec<u8>> = (0..3)
        .map(|i| gen_data(i as u64 * 13 + 5, 200_000 + i * 31, 256))
        .collect();
    let mut expected = Vec::new();
    for c in &chunks {
        expected.extend_from_slice(c);
    }

    // C 编多帧 → ruzstd MultiFrameDecoder 解
    let mut c_stream = Vec::new();
    for c in &chunks {
        c_stream.extend_from_slice(&zstd::encode_all(&c[..], 1).unwrap());
    }
    assert_eq!(multi_frame_read(&c_stream), expected);

    // ruzstd 编多帧 → C 解
    let mut rs_stream = Vec::new();
    for c in &chunks {
        rs_stream.extend_from_slice(&rs_encode(c));
    }
    assert_eq!(zstd::decode_all(&rs_stream[..]).unwrap(), expected);
}

/// skippable 帧（以及夹在数据帧之间的 skippable 帧）被跳过
#[test]
fn skippable_frames_are_skipped() {
    let a = gen_data(11, 120_000, 300);
    let b = gen_data(12, 90_000, 700);
    let mut stream = Vec::new();
    // magic 0x184D2A50 + u32 长度 + 载荷
    stream.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18, 4, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF]);
    stream.extend_from_slice(&rs_encode(&a));
    stream.extend_from_slice(&[0x51, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3]);
    stream.extend_from_slice(&rs_encode(&b));

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(multi_frame_read(&stream), expected);
}

/// 空流：干净返回 0 字节
#[test]
fn empty_stream_is_clean_eof() {
    assert!(multi_frame_read(&[]).is_empty());
}

/// 截断：半截帧头 / 被砍掉的帧体都必须报错，不能静默返回部分数据
#[test]
fn truncated_tail_is_an_error() {
    let chunk = gen_data(21, 150_000, 400);
    let frame = rs_encode(&chunk);

    // 半截帧头（只有 2 字节魔数）
    let mut half_header = frame.clone();
    half_header.extend_from_slice(&[0x28, 0xB5]);
    let mut dec = MultiFrameDecoder::new(&half_header[..]);
    let mut out = Vec::new();
    assert!(dec.read_to_end(&mut out).is_err(), "半截帧头必须报错");

    // 帧体被砍掉 3 字节
    let mut cut = frame.clone();
    cut.truncate(cut.len() - 3);
    let mut dec = MultiFrameDecoder::new(&cut[..]);
    let mut out = Vec::new();
    assert!(dec.read_to_end(&mut out).is_err(), "截断的帧体必须报错");
}

/// 与仓库既有的切片版 `FrameDecoder::decode_all` 同结果（两条多帧消费路径互相印证）
#[test]
fn agrees_with_frame_decoder_decode_all() {
    let chunks: Vec<Vec<u8>> = (0..2)
        .map(|i| gen_data(i as u64 * 17 + 3, 80_000 + i * 13, 128))
        .collect();
    let mut stream = Vec::new();
    let mut expected = Vec::new();
    for c in &chunks {
        stream.extend_from_slice(&rs_encode(c));
        expected.extend_from_slice(c);
    }

    let mut out = vec![0u8; expected.len()];
    let written = FrameDecoder::new().decode_all(&stream, &mut out).unwrap();
    assert_eq!(written, expected.len());
    assert_eq!(out, expected);
    assert_eq!(multi_frame_read(&stream), expected);
}
