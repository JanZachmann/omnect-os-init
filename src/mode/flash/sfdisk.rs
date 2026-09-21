//! Rewriting and applying `sfdisk -d` partition-table dumps.
//!
//! Flash mode 1 clones the running disk onto another device. The clone must
//! not inherit any growth the running disk picked up from `resize-data`, so
//! the dump taken from the source is rewritten here to reset the data
//! partition back to its shipped size before it is applied to the
//! destination.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::FlashError;
use crate::partition::layout::PARTITION_NUM_DATA;
#[cfg(feature = "dos")]
use crate::partition::layout::PARTITION_NUM_EXTENDED;

pub const SFDISK_CMD: &str = "/sbin/sfdisk";
const SFDISK_DUMP_FLAG: &str = "-d";
const SFDISK_OPERATION_DUMP: &str = "dump";
const SFDISK_OPERATION_APPLY: &str = "apply";

const SECTOR_SIZE: u64 = 512;
// sfdisk dumps are always in 512-byte sectors; a 1 KB block is two of those.
const KB_PER_SECTOR_NUMERATOR: u64 = 2;

const UNIT_FIELD_PREFIX: &str = "unit:";
const UNIT_SECTORS_VALUE: &str = "sectors";
#[cfg(feature = "gpt")]
const LAST_LBA_FIELD: &str = "last-lba:";
const DEVICE_LINE_PREFIX: &str = "/dev/";
const DEVICE_FIELD_SEPARATOR: &str = " : ";
const START_FIELD: &str = "start=";
const SIZE_FIELD: &str = "size=";

/// Rewrite a source dump so the data partition (and, for a DOS table, the
/// extended container that holds it) is reset to its shipped size.
///
/// Every other partition is carried over unchanged, so identities such as
/// GPT `label-id` or partition types and names survive the clone.
pub fn rewrite_dump(dump: &str, data_size_kb: u64) -> Result<String, FlashError> {
    verify_unit_sectors(dump)?;

    let mut lines: Vec<String> = dump.lines().map(str::to_string).collect();

    let data_idx = find_partition_line(&lines, PARTITION_NUM_DATA).ok_or_else(|| {
        FlashError::MalformedDump(format!(
            "no partition {PARTITION_NUM_DATA} (the data partition) found in dump"
        ))
    })?;
    let data_start = parse_field_u64(&lines[data_idx], START_FIELD)?;
    let data_sectors = data_size_kb * KB_PER_SECTOR_NUMERATOR;

    log::info!(
        "Resetting the data partition to its shipped size: {data_sectors} sectors ({} bytes)",
        data_sectors * SECTOR_SIZE
    );

    lines[data_idx] = set_field_u64(&lines[data_idx], SIZE_FIELD, data_sectors)?;

    rewrite_layout_specific(&mut lines, data_start, data_sectors)?;

    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

/// Dump the partition table of `device` with `sfdisk -d`.
pub fn dump(device: &Path) -> Result<String, FlashError> {
    let output = Command::new(SFDISK_CMD)
        .arg(SFDISK_DUMP_FLAG)
        .arg(device)
        .output()
        .map_err(|e| FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_DUMP.to_string(),
            reason: format!("failed to run {SFDISK_CMD}: {e}"),
        })?;

    if !output.status.success() {
        return Err(FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_DUMP.to_string(),
            reason: format!(
                "{SFDISK_CMD} {SFDISK_DUMP_FLAG} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Apply a rewritten dump to `device` by piping it into `sfdisk`.
pub fn apply(device: &Path, dump: &str) -> Result<(), FlashError> {
    let mut child = Command::new(SFDISK_CMD)
        .arg(device)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_APPLY.to_string(),
            reason: format!("failed to spawn {SFDISK_CMD}: {e}"),
        })?;

    child
        .stdin
        .take()
        .ok_or_else(|| FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_APPLY.to_string(),
            reason: "failed to open stdin".to_string(),
        })?
        .write_all(dump.as_bytes())
        .map_err(|e| FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_APPLY.to_string(),
            reason: format!("failed to write the dump to {SFDISK_CMD}: {e}"),
        })?;

    let output = child
        .wait_with_output()
        .map_err(|e| FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_APPLY.to_string(),
            reason: format!("failed to wait for {SFDISK_CMD}: {e}"),
        })?;

    if !output.status.success() {
        return Err(FlashError::PartitionTable {
            device: device.to_path_buf(),
            operation: SFDISK_OPERATION_APPLY.to_string(),
            reason: format!(
                "{SFDISK_CMD} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }

    Ok(())
}

#[cfg(feature = "gpt")]
fn rewrite_layout_specific(
    lines: &mut [String],
    data_start: u64,
    data_sectors: u64,
) -> Result<(), FlashError> {
    let last_lba_idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with(LAST_LBA_FIELD))
        .ok_or_else(|| FlashError::MalformedDump("missing 'last-lba:' header in dump".into()))?;
    let new_last_lba = data_start + data_sectors - 1;
    lines[last_lba_idx] = set_field_u64(&lines[last_lba_idx], LAST_LBA_FIELD, new_last_lba)?;
    Ok(())
}

#[cfg(feature = "dos")]
fn rewrite_layout_specific(
    lines: &mut [String],
    data_start: u64,
    data_sectors: u64,
) -> Result<(), FlashError> {
    let extended_idx = find_partition_line(lines, PARTITION_NUM_EXTENDED).ok_or_else(|| {
        FlashError::MalformedDump(format!(
            "no partition {PARTITION_NUM_EXTENDED} (the extended container) found in dump"
        ))
    })?;
    let extended_start = parse_field_u64(&lines[extended_idx], START_FIELD)?;
    let new_size = data_start - extended_start + data_sectors;
    lines[extended_idx] = set_field_u64(&lines[extended_idx], SIZE_FIELD, new_size)?;
    Ok(())
}

fn verify_unit_sectors(dump: &str) -> Result<(), FlashError> {
    let unit_line = dump
        .lines()
        .find(|l| l.trim_start().starts_with(UNIT_FIELD_PREFIX))
        .ok_or_else(|| FlashError::MalformedDump("missing 'unit:' declaration in dump".into()))?;
    let value = unit_line
        .split_once(':')
        .map(|(_, value)| value.trim())
        .unwrap_or_default();
    if value != UNIT_SECTORS_VALUE {
        return Err(FlashError::MalformedDump(format!(
            "dump is not in sectors: {unit_line}"
        )));
    }
    Ok(())
}

/// Find the line describing the partition numbered `partition_num`.
fn find_partition_line(lines: &[String], partition_num: u32) -> Option<usize> {
    lines
        .iter()
        .position(|l| partition_number(l) == Some(partition_num))
}

/// Parse the partition number out of a dump line's device path, e.g.
/// `/dev/mmcblk0p7 : start=...` -> `7`.
fn partition_number(line: &str) -> Option<u32> {
    let token = line
        .trim_start()
        .strip_prefix(DEVICE_LINE_PREFIX)?
        .split(DEVICE_FIELD_SEPARATOR)
        .next()?;
    let digit_start = token
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    token[digit_start..].parse().ok()
}

/// Read a `field=value` pair out of a dump line, up to the next comma.
fn field_value<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let after = line.split_once(field)?.1;
    let end = after.find(',').unwrap_or(after.len());
    Some(after[..end].trim())
}

fn parse_field_u64(line: &str, field: &str) -> Result<u64, FlashError> {
    field_value(line, field)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| FlashError::MalformedDump(format!("unparsable {field} field: {line}")))
}

/// Replace the numeric value of `field` in `line`, keeping the surrounding
/// text — including the original spacing before the digits — unchanged.
fn set_field_u64(line: &str, field: &str, value: u64) -> Result<String, FlashError> {
    let field_pos = line
        .find(field)
        .ok_or_else(|| FlashError::MalformedDump(format!("missing {field} field: {line}")))?;
    let value_start = field_pos + field.len();
    let rest = &line[value_start..];
    let ws_len = rest.len() - rest.trim_start().len();
    let digits_start = value_start + ws_len;
    let digits = &line[digits_start..];
    let digits_len = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if digits_len == 0 {
        return Err(FlashError::MalformedDump(format!(
            "unparsable {field} field: {line}"
        )));
    }
    let digits_end = digits_start + digits_len;
    Ok(format!(
        "{}{value}{}",
        &line[..digits_start],
        &line[digits_end..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA_SIZE_KB: u64 = 4096; // 8192 sectors

    #[cfg(feature = "gpt")]
    const GPT_DUMP: &str = "\
label: gpt
label-id: 6B1F0F1E-0000-4000-8000-000000000001
device: /dev/mmcblk0
unit: sectors
first-lba: 34
last-lba: 30777310
sector-size: 512

/dev/mmcblk0p1 : start=        8192, size=      131072, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=\"boot\"
/dev/mmcblk0p2 : start=      139264, size=     2097152, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"rootA\"
/dev/mmcblk0p3 : start=     2236416, size=     2097152, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"rootB\"
/dev/mmcblk0p4 : start=     4333568, size=       32768, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"factory\"
/dev/mmcblk0p5 : start=     4366336, size=       32768, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"cert\"
/dev/mmcblk0p6 : start=     4399104, size=       65536, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"etc\"
/dev/mmcblk0p7 : start=     4464640, size=    26312671, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=\"data\"
";

    #[cfg(feature = "gpt")]
    #[test]
    fn gpt_rewrite_shrinks_data_and_pulls_last_lba_in() {
        let out = rewrite_dump(GPT_DUMP, DATA_SIZE_KB).unwrap();
        // data start 4464640 + 4096*2 - 1
        assert!(
            out.contains("last-lba: 4472831"),
            "last-lba must follow the shrunk data partition:\n{out}"
        );
        let data_line = out
            .lines()
            .find(|l| l.contains("name=\"data\""))
            .expect("data line survives the rewrite");
        assert!(
            data_line.contains("size=        8192")
                || data_line.contains("size= 8192")
                || data_line.replace(' ', "").contains("size=8192"),
            "data must be reset to the shipped size: {data_line}"
        );
        // Everything else is carried over untouched.
        assert!(out.contains("start=        8192, size=      131072"));
        assert!(out.contains("label: gpt"));
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn gpt_rewrite_leaves_the_other_partitions_alone() {
        let out = rewrite_dump(GPT_DUMP, DATA_SIZE_KB).unwrap();
        for name in ["boot", "rootA", "rootB", "factory", "cert", "etc"] {
            let before = GPT_DUMP
                .lines()
                .find(|l| l.contains(&format!("name=\"{name}\"")))
                .unwrap();
            assert!(out.contains(before), "{name} must be carried over verbatim");
        }
    }

    #[cfg(feature = "dos")]
    const DOS_DUMP: &str = "\
label: dos
label-id: 0x00000001
device: /dev/sda
unit: sectors
sector-size: 512

/dev/sda1 : start=        8192, size=      131072, type=c
/dev/sda2 : start=      139264, size=     2097152, type=83
/dev/sda3 : start=     2236416, size=     2097152, type=83
/dev/sda4 : start=     4333568, size=    26443743, type=5
/dev/sda5 : start=     4335616, size=       32768, type=83
/dev/sda6 : start=     4370432, size=       32768, type=83
/dev/sda7 : start=     4405248, size=       65536, type=83
/dev/sda8 : start=     4472832, size=    26304479, type=83
";

    #[cfg(feature = "dos")]
    #[test]
    fn dos_rewrite_shrinks_data_and_the_extended_container_that_holds_it() {
        let out = rewrite_dump(DOS_DUMP, DATA_SIZE_KB).unwrap();
        let data = out.lines().find(|l| l.starts_with("/dev/sda8")).unwrap();
        assert!(
            data.replace(' ', "").contains("size=8192"),
            "data must be reset to the shipped size: {data}"
        );
        let extended = out.lines().find(|l| l.starts_with("/dev/sda4")).unwrap();
        // data_start - extended_start + DATA_SIZE*2 = 4472832 - 4333568 + 8192
        assert!(
            extended.replace(' ', "").contains("size=147456"),
            "the extended container must end with the data partition: {extended}"
        );
    }

    #[test]
    fn rewrite_rejects_a_dump_with_no_data_partition() {
        let err = rewrite_dump("label: gpt\nunit: sectors\n\n", DATA_SIZE_KB).unwrap_err();
        assert!(matches!(err, FlashError::MalformedDump(_)), "{err}");
    }

    #[test]
    fn rewrite_rejects_a_dump_with_an_unparsable_start_sector() {
        let bad = "label: gpt\nunit: sectors\nlast-lba: 100\n\n\
                   /dev/mmcblk0p7 : start=notanumber, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump(_))
        ));
    }

    #[test]
    fn rewrite_rejects_a_dump_in_units_other_than_sectors() {
        let bad = "label: gpt\nunit: cylinders\n\n\
                   /dev/mmcblk0p7 : start=100, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump(_))
        ));
    }
}
