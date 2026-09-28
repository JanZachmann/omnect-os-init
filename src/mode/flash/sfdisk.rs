//! Rewriting and applying `sfdisk -d` partition-table dumps.
//!
//! The clone must not inherit the growth `resize-data` applied to the running
//! disk, so the source dump is rewritten to reset the data partition to its
//! shipped size before it is applied to the destination.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::BuildConstant;
use crate::error::{FlashError, PartitionTableOperation};
#[cfg(feature = "dos")]
use crate::partition::layout::PARTITION_NUM_EXTENDED;
use crate::partition::layout::{PARTITION_NUM_DATA, partition_suffix};

const SFDISK_CMD: &str = "/sbin/sfdisk";
const SFDISK_DUMP_FLAG: &str = "-d";
#[cfg(feature = "gpt")]
const SFDISK_PART_UUID_FLAG: &str = "--part-uuid";

const SECTOR_SIZE: u64 = 512;
/// A 1 KB block is two sectors, which holds only for 512-byte sectors.
/// `verify_sector_scale` rejects a dump that declares any other sector size.
const SECTORS_PER_KB: u64 = 2;

const UNIT_FIELD_PREFIX: &str = "unit:";
const UNIT_SECTORS_VALUE: &str = "sectors";
const SECTOR_SIZE_FIELD_PREFIX: &str = "sector-size:";
#[cfg(feature = "gpt")]
const LAST_LBA_FIELD: &str = "last-lba:";
const DEVICE_LINE_PREFIX: &str = "/dev/";
const START_FIELD: &str = "start=";
const SIZE_FIELD: &str = "size=";

/// Rewrite the dump of `device` so the data partition (and, for a DOS table,
/// the extended container that holds it) is reset to its shipped size.
///
/// Every other line is carried over unchanged, so identities such as GPT
/// `label-id` or partition types and names survive the clone.
pub(crate) fn rewrite_dump(
    device: &Path,
    dump: &str,
    data_size_kb: u64,
) -> Result<String, FlashError> {
    let malformed = |reason: String| FlashError::MalformedDump {
        device: device.to_path_buf(),
        reason,
    };

    verify_sector_scale(dump).map_err(malformed)?;

    let mut lines: Vec<String> = dump.lines().map(str::to_string).collect();

    let data_idx = find_partition_line(&lines, PARTITION_NUM_DATA).ok_or_else(|| {
        malformed(format!(
            "no partition {PARTITION_NUM_DATA} (the data partition) found in dump"
        ))
    })?;
    let data_start = parse_field_u64(&lines[data_idx], START_FIELD).map_err(malformed)?;
    let unusable_data_size = |what: &str| FlashError::InvalidBuildConstant {
        name: BuildConstant::DataSize,
        reason: format!("{data_size_kb} KB does not fit {what}"),
    };
    let data_sectors = data_size_kb
        .checked_mul(SECTORS_PER_KB)
        .ok_or_else(|| unusable_data_size("a sector count"))?;
    let data_bytes = data_sectors
        .checked_mul(SECTOR_SIZE)
        .ok_or_else(|| unusable_data_size("a byte count"))?;

    log::info!(
        "Resetting the data partition to its shipped size: {data_sectors} sectors ({data_bytes} bytes)"
    );

    lines[data_idx] =
        set_field_u64(&lines[data_idx], SIZE_FIELD, data_sectors).map_err(malformed)?;

    rewrite_layout_specific(&mut lines, data_start, data_sectors).map_err(malformed)?;

    // The table about to be applied is the most useful thing in the run log of
    // a failed apply. One record per line, because a multi-line record
    // reaches `/dev/kmsg` as a single write with embedded newlines.
    log::info!("rewritten partition table for the destination:");
    for line in &lines {
        log::info!("{line}");
    }

    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

pub(crate) fn dump(device: &Path) -> Result<String, FlashError> {
    let dump_failed = |reason: String| FlashError::PartitionTable {
        device: device.to_path_buf(),
        operation: PartitionTableOperation::Dump,
        reason,
    };

    let output = Command::new(SFDISK_CMD)
        .arg(SFDISK_DUMP_FLAG)
        .arg(device)
        .output()
        .map_err(|e| dump_failed(format!("failed to run {SFDISK_CMD}: {e}")))?;

    if !output.status.success() {
        return Err(dump_failed(format!(
            "{SFDISK_CMD} {SFDISK_DUMP_FLAG} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn apply(device: &Path, dump: &str) -> Result<(), FlashError> {
    let apply_failed = |reason: String| FlashError::PartitionTable {
        device: device.to_path_buf(),
        operation: PartitionTableOperation::Apply,
        reason,
    };

    let mut child = Command::new(SFDISK_CMD)
        .arg(device)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| apply_failed(format!("failed to spawn {SFDISK_CMD}: {e}")))?;

    let written = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin".to_string())
        .and_then(|mut stdin| {
            stdin
                .write_all(dump.as_bytes())
                .map_err(|e| format!("failed to write the dump to {SFDISK_CMD}: {e}"))
        });

    // Reaped on both paths: a dump sfdisk rejects makes it exit before the
    // write completes, so the write reports a broken pipe while the message
    // saying what is wrong with the table is on the child's stderr.
    let output = child
        .wait_with_output()
        .map_err(|e| apply_failed(format!("failed to wait for {SFDISK_CMD}: {e}")))?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    if let Err(reason) = written {
        return Err(apply_failed(format!(
            "{reason}; {SFDISK_CMD} said: {stderr}"
        )));
    }

    if !output.status.success() {
        return Err(apply_failed(format!(
            "{SFDISK_CMD} failed ({}): {stderr}",
            output.status
        )));
    }

    Ok(())
}

#[cfg(feature = "gpt")]
fn part_uuid_args(device: &Path, part_num: u32, uuid: &str) -> Vec<String> {
    vec![
        SFDISK_PART_UUID_FLAG.to_string(),
        device.display().to_string(),
        part_num.to_string(),
        uuid.to_string(),
    ]
}

/// Give partition `part_num` of `device` a fresh GPT partition-entry UUID.
///
/// The UUID lives in the partition table, so it survives any later image copy
/// into that partition.
#[cfg(feature = "gpt")]
pub(crate) fn set_part_uuid(device: &Path, part_num: u32, uuid: &str) -> Result<(), FlashError> {
    let uuid_failed = |reason: String| FlashError::UuidFailed {
        device: device.to_path_buf(),
        reason,
    };

    let output = Command::new(SFDISK_CMD)
        .args(part_uuid_args(device, part_num, uuid))
        .output()
        .map_err(|e| uuid_failed(format!("failed to run {SFDISK_CMD}: {e}")))?;

    if !output.status.success() {
        return Err(uuid_failed(format!(
            "{SFDISK_CMD} {SFDISK_PART_UUID_FLAG} on partition {part_num} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

#[cfg(feature = "gpt")]
fn rewrite_layout_specific(
    lines: &mut [String],
    data_start: u64,
    data_sectors: u64,
) -> Result<(), String> {
    let last_lba_idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with(LAST_LBA_FIELD))
        .ok_or_else(|| "missing 'last-lba:' header in dump".to_string())?;
    let new_last_lba = data_start
        .checked_add(data_sectors)
        .and_then(|end| end.checked_sub(1))
        .ok_or_else(|| {
            format!(
                "data partition of {data_sectors} sectors starting at {data_start} \
                 computes an invalid last-lba"
            )
        })?;
    lines[last_lba_idx] = set_field_u64(&lines[last_lba_idx], LAST_LBA_FIELD, new_last_lba)?;
    Ok(())
}

#[cfg(feature = "dos")]
fn rewrite_layout_specific(
    lines: &mut [String],
    data_start: u64,
    data_sectors: u64,
) -> Result<(), String> {
    let extended_idx = find_partition_line(lines, PARTITION_NUM_EXTENDED).ok_or_else(|| {
        format!("no partition {PARTITION_NUM_EXTENDED} (the extended container) found in dump")
    })?;
    let extended_start = parse_field_u64(&lines[extended_idx], START_FIELD)?;
    let new_size = data_start
        .checked_sub(extended_start)
        .ok_or_else(|| {
            format!(
                "extended container starts after the data partition: {}",
                lines[extended_idx]
            )
        })?
        .checked_add(data_sectors)
        .ok_or_else(|| {
            format!(
                "data partition of {data_sectors} sectors starting at {data_start} \
                 computes an invalid extended-container size"
            )
        })?;
    lines[extended_idx] = set_field_u64(&lines[extended_idx], SIZE_FIELD, new_size)?;
    Ok(())
}

/// Reject a dump unless it declares both `unit: sectors` and `sector-size: 512`.
fn verify_sector_scale(dump: &str) -> Result<(), String> {
    let unit_line = dump
        .lines()
        .find(|l| l.trim_start().starts_with(UNIT_FIELD_PREFIX))
        .ok_or_else(|| "missing 'unit:' declaration in dump".to_string())?;
    let unit_value = field_value(unit_line, UNIT_FIELD_PREFIX)
        .ok_or_else(|| format!("unparsable 'unit:' line: {unit_line}"))?;
    if unit_value != UNIT_SECTORS_VALUE {
        return Err(format!("dump is not in sectors: {unit_line}"));
    }

    let sector_size_line = dump
        .lines()
        .find(|l| l.trim_start().starts_with(SECTOR_SIZE_FIELD_PREFIX))
        .ok_or_else(|| "missing 'sector-size:' declaration in dump".to_string())?;
    let sector_size = parse_field_u64(sector_size_line, SECTOR_SIZE_FIELD_PREFIX)?;
    if sector_size != SECTOR_SIZE {
        return Err(format!(
            "unsupported sector size (only {SECTOR_SIZE}-byte sectors are supported): {sector_size_line}"
        ));
    }
    Ok(())
}

fn find_partition_line(lines: &[String], partition_num: u32) -> Option<usize> {
    lines
        .iter()
        .position(|l| partition_number(l) == Some(partition_num))
}

/// Parse the partition number out of a dump line's device path, e.g.
/// `/dev/mmcblk0p7 : start=...` -> `7`.
fn partition_number(line: &str) -> Option<u32> {
    let device = line.split_whitespace().next()?;
    if !device.starts_with(DEVICE_LINE_PREFIX) {
        return None;
    }
    partition_suffix(Path::new(device))
}

/// Read a `field=value` pair out of a dump line, up to the next comma.
fn field_value<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let after = line.split_once(field)?.1;
    let end = after.find(',').unwrap_or(after.len());
    Some(after[..end].trim())
}

fn parse_field_u64(line: &str, field: &str) -> Result<u64, String> {
    field_value(line, field)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("unparsable {field} field: {line}"))
}

/// Replace the numeric value of `field` in `line`, keeping the surrounding
/// text — including the original spacing before the digits — unchanged.
fn set_field_u64(line: &str, field: &str, value: u64) -> Result<String, String> {
    let field_pos = line
        .find(field)
        .ok_or_else(|| format!("missing {field} field: {line}"))?;
    let value_start = field_pos + field.len();
    let rest = &line[value_start..];
    let ws_len = rest.len() - rest.trim_start().len();
    let digits_start = value_start + ws_len;
    let digits = &line[digits_start..];
    let digits_len = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if digits_len == 0 {
        return Err(format!("unparsable {field} field: {line}"));
    }
    let digits_end = digits_start + digits_len;
    Ok(format!(
        "{}{value}{}",
        &line[..digits_start],
        &line[digits_end..]
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[cfg(feature = "dos")]
    pub(crate) use DOS_DUMP as SOURCE_DUMP;
    #[cfg(feature = "gpt")]
    pub(crate) use GPT_DUMP as SOURCE_DUMP;

    const DATA_SIZE_KB: u64 = 4096; // 8192 sectors

    #[cfg(feature = "gpt")]
    #[test]
    fn part_uuid_args_name_the_disk_the_partition_and_the_new_uuid() {
        assert_eq!(
            part_uuid_args(
                Path::new("/dev/mmcblk2"),
                2,
                "9b7a1c3e-0000-4000-8000-000000000001"
            ),
            vec![
                "--part-uuid".to_string(),
                "/dev/mmcblk2".to_string(),
                "2".to_string(),
                "9b7a1c3e-0000-4000-8000-000000000001".to_string(),
            ]
        );
    }

    #[cfg(feature = "gpt")]
    pub(crate) const GPT_DUMP: &str = "\
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
        let out = rewrite_dump(Path::new("/dev/mmcblk0"), GPT_DUMP, DATA_SIZE_KB).unwrap();
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
            data_line.replace(' ', "").contains("size=8192,"),
            "data must be reset to the shipped size: {data_line}"
        );
        assert!(out.contains("start=        8192, size=      131072"));
        assert!(out.contains("label: gpt"));
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn gpt_rewrite_leaves_the_other_partitions_alone() {
        let out = rewrite_dump(Path::new("/dev/mmcblk0"), GPT_DUMP, DATA_SIZE_KB).unwrap();
        for name in ["boot", "rootA", "rootB", "factory", "cert", "etc"] {
            let before = GPT_DUMP
                .lines()
                .find(|l| l.contains(&format!("name=\"{name}\"")))
                .unwrap();
            assert!(out.contains(before), "{name} must be carried over verbatim");
        }
    }

    #[cfg(feature = "dos")]
    pub(crate) const DOS_DUMP: &str = "\
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
        let out = rewrite_dump(Path::new("/dev/sda"), DOS_DUMP, DATA_SIZE_KB).unwrap();
        let data = out.lines().find(|l| l.starts_with("/dev/sda8")).unwrap();
        assert!(
            data.replace(' ', "").contains("size=8192,"),
            "data must be reset to the shipped size: {data}"
        );
        let extended = out.lines().find(|l| l.starts_with("/dev/sda4")).unwrap();
        // data_start - extended_start + DATA_SIZE*2 = 4472832 - 4333568 + 8192
        assert!(
            extended.replace(' ', "").contains("size=147456,"),
            "the extended container must end with the data partition: {extended}"
        );
    }

    #[cfg(feature = "dos")]
    #[test]
    fn dos_rewrite_leaves_every_other_line_alone() {
        let out = rewrite_dump(Path::new("/dev/sda"), DOS_DUMP, DATA_SIZE_KB).unwrap();
        for line in DOS_DUMP
            .lines()
            .filter(|l| !l.starts_with("/dev/sda4") && !l.starts_with("/dev/sda8"))
        {
            assert!(
                out.lines().any(|o| o == line),
                "must be carried over verbatim: {line}"
            );
        }
    }

    /// The start sector of the data partition, and of the DOS extended
    /// container, is what the rewrite computes the new sizes from, so it must
    /// not move.
    #[test]
    fn the_rewrite_keeps_every_start_sector() {
        let (device, dump) = (Path::new("/dev/sda"), SOURCE_DUMP);
        let out = rewrite_dump(device, dump, DATA_SIZE_KB).unwrap();
        let starts = |text: &str| -> Vec<u64> {
            text.lines()
                .filter(|l| l.starts_with(DEVICE_LINE_PREFIX))
                .map(|l| parse_field_u64(l, START_FIELD).unwrap())
                .collect()
        };
        assert_eq!(starts(&out), starts(dump));
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn gpt_rewrite_rejects_a_dump_missing_last_lba() {
        let bad = "label: gpt\nunit: sectors\nsector-size: 512\n\n\
                   /dev/mmcblk0p7 : start=100, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(Path::new("/dev/mmcblk0"), bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump { .. })
        ));
    }

    #[test]
    fn rewrite_rejects_a_dump_with_no_data_partition() {
        let err = rewrite_dump(
            Path::new("/dev/mmcblk0"),
            "label: gpt\nunit: sectors\nsector-size: 512\n\n",
            DATA_SIZE_KB,
        )
        .unwrap_err();
        assert!(matches!(err, FlashError::MalformedDump { .. }), "{err}");
    }

    #[test]
    fn rewrite_rejects_a_dump_with_an_unparsable_start_sector() {
        // The data partition is 7 on GPT and 8 on DOS, so both indices carry
        // the bad start sector, and partition 4 is present so that the DOS
        // rewrite would otherwise get all the way through. Without either, one
        // layout stops at a missing partition and never reaches the parse.
        let bad = "label: gpt\nunit: sectors\nsector-size: 512\nlast-lba: 100\n\n\
                   /dev/mmcblk0p4 : start=50, size=10, type=5\n\
                   /dev/mmcblk0p7 : start=notanumber, size=10, name=\"data\"\n\
                   /dev/mmcblk0p8 : start=notanumber, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(Path::new("/dev/mmcblk0"), bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump { .. })
        ));
    }

    #[test]
    fn rewrite_rejects_a_dump_missing_sector_size() {
        let bad = "label: gpt\nunit: sectors\n\n\
                   /dev/mmcblk0p7 : start=100, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(Path::new("/dev/mmcblk0"), bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump { .. })
        ));
    }

    #[test]
    fn rewrite_rejects_a_dump_with_an_unsupported_sector_size() {
        let bad = "label: gpt\nunit: sectors\nsector-size: 4096\n\n\
                   /dev/mmcblk0p7 : start=100, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(Path::new("/dev/mmcblk0"), bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump { .. })
        ));
    }

    #[test]
    fn rewrite_rejects_a_dump_in_units_other_than_sectors() {
        let bad = "label: gpt\nunit: cylinders\n\n\
                   /dev/mmcblk0p7 : start=100, size=10, name=\"data\"\n";
        assert!(matches!(
            rewrite_dump(Path::new("/dev/mmcblk0"), bad, DATA_SIZE_KB),
            Err(FlashError::MalformedDump { .. })
        ));
    }
}
