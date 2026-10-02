//! Unmounting filesystems before a disk is imaged.

#[cfg(feature = "flash-mode-2")]
use std::cmp::Reverse;
#[cfg(feature = "flash-mode-2")]
use std::fs;
use std::path::Path;
#[cfg(feature = "flash-mode-2")]
use std::path::PathBuf;

use crate::bootloader::sync_filesystems;
use crate::error::FlashError;
use crate::filesystem::{is_path_mounted, mount_points, umount};
#[cfg(feature = "flash-mode-2")]
use crate::partition::device::{SYS_DEV_BLOCK, block_devnum, whole_disk_devnum};

#[cfg(feature = "flash-mode-2")]
const PROC_MOUNTS: &str = "/proc/mounts";
#[cfg(feature = "flash-mode-2")]
const DEV_DIR: &str = "/dev";

/// `/proc/mounts` writes these bytes as a backslash and three octal digits.
#[cfg(feature = "flash-mode-2")]
const MOUNTS_ESCAPES: [(&str, char); 4] = [
    ("\\040", ' '),
    ("\\011", '\t'),
    ("\\012", '\n'),
    ("\\134", '\\'),
];

/// Unmount the rootfs and its boot partition, syncing first.
pub(crate) fn unmount_rootfs(rootfs: &Path) -> Result<(), FlashError> {
    sync_filesystems();

    for path in [rootfs.join(mount_points::BOOT), rootfs.to_path_buf()] {
        if is_path_mounted(&path)? {
            umount(&path)?;
        }
    }

    Ok(())
}

/// Unmount the rootfs, then every other mount backed by `disk`.
#[cfg(feature = "flash-mode-2")]
pub(crate) fn unmount_target_disk(rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
    unmount_rootfs(rootfs)?;

    let disk_dev = block_devnum(disk).map_err(|reason| FlashError::InvalidDestination {
        device: disk.to_path_buf(),
        reason,
    })?;
    let proc_mounts = fs::read_to_string(PROC_MOUNTS).map_err(|source| FlashError::PathIo {
        path: PathBuf::from(PROC_MOUNTS),
        source,
    })?;

    let disk_of = |source: &Path| {
        let disk = block_devnum(source)
            .ok()
            .and_then(|devnum| whole_disk_devnum(Path::new(SYS_DEV_BLOCK), devnum));
        if disk.is_none() && source.starts_with(DEV_DIR) {
            log::warn!(
                "cannot tell which disk {} is on; it stays mounted",
                source.display()
            );
        }
        disk
    };
    for mount_point in mounts_backed_by(&proc_mounts, disk_dev, disk_of) {
        log::info!("unmounting {}", mount_point.display());
        umount(&mount_point)?;
    }

    Ok(())
}

/// The mount points whose source sits on `disk`, deepest first so a nested
/// mount goes before its parent.
#[cfg(feature = "flash-mode-2")]
fn mounts_backed_by(
    proc_mounts: &str,
    disk: u64,
    disk_of: impl Fn(&Path) -> Option<u64>,
) -> Vec<PathBuf> {
    let mut mount_points: Vec<PathBuf> = proc_mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = PathBuf::from(decode_mounts_field(fields.next()?));
            let mount_point = PathBuf::from(decode_mounts_field(fields.next()?));
            (disk_of(&source) == Some(disk)).then_some(mount_point)
        })
        .collect();
    mount_points.sort_by_key(|path| Reverse(path.components().count()));
    mount_points
}

#[cfg(feature = "flash-mode-2")]
fn decode_mounts_field(field: &str) -> String {
    MOUNTS_ESCAPES
        .iter()
        .fold(field.to_string(), |text, (escape, decoded)| {
            text.replace(escape, &decoded.to_string())
        })
}

#[cfg(all(test, feature = "flash-mode-2"))]
mod tests {
    use super::*;

    const DISK: u64 = 1;
    const OTHER_DISK: u64 = 2;

    /// Maps the test sources onto disks by name, standing in for the device
    /// number lookup.
    fn disk_of(source: &Path) -> Option<u64> {
        match source.to_str()? {
            "/dev/mmcblk1p1" | "/dev/mmcblk1p2" | "/dev/mmcblk1p7" | "/dev/mmcblk1p3 x" => {
                Some(DISK)
            }
            "/dev/mmcblk10p1" => Some(OTHER_DISK),
            _ => None,
        }
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_disk_that_is_not_a_block_device_is_refused() {
        let disk = tempfile::NamedTempFile::new().unwrap();
        let err = unmount_target_disk(Path::new("/nonexistent/rootfs"), disk.path()).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidDestination { device, .. } if device == disk.path()),
            "{err}"
        );
    }

    #[test]
    fn mounts_on_the_disk_are_kept_and_the_rest_dropped() {
        let text = "\
tmpfs /run tmpfs rw 0 0
proc /proc proc rw 0 0
/dev/mmcblk1p1 /rootfs/boot vfat rw 0 0
/dev/mmcblk10p1 /mnt/other ext4 rw 0 0
";
        assert_eq!(
            mounts_backed_by(text, DISK, disk_of),
            paths(&["/rootfs/boot"])
        );
    }

    #[test]
    fn the_deepest_mount_point_comes_first() {
        let text = "\
/dev/mmcblk1p1 /a ext4 rw 0 0
/dev/mmcblk1p2 /a/b/c ext4 rw 0 0
/dev/mmcblk1p7 /a/b ext4 rw 0 0
";
        assert_eq!(
            mounts_backed_by(text, DISK, disk_of),
            paths(&["/a/b/c", "/a/b", "/a"])
        );
    }

    #[test]
    fn octal_escapes_are_decoded() {
        let text = "\
/dev/mmcblk1p1 /mnt/with\\040space ext4 rw 0 0
/dev/mmcblk1p2 /mnt/tab\\011and\\012line\\134slash ext4 rw 0 0
/dev/mmcblk1p3\\040x /mnt/source ext4 rw 0 0
";
        assert_eq!(
            mounts_backed_by(text, DISK, disk_of),
            paths(&[
                "/mnt/with space",
                "/mnt/tab\tand\nline\\slash",
                "/mnt/source",
            ])
        );
    }
}
