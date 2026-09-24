//! Taking filesystems down before a disk is overwritten.
//!
//! Mode 1 only ever writes a different disk than the one it boots from, so
//! it unmounts just `/sysroot`. Modes 2 and 3 overwrite the disk they boot
//! from, so they additionally sweep every other mount backed by it.

use std::fs;
use std::path::Path;

use crate::bootloader::sync_filesystems;
use crate::error::FlashError;
use crate::filesystem::{is_path_mounted, mount_points, umount};
use crate::partition::device::partition_sep_for;

const PROC_MOUNTS_PATH: &str = "/proc/mounts";

/// Unmount `/sysroot` and its boot partition, syncing first.
///
/// A path that is already unmounted (a previous step may have taken it down)
/// is not an error.
pub fn unmount_sysroot(rootfs: &Path) -> Result<(), FlashError> {
    sync_filesystems();

    for path in [rootfs.join(mount_points::BOOT), rootfs.to_path_buf()] {
        if is_path_mounted(&path)? {
            umount(&path)?;
        }
    }

    Ok(())
}

/// Whether `rest` (a device name with `disk`'s prefix stripped) is empty or a
/// partition suffix of `disk` — a bare number for `sda` (`sda1`), a
/// `p`-separated one for a name ending in a digit (`mmcblk0p1`).
pub fn is_disk_or_partition_suffix(disk: &Path, rest: &str) -> bool {
    if rest.is_empty() {
        return true;
    }
    rest.strip_prefix(partition_sep_for(disk))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Every mount point in `proc_mounts` backed by `disk`, deepest first.
///
/// A line matches when its device field is `disk` itself or one of its
/// partitions, so `/dev/sda` does not also match `/dev/sdb1`.
pub fn mounts_backed_by(proc_mounts: &str, disk: &Path) -> Vec<String> {
    let disk_name = disk.to_string_lossy();
    let mut mounts: Vec<String> = proc_mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            let mount_point = fields.next()?;

            let rest = device.strip_prefix(disk_name.as_ref())?;
            is_disk_or_partition_suffix(disk, rest).then(|| mount_point.to_string())
        })
        .collect();

    // Deepest path first, so a nested mount is unmounted before the one it
    // sits inside.
    mounts.sort_by_key(|b| std::cmp::Reverse(b.matches('/').count()));
    mounts
}

/// Unmount `/sysroot` plus everything else `mounts_backed_by` finds mounted
/// from `disk`. Only modes that overwrite the disk they boot from need this;
/// mode 1 writes a different disk and needs only `unmount_sysroot`.
pub fn unmount_target_disk(rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
    unmount_sysroot(rootfs)?;

    let proc_mounts = fs::read_to_string(PROC_MOUNTS_PATH)?;
    for mount_point in mounts_backed_by(&proc_mounts, disk) {
        let path = Path::new(&mount_point);
        if is_path_mounted(path)? {
            umount(path)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_MOUNTS: &str = "\
proc /proc proc rw,relatime 0 0
/dev/mmcblk0p2 /sysroot ext4 ro,relatime 0 0
/dev/mmcblk0p1 /sysroot/boot vfat rw,relatime 0 0
/dev/mmcblk0p7 /sysroot/mnt/data ext4 rw,relatime 0 0
/dev/sda1 /mnt/usb vfat rw,relatime 0 0
tmpfs /tmp tmpfs rw,relatime 0 0
";

    #[test]
    fn only_mounts_on_the_target_disk_are_swept_deepest_first() {
        assert_eq!(
            mounts_backed_by(PROC_MOUNTS, Path::new("/dev/mmcblk0")),
            vec![
                "/sysroot/mnt/data".to_string(),
                "/sysroot/boot".to_string(),
                "/sysroot".to_string(),
            ],
            "a nested mount must be unmounted before the one it sits inside"
        );
    }

    #[test]
    fn a_neighbouring_device_is_left_alone() {
        assert!(mounts_backed_by(PROC_MOUNTS, Path::new("/dev/sdb")).is_empty());
        // A prefix match must not catch a different device.
        assert_eq!(
            mounts_backed_by(PROC_MOUNTS, Path::new("/dev/sda")),
            vec!["/mnt/usb".to_string()]
        );
    }

    #[test]
    fn a_disk_whose_name_extends_the_target_is_left_alone() {
        let mounts = "/dev/mmcblk10p1 /mnt/other ext4 rw,relatime 0 0\n";
        assert!(mounts_backed_by(mounts, Path::new("/dev/mmcblk1")).is_empty());
    }

    #[test]
    fn pseudo_filesystems_are_not_swept() {
        let found = mounts_backed_by(PROC_MOUNTS, Path::new("/dev/mmcblk0"));
        assert!(!found.iter().any(|m| m == "/proc" || m == "/tmp"));
    }
}
