//! Root device detection from kernel command line.
//!
//! Supports two omnect-os boot paths depending on the bootloader:
//!
//! - **GRUB** (`rootpart=N` + `bootpart_fsuuid=<uuid>`): GRUB probes the filesystem
//!   UUID of its boot partition via `probe --fs-uuid` and passes it as `bootpart_fsuuid=`
//!   on the kernel cmdline. initramfs calls `blkid --uuid <uuid>` to resolve the exact
//!   boot partition device, then strips the partition suffix to get the base disk.
//!
//! - **U-Boot** (`root=/dev/<device>`): full device path set by U-Boot bootargs
//!   (e.g. `root=/dev/mmcblk1p2`). Base device and separator are derived from the path.

#[cfg(feature = "flash-mode")]
use std::fs;
#[cfg(feature = "flash-mode")]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(feature = "flash-mode")]
use nix::sys::stat::{major, makedev, minor};

use crate::config::CmdlineConfig;
use crate::partition::{PartitionError, Result};

const DEVICE_WAIT_TIMEOUT_SECS: u64 = 30;
const DEVICE_POLL_INTERVAL_MS: u64 = 100;

#[cfg(feature = "grub")]
const BLKID_CMD: &str = "/sbin/blkid";

/// Block devices by device number, each a link to the device's sysfs
/// directory.
#[cfg(feature = "flash-mode")]
pub(crate) const SYS_DEV_BLOCK: &str = "/sys/dev/block";

#[cfg(feature = "flash-mode")]
pub(crate) const REASON_NOT_A_BLOCK_DEVICE: &str = "not a block device";

/// Represents the detected root block device and its properties.
#[derive(Debug, Clone)]
pub struct RootDevice {
    /// Base block device path (e.g., `/dev/sda`, `/dev/nvme0n1`, `/dev/mmcblk0`)
    pub base: PathBuf,
    /// Partition separator ("" for sda/vda, "p" for nvme0n1/mmcblk0)
    pub partition_sep: &'static str,
    /// Root partition device path (e.g., `/dev/sda2`, `/dev/mmcblk0p2`)
    pub root_partition: PathBuf,
}

impl RootDevice {
    /// Constructs the path to a specific partition number.
    pub fn partition_path(&self, partition_num: u32) -> PathBuf {
        partition_path(&self.base, self.partition_sep, partition_num)
    }
}

/// Detects the root device from parsed kernel command line parameters.
pub fn detect_root_device(cmdline: &CmdlineConfig) -> Result<RootDevice> {
    #[cfg(feature = "grub")]
    if let Some(part_str) = cmdline.get("rootpart") {
        let part_num: u32 = part_str.parse().map_err(|_| {
            PartitionError::DeviceDetection(format!(
                "rootpart= is not a valid partition number: {}",
                part_str
            ))
        })?;

        let fsuuid = cmdline.get("bootpart_fsuuid").ok_or_else(|| {
            PartitionError::DeviceDetection(
                "rootpart= present but bootpart_fsuuid= missing from cmdline".into(),
            )
        })?;

        return device_from_fsuuid(fsuuid, part_num);
    }

    #[cfg(feature = "uboot")]
    if let Some(root) = cmdline.get("root") {
        if !root.starts_with("/dev/") {
            return Err(PartitionError::DeviceDetection(format!(
                "root= must start with /dev/, got: {}",
                root
            )));
        }
        return device_from_path(root);
    }

    #[cfg(feature = "grub")]
    return Err(PartitionError::DeviceDetection(
        "rootpart= (GRUB) not found in kernel cmdline".into(),
    ));

    #[cfg(feature = "uboot")]
    Err(PartitionError::DeviceDetection(
        "root= (U-Boot) not found in kernel cmdline".into(),
    ))
}

/// Resolves the boot disk via the filesystem UUID of the boot partition (`bootpart_fsuuid=`).
///
/// GRUB runs `probe --fs-uuid` on `${root}` (the boot partition) and passes the result
/// on the kernel cmdline. `blkid` is retried in a loop until the UUID is found or the
/// timeout expires — block devices may not be ready immediately at initramfs startup.
#[cfg(feature = "grub")]
fn device_from_fsuuid(fsuuid: &str, part_num: u32) -> Result<RootDevice> {
    use std::process::Command;

    log::info!(
        "device_from_fsuuid: resolving boot partition UUID={}",
        fsuuid
    );

    let timeout = Duration::from_secs(DEVICE_WAIT_TIMEOUT_SECS);
    let start = Instant::now();
    let boot_part_str = loop {
        let output = Command::new(BLKID_CMD)
            .args(["--uuid", fsuuid])
            .output()
            .map_err(|e| PartitionError::DeviceDetection(format!("failed to run blkid: {}", e)))?;

        match output.status.code() {
            Some(0) => {
                // UUID found; stdout is the device path.
                let dev = std::str::from_utf8(&output.stdout)
                    .map_err(|_| {
                        PartitionError::DeviceDetection("blkid output is not UTF-8".into())
                    })?
                    .trim()
                    .to_string();
                log::info!(
                    "device_from_fsuuid: UUID={} resolved to {} after {:.1}s",
                    fsuuid,
                    dev,
                    start.elapsed().as_secs_f32()
                );
                break dev;
            }
            Some(2) => {
                // UUID not found yet; retry until timeout.
                if start.elapsed() >= timeout {
                    return Err(PartitionError::DeviceDetection(format!(
                        "blkid found no device with UUID={} within {}s",
                        fsuuid,
                        timeout.as_secs()
                    )));
                }
                log::debug!(
                    "device_from_fsuuid: UUID={} not found yet, retrying...",
                    fsuuid
                );
                thread::sleep(Duration::from_millis(DEVICE_POLL_INTERVAL_MS));
            }
            code => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(PartitionError::DeviceDetection(format!(
                    "blkid exited with status {:?}: {}",
                    code,
                    stderr.trim()
                )));
            }
        }
    };

    let rd = root_device_from_blkid(&boot_part_str, part_num)?;
    wait_for_device(&rd.root_partition)?;
    log::info!(
        "device_from_fsuuid: root device = {} (partition {})",
        rd.base.display(),
        part_num
    );
    Ok(rd)
}

/// Pure pipeline: given the device path returned by `blkid --uuid`, construct a `RootDevice`.
///
/// Separated from `device_from_fsuuid` so it can be driven by fixture data in tests.
#[cfg(feature = "grub")]
pub fn root_device_from_blkid(boot_part_dev: &str, part_num: u32) -> Result<RootDevice> {
    let name = PathBuf::from(boot_part_dev)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            PartitionError::DeviceDetection(format!("invalid blkid output: {}", boot_part_dev))
        })?
        .to_string();

    let (base_name, sep) = split_partition_suffix(&name)?;
    let base = PathBuf::from("/dev").join(&base_name);
    let root_partition = PathBuf::from(format!("/dev/{}{}{}", base_name, sep, part_num));

    Ok(RootDevice {
        base,
        partition_sep: sep,
        root_partition,
    })
}

/// Parses a `root=/dev/<device>` path string into a `RootDevice` without waiting
/// for the device node to appear. Used directly in tests and by `device_from_path`.
#[cfg(feature = "uboot")]
pub fn parse_device_path(path: &str) -> Result<RootDevice> {
    let root_partition = PathBuf::from(path);
    let name = root_partition
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| PartitionError::DeviceDetection(format!("invalid device path: {}", path)))?;
    let (base_name, sep) = split_partition_suffix(name)?;
    let base = PathBuf::from("/dev").join(&base_name);
    Ok(RootDevice {
        base,
        partition_sep: sep,
        root_partition,
    })
}

/// Builds a `RootDevice` from a full `root=/dev/<device>` path (U-Boot boot path).
#[cfg(feature = "uboot")]
pub fn device_from_path(path: &str) -> Result<RootDevice> {
    let root_partition = PathBuf::from(path);
    wait_for_device(&root_partition)?;
    let rd = parse_device_path(path)?;
    log::info!("root device from root= (U-Boot): {}", rd.base.display());
    Ok(rd)
}

/// The separator between a disk name and a partition number. The kernel adds
/// a `p` when the disk name ends in a digit (`mmcblk2p1`, `nvme0n1p1`).
fn partition_sep_for_name(disk_name: &str) -> &'static str {
    if disk_name.ends_with(|c: char| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    }
}

#[cfg(feature = "flash-mode-1")]
pub(crate) fn partition_sep_for(disk: &Path) -> &'static str {
    disk.file_name()
        .and_then(|name| name.to_str())
        .map_or("", partition_sep_for_name)
}

pub(crate) fn partition_path(disk: &Path, partition_sep: &str, num: u32) -> PathBuf {
    PathBuf::from(format!("{}{partition_sep}{num}", disk.display()))
}

/// Splits a partition device name into `(base_name, separator)`.
///
/// Examples: `"sda2"` → `("sda", "")`, `"mmcblk1p2"` → `("mmcblk1", "p")`
fn split_partition_suffix(name: &str) -> Result<(String, &'static str)> {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.is_empty() || stem.len() == name.len() {
        return Err(PartitionError::DeviceDetection(format!(
            "could not derive base device from: {}",
            name
        )));
    }

    let base = match stem.strip_suffix('p') {
        Some(disk) if partition_sep_for_name(disk) == "p" => disk,
        _ => stem,
    };
    Ok((base.to_string(), partition_sep_for_name(base)))
}

fn wait_for_device(device: &Path) -> Result<()> {
    let timeout = Duration::from_secs(DEVICE_WAIT_TIMEOUT_SECS);
    let start = Instant::now();
    loop {
        if device.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(PartitionError::DeviceDetection(format!(
                "device {} did not appear within {} seconds",
                device.display(),
                timeout.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(DEVICE_POLL_INTERVAL_MS));
    }
}

#[cfg(feature = "flash-mode")]
pub(crate) fn block_devnum(path: &Path) -> std::result::Result<u64, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("cannot stat: {e}"))?;
    if !metadata.file_type().is_block_device() {
        return Err(REASON_NOT_A_BLOCK_DEVICE.to_string());
    }
    Ok(metadata.rdev())
}

/// The device number of the whole disk `devnum` sits on: `devnum` itself for
/// a disk, the parent disk for a partition. `None` when sysfs does not list it.
#[cfg(feature = "flash-mode")]
pub(crate) fn whole_disk_devnum(sys_dev_block: &Path, devnum: u64) -> Option<u64> {
    let node = sys_dev_block.join(format!("{}:{}", major(devnum), minor(devnum)));
    if !node.exists() {
        return None;
    }
    if !node.join("partition").exists() {
        return Some(devnum);
    }
    // The kernel resolves `..` after following the link, so this reads the
    // `dev` file of the parent disk's directory.
    let parent = fs::read_to_string(node.join("../dev")).ok()?;
    let (parent_major, parent_minor) = parent.trim().split_once(':')?;
    Some(makedev(
        parent_major.parse().ok()?,
        parent_minor.parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_partition_suffix_sata() {
        assert_eq!(
            split_partition_suffix("sda2").unwrap(),
            ("sda".to_string(), "")
        );
    }

    #[test]
    fn test_split_partition_suffix_mmc() {
        assert_eq!(
            split_partition_suffix("mmcblk1p2").unwrap(),
            ("mmcblk1".to_string(), "p")
        );
    }

    #[test]
    fn test_split_partition_suffix_nvme() {
        assert_eq!(
            split_partition_suffix("nvme0n1p2").unwrap(),
            ("nvme0n1".to_string(), "p")
        );
    }

    #[test]
    fn test_split_partition_suffix_virtio() {
        assert_eq!(
            split_partition_suffix("vda2").unwrap(),
            ("vda".to_string(), "")
        );
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn test_partition_sep_for_bare_disks() {
        assert_eq!(partition_sep_for(Path::new("/dev/sda")), "");
        assert_eq!(partition_sep_for(Path::new("/dev/mmcblk2")), "p");
        assert_eq!(partition_sep_for(Path::new("/dev/nvme0n1")), "p");
    }

    #[test]
    fn test_root_device_partition_path_sata() {
        let device = RootDevice {
            base: PathBuf::from("/dev/sda"),
            partition_sep: "",
            root_partition: PathBuf::from("/dev/sda2"),
        };
        assert_eq!(device.partition_path(1), PathBuf::from("/dev/sda1"));
        assert_eq!(device.partition_path(7), PathBuf::from("/dev/sda7"));
    }

    #[test]
    fn test_root_device_partition_path_mmc() {
        let device = RootDevice {
            base: PathBuf::from("/dev/mmcblk0"),
            partition_sep: "p",
            root_partition: PathBuf::from("/dev/mmcblk0p2"),
        };
        assert_eq!(device.partition_path(1), PathBuf::from("/dev/mmcblk0p1"));
        assert_eq!(device.partition_path(7), PathBuf::from("/dev/mmcblk0p7"));
    }

    #[test]
    fn test_root_device_partition_path_nvme() {
        let device = RootDevice {
            base: PathBuf::from("/dev/nvme0n1"),
            partition_sep: "p",
            root_partition: PathBuf::from("/dev/nvme0n1p2"),
        };
        assert_eq!(device.partition_path(1), PathBuf::from("/dev/nvme0n1p1"));
        assert_eq!(device.partition_path(7), PathBuf::from("/dev/nvme0n1p7"));
    }

    #[test]
    fn test_split_partition_suffix_multi_digit_sata() {
        assert_eq!(
            split_partition_suffix("sda12").unwrap(),
            ("sda".to_string(), "")
        );
        assert_eq!(
            split_partition_suffix("sdb100").unwrap(),
            ("sdb".to_string(), "")
        );
    }

    #[test]
    fn test_split_partition_suffix_multi_digit_nvme() {
        assert_eq!(
            split_partition_suffix("nvme1n2p100").unwrap(),
            ("nvme1n2".to_string(), "p")
        );
    }

    #[test]
    fn test_split_partition_suffix_multi_digit_mmc() {
        assert_eq!(
            split_partition_suffix("mmcblk1p12").unwrap(),
            ("mmcblk1".to_string(), "p")
        );
    }

    #[test]
    fn test_split_partition_suffix_virtio_second_disk() {
        assert_eq!(
            split_partition_suffix("vdb7").unwrap(),
            ("vdb".to_string(), "")
        );
    }

    #[test]
    fn test_split_partition_suffix_loop_and_md() {
        assert_eq!(
            split_partition_suffix("loop0p1").unwrap(),
            ("loop0".to_string(), "p")
        );
        assert_eq!(
            split_partition_suffix("md0p1").unwrap(),
            ("md0".to_string(), "p")
        );
    }

    #[test]
    fn splitting_a_partition_name_inverts_building_it() {
        for disk in ["sda", "vdb", "mmcblk1", "nvme0n1", "loop0", "md0"] {
            let sep = partition_sep_for_name(disk);
            let name = partition_path(Path::new(disk), sep, 7);
            assert_eq!(
                split_partition_suffix(name.to_str().unwrap()).unwrap(),
                (disk.to_string(), sep),
                "{disk}"
            );
        }
    }

    #[test]
    fn test_split_partition_suffix_whole_disk_errors() {
        assert!(split_partition_suffix("sda").is_err());
        // dm-0: trailing digit after hyphen parses as a suffix, yielding base="dm-" —
        // incorrect, but omnect-os does not target device-mapper devices.
        assert!(split_partition_suffix("dm-0").is_ok());
    }

    #[test]
    fn test_root_device_partition_path_virtio() {
        let device = RootDevice {
            base: PathBuf::from("/dev/vda"),
            partition_sep: "",
            root_partition: PathBuf::from("/dev/vda2"),
        };
        assert_eq!(device.partition_path(1), PathBuf::from("/dev/vda1"));
        assert_eq!(device.partition_path(7), PathBuf::from("/dev/vda7"));
    }

    // --- detect_root_device error paths ---

    #[cfg(feature = "grub")]
    #[test]
    fn test_detect_root_device_grub_missing_rootpart() {
        // No rootpart= on cmdline → error before blkid is called.
        let cfg = crate::config::CmdlineConfig::parse("ro quiet");
        let result = detect_root_device(&cfg);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("rootpart"),
            "error should mention 'rootpart', got: {msg}"
        );
    }

    #[cfg(feature = "grub")]
    #[test]
    fn test_detect_root_device_grub_missing_fsuuid() {
        // rootpart= present but bootpart_fsuuid= missing → error before blkid is called.
        let cfg = crate::config::CmdlineConfig::parse("rootpart=2 ro quiet");
        let result = detect_root_device(&cfg);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("bootpart_fsuuid"),
            "error should mention 'bootpart_fsuuid', got: {msg}"
        );
    }

    #[cfg(feature = "grub")]
    #[test]
    fn test_detect_root_device_grub_non_numeric_rootpart() {
        // rootpart= is not a number → parse error before blkid is called.
        let cfg = crate::config::CmdlineConfig::parse("rootpart=sda2 bootpart_fsuuid=ABCD-1234 ro");
        let result = detect_root_device(&cfg);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not a valid partition number") || msg.contains("sda2"),
            "error should describe a parse failure, got: {msg}"
        );
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn test_detect_root_device_uboot_missing_root() {
        // No root= on cmdline → error immediately, no device wait.
        let cfg = crate::config::CmdlineConfig::parse("ro quiet");
        let result = detect_root_device(&cfg);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("root="),
            "error should mention 'root=', got: {msg}"
        );
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn test_detect_root_device_uboot_root_without_dev_prefix() {
        // root= present but does not start with /dev/ → rejected before device wait.
        let cfg = crate::config::CmdlineConfig::parse("root=mmcblk0p2 ro quiet");
        let result = detect_root_device(&cfg);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("/dev/"),
            "error should mention '/dev/', got: {msg}"
        );
    }
}

#[cfg(all(test, feature = "flash-mode"))]
mod devnum_tests {
    use super::*;

    /// A sysfs tree with `mmcblk1` (179:0) and its partition `mmcblk1p2`
    /// (179:2) linked from `dev/block`, the way the kernel lays it out.
    fn fake_sys_dev_block() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let disk = root.path().join("devices/block/mmcblk1");
        let partition = disk.join("mmcblk1p2");
        fs::create_dir_all(&partition).unwrap();
        fs::write(disk.join("dev"), "179:0\n").unwrap();
        fs::write(partition.join("dev"), "179:2\n").unwrap();
        fs::write(partition.join("partition"), "2\n").unwrap();

        let by_number = root.path().join("dev/block");
        fs::create_dir_all(&by_number).unwrap();
        std::os::unix::fs::symlink(&disk, by_number.join("179:0")).unwrap();
        std::os::unix::fs::symlink(&partition, by_number.join("179:2")).unwrap();
        root
    }

    #[test]
    fn a_partition_maps_onto_its_parent_disk() {
        let sys = fake_sys_dev_block();
        let by_number = sys.path().join("dev/block");
        assert_eq!(
            whole_disk_devnum(&by_number, makedev(179, 2)),
            Some(makedev(179, 0))
        );
    }

    #[test]
    fn a_disk_maps_onto_itself() {
        let sys = fake_sys_dev_block();
        let by_number = sys.path().join("dev/block");
        assert_eq!(
            whole_disk_devnum(&by_number, makedev(179, 0)),
            Some(makedev(179, 0))
        );
    }

    #[test]
    fn a_regular_file_has_no_block_device_number() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            block_devnum(file.path()),
            Err(REASON_NOT_A_BLOCK_DEVICE.to_string())
        );
    }

    #[test]
    fn a_device_sysfs_does_not_list_has_no_disk() {
        let sys = fake_sys_dev_block();
        let by_number = sys.path().join("dev/block");
        assert_eq!(whole_disk_devnum(&by_number, makedev(179, 8)), None);
    }
}
