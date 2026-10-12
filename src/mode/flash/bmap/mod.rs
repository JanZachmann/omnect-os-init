//! Flash an image as its bmap file describes it.
//!
//! The bmap comes from the operator, so every value in it is checked before the
//! first write, and a bad one is an error, never a panic.

mod copy;
mod parse;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::FlashError;
use crate::mode::flash::rawio::ByteRange;

pub(crate) use copy::copy;

const SHA256_LEN: usize = 32;
/// Far above the size of a real bmap, which is a few KiB.
const MAX_BMAP_LEN: u64 = 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct MappedRange {
    bytes: ByteRange,
    sha256: [u8; SHA256_LEN],
}

/// A checked bmap: ranges are sorted, do not overlap and end inside the image.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Bmap {
    image_size: u64,
    ranges: Vec<MappedRange>,
}

pub(crate) enum Source<'a> {
    Xz(&'a Path),
    /// Must be seekable: a FIFO fails at the first gap.
    #[cfg_attr(feature = "flash-mode-2-direct", allow(dead_code))]
    Raw(&'a Path),
}

pub(crate) enum Destination<'a> {
    /// Must already exist.
    Device(&'a Path),
    /// Created, or truncated, to the image size.
    #[cfg_attr(feature = "flash-mode-2-direct", allow(dead_code))]
    File(&'a Path),
}

impl Source<'_> {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Xz(path) | Self::Raw(path) => path,
        }
    }
}

impl Destination<'_> {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Device(path) | Self::File(path) => path,
        }
    }
}

impl Bmap {
    pub(crate) fn image_size(&self) -> u64 {
        self.image_size
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

pub(crate) fn read(path: &Path) -> Result<Bmap, FlashError> {
    let invalid = |reason: String| FlashError::InvalidBmap {
        path: path.to_path_buf(),
        reason,
    };
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_BMAP_LEN + 1).read_to_end(&mut bytes))
        .map_err(|source| FlashError::PathIo {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_BMAP_LEN {
        return Err(invalid(format!("larger than {MAX_BMAP_LEN} bytes")));
    }
    let text = String::from_utf8(bytes).map_err(|e| invalid(e.to_string()))?;
    Bmap::parse(&text).map_err(invalid)
}

#[cfg(test)]
mod test_support {
    use std::io::Write;

    use lzma_rust2::{XzOptions, XzWriter};
    use sha2::{Digest, Sha256};

    use crate::mode::flash::bmap::SHA256_LEN;

    pub(super) const BLOCK_SIZE: u64 = 4096;

    /// A bmap in bmaptool's layout, with a correct file checksum.
    pub(super) fn bmap_xml(image: &[u8], blocks: &[(u64, u64)]) -> String {
        let image_size = image.len() as u64;
        let mut mapped = 0;
        let mut ranges = String::new();
        for &(first, last) in blocks {
            mapped += last - first + 1;
            let start = usize::try_from(first * BLOCK_SIZE).unwrap();
            let end = usize::try_from((last + 1) * BLOCK_SIZE)
                .unwrap()
                .min(image.len());
            let sha = hex::encode(Sha256::digest(&image[start..end]));
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

    pub(super) fn with_checksum(zeroed: &str) -> String {
        let sum = hex::encode(Sha256::digest(zeroed.as_bytes()));
        zeroed.replacen(&"0".repeat(2 * SHA256_LEN), &sum, 1)
    }

    /// An image of `blocks` blocks, each filled with its block number + 1, so a
    /// zero byte in the output is a byte that was not written.
    pub(super) fn image(blocks: u64, tail: u64) -> Vec<u8> {
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

    pub(super) fn xz(data: &[u8]) -> Vec<u8> {
        let mut writer = XzWriter::new(Vec::new(), XzOptions::with_preset(1)).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap()
    }

    pub(super) const FIXTURE_BMAP: &str = include_str!("../testdata/wic.bmap");
    pub(super) const FIXTURE_XZ: &[u8] = include_bytes!("../testdata/wic.xz");
    pub(super) const FIXTURE_SHA256: &str =
        "f41a80f9f783ec8915b3420421664c25511937a60fbf9d9001b9b6ebbb53a2b7";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::flash::bmap::test_support::{
        BLOCK_SIZE, FIXTURE_BMAP, FIXTURE_SHA256, FIXTURE_XZ,
    };
    use crate::mode::flash::rawio::zero_range;
    use sha2::{Digest, Sha256};
    use std::fs;

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

    #[test]
    fn a_bmap_file_that_is_not_xml_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wic.bmap");
        fs::write(&path, "not a bmap").unwrap();
        assert!(matches!(read(&path), Err(FlashError::InvalidBmap { .. })));
        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(matches!(read(&path), Err(FlashError::InvalidBmap { .. })));
    }

    #[test]
    fn a_bmap_file_above_the_size_limit_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wic.bmap");
        let len = usize::try_from(MAX_BMAP_LEN).unwrap();
        let too_large = |len: usize| {
            let padding = " ".repeat(len - FIXTURE_BMAP.len());
            fs::write(&path, format!("{FIXTURE_BMAP}{padding}")).unwrap();
            read(&path).is_err_and(|e| e.to_string().contains("larger than"))
        };
        assert!(!too_large(len), "at the limit");
        assert!(too_large(len + 1));
    }

    #[test]
    fn a_bmap_file_that_cannot_be_read_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            read(&dir.path().join("wic.bmap")),
            Err(FlashError::PathIo { .. })
        ));
    }

    #[test]
    fn flashing_the_bmaptool_fixture_and_zeroing_the_rest_gives_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let bmap = Bmap::parse(FIXTURE_BMAP).unwrap();
        let source = dir.path().join("wic.xz");
        fs::write(&source, FIXTURE_XZ).unwrap();
        let decoded = dir.path().join("wic");
        let disk = dir.path().join("sdz");
        let disk_len = usize::try_from(bmap.image_size).unwrap() + 4096;
        fs::write(&disk, vec![0xa5; disk_len]).unwrap();

        copy(&bmap, &Source::Xz(&source), &Destination::File(&decoded)).unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(fs::read(&decoded).unwrap())),
            FIXTURE_SHA256
        );

        copy(&bmap, &Source::Raw(&decoded), &Destination::Device(&disk)).unwrap();
        let written = fs::read(&disk).unwrap();
        let block = usize::try_from(BLOCK_SIZE).unwrap();
        assert!(
            written[2 * block..5 * block].iter().all(|&b| b == 0xa5),
            "unmapped blocks keep the old data"
        );

        let all = ByteRange {
            offset: 0,
            len: bmap.image_size,
        };
        for gap in bmap.unmapped(&all) {
            zero_range(&disk, &gap).unwrap();
        }
        let flashed = fs::read(&disk).unwrap();
        let image_len = usize::try_from(bmap.image_size).unwrap();
        assert_eq!(
            hex::encode(Sha256::digest(&flashed[..image_len])),
            FIXTURE_SHA256
        );
        assert!(
            flashed[image_len..].iter().all(|&b| b == 0xa5),
            "behind the image"
        );
    }
}
