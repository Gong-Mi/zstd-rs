//! Regression cases for the distinction between byte entropy and LZ matches.
use ruzstd::decoding::FrameDecoder;
use ruzstd::encoding::{compress_to_vec, CompressionLevel};

fn random_bytes(size: usize) -> Vec<u8> {
    let mut seed = 7u64;
    (0..size)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

fn first_block_type(compressed: &[u8]) -> u8 {
    let mut source = compressed;
    FrameDecoder::new().reset(&mut source).unwrap();
    (source[0] >> 1) & 3
}

fn check_decode(compressed: &[u8], original: &[u8]) {
    assert_eq!(zstd::decode_all(compressed).unwrap(), original);
    let mut decoded = Vec::with_capacity(original.len());
    FrameDecoder::new()
        .decode_all_to_vec(compressed, &mut decoded)
        .unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn full_byte_alphabet_period_is_not_incompressible() {
    let original: Vec<u8> = (0u8..=255).cycle().take(128 * 1024).collect();
    let compressed = compress_to_vec(original.as_slice(), CompressionLevel::Fastest);
    check_decode(&compressed, &original);
    assert!(compressed.len() < original.len() / 4, "periodic input stored raw");
}

#[test]
fn high_entropy_prefix_does_not_hide_repeated_tail() {
    let mut original = random_bytes(4096);
    for (index, byte) in (0u8..=255).enumerate() {
        original[index] = byte;
    }
    original.extend_from_slice(&b"the repeated tail still has long distance matches ".repeat(2700));
    original.truncate(128 * 1024);
    let compressed = compress_to_vec(original.as_slice(), CompressionLevel::Fastest);
    check_decode(&compressed, &original);
    assert!(compressed.len() < original.len() / 4, "repeated tail skipped");
}

#[test]
fn random_blocks_still_fall_back_without_expansion() {
    for size in [1024, 65537, 128 * 1024, 256 * 1024] {
        let original = random_bytes(size);
        let compressed = compress_to_vec(original.as_slice(), CompressionLevel::Fastest);
        check_decode(&compressed, &original);
        assert_eq!(first_block_type(&compressed), 0, "random fixture must use raw");
        assert!(compressed.len() <= original.len() + 32);
    }
}
