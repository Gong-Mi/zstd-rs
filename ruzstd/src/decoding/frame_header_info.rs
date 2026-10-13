//! 帧头只读检视接口（对齐 C 的 `ZSTD_getFrameHeader`）。
//!
//! C 允许调用方在完整解码之前查询帧头：内容大小、窗口需求、字典 ID、校验和标志。
//! 该模块把内部帧头解析暴露成稳定的公开视图，供工具/调用方使用。

use super::errors::ReadFrameHeaderError;
use super::frame::read_frame_header;
use crate::io::Read;

/// 一个 zstd 帧的头部信息（不含帧体），字段语义见 RFC 8878 / zstd 格式文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeaderInfo {
    /// 帧体解压后的总字节数。帧头未携带 `Frame_Content_Size` 字段时为 `None`
    /// （对齐 C 的 `ZSTD_CONTENTSIZE_UNKNOWN`）。
    pub frame_content_size: Option<u64>,
    /// `Single_Segment_flag`：数据必须在单一连续内存段内重建，此时窗口描述符不出现。
    pub single_segment: bool,
    /// 帧尾是否带 32 位内容校验和（`Content_Checksum_flag`）。
    pub content_checksum: bool,
    /// 帧头携带的字典 ID（`Dictionary_ID`）。
    pub dictionary_id: Option<u32>,
    /// 解码该帧所需的最小缓冲（窗口大小）。
    ///
    /// 窗口描述符超出格式合法范围（小于 1KiB 或大于格式上限）时为 `None`；
    /// 这种帧在真正解码时会被拒绝。
    pub window_size: Option<u64>,
    /// 帧头本身占用的字节数，含 4 字节 magic 与帧头描述符。
    pub header_size: u8,
}

/// 读取并解析一个帧头，不消费帧体。
///
/// 对齐 C 的 `ZSTD_getFrameHeader`：可在完整解码前得知内容大小与窗口需求。
/// 遇到 skippable frame 时返回 [`ReadFrameHeaderError::SkipFrame`]，与解码路径一致。
pub fn read_frame_header_info<R: Read>(
    mut source: R,
) -> Result<FrameHeaderInfo, ReadFrameHeaderError> {
    let (header, header_size) = read_frame_header(&mut source)?;

    // `Frame_Content_Size` 字段是否存在由 descriptor 决定：
    // flag=0 且非 single_segment 时字段被省略，此时内部表示与“内容大小为 0”无法区分。
    let fcs_present = match header.descriptor.frame_content_size_flag() {
        0 => header.descriptor.single_segment_flag(),
        _ => true,
    };

    Ok(FrameHeaderInfo {
        frame_content_size: if fcs_present {
            Some(header.frame_content_size())
        } else {
            None
        },
        single_segment: header.descriptor.single_segment_flag(),
        content_checksum: header.descriptor.content_checksum_flag(),
        dictionary_id: header.dictionary_id(),
        window_size: header.window_size().ok(),
        header_size,
    })
}
