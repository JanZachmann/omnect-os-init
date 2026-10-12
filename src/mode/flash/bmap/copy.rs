use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};

use lzma_rust2::{XzReader, lzma2_get_memory_usage};
use sha2::{Digest, Sha256};

use crate::error::FlashError;
use crate::mode::flash::bmap::{Bmap, Destination, Source};
use crate::mode::flash::rawio::{ByteRange, COPY_BUFFER_SIZE, chunk_len, open_existing_for_write};

/// The largest dictionary of the xz presets (`-9`). The decoder allocates the
/// dictionary that each block header asks for, so a larger one is rejected
/// before it can push the device out of memory.
const MAX_XZ_DICT_SIZE: u32 = 64 * MIB;
const MIB: u32 = 1024 * 1024;

/// Copy the mapped ranges of `source` to `destination` and check each range's
/// checksum.
pub(crate) fn copy(
    bmap: &Bmap,
    source: &Source<'_>,
    destination: &Destination<'_>,
) -> Result<(), FlashError> {
    let failed = |reason: String| FlashError::CopyFailed {
        src: source.path().to_path_buf(),
        dst: destination.path().to_path_buf(),
        reason,
    };

    let input = File::open(source.path()).map_err(|e| failed(format!("opening source: {e}")))?;
    let mut input: Box<dyn Input> = match source {
        Source::Xz(_) => Box::new(xz_stream(input)),
        Source::Raw(_) => Box::new(Seekable::new(input)),
    };

    let mut output = match destination {
        Destination::Device(path) => open_existing_for_write(path),
        Destination::File(path) => OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .and_then(|file| file.set_len(bmap.image_size).map(|()| file)),
    }
    .map_err(|e| failed(format!("opening destination: {e}")))?;

    copy_stream(bmap, input.as_mut(), &mut output).map_err(failed)?;
    output
        .sync_all()
        .map_err(|e| failed(format!("syncing destination: {e}")))
}

/// Read exactly `len` bytes, handing each chunk to `sink`.
fn pass_through(
    input: &mut dyn Input,
    len: u64,
    buf: &mut [u8],
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut left = len;
    while left > 0 {
        let chunk = chunk_len(left, buf.len());
        let read = input.read(&mut buf[..chunk]).map_err(|e| match e.kind() {
            ErrorKind::OutOfMemory => format!(
                "decoding needs too much memory, the xz dictionary limit is {} MiB: {e}",
                MAX_XZ_DICT_SIZE / MIB
            ),
            _ => format!("reading source: {e}"),
        })?;
        if read == 0 {
            return Err(format!(
                "source ended at byte {}, the image has more",
                input.position()
            ));
        }
        sink(&buf[..read])?;
        left -= read as u64;
    }
    Ok(())
}

trait Input: Read {
    /// The number of image bytes read or skipped so far.
    fn position(&self) -> u64;
    /// Move forward to `target` without using the bytes in between.
    fn skip_to(&mut self, target: u64, buf: &mut [u8]) -> Result<(), String>;
}

fn distance(position: u64, target: u64) -> Result<u64, String> {
    target
        .checked_sub(position)
        .ok_or_else(|| format!("cannot move back from byte {position} to {target}"))
}

fn xz_stream<R: Read>(compressed: R) -> Stream<XzReader<FullReads<R>>> {
    Stream::new(XzReader::new_mem_limit(
        FullReads(compressed),
        true,
        lzma2_get_memory_usage(MAX_XZ_DICT_SIZE),
    ))
}

/// The xz decoder treats a short read as a broken stream; a pipe returns only
/// what has arrived, so each read here fills the buffer or reaches the end of
/// input.
struct FullReads<R>(R);

impl<R: Read> Read for FullReads<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.0.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
}

struct Stream<R> {
    inner: R,
    position: u64,
}

impl<R> Stream<R> {
    fn new(inner: R) -> Self {
        Self { inner, position: 0 }
    }
}

impl<R: Read> Read for Stream<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Read> Input for Stream<R> {
    fn position(&self) -> u64 {
        self.position
    }

    fn skip_to(&mut self, target: u64, buf: &mut [u8]) -> Result<(), String> {
        let len = distance(self.position, target)?;
        pass_through(self, len, buf, &mut |_| Ok(()))
    }
}

/// Gaps are seeked over: on ramfs, reading a hole allocates a zero page that
/// stays in RAM.
#[cfg_attr(feature = "flash-mode-2-direct", allow(dead_code))]
struct Seekable<R> {
    inner: R,
    position: u64,
}

impl<R> Seekable<R> {
    fn new(inner: R) -> Self {
        Self { inner, position: 0 }
    }
}

impl<R: Read> Read for Seekable<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Read + Seek> Input for Seekable<R> {
    fn position(&self) -> u64 {
        self.position
    }

    fn skip_to(&mut self, target: u64, _buf: &mut [u8]) -> Result<(), String> {
        distance(self.position, target)?;
        self.inner
            .seek(SeekFrom::Start(target))
            .map_err(|e| format!("seeking source: {e}"))?;
        self.position = target;
        Ok(())
    }
}

/// The source is read to its end, so a cut xz stream or data after the image
/// is an error instead of a complete image.
fn copy_stream<W: Write + Seek>(
    bmap: &Bmap,
    input: &mut dyn Input,
    output: &mut W,
) -> Result<(), String> {
    let mut buf = vec![0u8; COPY_BUFFER_SIZE];

    for range in &bmap.ranges {
        let ByteRange { offset, len } = range.bytes;
        input.skip_to(offset, &mut buf)?;
        output
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seeking destination: {e}"))?;

        let mut hasher = Sha256::new();
        pass_through(input, len, &mut buf, &mut |chunk| {
            hasher.update(chunk);
            output
                .write_all(chunk)
                .map_err(|e| format!("writing destination: {e}"))
        })?;
        if hasher.finalize()[..] != range.sha256 {
            return Err(format!(
                "checksum mismatch in bytes {offset}..{}",
                offset + len
            ));
        }
    }

    input.skip_to(bmap.image_size, &mut buf)?;
    let extra = input
        .read(&mut buf[..1])
        .map_err(|e| format!("reading source: {e}"))?;
    if extra != 0 {
        return Err(format!(
            "source is longer than the image of {} bytes",
            bmap.image_size
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::flash::bmap::test_support::{
        BLOCK_SIZE, FIXTURE_BMAP, FIXTURE_XZ, bmap_xml, image, xz,
    };
    use lzma_rust2::{XzOptions, XzWriter};
    use std::io::Cursor;

    fn copied(bmap: &Bmap, source: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Cursor::new(Vec::new());
        copy_stream(bmap, &mut Stream::new(Cursor::new(source)), &mut out)?;
        Ok(out.into_inner())
    }

    struct Counting<'a> {
        inner: Cursor<&'a [u8]>,
        read: u64,
    }

    impl Read for Counting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }

    impl Seek for Counting<'_> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn an_input_does_not_move_back() {
        let data = [0u8; 10];
        let mut buf = [0u8; 4];
        let inputs: [Box<dyn Input>; 2] = [
            Box::new(Stream::new(Cursor::new(&data))),
            Box::new(Seekable::new(Cursor::new(&data))),
        ];
        for mut input in inputs {
            input.skip_to(6, &mut buf).unwrap();
            assert_eq!(input.position(), 6);
            input.skip_to(6, &mut buf).unwrap();
            let err = input.skip_to(5, &mut buf).unwrap_err();
            assert!(err.contains("cannot move back"), "{err}");
        }
    }

    #[test]
    fn a_seekable_source_reads_only_the_mapped_ranges() {
        let image = image(5, 100);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(1, 1), (3, 3)])).unwrap();
        let mut input = Seekable::new(Counting {
            inner: Cursor::new(&image),
            read: 0,
        });
        let mut out = Cursor::new(Vec::new());
        copy_stream(&bmap, &mut input, &mut out).unwrap();

        assert_eq!(input.inner.read, 2 * BLOCK_SIZE);
        let block = usize::try_from(BLOCK_SIZE).unwrap();
        assert_eq!(out.get_ref()[block..2 * block], image[block..2 * block]);
    }

    #[test]
    fn only_the_mapped_ranges_are_written() {
        let image = image(5, 100);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(1, 1), (3, 5)])).unwrap();
        let out = copied(&bmap, &image).unwrap();

        let block = usize::try_from(BLOCK_SIZE).unwrap();
        assert!(out[..block].iter().all(|&b| b == 0));
        assert_eq!(out[block..2 * block], image[block..2 * block]);
        assert!(out[2 * block..3 * block].iter().all(|&b| b == 0));
        assert_eq!(out[3 * block..], image[3 * block..]);
    }

    #[test]
    fn a_wrong_byte_in_a_range_fails_its_checksum() {
        let image = image(3, 0);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 2)])).unwrap();
        let mut broken = image.clone();
        broken[5000] ^= 1;
        let err = copied(&bmap, &broken).unwrap_err();
        assert!(err.contains("checksum mismatch in bytes 0..12288"), "{err}");
    }

    #[test]
    fn a_source_that_ends_early_is_an_error() {
        let image = image(4, 0);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(3, 3)])).unwrap();
        let block = usize::try_from(BLOCK_SIZE).unwrap();
        for cut in [block, 3 * block + 1] {
            let err = copied(&bmap, &image[..cut]).unwrap_err();
            assert!(
                err.contains(&format!("source ended at byte {cut}")),
                "{err}"
            );
        }
    }

    #[test]
    fn a_source_longer_than_the_image_is_an_error() {
        let image = image(2, 0);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 0)])).unwrap();
        let mut longer = image.clone();
        longer.push(0);
        assert!(copied(&bmap, &longer).unwrap_err().contains("longer"));
    }

    /// Returns one byte per read, as a pipe does when the writer is slow.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let (Some(slot), Some((&byte, rest))) = (buf.first_mut(), self.0.split_first()) else {
                return Ok(0);
            };
            *slot = byte;
            self.0 = rest;
            Ok(1)
        }
    }

    #[test]
    fn an_xz_stream_that_arrives_in_small_reads_is_decoded() {
        // Different sizes give different block padding lengths.
        for tail in 1..=8 {
            let image = image(2, tail);
            let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 2)])).unwrap();
            let compressed = xz(&image);
            let mut out = Cursor::new(Vec::new());
            copy_stream(&bmap, &mut xz_stream(Trickle(&compressed)), &mut out)
                .unwrap_or_else(|e| panic!("tail {tail}: {e}"));
            assert_eq!(out.into_inner(), image, "tail {tail}");
        }
    }

    #[test]
    fn a_multi_block_xz_stream_that_arrives_in_small_reads_is_decoded() {
        const XZ_BLOCK_SIZE: u64 = 3 * BLOCK_SIZE;
        for tail in 1..=8 {
            let image = image(8, tail);
            let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 8)])).unwrap();
            let mut options = XzOptions::with_preset(1);
            options.set_block_size(std::num::NonZeroU64::new(XZ_BLOCK_SIZE));
            let mut writer = XzWriter::new(Vec::new(), options).unwrap();
            writer.write_all(&image).unwrap();
            let compressed = writer.finish().unwrap();

            let mut out = Cursor::new(Vec::new());
            copy_stream(&bmap, &mut xz_stream(Trickle(&compressed)), &mut out)
                .unwrap_or_else(|e| panic!("tail {tail}: {e}"));
            assert_eq!(out.into_inner(), image, "tail {tail}");
        }
    }

    #[test]
    fn a_raw_stream_that_arrives_in_small_reads_is_copied() {
        let image = image(5, 100);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(1, 1), (3, 5)])).unwrap();
        let mut out = Cursor::new(Vec::new());
        copy_stream(&bmap, &mut Stream::new(Trickle(&image)), &mut out).unwrap();
        let block = usize::try_from(BLOCK_SIZE).unwrap();
        assert_eq!(out.get_ref()[3 * block..], image[3 * block..]);
    }

    /// Fails every other read with `Interrupted`.
    struct Interrupting<R> {
        inner: R,
        interrupt: bool,
    }

    impl<R: Read> Read for Interrupting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(ErrorKind::Interrupted.into());
            }
            self.inner.read(buf)
        }
    }

    #[test]
    fn full_reads_fill_the_buffer_until_the_input_ends() {
        let data: Vec<u8> = (0..10).collect();
        let mut reader = FullReads(Interrupting {
            inner: Trickle(&data),
            interrupt: false,
        });
        let mut buf = [0u8; 6];
        assert_eq!(reader.read(&mut buf).unwrap(), 6);
        assert_eq!(buf, [0, 1, 2, 3, 4, 5]);
        assert_eq!(reader.read(&mut buf).unwrap(), 4, "the input ends");
        assert_eq!(buf[..4], [6, 7, 8, 9]);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    fn crc32(data: &[u8]) -> u32 {
        const POLYNOMIAL: u32 = 0xEDB8_8320;
        !data.iter().fold(!0, |crc, &byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ POLYNOMIAL
                } else {
                    crc >> 1
                }
            })
        })
    }

    /// Sets the dictionary size byte of the first block and fixes the block
    /// header CRC.
    fn with_dict_byte(mut compressed: Vec<u8>, dict_byte: u8) -> Vec<u8> {
        const STREAM_HEADER_LEN: usize = 12;
        const CRC_LEN: usize = 4;
        const LZMA2_FILTER: [u8; 2] = [0x21, 0x01];
        let header_len = (usize::from(compressed[STREAM_HEADER_LEN]) + 1) * 4;
        let crc_at = STREAM_HEADER_LEN + header_len - CRC_LEN;
        let filter = compressed[STREAM_HEADER_LEN..crc_at]
            .windows(LZMA2_FILTER.len())
            .position(|bytes| bytes == LZMA2_FILTER)
            .unwrap();
        compressed[STREAM_HEADER_LEN + filter + LZMA2_FILTER.len()] = dict_byte;
        let crc = crc32(&compressed[STREAM_HEADER_LEN..crc_at]);
        compressed[crc_at..crc_at + CRC_LEN].copy_from_slice(&crc.to_le_bytes());
        compressed
    }

    #[test]
    fn an_xz_dictionary_above_the_limit_is_an_error() {
        const DICT_64_MIB: u8 = 28;
        const DICT_96_MIB: u8 = 29;
        let image = image(2, 0);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 1)])).unwrap();
        let decode = |dict_byte| {
            let compressed = with_dict_byte(xz(&image), dict_byte);
            let mut out = Cursor::new(Vec::new());
            copy_stream(&bmap, &mut xz_stream(Cursor::new(compressed)), &mut out)
        };
        decode(DICT_64_MIB).unwrap();
        let err = decode(DICT_96_MIB).unwrap_err();
        assert!(err.contains("limit is 64 MiB"), "{err}");
    }

    #[test]
    fn an_xz_image_is_decoded_into_a_new_sparse_file() {
        let dir = tempfile::tempdir().unwrap();
        let image = image(6, 10);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 1), (4, 6)])).unwrap();
        let source = dir.path().join("wic.xz");
        std::fs::write(&source, xz(&image)).unwrap();
        let decoded = dir.path().join("wic");
        std::fs::write(&decoded, b"left over from an earlier run").unwrap();

        copy(&bmap, &Source::Xz(&source), &Destination::File(&decoded)).unwrap();

        let out = std::fs::read(&decoded).unwrap();
        let block = usize::try_from(BLOCK_SIZE).unwrap();
        assert_eq!(out.len(), image.len());
        assert_eq!(out[..2 * block], image[..2 * block]);
        assert!(out[2 * block..4 * block].iter().all(|&b| b == 0));
        assert_eq!(out[4 * block..], image[4 * block..]);
    }

    #[test]
    fn a_cut_xz_stream_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let image = image(4, 0);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 3)])).unwrap();
        let mut compressed = xz(&image);
        compressed.truncate(compressed.len() - 8);
        let source = dir.path().join("wic.xz");
        std::fs::write(&source, compressed).unwrap();

        let result = copy(
            &bmap,
            &Source::Xz(&source),
            &Destination::File(&dir.path().join("wic")),
        );
        assert!(
            matches!(result, Err(FlashError::CopyFailed { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_device_destination_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let image = image(1, 0);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 0)])).unwrap();
        let source = dir.path().join("wic");
        std::fs::write(&source, &image).unwrap();
        let device = dir.path().join("sdz");

        assert!(copy(&bmap, &Source::Raw(&source), &Destination::Device(&device)).is_err());
        assert!(!device.exists());
    }

    #[test]
    fn data_after_the_xz_stream_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let bmap = Bmap::parse(FIXTURE_BMAP).unwrap();
        let source = dir.path().join("wic.xz");
        std::fs::write(&source, [FIXTURE_XZ, b"trailing garbage"].concat()).unwrap();

        let result = copy(
            &bmap,
            &Source::Xz(&source),
            &Destination::File(&dir.path().join("wic")),
        );
        assert!(
            matches!(result, Err(FlashError::CopyFailed { .. })),
            "{result:?}"
        );
    }
}
