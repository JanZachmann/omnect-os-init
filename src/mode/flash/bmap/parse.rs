//! Parse a bmap file and check every value in it.

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::mode::flash::bmap::{Bmap, MappedRange, SHA256_LEN};
use crate::mode::flash::rawio::ByteRange;

const SUPPORTED_MAJOR_VERSION: u64 = 2;
const CHECKSUM_TYPE: &str = "sha256";

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

fn number(name: &str, text: &str) -> Result<u64, String> {
    text.trim()
        .parse()
        .map_err(|e| format!("{name} '{}': {e}", text.trim()))
}

fn sha256_from_hex(text: &str) -> Result<[u8; SHA256_LEN], String> {
    let text = text.trim();
    let mut digest = [0u8; SHA256_LEN];
    hex::decode_to_slice(text, &mut digest)
        .map_err(|_| format!("checksum '{text}' is not a sha256"))?;
    Ok(digest)
}

/// The file checksum is the sha256 of the file with the checksum value
/// replaced by zeros.
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

/// `"major.minor"`, both numbers.
fn check_version(version: &str) -> Result<(), String> {
    let supported = version.split_once('.').is_some_and(|(major, minor)| {
        major.parse() == Ok(SUPPORTED_MAJOR_VERSION) && minor.parse::<u64>().is_ok()
    });
    if supported {
        Ok(())
    } else {
        Err(format!("bmap version '{version}' is not supported"))
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
            if last >= blocks_count {
                return Err(format!(
                    "range '{}' ends behind the image end",
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

        // A real image always maps its partition table.
        if ranges.is_empty() {
            return Err("bmap maps no blocks".to_string());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::flash::bmap::test_support::{
        BLOCK_SIZE, FIXTURE_BMAP, bmap_xml, image, with_checksum,
    };

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
    fn adjacent_ranges_and_any_minor_version_are_accepted() {
        let xml = bmap_xml(&image(4, 0), &[(0, 1), (2, 2)]);
        assert_eq!(Bmap::parse(&xml).unwrap().ranges.len(), 2);
        let xml = edited(&xml, "version=\"2.0\"", "version=\"2.1\"");
        assert!(Bmap::parse(&xml).is_ok());
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
            ("version=\"2.0\"", "version=\"2\"", "version"),
            ("version=\"2.0\"", "version=\"2.x\"", "version"),
            ("version=\"2.0\"", "version=\" 2.0\"", "version"),
            ("> 0-1 <", "> 0-1-2 <", "range end"),
            ("> 0-1 <", "> 0-18446744073709551615 <", "byte offset"),
            ("<Range chksum=", "<Range sum=", "missing field"),
            (
                "<BlockSize>",
                "<ImageSize> 1 </ImageSize><BlockSize>",
                "duplicate field",
            ),
            ("sha256 </Checksum", "md5 </Checksum", "checksum type"),
            ("> 0-1 <", "> 1-0 <", "ends before it starts"),
            ("> 0-1 <", "> a-1 <", "range start"),
            ("> 0-1 <", "> 0- <", "range end"),
            ("> 0-1 <", "> 18446744073709551615 <", "byte offset"),
            ("\"> 3 <", "\"> 1 <", "overlaps"),
            ("\"> 3 <", "\"> 9 <", "behind the image end"),
            ("\"> 3 <", "\"> 3-4 <", "behind the image end"),
            ("<BlockSize> 4096", "<BlockSize> 0", "BlockSize is 0"),
            ("<BlocksCount> 4", "<BlocksCount> 5", "BlocksCount"),
            (
                "<MappedBlocksCount> 3",
                "<MappedBlocksCount> 4",
                "MappedBlocksCount",
            ),
            ("<ImageSize> 16384", "<ImageSize> x", "ImageSize"),
            ("</BlockMap>", "", "ill-formed document"),
        ];
        for (from, to, expected) in cases {
            let changed = edited(&xml, from, to);
            assert_ne!(changed, xml, "{from} not found");
            let err = Bmap::parse(&changed).unwrap_err();
            assert!(err.contains(expected), "{from} -> {to}: {err}");
        }
    }

    #[test]
    fn a_bmap_that_maps_nothing_is_an_error() {
        for image in [image(0, 0), image(4, 0)] {
            let err = Bmap::parse(&bmap_xml(&image, &[])).unwrap_err();
            assert!(
                err.contains("maps no blocks"),
                "{} bytes: {err}",
                image.len()
            );
        }
    }

    #[test]
    fn a_bad_range_checksum_is_an_error() {
        const CHKSUM_ATTR: &str = "chksum=\"";
        let xml = bmap_xml(&image(4, 0), &[(0, 1)]);
        let sha_start = xml.find(CHKSUM_ATTR).unwrap() + CHKSUM_ATTR.len();
        let sha = &xml[sha_start..sha_start + 2 * SHA256_LEN];
        let plus_sign = format!("+{}", &sha[1..]);
        for bad in ["zz", plus_sign.as_str()] {
            let err = Bmap::parse(&edited(&xml, sha, bad)).unwrap_err();
            assert!(err.contains("not a sha256"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_bmap_from_bmaptool_is_accepted() {
        let bmap = Bmap::parse(FIXTURE_BMAP).unwrap();
        assert_eq!(bmap.image_size, 20 * BLOCK_SIZE + 100);
        let bytes: Vec<_> = bmap
            .ranges
            .iter()
            .map(|r| (r.bytes.offset / BLOCK_SIZE, r.bytes.len))
            .collect();
        assert_eq!(
            bytes,
            [
                (0, 2 * BLOCK_SIZE),
                (5, BLOCK_SIZE),
                (9, 3 * BLOCK_SIZE),
                (20, 100)
            ]
        );
    }
}
