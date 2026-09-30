use super::scratch::FSEScratch;
use super::sequence_section_decoder::{lookup_ll_code, lookup_ml_code, maybe_update_fse_tables};
use crate::bit_io::BitReaderReversed;
use crate::blocks::sequence_section::SequencesHeader;
use crate::blocks::sequence_section::MAX_OFFSET_CODE;
use crate::decoding::decode_buffer::DecodeBuffer;
use crate::decoding::errors::DecodeBufferError;
use crate::decoding::errors::FSEDecoderError;
use crate::decoding::errors::{DecodeSequenceError, ExecuteSequencesError};
#[cfg(feature = "prof")]
use crate::decoding::prof;
use crate::fse::FSEDecoder;

/// Error type for the fused sequence decode+execute pass.
///
/// On corrupt input the fused pass may report an execution error for an early
/// sequence where the two-pass version would have reported a decode error for
/// a later sequence; both are fatal for the frame either way. For valid input
/// the output is bit-identical to the two-pass version.
#[derive(Debug)]
pub enum FusedSequencesError {
    Decode(DecodeSequenceError),
    Execute(ExecuteSequencesError),
}

impl From<DecodeSequenceError> for FusedSequencesError {
    fn from(val: DecodeSequenceError) -> Self {
        Self::Decode(val)
    }
}
impl From<ExecuteSequencesError> for FusedSequencesError {
    fn from(val: ExecuteSequencesError) -> Self {
        Self::Execute(val)
    }
}
impl From<FSEDecoderError> for FusedSequencesError {
    fn from(val: FSEDecoderError) -> Self {
        Self::Decode(DecodeSequenceError::from(val))
    }
}
impl From<DecodeBufferError> for FusedSequencesError {
    fn from(val: DecodeBufferError) -> Self {
        Self::Execute(ExecuteSequencesError::from(val))
    }
}

/// Decode the sequence section and execute each sequence inline, in a single
/// pass over the bit stream.
///
/// This is bit-identical to `decode_sequences` followed by
/// `execute_sequences` for valid input, but avoids materializing the
/// intermediate `Vec<Sequence>` (a write+read of 12 bytes per sequence) and
/// re-walking the literals buffer in a second loop.
pub fn decode_and_execute_sequences(
    section: &SequencesHeader,
    source: &[u8],
    fse: &mut FSEScratch,
    buffer: &mut DecodeBuffer,
    literals_buffer: &[u8],
    offset_hist: &mut [u32; 3],
) -> Result<(), FusedSequencesError> {
    let bytes_read = maybe_update_fse_tables(section, source, fse)?;

    let bit_stream = &source[bytes_read..];
    let mut br = BitReaderReversed::new(bit_stream);

    //skip the 0 padding at the end of the last byte of the bit stream and throw away the first 1 found
    let mut skipped_bits = 0;
    loop {
        let val = br.get_bits(1);
        skipped_bits += 1;
        if val == 1 || skipped_bits > 8 {
            break;
        }
    }
    if skipped_bits > 8 {
        //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
        return Err(DecodeSequenceError::ExtraPadding { skipped_bits }.into());
    }

    let ll_rle = fse.ll_rle;
    let ml_rle = fse.ml_rle;
    let of_rle = fse.of_rle;
    let mut ll_dec = FSEDecoder::new(&fse.literal_lengths);
    let mut ml_dec = FSEDecoder::new(&fse.match_lengths);
    let mut of_dec = FSEDecoder::new(&fse.offsets);

    if ll_rle.is_none() {
        ll_dec.init_state(&mut br)?;
    }
    if of_rle.is_none() {
        of_dec.init_state(&mut br)?;
    }
    if ml_rle.is_none() {
        ml_dec.init_state(&mut br)?;
    }

    let num_sequences = section.num_sequences as usize;

    let mut literals_copy_counter = 0usize;
    let old_buffer_size = buffer.len();
    let mut seq_sum = 0u32;

    #[cfg(feature = "prof")]
    let (mut fse_acc, mut lit_acc, mut match_acc, mut samples) = (0u64, 0u64, 0u64, 0u64);

    for seq_idx in 0..num_sequences {
        // Only every 64th sequence is timed: timing every one would add
        // 20-30ns/sequence to a ~20ns/sequence loop and swamp the split.
        #[cfg(feature = "prof")]
        let sampled = seq_idx & 63 == 0;
        #[cfg(feature = "prof")]
        let t_fse = if sampled { prof::tick() } else { 0 };
        let ll_code = if let Some(ll_rle) = ll_rle {
            ll_rle
        } else {
            ll_dec.decode_symbol()
        };
        let ml_code = if let Some(ml_rle) = ml_rle {
            ml_rle
        } else {
            ml_dec.decode_symbol()
        };
        let of_code = if let Some(of_rle) = of_rle {
            of_rle
        } else {
            of_dec.decode_symbol()
        };

        let (ll_value, ll_num_bits) = lookup_ll_code(ll_code);
        let (ml_value, ml_num_bits) = lookup_ml_code(ml_code);

        if of_code > MAX_OFFSET_CODE {
            return Err(DecodeSequenceError::UnsupportedOffset {
                offset_code: of_code,
            }
            .into());
        }

        let (obits, ml_add, ll_add) = br.get_bits_triple(of_code, ml_num_bits, ll_num_bits);
        let offset = obits as u32 + (1u32 << of_code);

        if offset == 0 {
            return Err(DecodeSequenceError::ZeroOffset.into());
        }

        let seq_ll = ll_value + ll_add as u32;
        let seq_ml = ml_value + ml_add as u32;

        // ── inline execution ( mirrors execute_sequences ) ──
        #[cfg(feature = "prof")]
        let t_lit = if sampled {
            fse_acc += prof::tick().wrapping_sub(t_fse);
            prof::tick()
        } else {
            0
        };
        if seq_ll > 0 {
            let high = literals_copy_counter + seq_ll as usize;
            if high > literals_buffer.len() {
                return Err(ExecuteSequencesError::NotEnoughBytesForSequence {
                    wanted: high,
                    have: literals_buffer.len(),
                }
                .into());
            }
            let literals = &literals_buffer[literals_copy_counter..high];
            literals_copy_counter = high;

            buffer.push(literals);
        }

        #[cfg(feature = "prof")]
        let t_match = if sampled {
            lit_acc += prof::tick().wrapping_sub(t_lit);
            prof::tick()
        } else {
            0
        };
        let actual_offset = do_offset_history(offset, seq_ll, &mut *offset_hist);
        if actual_offset == 0 {
            return Err(ExecuteSequencesError::ZeroOffset.into());
        }
        if seq_ml > 0 {
            buffer.repeat(actual_offset as usize, seq_ml as usize)?;
        }

        #[cfg(feature = "prof")]
        if sampled {
            match_acc += prof::tick().wrapping_sub(t_match);
            samples += 1;
        }

        seq_sum += seq_ml;
        seq_sum += seq_ll;

        if seq_idx + 1 < num_sequences {
            if ll_rle.is_none() {
                ll_dec.update_state(&mut br);
            }
            if ml_rle.is_none() {
                ml_dec.update_state(&mut br);
            }
            if of_rle.is_none() {
                of_dec.update_state(&mut br);
            }
        }

        if br.bits_remaining() < 0 {
            return Err(DecodeSequenceError::NotEnoughBytesForNumSequences.into());
        }
    }

    if br.bits_remaining() > 0 {
        return Err(DecodeSequenceError::ExtraBits {
            bits_remaining: br.bits_remaining(),
        }
        .into());
    }

    if literals_copy_counter < literals_buffer.len() {
        let rest_literals = &literals_buffer[literals_copy_counter..];
        buffer.push(rest_literals);
        seq_sum += rest_literals.len() as u32;
    }

    #[cfg(feature = "prof")]
    {
        prof::flush(&prof::FSE_TICKS, fse_acc);
        prof::flush(&prof::LIT_COPY_TICKS, lit_acc);
        prof::flush(&prof::MATCH_COPY_TICKS, match_acc);
        prof::count(&prof::SAMPLES, samples);
        prof::count(&prof::SEQS, num_sequences as u64);
        prof::count(&prof::BLOCKS, 1);
    }

    let diff = buffer.len() - old_buffer_size;
    assert!(
        seq_sum as usize == diff,
        "Seq_sum: {} is different from the difference in buffersize: {}",
        seq_sum,
        diff
    );
    Ok(())
}

/// Update the most recently used offsets to reflect the provided offset value, and return the
/// "actual" offset needed because offsets are not stored in a raw way, some transformations are needed
/// before you get a functional number.
fn do_offset_history(offset_value: u32, lit_len: u32, scratch: &mut [u32; 3]) -> u32 {
    let actual_offset = if lit_len > 0 {
        match offset_value {
            1..=3 => scratch[offset_value as usize - 1],
            _ => {
                //new offset
                offset_value - 3
            }
        }
    } else {
        match offset_value {
            1..=2 => scratch[offset_value as usize],
            // A malformed dictionary can seed scratch[0] with 0; saturate so this
            // resolves to 0 (rejected upstream as ZeroOffset) instead of
            // underflowing. See #115.
            3 => scratch[0].saturating_sub(1),
            _ => {
                //new offset
                offset_value - 3
            }
        }
    };

    //update history
    if lit_len > 0 {
        match offset_value {
            1 => {
                //nothing
            }
            2 => {
                scratch[1] = scratch[0];
                scratch[0] = actual_offset;
            }
            _ => {
                scratch[2] = scratch[1];
                scratch[1] = scratch[0];
                scratch[0] = actual_offset;
            }
        }
    } else {
        match offset_value {
            1 => {
                scratch[1] = scratch[0];
                scratch[0] = actual_offset;
            }
            2 => {
                scratch[2] = scratch[1];
                scratch[1] = scratch[0];
                scratch[0] = actual_offset;
            }
            _ => {
                scratch[2] = scratch[1];
                scratch[1] = scratch[0];
                scratch[0] = actual_offset;
            }
        }
    }

    actual_offset
}

#[cfg(test)]
mod tests {
    use super::do_offset_history;

    #[test]
    fn repeat_offset_minus_one_with_zero_history_does_not_underflow() {
        // A malformed dictionary can seed offset history slot 0 with 0. With
        // literal length 0 and offset code 3 ("repeat the most recent offset,
        // minus one"), `scratch[0] - 1` must not underflow; it should resolve to
        // 0, which the caller rejects as ExecuteSequencesError::ZeroOffset rather
        // than panicking (debug) or wrapping to u32::MAX (release). See #115.
        let mut scratch = [0u32, 4, 8];
        assert_eq!(do_offset_history(3, 0, &mut scratch), 0);
    }
}
