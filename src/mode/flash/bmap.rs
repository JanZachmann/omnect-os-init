//! Flash an image as its bmap file describes it.
//!
//! The bmap comes from the operator, so every value in it is checked before the
//! first write, and a bad one is an error, never a panic.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use lzma_rust2::XzReader;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::FlashError;
use crate::mode::flash::rawio::{ByteRange, COPY_BUFFER_SIZE, open_existing_for_write};

/// Major version 2 introduced `ChecksumType` and the tag names parsed here.
const SUPPORTED_MAJOR_VERSION: u64 = 2;
const CHECKSUM_TYPE: &str = "sha256";
pub(crate) const SHA256_LEN: usize = 32;

#[derive(Debug, Deserialize)]
struct XmlBmap {
    #[serde(rename = "@version")]
    version: String,
    #[serde(rename = "ImageSize")]
    image_size: String,
    #[serde(rename = "BlockSize")]
    block_size: String,
    #[serde(rename = "BlocksCount")]
    blocks_count: String,
    #[serde(rename = "MappedBlocksCount")]
    mapped_blocks_count: String,
    #[serde(rename = "ChecksumType")]
    checksum_type: String,
    #[serde(rename = "BmapFileChecksum")]
    file_checksum: String,
    #[serde(rename = "BlockMap")]
    block_map: XmlBlockMap,
}

#[derive(Debug, Deserialize)]
struct XmlBlockMap {
    #[serde(rename = "Range", default)]
    ranges: Vec<XmlRange>,
}

#[derive(Debug, Deserialize)]
struct XmlRange {
    #[serde(rename = "@chksum")]
    chksum: String,
    #[serde(rename = "$text")]
    blocks: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MappedRange {
    pub(crate) bytes: ByteRange,
    pub(crate) sha256: [u8; SHA256_LEN],
}

/// A checked bmap: ranges are sorted, do not overlap and end inside the image.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Bmap {
    pub(crate) image_size: u64,
    pub(crate) ranges: Vec<MappedRange>,
}

pub(crate) enum Source<'a> {
    Xz(&'a Path),
    /// An image the verify pass already decoded.
    #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
    Raw(&'a Path),
}

pub(crate) enum Destination<'a> {
    Device(&'a Path),
    /// Created, or truncated, to the image size.
    #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
    File(&'a Path),
}

impl Source<'_> {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Xz(path) => path,
            #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
            Self::Raw(path) => path,
        }
    }
}

impl Destination<'_> {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Device(path) => path,
            #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
            Self::File(path) => path,
        }
    }
}

fn number(name: &str, text: &str) -> Result<u64, String> {
    text.trim()
        .parse()
        .map_err(|e| format!("{name} '{}': {e}", text.trim()))
}

fn sha256_from_hex(hex: &str) -> Result<[u8; SHA256_LEN], String> {
    let hex = hex.trim();
    let invalid = || format!("checksum '{hex}' is not a sha256");
    if hex.len() != 2 * SHA256_LEN {
        return Err(invalid());
    }
    let mut digest = [0u8; SHA256_LEN];
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_| invalid())?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| invalid())?;
    }
    Ok(digest)
}

/// bmaptool hashes the file with its own checksum replaced by zeros.
fn check_file_checksum(text: &str, checksum: &str) -> Result<(), String> {
    let checksum = checksum.trim();
    let expected = sha256_from_hex(checksum)?;
    let zeros = "0".repeat(checksum.len());
    let actual = Sha256::digest(text.replacen(checksum, &zeros, 1).as_bytes());
    if actual[..] != expected {
        return Err("bmap file checksum does not match".to_string());
    }
    Ok(())
}

fn check_version(version: &str) -> Result<(), String> {
    let major = version.split('.').next().unwrap_or_default();
    match major.parse::<u64>() {
        Ok(SUPPORTED_MAJOR_VERSION) => Ok(()),
        _ => Err(format!("bmap version '{version}' is not supported")),
    }
}

/// `"first-last"` or `"block"`, both inclusive.
fn block_span(text: &str) -> Result<(u64, u64), String> {
    let text = text.trim();
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let first = number("range start", first)?;
    let last = number("range end", last)?;
    if last < first {
        return Err(format!("range '{text}' ends before it starts"));
    }
    Ok((first, last))
}

impl Bmap {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let xml: XmlBmap = quick_xml::de::from_str(text).map_err(|e| e.to_string())?;
        check_version(&xml.version)?;
        if xml.checksum_type.trim() != CHECKSUM_TYPE {
            return Err(format!(
                "checksum type '{}' is not supported",
                xml.checksum_type.trim()
            ));
        }
        check_file_checksum(text, &xml.file_checksum)?;

        let image_size = number("ImageSize", &xml.image_size)?;
        let block_size = number("BlockSize", &xml.block_size)?;
        if block_size == 0 {
            return Err("BlockSize is 0".to_string());
        }
        let blocks_count = number("BlocksCount", &xml.blocks_count)?;
        if blocks_count != image_size.div_ceil(block_size) {
            return Err(format!(
                "BlocksCount {blocks_count} does not match ImageSize {image_size}"
            ));
        }

        let overflow = || "range does not fit a byte offset".to_string();
        let mut ranges = Vec::with_capacity(xml.block_map.ranges.len());
        let mut mapped_blocks: u64 = 0;
        let mut next_free: u64 = 0;
        for range in &xml.block_map.ranges {
            let (first, last) = block_span(&range.blocks)?;
            let offset = first.checked_mul(block_size).ok_or_else(overflow)?;
            let end = last
                .checked_add(1)
                .and_then(|n| n.checked_mul(block_size))
                .ok_or_else(overflow)?
                // The last block of the image may be partial.
                .min(image_size);
            if offset >= image_size {
                return Err(format!(
                    "range '{}' starts behind the image end",
                    range.blocks.trim()
                ));
            }
            if offset < next_free {
                return Err(format!(
                    "range '{}' overlaps or is out of order",
                    range.blocks.trim()
                ));
            }
            mapped_blocks = mapped_blocks
                .checked_add(last - first + 1)
                .ok_or_else(overflow)?;
            next_free = end;
            ranges.push(MappedRange {
                bytes: ByteRange {
                    offset,
                    len: end - offset,
                },
                sha256: sha256_from_hex(&range.chksum)?,
            });
        }

        let mapped_blocks_count = number("MappedBlocksCount", &xml.mapped_blocks_count)?;
        if mapped_blocks != mapped_blocks_count {
            return Err(format!(
                "MappedBlocksCount {mapped_blocks_count} does not match the {mapped_blocks} \
                 blocks of the ranges"
            ));
        }

        Ok(Self { image_size, ranges })
    }

    /// The parts of `area` that no range of the image writes.
    pub(crate) fn unmapped(&self, area: &ByteRange) -> Vec<ByteRange> {
        let area_end = area.offset.saturating_add(area.len);
        let mut gaps = Vec::new();
        let mut cursor = area.offset;
        for range in &self.ranges {
            let start = range.bytes.offset;
            let end = start + range.bytes.len;
            if start >= area_end {
                break;
            }
            if start > cursor {
                gaps.push(ByteRange {
                    offset: cursor,
                    len: start - cursor,
                });
            }
            cursor = cursor.max(end);
        }
        if cursor < area_end {
            gaps.push(ByteRange {
                offset: cursor,
                len: area_end - cursor,
            });
        }
        gaps
    }
}

/// Read and check the bmap at `path`.
pub(crate) fn read(path: &Path) -> Result<Bmap, FlashError> {
    let invalid = |reason: String| FlashError::InvalidBmap {
        path: path.to_path_buf(),
        reason,
    };
    let text = std::fs::read_to_string(path).map_err(|e| invalid(e.to_string()))?;
    Bmap::parse(&text).map_err(invalid)
}

/// Fail when the image is larger than `device`.
pub(crate) fn check_fits(bmap: &Bmap, device: &Path) -> Result<(), FlashError> {
    let unusable = |reason: String| FlashError::InvalidDestination {
        device: device.to_path_buf(),
        reason,
    };
    let size = File::open(device)
        .and_then(|mut file| file.seek(SeekFrom::End(0)))
        .map_err(|e| unusable(format!("cannot determine size: {e}")))?;
    if bmap.image_size > size {
        return Err(unusable(format!(
            "the image needs {} bytes, the device has {size}",
            bmap.image_size
        )));
    }
    Ok(())
}

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
        Source::Xz(_) => Box::new(Stream(XzReader::new(input, true))),
        #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
        Source::Raw(_) => Box::new(Seekable(input)),
    };

    let mut output = match destination {
        Destination::Device(path) => open_existing_for_write(path),
        #[cfg(any(test, not(feature = "flash-mode-2-direct")))]
        Destination::File(path) => std::fs::OpenOptions::new()
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
        let chunk = usize::try_from(left).unwrap_or(buf.len()).min(buf.len());
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

/// A stream moves forward only by reading.
struct Stream<R>(R);

impl<R: Read> Read for Stream<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
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
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
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
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    const BLOCK_SIZE: u64 = 4096;

    /// A bmap in bmaptool's layout, with a correct file checksum.
    pub(crate) fn bmap_xml(image: &[u8], blocks: &[(u64, u64)]) -> String {
        let image_size = image.len() as u64;
        let mut mapped = 0;
        let mut ranges = String::new();
        for &(first, last) in blocks {
            mapped += last - first + 1;
            let start = usize::try_from(first * BLOCK_SIZE).unwrap();
            let end = usize::try_from((last + 1) * BLOCK_SIZE)
                .unwrap()
                .min(image.len());
            let sha = hex(&Sha256::digest(&image[start..end]));
            let span = if first == last {
                format!("{first}")
            } else {
                format!("{first}-{last}")
            };
            ranges.push_str(&format!(
                "        <Range chksum=\"{sha}\"> {span} </Range>\n"
            ));
        }
        with_checksum(&format!(
            "<?xml version=\"1.0\" ?>\n\
             <!-- comment -->\n\
             <bmap version=\"2.0\">\n\
             \x20   <ImageSize> {image_size} </ImageSize>\n\
             \x20   <BlockSize> {BLOCK_SIZE} </BlockSize>\n\
             \x20   <BlocksCount> {} </BlocksCount>\n\
             \x20   <MappedBlocksCount> {mapped} </MappedBlocksCount>\n\
             \x20   <ChecksumType> sha256 </ChecksumType>\n\
             \x20   <BmapFileChecksum> {} </BmapFileChecksum>\n\
             \x20   <BlockMap>\n{ranges}    </BlockMap>\n\
             </bmap>\n",
            image_size.div_ceil(BLOCK_SIZE),
            "0".repeat(2 * SHA256_LEN),
        ))
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn with_checksum(zeroed: &str) -> String {
        let sum = hex(&Sha256::digest(zeroed.as_bytes()));
        zeroed.replacen(&"0".repeat(2 * SHA256_LEN), &sum, 1)
    }

    /// An image of `blocks` blocks, each filled with its block number + 1, so a
    /// zero byte in the output is a byte that was not written.
    fn image(blocks: u64, tail: u64) -> Vec<u8> {
        let mut image = Vec::new();
        for block in 0..blocks {
            image.extend(std::iter::repeat_n(
                u8::try_from(block + 1).unwrap(),
                usize::try_from(BLOCK_SIZE).unwrap(),
            ));
        }
        image.extend(std::iter::repeat_n(0xee, usize::try_from(tail).unwrap()));
        image
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut writer =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap()
    }

    /// Rewrite `xml` and fix its file checksum again, so only the edit is wrong.
    fn edited(xml: &str, from: &str, to: &str) -> String {
        const OPEN_TAG: &str = "<BmapFileChecksum> ";
        let checksum_start = xml.find(OPEN_TAG).unwrap() + OPEN_TAG.len();
        let old = &xml[checksum_start..checksum_start + 2 * SHA256_LEN];
        let zeroed = xml
            .replacen(old, &"0".repeat(2 * SHA256_LEN), 1)
            .replacen(from, to, 1);
        with_checksum(&zeroed)
    }

    #[test]
    fn a_bmap_from_bmaptool_parses_to_byte_ranges() {
        let image = image(5, 100);
        let bmap = Bmap::parse(&bmap_xml(&image, &[(0, 1), (3, 3), (5, 5)])).unwrap();
        assert_eq!(bmap.image_size, 5 * BLOCK_SIZE + 100);
        let bytes: Vec<_> = bmap.ranges.iter().map(|r| &r.bytes).collect();
        assert_eq!(
            bytes,
            [
                &ByteRange {
                    offset: 0,
                    len: 2 * BLOCK_SIZE
                },
                &ByteRange {
                    offset: 3 * BLOCK_SIZE,
                    len: BLOCK_SIZE
                },
                // The partial last block ends at the image end.
                &ByteRange {
                    offset: 5 * BLOCK_SIZE,
                    len: 100
                },
            ]
        );
    }

    #[test]
    fn a_changed_bmap_fails_the_file_checksum() {
        let xml = bmap_xml(&image(4, 0), &[(0, 1)]).replacen("0-1", "0-2", 1);
        let err = Bmap::parse(&xml).unwrap_err();
        assert!(err.contains("file checksum"), "{err}");
    }

    #[test]
    fn a_bad_bmap_is_an_error_not_a_panic() {
        let xml = bmap_xml(&image(4, 0), &[(0, 1), (3, 3)]);
        let cases = [
            ("version=\"2.0\"", "version=\"3.0\"", "version"),
            ("version=\"2.0\"", "version=\"1.4\"", "version"),
            ("sha256 </Checksum", "md5 </Checksum", "checksum type"),
            ("> 0-1 <", "> 1-0 <", "ends before it starts"),
            ("> 0-1 <", "> a-1 <", "range start"),
            ("> 0-1 <", "> 0- <", "range end"),
            ("> 0-1 <", "> 18446744073709551615 <", "byte offset"),
            ("\"> 3 <", "\"> 1 <", "overlaps"),
            ("\"> 3 <", "\"> 9 <", "behind the image end"),
            ("<BlockSize> 4096", "<BlockSize> 0", "BlockSize is 0"),
            ("<BlocksCount> 4", "<BlocksCount> 5", "BlocksCount"),
            (
                "<MappedBlocksCount> 3",
                "<MappedBlocksCount> 4",
                "MappedBlocksCount",
            ),
            ("<ImageSize> 16384", "<ImageSize> x", "ImageSize"),
            ("</BlockMap>", "", ""),
        ];
        for (from, to, expected) in cases {
            let changed = edited(&xml, from, to);
            assert_ne!(changed, xml, "{from} not found");
            let err = Bmap::parse(&changed).unwrap_err();
            assert!(err.contains(expected), "{from} -> {to}: {err}");
        }
    }

    #[test]
    fn a_bad_range_checksum_is_an_error() {
        let xml = bmap_xml(&image(4, 0), &[(0, 1)]);
        let sha_start = xml.find("chksum=\"").unwrap() + 8;
        let sha = &xml[sha_start..sha_start + 2 * SHA256_LEN];
        let err = Bmap::parse(&edited(&xml, sha, "zz")).unwrap_err();
        assert!(err.contains("not a sha256"), "{err}");
    }

    fn bmap_of(ranges: &[(u64, u64)]) -> Bmap {
        Bmap {
            image_size: u64::MAX,
            ranges: ranges
                .iter()
                .map(|&(offset, len)| MappedRange {
                    bytes: ByteRange { offset, len },
                    sha256: [0; SHA256_LEN],
                })
                .collect(),
        }
    }

    fn gaps(bmap: &Bmap, offset: u64, len: u64) -> Vec<(u64, u64)> {
        bmap.unmapped(&ByteRange { offset, len })
            .into_iter()
            .map(|r| (r.offset, r.len))
            .collect()
    }

    #[test]
    fn the_unmapped_parts_of_an_area_are_the_gaps_between_ranges() {
        let bmap = bmap_of(&[(10, 10), (20, 5), (40, 20)]);
        assert_eq!(gaps(&bmap, 0, 100), [(0, 10), (25, 15), (60, 40)]);
        assert_eq!(
            gaps(&bmap, 0, 50),
            [(0, 10), (25, 15)],
            "range crosses the end"
        );
        assert_eq!(gaps(&bmap, 12, 5), [], "inside one range");
        assert_eq!(gaps(&bmap, 0, 5), [(0, 5)], "before every range");
        assert!(gaps(&bmap_of(&[]), 0, 7) == [(0, 7)], "no ranges");
        assert_eq!(gaps(&bmap, 0, 0), [], "empty area");
    }

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
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n as u64;
            Ok(n)
        }
    }

    impl Seek for Counting<'_> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
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
    fn an_image_larger_than_the_device_does_not_fit() {
        let dir = tempfile::tempdir().unwrap();
        let bmap = bmap_of(&[]);
        let device = dir.path().join("sdz");
        std::fs::write(&device, [0u8; 16]).unwrap();

        assert!(
            check_fits(
                &Bmap {
                    image_size: 16,
                    ..bmap_of(&[])
                },
                &device
            )
            .is_ok()
        );
        assert!(matches!(
            check_fits(
                &Bmap {
                    image_size: 17,
                    ..bmap
                },
                &device
            ),
            Err(FlashError::InvalidDestination { .. })
        ));
    }

    #[test]
    fn a_bmap_file_that_is_not_xml_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wic.bmap");
        std::fs::write(&path, "not a bmap").unwrap();
        assert!(matches!(read(&path), Err(FlashError::InvalidBmap { .. })));
    }
}
