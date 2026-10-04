//! Decoder for zstd streams that contain **more than one frame**.
//!
//! [super::StreamingDecoder] only consumes a single frame (see its documentation and
//! <https://github.com/KillingSpark/zstd-rs/issues/57>), while the format allows an archive to be a
//! concatenation of independent frames — which is exactly what chunked / multi-process compression
//! produces: every worker compresses its own chunk into its own frame and the results are appended.
//!
//! This decoder consumes all of them in one pass, skipping *skippable* frames on the way.
//!
//! ```no_run
//! use std::io::Read;
//! use ruzstd::decoding::MultiFrameDecoder;
//!
//! let archive: &[u8] = todo!("a concatenation of zstd frames");
//! let mut out = Vec::new();
//! MultiFrameDecoder::new(archive).read_to_end(&mut out).unwrap();
//! ```

use crate::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use crate::decoding::{BlockDecodingStrategy, FrameDecoder};
use crate::io::{Error, ErrorKind, Read};

/// [Read] adapter that consumes every frame of a multi-frame zstd stream.
///
/// Frames are decoded strictly in order and each frame's decoded bytes are handed out before the
/// next frame is started, so the output is the concatenation of the payloads.
///
/// A trailing partial frame header (< 4 bytes) is reported as an error instead of being treated as
/// a clean end of stream: silently dropping the tail of an archive would hide truncated input.
pub struct MultiFrameDecoder<READ: Read> {
    source: PeekReader<READ>,
    decoder: FrameDecoder,
    /// 已经为当前帧读过帧头
    in_frame: bool,
    /// 流已完整消费（干净结束）
    finished: bool,
}

impl<READ: Read> MultiFrameDecoder<READ> {
    /// Create a decoder for a stream that may contain any number of frames.
    pub fn new(source: READ) -> MultiFrameDecoder<READ> {
        MultiFrameDecoder {
            source: PeekReader::new(source),
            decoder: FrameDecoder::new(),
            in_frame: false,
            finished: false,
        }
    }

    /// A reference to the underlying reader.
    pub fn get_ref(&self) -> &READ {
        self.source.get_ref()
    }

    /// A mutable reference to the underlying reader.
    ///
    /// Reading from it directly is only sound between frames; prefer consuming this decoder.
    pub fn get_mut(&mut self) -> &mut READ {
        self.source.get_mut()
    }

    /// Unwrap this decoder, returning the underlying reader.
    pub fn into_inner(self) -> READ {
        self.source.into_inner()
    }

    /// 开始下一帧；`Ok(false)` = 流干净结束（读到 EOF，且没有半截帧头）
    fn start_next_frame(&mut self) -> Result<bool, Error> {
        // 先自己读帧头魔数，才能把"干净 EOF"与"截断"分开
        let got = self.source.peek_magic()?;
        match got {
            0 => return Ok(false),
            4 => {}
            // 半截帧头：明确报错，避免静默丢尾
            _ => return Err(truncated_frame_header()),
        }
        loop {
            match self.decoder.reset(&mut self.source) {
                Ok(()) => return Ok(true),
                Err(FrameDecoderError::ReadFrameHeaderError(
                    ReadFrameHeaderError::SkipFrame { length, .. },
                )) => {
                    // skippable 帧：跳过载荷后继续找下一帧
                    self.skip_bytes(length as u64)?;
                    match self.source.peek_magic()? {
                        0 => return Ok(false),
                        4 => continue,
                        _ => return Err(truncated_frame_header()),
                    }
                }
                Err(e) => return Err(frame_error_to_io(e)),
            }
        }
    }

    fn skip_bytes(&mut self, mut remaining: u64) -> Result<(), Error> {
        let mut scratch = [0u8; 512];
        while remaining > 0 {
            let want = core::cmp::min(remaining, scratch.len() as u64) as usize;
            self.source.read_exact(&mut scratch[..want])?;
            remaining -= want as u64;
        }
        Ok(())
    }
}

impl<READ: Read> Read for MultiFrameDecoder<READ> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.in_frame {
                if self.finished {
                    return Ok(0);
                }
                if !self.start_next_frame()? {
                    self.finished = true;
                    return Ok(0);
                }
                self.in_frame = true;
            }

            // 与 StreamingDecoder 相同的驱动方式：把当前帧解到"至少能收集一次"为止
            while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                self.decoder
                    .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                    .map_err(frame_error_to_io)?;
            }

            let read = self.decoder.read(buf)?;
            if read > 0 {
                return Ok(read);
            }
            // 当前帧已排空 → 下一轮处理下一帧
            self.in_frame = false;
        }
    }
}

/// 流在帧头处结束（1~3 字节残留）时报的错，而不是当成正常 EOF
fn truncated_frame_header() -> Error {
    #[cfg(feature = "std")]
    {
        Error::new(
            ErrorKind::UnexpectedEof,
            "truncated frame header in multi-frame zstd stream",
        )
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(
            ErrorKind::UnexpectedEof,
            alloc::boxed::Box::new("truncated frame header in multi-frame zstd stream"),
        )
    }
}

fn frame_error_to_io(e: FrameDecoderError) -> Error {
    #[cfg(feature = "std")]
    {
        Error::other(e)
    }
    #[cfg(not(feature = "std"))]
    {
        Error::new(ErrorKind::Other, alloc::boxed::Box::new(e))
    }
}

/// 允许"先看出 4 字节魔数、再原样交给 FrameDecoder"的读取包装
struct PeekReader<READ: Read> {
    peeked: [u8; 4],
    peeked_len: u8,
    pos: u8,
    inner: READ,
}

impl<READ: Read> PeekReader<READ> {
    fn new(inner: READ) -> Self {
        PeekReader {
            peeked: [0; 4],
            peeked_len: 0,
            pos: 0,
            inner,
        }
    }

    fn get_ref(&self) -> &READ {
        &self.inner
    }

    fn get_mut(&mut self) -> &mut READ {
        &mut self.inner
    }

    fn into_inner(self) -> READ {
        self.inner
    }

    /// 读取至多 4 字节到回放缓冲，返回实际读到的字节数（0 = 底层已 EOF）
    fn peek_magic(&mut self) -> Result<u8, Error> {
        self.peeked_len = 0;
        self.pos = 0;
        while (self.peeked_len as usize) < self.peeked.len() {
            let n = self.inner.read(&mut self.peeked[self.peeked_len as usize..])?;
            if n == 0 {
                break;
            }
            self.peeked_len += n as u8;
        }
        Ok(self.peeked_len)
    }
}

impl<READ: Read> Read for PeekReader<READ> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if (self.pos as usize) < (self.peeked_len as usize) {
            let available = (self.peeked_len - self.pos) as usize;
            let n = core::cmp::min(buf.len(), available);
            buf[..n].copy_from_slice(&self.peeked[self.pos as usize..self.pos as usize + n]);
            self.pos += n as u8;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}
