//! Utilities and representations for a frame header.
use crate::bit_io::BitWriter;
use crate::common::MAGIC_NUM;
use crate::encoding::util::{find_min_size, minify_val};
use alloc::vec::Vec;

/// A header for a single Zstandard frame.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header>
#[derive(Debug)]
pub struct FrameHeader {
    /// Optionally, the original (uncompressed) size of the data within the frame in bytes.
    /// If not present, `window_size` must be set.
    pub frame_content_size: Option<u64>,
    /// If set to true, data must be regenerated within a single
    /// continuous memory segment.
    pub single_segment: bool,
    /// If set to true, a 32 bit content checksum will be present
    /// at the end of the frame.
    pub content_checksum: bool,
    /// If a dictionary ID is provided, the ID of that dictionary.
    pub dictionary_id: Option<u64>,
    /// The minimum memory buffer required to compress a frame. If not present,
    /// `single_segment` will be set to true. If present, this value must be greater than 1KB
    /// and less than 3.75TB. Encoders should not generate a frame that requires a window size larger than
    /// 8mb.
    pub window_size: Option<u64>,
}

impl FrameHeader {
    /// Writes the serialized frame header into the provided buffer.
    ///
    /// The returned header *does include* a frame header descriptor.
    pub fn serialize(self, output: &mut Vec<u8>) {
        vprintln!("Serializing frame with header: {self:?}");
        // https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header
        // Magic Number:
        output.extend_from_slice(&MAGIC_NUM.to_le_bytes());

        // `Frame_Header_Descriptor`:
        output.push(self.descriptor());

        // `Window_Descriptor
        // TODO: https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#window_descriptor
        if !self.single_segment {
            if let Some(window_size) = self.window_size {
                let log = window_size.next_power_of_two().ilog2();
                let exponent = if log > 10 { log - 10 } else { 1 } as u8;
                output.push(exponent << 3);
            }
        }

        if let Some(id) = self.dictionary_id {
            output.extend(minify_val(id));
        }

        if let Some(frame_content_size) = self.frame_content_size {
            output.extend(fcs_bytes(frame_content_size, self.single_segment));
        }
    }

    /// Generate a serialized frame header descriptor for the frame header.
    ///
    /// https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header_descriptor
    fn descriptor(&self) -> u8 {
        let mut bw = BitWriter::new();
        // A frame header starts with a frame header descriptor.
        // It describes what other fields are present
        // https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_header_descriptor
        // Writing the frame header descriptor:
        // `Frame_Content_Size_flag`:
        // The Frame_Content_Size_flag specifies if
        // the Frame_Content_Size field is provided within the header.
        // If the `Single_Segment_flag` is set and this value is zero,
        // the size of the FCS field is 1 byte.
        // Otherwise, the FCS field is omitted.
        // | Value | Size of field (Bytes)
        // | 0     | 0 or 1
        // | 1     | 2
        // | 2     | 4
        // | 3     | 8
        // 字段长度与 flag 的对应关系完全按 C 的 `fcsCode` 语义取（见 [`fcs_field_size`]）：
        // 65592..=65791 走 2 字节（-256 偏移），>= 0xFFFFFFFF 走 8 字节。

        // `Dictionary_ID_flag`:
        if let Some(id) = self.dictionary_id {
            let flag_value: u8 = match find_min_size(id) {
                0 => 0,
                1 => 1,
                2 => 2,
                4 => 3,
                _ => panic!(),
            };
            bw.write_bits(flag_value, 2);
        } else {
            // A `Dictionary_ID` was not provided
            bw.write_bits(0u8, 2);
        }

        // `Content_Checksum_flag`:
        if self.content_checksum {
            bw.write_bits(1u8, 1);
        } else {
            bw.write_bits(0u8, 1);
        }

        // `Reserved_bit`:
        // This value must be zero
        bw.write_bits(0u8, 1);

        // `Unused_bit`:
        // An encoder compliant with this spec must set this bit to zero
        bw.write_bits(0u8, 1);

        // `Single_Segment_flag`:
        // If this flag is set, data must be regenerated within a single continuous memory segment,
        // and the `Frame_Content_Size` field must be present in the header.
        // If this flag is not set, the `Window_Descriptor` field must be present in the frame header.
        if self.single_segment {
            assert!(self.frame_content_size.is_some(), "if the `single_segment` flag is set to true, then a frame content size must be provided");
            bw.write_bits(1u8, 1);
        } else {
            assert!(
                self.window_size.is_some(),
                "if the `single_segment` flag is set to false, then a window size must be provided"
            );
            bw.write_bits(0u8, 1);
        }

        if let Some(frame_content_size) = self.frame_content_size {
            let flag_value: u8 = match fcs_field_size(frame_content_size, self.single_segment) {
                1 | 0 => 0,
                2 => 1,
                4 => 2,
                _ => 3,
            };

            bw.write_bits(flag_value, 2);
        } else {
            // `Frame_Content_Size` was not provided
            bw.write_bits(0u8, 2);
        }

        bw.dump()[0]
    }
}

/// The size in bytes of the `Frame_Content_Size` field, following the C reference encoder's
/// `fcsCode` computation.
///
/// C (`ZSTD_writeFrameHeader` in `lib/compress/zstd_compress.c`):
/// ```text
/// fcsCode = (pledgedSrcSize>=256) + (pledgedSrcSize>=65536+256) + (pledgedSrcSize>=0xFFFFFFFF)
/// ```
/// which selects 1, 2, 4 or 8 field bytes. `fcsCode == 0` means the field is omitted (0 bytes)
/// in a multi-segment frame, and is a single byte in a single-segment frame.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_content_size>
fn fcs_field_size(frame_content_size: u64, single_segment: bool) -> usize {
    let fcs_code = u32::from(frame_content_size >= 256)
        + u32::from(frame_content_size >= 65536 + 256)
        + u32::from(frame_content_size >= 0xFFFF_FFFF);

    match fcs_code {
        0 if single_segment => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    }
}

/// Serialize the `Frame_Content_Size` field exactly like the C reference encoder.
///
/// Identical to [`minify_val`], but it implements the following edge case:
///
/// > When FCS_Field_Size is 1, 4 or 8 bytes, the value is read directly. When FCS_Field_Size is 2, the offset of 256 is added.
///
/// A field size of 0 (multi-segment frame with a content size below 256) emits no bytes.
///
/// <https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md#frame_content_size>
fn fcs_bytes(val: u64, single_segment: bool) -> Vec<u8> {
    let new_size = fcs_field_size(val, single_segment);
    if new_size == 0 {
        return Vec::new();
    }

    let mut val = val;
    if new_size == 2 {
        val -= 256;
    }
    val.to_le_bytes()[0..new_size].to_vec()
}

#[cfg(test)]
mod tests {
    use super::FrameHeader;
    use crate::decoding::frame::{read_frame_header, FrameDescriptor};
    use alloc::vec::Vec;

    #[test]
    fn frame_header_descriptor_decode() {
        let header = FrameHeader {
            frame_content_size: Some(1),
            single_segment: true,
            content_checksum: false,
            dictionary_id: None,
            window_size: None,
        };
        let descriptor = header.descriptor();
        let decoded_descriptor = FrameDescriptor(descriptor);
        assert_eq!(decoded_descriptor.frame_content_size_bytes().unwrap(), 1);
        assert!(!decoded_descriptor.content_checksum_flag());
        assert_eq!(decoded_descriptor.dictionary_id_bytes().unwrap(), 0);
    }

    #[test]
    fn frame_header_decode() {
        let header = FrameHeader {
            frame_content_size: Some(1),
            single_segment: true,
            content_checksum: false,
            dictionary_id: None,
            window_size: None,
        };

        let mut serialized_header = Vec::new();
        header.serialize(&mut serialized_header);
        let parsed_header = read_frame_header(serialized_header.as_slice()).unwrap().0;
        assert!(parsed_header.dictionary_id().is_none());
        assert_eq!(parsed_header.frame_content_size(), 1);
    }

    #[test]
    #[should_panic]
    fn catches_single_segment_no_fcs() {
        let header = FrameHeader {
            frame_content_size: None,
            single_segment: true,
            content_checksum: false,
            dictionary_id: None,
            window_size: Some(1),
        };

        let mut serialized_header = Vec::new();
        header.serialize(&mut serialized_header);
    }

    #[test]
    #[should_panic]
    fn catches_single_segment_no_winsize() {
        let header = FrameHeader {
            frame_content_size: Some(7),
            single_segment: false,
            content_checksum: false,
            dictionary_id: None,
            window_size: None,
        };

        let mut serialized_header = Vec::new();
        header.serialize(&mut serialized_header);
    }
}
