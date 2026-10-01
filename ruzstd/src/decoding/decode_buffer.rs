use crate::io::{Error, Read, Write};
use alloc::vec::Vec;
#[cfg(feature = "hash")]
use core::hash::Hasher;

use super::ringbuffer::RingBuffer;
use crate::decoding::errors::DecodeBufferError;

/// 直写后端的绑定状态：调用方给了连续 dst 时，解码直接写进它，
/// 不再"先写环形缓冲、再抽出"——省掉每个输出字节的第二次写与一次读。
struct DirectOut {
    /// 调用方缓冲的起点（仅在本帧解码期间有效，由 `set_direct_output` 的
    /// 调用方保证生命周期覆盖整个解码过程）。
    ptr: *mut u8,
    cap: usize,
    /// 已写入 dst 的字节数；同时是本帧可回溯窗口的长度。
    written: usize,
    /// 已通过 read/read_all 交代给调用方的字节数。
    drained: usize,
    /// 曾出现超出 dst 容量的写入尝试（调用方缓冲太小）。
    overflow: bool,
}

pub struct DecodeBuffer {
    buffer: RingBuffer,
    direct: Option<DirectOut>,
    pub dict_content: Vec<u8>,

    pub window_size: usize,
    total_output_counter: u64,
    #[cfg(feature = "hash")]
    pub hash: twox_hash::XxHash64,
}

// SAFETY: `direct` 只在 `decode_all` 的单次调用内绑定（绑定到解绑之间不执行用户
// 代码），因此不可能在持有裸指针的期间把解码器移到别的线程；指针指向的缓冲由
// `set_direct_output` 的 unsafe 契约保证生命周期与独占。
unsafe impl Send for DecodeBuffer {}

impl Read for DecodeBuffer {
    fn read(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        if let Some(d) = self.direct.as_mut() {
            // 直写：字节已经在调用方缓冲里，这里只交代进度。
            let n = d.written - d.drained;
            d.drained = d.written;
            return Ok(n);
        }
        let max_amount = self.can_drain_to_window_size().unwrap_or(0);
        let amount = max_amount.min(target.len());

        let mut written = 0;
        self.drain_to(amount, |buf| {
            target[written..][..buf.len()].copy_from_slice(buf);
            written += buf.len();
            (buf.len(), Ok(()))
        })?;
        Ok(amount)
    }
}

impl DecodeBuffer {
    pub fn new(window_size: usize) -> DecodeBuffer {
        DecodeBuffer {
            buffer: RingBuffer::new(),
            direct: None,
            dict_content: Vec::new(),
            window_size,
            total_output_counter: 0,
            #[cfg(feature = "hash")]
            hash: twox_hash::XxHash64::with_seed(0),
        }
    }

    pub fn reset(&mut self, window_size: usize) {
        self.window_size = window_size;
        self.direct = None;
        self.buffer.clear();
        self.buffer.reserve(self.window_size);
        self.dict_content.clear();
        self.total_output_counter = 0;
        #[cfg(feature = "hash")]
        {
            self.hash = twox_hash::XxHash64::with_seed(0);
        }
    }

    pub fn len(&self) -> usize {
        match &self.direct {
            Some(d) => d.written,
            None => self.buffer.len(),
        }
    }

    /// 把调用方的连续缓冲绑定为输出目标（known 尺寸腿）。
    ///
    /// # Safety
    ///
    /// 调用方必须保证 `dst` 指向的内存在 `clear_direct_output` 之前一直有效、
    /// 不被别名写入；解码过程中所有输出都写进这块内存，`read`/`read_all`
    /// 只报告进度、不再拷贝。
    pub unsafe fn set_direct_output(&mut self, dst: &mut [u8]) {
        self.direct = Some(DirectOut {
            ptr: dst.as_mut_ptr(),
            cap: dst.len(),
            written: 0,
            drained: 0,
            overflow: false,
        });
    }

    /// 解开直写绑定并返回本帧写入的字节数。
    pub fn clear_direct_output(&mut self) -> usize {
        match self.direct.take() {
            Some(d) => d.written,
            None => 0,
        }
    }

    /// 本次直写是否发生过容量不足（对应 TargetTooSmall）。
    pub fn direct_overflowed(&self) -> bool {
        self.direct.as_ref().is_some_and(|d| d.overflow)
    }

    fn direct_write(&mut self, data: &[u8]) -> bool {
        let Some(d) = self.direct.as_mut() else {
            return false;
        };
        if d.written + data.len() > d.cap {
            d.overflow = true;
            return true;
        }
        unsafe {
            // SAFETY: 容量已检查；ptr 生命周期由 set_direct_output 的调用方保证，
            // 且 written 之前的区域是本帧自己写过的，不与 data 重叠（data 来自
            // literals_buffer 或同帧更早的输出，见 repeat 的调用约定）。
            core::ptr::copy_nonoverlapping(data.as_ptr(), d.ptr.add(d.written), data.len());
        }
        d.written += data.len();
        true
    }

    /// 直写模式下的周期拷贝（等价于 ringbuffer 的 repeat 路径，但写进 dst）。
    fn direct_repeat(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), DecodeBufferError> {
        let (dst_base, written, cap, overflow) = match self.direct.as_ref() {
            Some(d) => (d.ptr, d.written, d.cap, d.overflow),
            None => return Ok(()),
        };
        let _ = overflow;
        if written + match_length > cap {
            if let Some(d) = self.direct.as_mut() {
                d.overflow = true;
            }
            return Ok(());
        }
        if offset > written {
            // 冷路径：字典 / 跨帧——先把需要的那段字典字节落进 dst，再以"现有总长"
            // 为周期继续（与 RingBuffer 版的 repeat_from_dict 同义：此时 offset 指到
            // 缓冲区起点，即字典与输出开头相接的位置）。
            if self.total_output_counter > self.window_size as u64 {
                return Err(DecodeBufferError::OffsetTooBig {
                    offset,
                    buf_len: written,
                });
            }
            let bytes_from_dict = offset - written;
            let dlen = self.dict_content.len();
            if bytes_from_dict > dlen {
                return Err(DecodeBufferError::NotEnoughBytesInDictionary {
                    got: dlen,
                    need: bytes_from_dict,
                });
            }
            let take = bytes_from_dict.min(match_length);
            let low = dlen - bytes_from_dict;
            let new_written = {
                // 字段级拆分借用：dict_content（只读）与 direct（可变）互不相干。
                let dict_slice = &self.dict_content[low..low + take];
                let Some(d) = self.direct.as_mut() else {
                    return Ok(());
                };
                if d.written + take > d.cap {
                    d.overflow = true;
                    return Ok(());
                }
                unsafe {
                    // SAFETY: 容量已检查；take 来自 dict_content，dst 与字典不重叠。
                    core::ptr::copy_nonoverlapping(dict_slice.as_ptr(), d.ptr.add(d.written), take);
                }
                d.written += take;
                d.written
            };
            self.total_output_counter += take as u64;
            let rest = match_length - take;
            if rest == 0 {
                return Ok(());
            }
            return self.direct_repeat(new_written, rest);
        }

        let start_idx = written - offset;
        // 与 repeat_in_chunks 同样的倍增步长：overlap 时按 1x/2x/4x… 拷贝。
        let mut copied = 0usize;
        let mut step = offset;
        while copied < match_length {
            if step > match_length - copied {
                step = match_length - copied;
            }
            unsafe {
                // SAFETY: 源**固定**为 start_idx（模式以 offset 为周期，所以"再拷一遍
                // 开头 step 个字节"等价于接着周期序列的后续字节——与 RingBuffer 版
                // extend_from_within_unchecked(start_idx, step) 同义）。设已拷总量
                // copied_j，非截断步长满足 step_j = copied_j + offset（归纳：step_0 =
                // offset，之后每步翻倍），于是源末端 start_idx + step_j == written + copied_j
                // 恰好等于目标起点 ⇒ 两区间相邻不重叠；dst 容量在入口已按 match_length 检查。
                core::ptr::copy_nonoverlapping(
                    dst_base.add(start_idx),
                    dst_base.add(written + copied),
                    step,
                );
            }
            copied += step;
            if offset == 0 {
                break;
            }
            step *= 2;
        }
        if let Some(d) = self.direct.as_mut() {
            d.written += match_length;
        }
        self.total_output_counter += match_length as u64;
        Ok(())
    }

    pub fn extend_and_fill(&mut self, fill_with: u8, fill_length: usize) {
        if let Some(d) = self.direct.as_mut() {
            // 直写：RLE 块直接填调用方缓冲。
            if d.written + fill_length > d.cap {
                d.overflow = true;
                return;
            }
            unsafe {
                // SAFETY: 容量已检查，ptr 生命周期见 set_direct_output 契约。
                core::ptr::write_bytes(d.ptr.add(d.written), fill_with, fill_length);
            }
            d.written += fill_length;
            self.total_output_counter += fill_length as u64;
            return;
        }
        self.buffer.extend_and_fill(fill_with, fill_length);
    }

    pub fn extend_from_reader<R: Read>(
        &mut self,
        mut read: R,
        fill_length: usize,
    ) -> Result<(), crate::io::Error> {
        if self.direct.is_some() {
            // 直写：raw 块直接读进调用方缓冲。
            let (ptr, cap, written) = match self.direct.as_ref() {
                Some(d) => (d.ptr, d.cap, d.written),
                None => return Ok(()),
            };
            if written + fill_length > cap {
                if let Some(d) = self.direct.as_mut() {
                    d.overflow = true;
                }
                return Ok(());
            }
            let target = unsafe {
                // SAFETY: 容量已按 fill_length 检查；ptr 生命周期见 set_direct_output 契约。
                core::slice::from_raw_parts_mut(ptr.add(written), fill_length)
            };
            read.read_exact(target)?;
            if let Some(d) = self.direct.as_mut() {
                d.written += fill_length;
            }
            self.total_output_counter += fill_length as u64;
            return Ok(());
        }
        self.buffer.extend_from_reader(read, fill_length)
    }

    pub fn push(&mut self, data: &[u8]) {
        if self.direct.is_some() {
            let _ = self.direct_write(data);
            self.total_output_counter += data.len() as u64;
            return;
        }
        self.buffer.extend(data);
        self.total_output_counter += data.len() as u64;
    }

    pub fn repeat(&mut self, offset: usize, match_length: usize) -> Result<(), DecodeBufferError> {
        if self.direct.is_some() {
            return self.direct_repeat(offset, match_length);
        }
        if offset > self.buffer.len() {
            self.repeat_from_dict(offset, match_length)
        } else {
            let buf_len = self.buffer.len();
            let start_idx = buf_len - offset;
            let end_idx = start_idx + match_length;

            self.buffer.reserve(match_length);
            if end_idx > buf_len {
                // We need to copy in chunks.
                self.repeat_in_chunks(offset, match_length, start_idx);
            } else {
                // can just copy parts of the existing buffer
                // SAFETY: Requirements checked:
                // 1. start_idx + match_length must be <= self.buffer.len()
                //      We know that:
                //      1. start_idx = self.buffer.len() - offset
                //      2. end_idx = start_idx + match_length
                //      3. end_idx <= self.buffer.len()
                //      Thus follows: start_idx + match_length <= self.buffer.len()
                //
                // 2. explicitly reserved enough memory for the whole match_length
                unsafe {
                    self.buffer
                        .extend_from_within_unchecked(start_idx, match_length)
                };
            }

            self.total_output_counter += match_length as u64;
            Ok(())
        }
    }

    fn repeat_in_chunks(&mut self, offset: usize, match_length: usize, start_idx: usize) {
        // The source region has period `offset` and grows as we copy: after
        // copying k*offset bytes total, the periodic run from `start_idx` is
        // (k+1)*offset bytes long. So instead of copying offset-sized chunks
        // one by one, double the chunk size every step: 1x, 2x, 4x, ...
        // Number of extend calls drops from match_length/offset to
        // log2(match_length/offset).
        //
        // SAFETY (for each extend_from_within_unchecked call): let buf_len be
        // the buffer length on entry to this function and step the chunk size
        // of the current iteration. Before iteration j with cumulative copied
        // c_j = step_0 + ... + step_{j-1} = step_j - offset (telescoping, since
        // step_{i+1} = 2*step_i except the final partial chunk):
        //   buffer.len() = buf_len + c_j
        //   start_idx + step_j = buf_len - offset + step_j = buf_len + c_j
        // so start_idx + step_j == buffer.len() <= buffer.len(), and the
        // caller reserved `match_length` up front so capacity is sufficient.
        let mut copied = 0usize;
        let mut step = offset;
        while copied < match_length {
            if step > match_length - copied {
                step = match_length - copied;
            }
            unsafe { self.buffer.extend_from_within_unchecked(start_idx, step) };
            copied += step;
            step *= 2;
        }
    }

    #[cold]
    fn repeat_from_dict(
        &mut self,
        offset: usize,
        match_length: usize,
    ) -> Result<(), DecodeBufferError> {
        if self.total_output_counter <= self.window_size as u64 {
            // at least part of that repeat is from the dictionary content
            let bytes_from_dict = offset - self.buffer.len();

            if bytes_from_dict > self.dict_content.len() {
                return Err(DecodeBufferError::NotEnoughBytesInDictionary {
                    got: self.dict_content.len(),
                    need: bytes_from_dict,
                });
            }

            if bytes_from_dict < match_length {
                let dict_slice = &self.dict_content[self.dict_content.len() - bytes_from_dict..];
                self.buffer.extend(dict_slice);

                self.total_output_counter += bytes_from_dict as u64;
                return self.repeat(self.buffer.len(), match_length - bytes_from_dict);
            } else {
                let low = self.dict_content.len() - bytes_from_dict;
                let high = low + match_length;
                let dict_slice = &self.dict_content[low..high];
                self.buffer.extend(dict_slice);
            }
            Ok(())
        } else {
            Err(DecodeBufferError::OffsetTooBig {
                offset,
                buf_len: self.buffer.len(),
            })
        }
    }

    /// Check if and how many bytes can currently be drawn from the buffer
    pub fn can_drain_to_window_size(&self) -> Option<usize> {
        if self.direct.is_some() {
            // 直写下没有"待抽出"的字节，全部已在调用方缓冲里。
            return None;
        }
        if self.buffer.len() > self.window_size {
            Some(self.buffer.len() - self.window_size)
        } else {
            None
        }
    }

    //How many bytes can be drained if the window_size does not have to be maintained
    pub fn can_drain(&self) -> usize {
        match &self.direct {
            Some(d) => d.written - d.drained,
            None => self.buffer.len(),
        }
    }

    /// 直写模式下的进度交代（等价于 read 的效果，但不需要传入调用方切片）。
    /// 非直写模式返回 None。
    pub fn direct_progress(&mut self) -> Option<usize> {
        let d = self.direct.as_mut()?;
        let n = d.written - d.drained;
        d.drained = d.written;
        Some(n)
    }

    /// Drain as much as possible while retaining enough so that decoding si still possible with the required window_size
    /// At best call only if can_drain_to_window_size reports a 'high' number of bytes to reduce allocations
    pub fn drain_to_window_size(&mut self) -> Option<Vec<u8>> {
        //TODO investigate if it is possible to return the std::vec::Drain iterator directly without collecting here
        match self.can_drain_to_window_size() {
            None => None,
            Some(can_drain) => {
                let mut vec = Vec::with_capacity(can_drain);
                self.drain_to(can_drain, |buf| {
                    vec.extend_from_slice(buf);
                    (buf.len(), Ok(()))
                })
                .ok()?;
                Some(vec)
            }
        }
    }

    pub fn drain_to_window_size_writer(&mut self, mut sink: impl Write) -> Result<usize, Error> {
        match self.can_drain_to_window_size() {
            None => Ok(0),
            Some(can_drain) => self.drain_to(can_drain, |buf| write_all_bytes(&mut sink, buf)),
        }
    }

    /// drain the buffer completely
    pub fn drain(&mut self) -> Vec<u8> {
        let (slice1, slice2) = self.buffer.as_slices();
        #[cfg(feature = "hash")]
        {
            self.hash.write(slice1);
            self.hash.write(slice2);
        }

        let mut vec = Vec::with_capacity(slice1.len() + slice2.len());
        vec.extend_from_slice(slice1);
        vec.extend_from_slice(slice2);
        self.buffer.clear();
        vec
    }

    pub fn drain_to_writer(&mut self, mut sink: impl Write) -> Result<usize, Error> {
        let write_limit = self.buffer.len();
        self.drain_to(write_limit, |buf| write_all_bytes(&mut sink, buf))
    }

    pub fn read_all(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        if let Some(d) = self.direct.as_mut() {
            let n = d.written - d.drained;
            d.drained = d.written;
            return Ok(n);
        }
        let amount = self.buffer.len().min(target.len());

        let mut written = 0;
        self.drain_to(amount, |buf| {
            target[written..][..buf.len()].copy_from_slice(buf);
            written += buf.len();
            (buf.len(), Ok(()))
        })?;
        Ok(amount)
    }

    /// Semantics of write_bytes:
    /// Should dump as many of the provided bytes as possible to whatever sink until no bytes are left or an error is encountered
    /// Return how many bytes have actually been dumped to the sink.
    #[allow(clippy::too_many_arguments)]
    fn drain_to(
        &mut self,
        amount: usize,
        mut write_bytes: impl FnMut(&[u8]) -> (usize, Result<(), Error>),
    ) -> Result<usize, Error> {
        if amount == 0 {
            return Ok(0);
        }

        struct DrainGuard<'a> {
            buffer: &'a mut RingBuffer,
            amount: usize,
        }

        impl Drop for DrainGuard<'_> {
            fn drop(&mut self) {
                if self.amount != 0 {
                    self.buffer.drop_first_n(self.amount);
                }
            }
        }

        let mut drain_guard = DrainGuard {
            buffer: &mut self.buffer,
            amount: 0,
        };

        let (slice1, slice2) = drain_guard.buffer.as_slices();
        let n1 = slice1.len().min(amount);
        let n2 = slice2.len().min(amount - n1);

        if n1 != 0 {
            let (written1, res1) = write_bytes(&slice1[..n1]);
            #[cfg(feature = "hash")]
            self.hash.write(&slice1[..written1]);
            drain_guard.amount += written1;

            // Apparently this is what clippy thinks is the best way of expressing this
            res1?;

            // Only if the first call to write_bytes was not a partial write we can continue with slice2
            // Partial writes SHOULD never happen without res1 being an error, but lets just protect against it anyways.
            if written1 == n1 && n2 != 0 {
                let (written2, res2) = write_bytes(&slice2[..n2]);
                #[cfg(feature = "hash")]
                self.hash.write(&slice2[..written2]);
                drain_guard.amount += written2;

                // Apparently this is what clippy thinks is the best way of expressing this
                res2?;
            }
        }

        let amount_written = drain_guard.amount;
        // Make sure we don't accidentally drop `DrainGuard` earlier.
        drop(drain_guard);

        Ok(amount_written)
    }
}

/// Like Write::write_all but returns partial write length even on error
fn write_all_bytes(mut sink: impl Write, buf: &[u8]) -> (usize, Result<(), Error>) {
    let mut written = 0;
    while written < buf.len() {
        match sink.write(&buf[written..]) {
            Ok(0) => return (written, Ok(())),
            Ok(w) => written += w,
            Err(e) => return (written, Err(e)),
        }
    }
    (written, Ok(()))
}

#[cfg(test)]
mod tests {
    use super::DecodeBuffer;
    use crate::io::{Error, ErrorKind, Write};

    extern crate std;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn short_writer() {
        struct ShortWriter {
            buf: Vec<u8>,
            write_len: usize,
        }

        impl Write for ShortWriter {
            fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Error> {
                if buf.len() > self.write_len {
                    self.buf.extend_from_slice(&buf[..self.write_len]);
                    Ok(self.write_len)
                } else {
                    self.buf.extend_from_slice(buf);
                    Ok(buf.len())
                }
            }

            fn flush(&mut self) -> std::result::Result<(), Error> {
                Ok(())
            }
        }

        let mut short_writer = ShortWriter {
            buf: vec![],
            write_len: 10,
        };

        let mut decode_buf = DecodeBuffer::new(100);
        decode_buf.push(b"0123456789");
        decode_buf.repeat(10, 90).unwrap();
        let repeats = 1000;
        for _ in 0..repeats {
            assert_eq!(decode_buf.len(), 100);
            decode_buf.repeat(10, 50).unwrap();
            assert_eq!(decode_buf.len(), 150);
            decode_buf
                .drain_to_window_size_writer(&mut short_writer)
                .unwrap();
            assert_eq!(decode_buf.len(), 100);
        }

        assert_eq!(short_writer.buf.len(), repeats * 50);
        decode_buf.drain_to_writer(&mut short_writer).unwrap();
        assert_eq!(short_writer.buf.len(), repeats * 50 + 100);
    }

    #[test]
    fn wouldblock_writer() {
        struct WouldblockWriter {
            buf: Vec<u8>,
            last_blocked: usize,
            block_every: usize,
        }

        impl Write for WouldblockWriter {
            fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Error> {
                if self.last_blocked < self.block_every {
                    self.buf.extend_from_slice(buf);
                    self.last_blocked += 1;
                    Ok(buf.len())
                } else {
                    self.last_blocked = 0;
                    Err(Error::from(ErrorKind::WouldBlock))
                }
            }

            fn flush(&mut self) -> std::result::Result<(), Error> {
                Ok(())
            }
        }

        let mut short_writer = WouldblockWriter {
            buf: vec![],
            last_blocked: 0,
            block_every: 5,
        };

        let mut decode_buf = DecodeBuffer::new(100);
        decode_buf.push(b"0123456789");
        decode_buf.repeat(10, 90).unwrap();
        let repeats = 1000;
        for _ in 0..repeats {
            assert_eq!(decode_buf.len(), 100);
            decode_buf.repeat(10, 50).unwrap();
            assert_eq!(decode_buf.len(), 150);
            loop {
                match decode_buf.drain_to_window_size_writer(&mut short_writer) {
                    Ok(written) => {
                        if written == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        if e.kind() == ErrorKind::WouldBlock {
                            continue;
                        } else {
                            panic!("Unexpected error {:?}", e);
                        }
                    }
                }
            }
            assert_eq!(decode_buf.len(), 100);
        }

        assert_eq!(short_writer.buf.len(), repeats * 50);
        loop {
            match decode_buf.drain_to_writer(&mut short_writer) {
                Ok(written) => {
                    if written == 0 {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        continue;
                    } else {
                        panic!("Unexpected error {:?}", e);
                    }
                }
            }
        }
        assert_eq!(short_writer.buf.len(), repeats * 50 + 100);
    }
}
