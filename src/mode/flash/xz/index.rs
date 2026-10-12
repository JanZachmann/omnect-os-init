use std::io::{self, BufReader, Read, Seek, SeekFrom};

const STREAM_HEADER_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];
const STREAM_FOOTER_MAGIC: [u8; 2] = *b"YZ";
const STREAM_HEADER_LEN: u64 = 12;
const STREAM_FOOTER_LEN: u64 = 12;
const STREAM_FLAGS_LEN: usize = 2;
const HEADER_FLAGS_AT: usize = STREAM_HEADER_MAGIC.len();
const FOOTER_BACKWARD_SIZE_AT: usize = 4;
const FOOTER_FLAGS_AT: usize = 8;
const INDEX_INDICATOR: u8 = 0x00;
/// Blocks, the index and the backward size are counted in units of 4 bytes.
const XZ_ALIGNMENT: u64 = 4;
const MAX_NUMBER_LEN: u32 = 9;
const NUMBER_PAYLOAD_BITS: u32 = 7;
const NUMBER_MORE_BIT: u8 = 0x80;

/// The size `file` decodes to, from the index at its end. Only a file that
/// holds exactly one xz stream is accepted. The checksums are left to the
/// decoder, which checks the index against the data again.
// TODO: remove me, as soon as a lzma-rust2 release that reports the
// uncompressed size of an xz file is CR.
pub(crate) fn uncompressed_size<F: Read + Seek>(file: &mut F) -> Result<u64, String> {
    let io = |e: io::Error| format!("reading the xz index: {e}");
    let file_len = file.seek(SeekFrom::End(0)).map_err(io)?;
    if file_len < STREAM_HEADER_LEN + STREAM_FOOTER_LEN {
        return Err(format!("{file_len} bytes are too short for an xz file"));
    }

    let mut header = [0u8; STREAM_HEADER_LEN as usize];
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    file.read_exact(&mut header).map_err(io)?;
    let mut footer = [0u8; STREAM_FOOTER_LEN as usize];
    file.seek(SeekFrom::Start(file_len - STREAM_FOOTER_LEN))
        .map_err(io)?;
    file.read_exact(&mut footer).map_err(io)?;

    if header[..HEADER_FLAGS_AT] != STREAM_HEADER_MAGIC
        || footer[FOOTER_FLAGS_AT + STREAM_FLAGS_LEN..] != STREAM_FOOTER_MAGIC
    {
        return Err("not an xz file, or data follows the xz stream".to_string());
    }
    if header[HEADER_FLAGS_AT..HEADER_FLAGS_AT + STREAM_FLAGS_LEN]
        != footer[FOOTER_FLAGS_AT..FOOTER_FLAGS_AT + STREAM_FLAGS_LEN]
    {
        return Err("the stream header and footer flags differ".to_string());
    }

    let mut backward_size = [0u8; 4];
    backward_size.copy_from_slice(&footer[FOOTER_BACKWARD_SIZE_AT..FOOTER_FLAGS_AT]);
    let index_len = (u64::from(u32::from_le_bytes(backward_size)) + 1) * XZ_ALIGNMENT;
    let index_start = (file_len - STREAM_FOOTER_LEN)
        .checked_sub(index_len)
        .filter(|&start| start >= STREAM_HEADER_LEN)
        .ok_or_else(|| format!("the xz index of {index_len} bytes does not fit the file"))?;

    file.seek(SeekFrom::Start(index_start)).map_err(io)?;
    let mut index = BufReader::new(file.take(index_len));
    if read_byte(&mut index)? != INDEX_INDICATOR {
        return Err("no xz index where the footer points".to_string());
    }

    let records = read_number(&mut index)?;
    let mut blocks_len: u64 = 0;
    let mut size: u64 = 0;
    for _ in 0..records {
        let unpadded = read_number(&mut index)?;
        if unpadded == 0 {
            return Err("an xz index record has a block of 0 bytes".to_string());
        }
        blocks_len = unpadded
            .checked_next_multiple_of(XZ_ALIGNMENT)
            .and_then(|padded| blocks_len.checked_add(padded))
            .ok_or("the xz block sizes overflow")?;
        size = size
            .checked_add(read_number(&mut index)?)
            .ok_or("the xz uncompressed size overflows")?;
    }

    let stream_len = [STREAM_HEADER_LEN, index_len, STREAM_FOOTER_LEN]
        .into_iter()
        .try_fold(blocks_len, u64::checked_add);
    if stream_len != Some(file_len) {
        return Err("the file is not exactly one xz stream".to_string());
    }
    Ok(size)
}

fn read_byte(reader: &mut impl Read) -> Result<u8, String> {
    let mut byte = [0u8; 1];
    reader
        .read_exact(&mut byte)
        .map_err(|e| format!("reading the xz index: {e}"))?;
    Ok(byte[0])
}

/// A number in the xz multibyte encoding: 7 bits per byte, least significant
/// first, the high bit set on every byte but the last.
fn read_number(reader: &mut impl Read) -> Result<u64, String> {
    let mut value: u64 = 0;
    for i in 0..MAX_NUMBER_LEN {
        let byte = read_byte(reader)?;
        value |= u64::from(byte & !NUMBER_MORE_BIT) << (i * NUMBER_PAYLOAD_BITS);
        if byte & NUMBER_MORE_BIT == 0 {
            if i > 0 && byte == 0 {
                return Err("an xz index number has a trailing zero byte".to_string());
            }
            return Ok(value);
        }
    }
    Err(format!(
        "an xz index number is longer than {MAX_NUMBER_LEN} bytes"
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::num::NonZeroU64;

    use super::*;
    use crate::mode::flash::xz::test_support::{xz, xz_with};

    const DATA_LEN: usize = 3 * 4096 + 17;

    fn data() -> Vec<u8> {
        (0..DATA_LEN).map(|i| (i % 251) as u8).collect()
    }

    fn size_of(file: Vec<u8>) -> Result<u64, String> {
        uncompressed_size(&mut Cursor::new(file))
    }

    #[test]
    fn the_size_of_a_single_block_stream_is_read_from_its_index() {
        assert_eq!(size_of(xz(&data())).unwrap(), DATA_LEN as u64);
    }

    #[test]
    fn the_sizes_of_all_blocks_are_added() {
        let block_size = NonZeroU64::new(1000);
        assert_eq!(
            size_of(xz_with(&data(), block_size)).unwrap(),
            DATA_LEN as u64
        );
    }

    #[test]
    fn an_empty_stream_has_size_zero() {
        assert_eq!(size_of(xz(&[])).unwrap(), 0);
    }

    #[test]
    fn two_concatenated_streams_are_refused() {
        let file = [xz(&data()), xz(&data())].concat();
        let err = size_of(file).unwrap_err();
        assert!(err.contains("not exactly one xz stream"), "{err}");
    }

    #[test]
    fn data_after_the_stream_is_refused() {
        let file = [xz(&data()), b"trailing".to_vec()].concat();
        assert!(size_of(file).is_err());
    }

    #[test]
    fn a_file_that_is_not_xz_is_refused() {
        for file in [Vec::new(), vec![0u8; 64], data()] {
            assert!(size_of(file.clone()).is_err(), "{} bytes", file.len());
        }
    }

    #[test]
    fn a_cut_stream_is_refused() {
        let mut file = xz(&data());
        file.truncate(file.len() - 1);
        assert!(size_of(file).is_err());
    }

    #[test]
    fn a_footer_that_points_outside_the_file_is_refused() {
        let mut file = xz(&data());
        let at = file.len() - STREAM_FOOTER_LEN as usize + FOOTER_BACKWARD_SIZE_AT;
        file[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = size_of(file).unwrap_err();
        assert!(err.contains("does not fit the file"), "{err}");
    }

    #[test]
    fn index_numbers_decode_as_the_xz_format_defines() {
        let number = |bytes: &[u8]| read_number(&mut Cursor::new(bytes));
        assert_eq!(number(&[0x00]).unwrap(), 0);
        assert_eq!(number(&[0x7f]).unwrap(), 0x7f);
        assert_eq!(number(&[0x80, 0x01]).unwrap(), 0x80);
        assert_eq!(number(&[0xff, 0xff, 0x03]).unwrap(), 0xffff);
        assert!(number(&[0x80, 0x00]).is_err(), "trailing zero byte");
        assert!(number(&[0x80; 10]).is_err(), "longer than 9 bytes");
        assert!(number(&[0x80]).is_err(), "cut");
    }
}
