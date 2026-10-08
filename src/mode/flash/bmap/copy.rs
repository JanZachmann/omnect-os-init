//! Decode the image and write the ranges the bmap maps.

use std::fs::File;
#[cfg(any(test, not(feature = "flash-mode-2-direct")))]
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};

use lzma_rust2::XzReader;
use sha2::{Digest, Sha256};

use crate::error::FlashError;
use crate::mode::flash::bmap::{Bmap, Destination, Source};
use crate::mode::flash::rawio::{ByteRange, COPY_BUFFER_SIZE, chunk_len, open_existing_for_write};

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
        #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
        Source::Raw(_) => Box::new(Seekable(input)),
    };

    let mut output = match destination {
        Destination::Device(path) => open_existing_for_write(path),
        #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
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
    input: &mut dyn Read,
    len: u64,
    buf: &mut [u8],
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    position: u64,
) -> Result<(), String> {
    let mut left = len;
    while left > 0 {
        let chunk = chunk_len(left, buf.len());
        let read = input
            .read(&mut buf[..chunk])
            .map_err(|e| format!("reading source: {e}"))?;
        if read == 0 {
            return Err(format!(
                "source ended at byte {}, the image has more",
                position + (len - left)
            ));
        }
        sink(&buf[..read])?;
        left -= read as u64;
    }
    Ok(())
}

trait Input: Read {
    /// Move forward from `position` to `target` without using the bytes.
    fn skip(&mut self, position: u64, target: u64, buf: &mut [u8]) -> Result<(), String>;
}

fn xz_stream<R: Read>(compressed: R) -> Stream<XzReader<FullReads<R>>> {
    Stream(XzReader::new(FullReads(compressed), true))
}

/// A pipe returns only the bytes that have arrived so far. The xz decoder
/// reads the padding after a block with a single `read` and takes a short
/// count as a broken stream, so each read here waits for the whole buffer or
/// the end of the input.
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

/// A stream moves forward only by reading.
struct Stream<R>(R);

impl<R: Read> Read for Stream<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R: Read> Input for Stream<R> {
    fn skip(&mut self, position: u64, target: u64, buf: &mut [u8]) -> Result<(), String> {
        pass_through(
            &mut self.0,
            target - position,
            buf,
            &mut |_| Ok(()),
            position,
        )
    }
}

/// On ramfs, reading a hole of a sparse file puts a zero page into a page
/// cache that cannot be evicted, so the gaps are seeked over.
#[cfg(any(test, not(feature = "flash-mode-2-direct")))]
struct Seekable<R>(R);

#[cfg(any(test, not(feature = "flash-mode-2-direct")))]
impl<R: Read> Read for Seekable<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(any(test, not(feature = "flash-mode-2-direct")))]
impl<R: Read + Seek> Input for Seekable<R> {
    fn skip(&mut self, _position: u64, target: u64, _buf: &mut [u8]) -> Result<(), String> {
        self.0
            .seek(SeekFrom::Start(target))
            .map(|_| ())
            .map_err(|e| format!("seeking source: {e}"))
    }
}

/// The source must end at the image end, so a stream with a bad end (an xz
/// footer, extra data) fails instead of being taken as complete.
fn copy_stream<W: Write + Seek>(
    bmap: &Bmap,
    input: &mut dyn Input,
    output: &mut W,
) -> Result<(), String> {
    let mut buf = vec![0u8; COPY_BUFFER_SIZE];
    let mut position: u64 = 0;

    for range in &bmap.ranges {
        let ByteRange { offset, len } = range.bytes;
        input.skip(position, offset, &mut buf)?;
        output
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seeking destination: {e}"))?;

        let mut hasher = Sha256::new();
        pass_through(
            input,
            len,
            &mut buf,
            &mut |chunk| {
                hasher.update(chunk);
                output
                    .write_all(chunk)
                    .map_err(|e| format!("writing destination: {e}"))
            },
            offset,
        )?;
        if hasher.finalize()[..] != range.sha256 {
            return Err(format!(
                "checksum mismatch in bytes {offset}..{}",
                offset + len
            ));
        }
        position = offset + len;
    }

    input.skip(position, bmap.image_size, &mut buf)?;
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
    use std::io::Cursor;

    fn copied(bmap: &Bmap, source: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Cursor::new(Vec::new());
        copy_stream(bmap, &mut Stream(Cursor::new(source)), &mut out)?;
        Ok(out.into_inner())
    }

    /// Counts the bytes read through it.
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
    fn a_seekable_source_reads_only_the_mapped_ranges() {
        let image = image(5, 100);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(1, 1), (3, 3)])).unwrap();
        let mut input = Seekable(Counting {
            inner: Cursor::new(&image),
            read: 0,
        });
        let mut out = Cursor::new(Vec::new());
        copy_stream(&bmap, &mut input, &mut out).unwrap();

        assert_eq!(input.0.read, 2 * BLOCK_SIZE);
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
