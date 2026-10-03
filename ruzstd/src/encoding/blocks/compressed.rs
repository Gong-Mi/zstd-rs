use alloc::vec::Vec;

use crate::{
    bit_io::BitWriter,
    encoding::frame_compressor::CompressState,
    encoding::{Matcher, Sequence},
    fse::fse_encoder::{build_table_from_data, FSETable, State},
    huff0::huff0_encoder,
};

/// Entropy tables built by a trial block, committed only when it is emitted.
#[must_use = "commit updates only when the compressed block is emitted"]
#[derive(Default)]
pub(crate) struct EntropyUpdates {
    huffman: Option<huff0_encoder::HuffmanTable>,
    ll: Option<FSETable>,
    ml: Option<FSETable>,
    of: Option<FSETable>,
}

impl EntropyUpdates {
    pub(crate) fn commit<M: Matcher>(self, state: &mut CompressState<M>) {
        if let Some(table) = self.huffman {
            state.last_huff_table = Some(table);
        }
        if let Some(table) = self.ll {
            state.fse_tables.ll_previous = Some(table);
        }
        if let Some(table) = self.ml {
            state.fse_tables.ml_previous = Some(table);
        }
        if let Some(table) = self.of {
            state.fse_tables.of_previous = Some(table);
        }
    }
}

/// A block of [`crate::common::BlockType::Compressed`].
/// The returned table updates must be discarded if the caller emits raw instead.
pub(crate) fn compress_block<M: Matcher>(
    state: &mut CompressState<M>,
    output: &mut Vec<u8>,
) -> EntropyUpdates {
    let mut updates = EntropyUpdates::default();
    let mut literals_vec = Vec::new();
    let mut sequences = Vec::new();
    // 取出偏移历史到局部，避免与 `state.matcher` 的可变借用冲突（块尾写回）
    let mut offset_hist = state.offset_hist;
    state.matcher.start_matching(|seq| match seq {
        Sequence::Literals { literals } => literals_vec.extend_from_slice(literals),
        Sequence::Triple {
            literals,
            offset,
            match_len,
        } => {
            literals_vec.extend_from_slice(literals);
            sequences.push(crate::blocks::sequence_section::Sequence {
                ll: literals.len() as u32,
                ml: match_len as u32,
                of: encode_offset_value(offset as u32, literals.len() as u32, &mut offset_hist),
            });
        }
    });
    state.offset_hist = offset_hist;

    // literals section

    let mut writer = BitWriter::from(output);
    if literals_vec.len() > 1024 {
        if let Some(table) =
            compress_literals(&literals_vec, state.last_huff_table.as_ref(), &mut writer)
        {
            updates.huffman = Some(table);
        }
    } else {
        raw_literals(&literals_vec, &mut writer);
    }

    // sequences section

    if sequences.is_empty() {
        writer.write_bits(0u8, 8);
    } else {
        encode_seqnum(sequences.len(), &mut writer);

        // 复用判定用的"本块符号存在位图"（ll/ml/of 码值都 < 64，一趟算完）
        let (mut ll_present, mut ml_present, mut of_present) = (0u64, 0u64, 0u64);
        for seq in &sequences {
            let (ll_c, ml_c, of_c) = (
                encode_literal_length(seq.ll).0,
                encode_match_len(seq.ml).0,
                encode_offset(seq.of).0,
            );
            debug_assert!(ll_c < 64 && ml_c < 64 && of_c < 64);
            ll_present |= 1u64 << ll_c;
            ml_present |= 1u64 << ml_c;
            of_present |= 1u64 << of_c;
        }

        // Choose the tables：上一张覆盖本块全部符号就复用（RepeateLast），否则重建
        let ll_mode = choose_table(
            state.fse_tables.ll_previous.as_ref(),
            &state.fse_tables.ll_default,
            sequences.iter().map(|seq| encode_literal_length(seq.ll).0),
            9,
            ll_present,
        );
        let ml_mode = choose_table(
            state.fse_tables.ml_previous.as_ref(),
            &state.fse_tables.ml_default,
            sequences.iter().map(|seq| encode_match_len(seq.ml).0),
            9,
            ml_present,
        );
        let of_mode = choose_table(
            state.fse_tables.of_previous.as_ref(),
            &state.fse_tables.of_default,
            sequences.iter().map(|seq| encode_offset(seq.of).0),
            8,
            of_present,
        );

        writer.write_bits(encode_fse_table_modes(&ll_mode, &ml_mode, &of_mode), 8);

        encode_table(&ll_mode, &mut writer);
        encode_table(&of_mode, &mut writer);
        encode_table(&ml_mode, &mut writer);

        encode_sequences(
            &sequences,
            &mut writer,
            ll_mode.as_ref(),
            ml_mode.as_ref(),
            of_mode.as_ref(),
        );

        if let FseTableMode::Encoded(table) = ll_mode {
            updates.ll = Some(table)
        }
        if let FseTableMode::Encoded(table) = ml_mode {
            updates.ml = Some(table)
        }
        if let FseTableMode::Encoded(table) = of_mode {
            updates.of = Some(table)
        }
    }
    writer.flush();
    updates
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum FseTableMode<'a> {
    /// 格式里还有"预定义表"这一种模式；本实现始终走 新表/复用上一张 两条路，
    /// 保留该变体以对应格式定义。
    #[allow(dead_code)]
    Predefined(&'a FSETable),
    Encoded(FSETable),
    RepeateLast(&'a FSETable),
}

impl FseTableMode<'_> {
    pub fn as_ref(&self) -> &FSETable {
        match self {
            Self::Predefined(t) => t,
            Self::RepeateLast(t) => t,
            Self::Encoded(t) => t,
        }
    }
}

fn choose_table<'a>(
    previous: Option<&'a FSETable>,
    _default_table: &'a FSETable,
    data: impl Iterator<Item = u8>,
    max_log: u8,
    present: u64,
) -> FseTableMode<'a> {
    // 参考实现每块做一次 "新表 vs 复用上一张" 的判定；这里同样：上一张表存在且
    // **覆盖本块全部符号**才复用（覆盖性是正确性门，不是性能门——解码端遇到未覆盖
    // 符号会取到空状态表）。
    if let Some(prev) = previous {
        if prev.covers_present(present) {
            return FseTableMode::RepeateLast(prev);
        }
    }
    FseTableMode::Encoded(build_table_from_data(data, max_log, true))
}

fn encode_table(mode: &FseTableMode<'_>, writer: &mut BitWriter<&mut Vec<u8>>) {
    match mode {
        FseTableMode::Predefined(_) => {}
        FseTableMode::RepeateLast(_) => {}
        FseTableMode::Encoded(table) => table.write_table(writer),
    }
}

fn encode_fse_table_modes(
    ll_mode: &FseTableMode<'_>,
    ml_mode: &FseTableMode<'_>,
    of_mode: &FseTableMode<'_>,
) -> u8 {
    fn mode_to_bits(mode: &FseTableMode<'_>) -> u8 {
        match mode {
            FseTableMode::Predefined(_) => 0,
            FseTableMode::Encoded(_) => 2,
            FseTableMode::RepeateLast(_) => 3,
        }
    }
    mode_to_bits(ll_mode) << 6 | mode_to_bits(of_mode) << 4 | mode_to_bits(ml_mode) << 2
}

fn encode_sequences(
    sequences: &[crate::blocks::sequence_section::Sequence],
    writer: &mut BitWriter<&mut Vec<u8>>,
    ll_table: &FSETable,
    ml_table: &FSETable,
    of_table: &FSETable,
) {
    let sequence = sequences[sequences.len() - 1];
    let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
    let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
    let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);
    let mut ll_state: &State = ll_table.start_state(ll_code);
    let mut ml_state: &State = ml_table.start_state(ml_code);
    let mut of_state: &State = of_table.start_state(of_code);

    writer.write_bits(ll_add_bits, ll_num_bits);
    writer.write_bits(ml_add_bits, ml_num_bits);
    writer.write_bits(of_add_bits, of_num_bits);

    // encode backwards so the decoder reads the first sequence first
    if sequences.len() > 1 {
        for sequence in (0..=sequences.len() - 2).rev() {
            let sequence = sequences[sequence];
            let (ll_code, ll_add_bits, ll_num_bits) = encode_literal_length(sequence.ll);
            let (of_code, of_add_bits, of_num_bits) = encode_offset(sequence.of);
            let (ml_code, ml_add_bits, ml_num_bits) = encode_match_len(sequence.ml);

            {
                let next = of_table.next_state(of_code, of_state.index);
                let diff = of_state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                of_state = next;
            }
            {
                let next = ml_table.next_state(ml_code, ml_state.index);
                let diff = ml_state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                ml_state = next;
            }
            {
                let next = ll_table.next_state(ll_code, ll_state.index);
                let diff = ll_state.index - next.baseline;
                writer.write_bits(diff as u64, next.num_bits as usize);
                ll_state = next;
            }

            writer.write_bits(ll_add_bits, ll_num_bits);
            writer.write_bits(ml_add_bits, ml_num_bits);
            writer.write_bits(of_add_bits, of_num_bits);
        }
    }
    writer.write_bits(ml_state.index as u64, ml_table.table_size.ilog2() as usize);
    writer.write_bits(of_state.index as u64, of_table.table_size.ilog2() as usize);
    writer.write_bits(ll_state.index as u64, ll_table.table_size.ilog2() as usize);

    let bits_to_fill = writer.misaligned();
    if bits_to_fill == 0 {
        writer.write_bits(1u32, 8);
    } else {
        writer.write_bits(1u32, bits_to_fill);
    }
}

fn encode_seqnum(seqnum: usize, writer: &mut BitWriter<impl AsMut<Vec<u8>>>) {
    const UPPER_LIMIT: usize = 0xFFFF + 0x7F00;
    match seqnum {
        1..=127 => writer.write_bits(seqnum as u32, 8),
        128..=0x7FFF => {
            let upper = ((seqnum >> 8) | 0x80) as u8;
            let lower = seqnum as u8;
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        0x8000..=UPPER_LIMIT => {
            let encode = seqnum - 0x7F00;
            let upper = (encode >> 8) as u8;
            let lower = encode as u8;
            writer.write_bits(255u8, 8);
            writer.write_bits(upper, 8);
            writer.write_bits(lower, 8);
        }
        _ => unreachable!(),
    }
}

fn encode_literal_length(len: u32) -> (u8, u32, usize) {
    match len {
        0..=15 => (len as u8, 0, 0),
        16..=17 => (16, len - 16, 1),
        18..=19 => (17, len - 18, 1),
        20..=21 => (18, len - 20, 1),
        22..=23 => (19, len - 22, 1),
        24..=27 => (20, len - 24, 2),
        28..=31 => (21, len - 28, 2),
        32..=39 => (22, len - 32, 3),
        40..=47 => (23, len - 40, 3),
        48..=63 => (24, len - 48, 4),
        64..=127 => (25, len - 64, 6),
        128..=255 => (26, len - 128, 7),
        256..=511 => (27, len - 256, 8),
        512..=1023 => (28, len - 512, 9),
        1024..=2047 => (29, len - 1024, 10),
        2048..=4095 => (30, len - 2048, 11),
        4096..=8191 => (31, len - 4096, 12),
        8192..=16383 => (32, len - 8192, 13),
        16384..=32767 => (33, len - 16384, 14),
        32768..=65535 => (34, len - 32768, 15),
        65536..=131071 => (35, len - 65536, 16),
        131072.. => unreachable!(),
    }
}

fn encode_match_len(len: u32) -> (u8, u32, usize) {
    match len {
        0..=2 => unreachable!(),
        3..=34 => (len as u8 - 3, 0, 0),
        35..=36 => (32, len - 35, 1),
        37..=38 => (33, len - 37, 1),
        39..=40 => (34, len - 39, 1),
        41..=42 => (35, len - 41, 1),
        43..=46 => (36, len - 43, 2),
        47..=50 => (37, len - 47, 2),
        51..=58 => (38, len - 51, 3),
        59..=66 => (39, len - 59, 3),
        67..=82 => (40, len - 67, 4),
        83..=98 => (41, len - 83, 4),
        99..=130 => (42, len - 99, 5),
        131..=258 => (43, len - 131, 7),
        259..=514 => (44, len - 259, 8),
        515..=1026 => (45, len - 515, 9),
        1027..=2050 => (46, len - 1027, 10),
        2051..=4098 => (47, len - 2051, 11),
        4099..=8194 => (48, len - 4099, 12),
        8195..=16386 => (49, len - 8195, 13),
        16387..=32770 => (50, len - 16387, 14),
        32771..=65538 => (51, len - 32771, 15),
        65539..=131074 => (52, len - 65539, 16),
        131075.. => unreachable!(),
    }
}

/// 为 (实际偏移, 字面量长度) 选出最小的 Offset_Value，并按解码端 `do_offset_history`
/// 的规则更新偏移历史（1..=3 为重复偏移码，≥4 表示新偏移 = 值 - 3）。
///
/// 与解码端逐分支镜像：ll>0 时 1/2/3 → hist[0]/hist[1]/hist[2]；ll==0 时
/// 1/2/3 → hist[1]/hist[2]/hist[0]-1。历史更新也必须一致，否则后续序列的
/// 重复偏移码会解成别的偏移（cross_validate 的 "ruzstd 编码 → C 解码" 是验证门）。
fn encode_offset_value(actual_offset: u32, lit_len: u32, hist: &mut [u32; 3]) -> u32 {
    let value = if lit_len > 0 {
        if actual_offset == hist[0] {
            1
        } else if actual_offset == hist[1] {
            2
        } else if actual_offset == hist[2] {
            3
        } else {
            actual_offset + 3
        }
    } else if actual_offset == hist[1] {
        1
    } else if actual_offset == hist[2] {
        2
    } else if hist[0] > 0 && actual_offset == hist[0] - 1 {
        3
    } else {
        actual_offset + 3
    };

    if lit_len > 0 {
        match value {
            // 复用最近偏移：历史不变
            1 => {}
            2 => {
                hist[1] = hist[0];
                hist[0] = actual_offset;
            }
            _ => {
                hist[2] = hist[1];
                hist[1] = hist[0];
                hist[0] = actual_offset;
            }
        }
    } else {
        match value {
            // ll==0：解码端把 hist[1]/hist[2] 当作最近偏移；码 1 只做部分后移
            1 => {
                hist[1] = hist[0];
                hist[0] = actual_offset;
            }
            _ => {
                hist[2] = hist[1];
                hist[1] = hist[0];
                hist[0] = actual_offset;
            }
        }
    }
    value
}

fn encode_offset(len: u32) -> (u8, u32, usize) {
    let log = len.ilog2();
    let lower = len & ((1 << log) - 1);
    (log as u8, lower, log as usize)
}

fn raw_literals(literals: &[u8], writer: &mut BitWriter<&mut Vec<u8>>) {
    writer.write_bits(0u8, 2);
    writer.write_bits(0b11u8, 2);
    writer.write_bits(literals.len() as u32, 20);
    writer.append_bytes(literals);
}

#[repr(C)]
pub(crate) struct HuffmanBoundDiagnosticControl {
    prefix: [u8; 16],
    mode: core::sync::atomic::AtomicU8,
    suffix: [u8; 16],
}

// Diagnostic branch only: never merge this switch into the production PR.
#[used]
#[no_mangle]
pub(crate) static RUZSTD_HUFF_BOUND_DIAGNOSTIC: HuffmanBoundDiagnosticControl =
    HuffmanBoundDiagnosticControl {
        prefix: *b"RUZSTD_HUFFBOUND",
        mode: core::sync::atomic::AtomicU8::new(1),
        suffix: *b"_MODE_CONTROL_AB",
    };

pub(crate) fn diagnostic_huffman_bound_mode() -> u8 {
    RUZSTD_HUFF_BOUND_DIAGNOSTIC
        .mode
        .load(core::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn diagnostic_literal_buffer(size: usize) -> (usize, usize) {
    let literals: Vec<u8> = (0u8..=255).cycle().take(size).collect();
    let mut output = Vec::new();
    let mut writer = BitWriter::from(&mut output);
    assert!(compress_literals(&literals, None, &mut writer).is_none());
    writer.flush();
    (output.len(), output.capacity())
}

fn compress_literals(
    literals: &[u8],
    last_table: Option<&huff0_encoder::HuffmanTable>,
    writer: &mut BitWriter<&mut Vec<u8>>,
) -> Option<huff0_encoder::HuffmanTable> {
    let reset_idx = writer.index();

    // Same histogram and table construction as build_from_data, retained so
    // the chosen table's payload size can be checked without encoding it.
    let mut counts = [0usize; 256];
    let mut max_symbol = 0u8;
    for &symbol in literals {
        counts[symbol as usize] += 1;
        max_symbol = max_symbol.max(symbol);
    }
    let counts = &counts[..=max_symbol as usize];
    let new_encoder_table = huff0_encoder::HuffmanTable::build_from_counts(counts);

    let (encoder_table, new_table) = if let Some(_table) = last_table {
        if let Some(diff) = _table.can_encode(&new_encoder_table) {
            // TODO this is a very simple heuristic, maybe we should try to do better
            if diff > 5 {
                (&new_encoder_table, true)
            } else {
                (_table, false)
            }
        } else {
            (&new_encoder_table, true)
        }
    } else {
        (&new_encoder_table, true)
    };

    // Headers, a new table, jump-table bytes and stream end markers can only
    // increase this payload bound. The existing fallback compares the entire
    // encoded section with literals.len(), so this case must produce raw.
    // Diagnostic replacement: preserve the original trial and its allocation
    // shape. The runtime switch now controls only 8-bit symbol batching.
    let _payload_bits = core::hint::black_box(encoder_table.payload_bit_len(counts));

    if new_table {
        writer.write_bits(2u8, 2); // compressed literals type
    } else {
        writer.write_bits(3u8, 2); // treeless compressed literals type
    }

    let (size_format, size_bits) = match literals.len() {
        0..6 => (0b00u8, 10),
        6..1024 => (0b01, 10),
        1024..16384 => (0b10, 14),
        16384..262144 => (0b11, 18),
        _ => unimplemented!("too many literals"),
    };

    writer.write_bits(size_format, 2);
    writer.write_bits(literals.len() as u32, size_bits);
    let size_index = writer.index();
    writer.write_bits(0u32, size_bits);
    let index_before = writer.index();
    let mut encoder = huff0_encoder::HuffmanEncoder::new(encoder_table, writer);
    if size_format == 0 {
        encoder.encode(literals, new_table)
    } else {
        encoder.encode4x(literals, new_table)
    };
    let encoded_len = (writer.index() - index_before) / 8;
    writer.change_bits(size_index, encoded_len as u64, size_bits);
    let total_len = (writer.index() - reset_idx) / 8;

    // If encoded len is bigger than the raw literals we are better off just writing the raw literals here
    if total_len >= literals.len() {
        writer.reset_to(reset_idx);
        raw_literals(literals, writer);
        None
    } else if new_table {
        Some(new_encoder_table)
    } else {
        None
    }
}

#[cfg(test)]
mod literal_size_tests {
    use super::{compress_literals, raw_literals};
    use crate::bit_io::BitWriter;
    use crate::huff0::huff0_encoder::HuffmanTable;
    use alloc::vec::Vec;

    #[test]
    fn previous_table_missing_symbol_uses_new_table() {
        let previous = HuffmanTable::build_from_weights(&[2, 2, 0, 2, 2]);
        let literals: Vec<u8> = (0u8..=4).cycle().take(4097).collect();
        let current = HuffmanTable::build_from_data(&literals);
        assert!(previous.can_encode(&current).is_none());
        let mut expected = Vec::new();
        let mut writer = BitWriter::from(&mut expected);
        assert!(compress_literals(&literals, None, &mut writer).is_some());
        writer.flush();
        let mut output = Vec::new();
        let mut writer = BitWriter::from(&mut output);
        assert!(compress_literals(&literals, Some(&previous), &mut writer).is_some());
        writer.flush();
        assert_eq!(output, expected);
    }

    #[test]
    fn payload_bound_uses_selected_previous_table_and_preserves_prefix() {
        let mut weights = [2usize; 256];
        weights[0] = 3;
        weights[1] = 1;
        weights[2] = 1;
        let previous = HuffmanTable::build_from_weights(&weights);
        let literals: Vec<u8> = (0usize..65536)
            .map(|i| if i < 4096 { i as u8 } else { 0 })
            .collect();
        let current = HuffmanTable::build_from_data(&literals);
        assert_eq!(previous.can_encode(&current), Some(3));
        let mut counts = [0usize; 256];
        for &symbol in &literals {
            counts[symbol as usize] += 1;
        }
        assert_eq!(current.payload_bit_len(&counts), literals.len() * 8);
        assert!(previous.payload_bit_len(&counts) < literals.len() * 8);
        let mut output = Vec::new();
        let mut writer = BitWriter::from(&mut output);
        writer.write_bits(0x5Au8, 8);
        assert!(compress_literals(&literals, Some(&previous), &mut writer).is_none());
        writer.flush();
        assert_eq!(output[0], 0x5A);
        assert_eq!(output[1] & 3, 3); // Treeless: the previous table remains active.
        assert!(output.len() < literals.len());
        let mut output = Vec::new();
        let mut writer = BitWriter::from(&mut output);
        writer.write_bits(0x5Au8, 8);
        assert!(compress_literals(&literals, None, &mut writer).is_none());
        writer.flush();
        assert_eq!(output[0], 0x5A);
        assert_eq!(output[1] & 3, 0); // Fresh full-alphabet table must fall back raw.
    }

    #[test]
    fn full_alphabet_literals_preserve_raw_bytes_and_previous_table() {
        let previous_data: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let previous = HuffmanTable::build_from_data(&previous_data);
        for size in [1025, 4097, 65536, 131071] {
            let literals: Vec<u8> = (0u8..=255).cycle().take(size).collect();
            for table in [None, Some(&previous)] {
                let mut output = Vec::new();
                let mut writer = BitWriter::from(&mut output);
                assert!(compress_literals(&literals, table, &mut writer).is_none());
                writer.flush();
                let mut expected = Vec::new();
                let mut writer = BitWriter::from(&mut expected);
                raw_literals(&literals, &mut writer);
                writer.flush();
                assert_eq!(output, expected);
            }
        }
    }
}

#[cfg(test)]
mod offset_value_tests {
    use super::encode_offset_value;
    use crate::decoding::sequence_execution::do_offset_history;

    /// 编码侧选的码必须被解码端规则还原成同一个偏移，且两侧历史演进一致。
    /// 覆盖 ll>0 与 ll==0（后者的码→历史映射不同）以及新偏移路径。
    #[test]
    fn round_trip_against_decoder_rule() {
        let offsets = [1u32, 2, 3, 4, 5, 8, 16, 63, 64, 1000];
        for &start in &[[1u32, 4, 8], [4, 8, 1], [7, 7, 7], [2, 3, 5]] {
            for &ll in &[1u32, 5, 100] {
                for &ll0 in &[0u32] {
                    for &lit in &[ll, ll0] {
                        let mut eh = start;
                        let mut dh = start;
                        for (i, &off) in offsets.iter().enumerate() {
                            // 每 3 步插一个新偏移，保证历史被推动
                            let actual = if i % 3 == 2 { 17 + i as u32 } else { off };
                            let value = encode_offset_value(actual, lit, &mut eh);
                            let decoded = do_offset_history(value, lit, &mut dh);
                            assert_eq!(
                                decoded, actual,
                                "start={start:?} ll={lit} value={value} 应还原 {actual}"
                            );
                            assert_eq!(eh, dh, "历史演进必须一致 (start={start:?}, ll={lit})");
                        }
                    }
                }
            }
        }
    }
}
