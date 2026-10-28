//! Filesystem operations
//!
//! This module handles:
//! - Mounting and unmounting filesystems
//! - Running fsck before mounting
//! - Overlayfs setup for etc and home
//! - Reformatting a partition as ext4

mod boot_sequence;
mod fsck;
mod mount;
mod overlayfs;
#[cfg(any(feature = "factory-reset", feature = "flash-mode-1"))]
mod reformat;
#[cfg(feature = "resize-data")]
pub mod resize_data;

#[cfg(feature = "factory-reset")]
pub(crate) use self::boot_sequence::{PartitionMountSpec, mount_tracked_partition};
pub use self::boot_sequence::{
    drain_fsck_env, fsck_and_record, mount_core_partitions, mount_remaining_partitions,
    persist_fsck_results,
};
pub use self::fsck::{FsckExitCode, FsckResult, check_filesystem, check_filesystem_lenient};
#[cfg(feature = "factory-reset")]
pub(crate) use self::mount::unmount_tracked;
pub use self::mount::{
    FsType, MountOptions, MountPoint, is_path_mounted, mount, mount_bind, mount_bind_private,
    mount_tmpfs, umount,
};
#[cfg(feature = "factory-reset")]
pub(crate) use self::overlayfs::CP_CMD;
pub(crate) use self::overlayfs::paths;
pub use self::overlayfs::{
    mount_points, setup_data_overlay, setup_etc_overlay, setup_raw_rootfs_mount,
};
#[cfg(feature = "factory-reset")]
pub(crate) use self::overlayfs::{setup_data_overlay_tracked, setup_etc_overlay_tracked};
#[cfg(any(feature = "factory-reset", feature = "flash-mode-1"))]
pub(crate) use self::reformat::reformat_ext4;

use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::FilesystemError;

pub type Result<T> = std::result::Result<T, FilesystemError>;

/// The logs the init writes can name devices, inodes and files, and only root
/// reads them.
const ROOT_ONLY_FILE_MODE: u32 = 0o600;
const ROOT_ONLY_DIR_MODE: u32 = 0o700;

/// Create or truncate `path` as a file only root can read. A file left by an
/// earlier boot gets the mode too.
pub(crate) fn create_root_only_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(ROOT_ONLY_FILE_MODE)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(ROOT_ONLY_FILE_MODE))?;
    Ok(file)
}

/// Create `path` as a directory only root can enter. Missing parents get the
/// usual mode.
pub(crate) fn create_root_only_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, Permissions::from_mode(ROOT_ONLY_DIR_MODE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn a_new_root_only_file_has_mode_600() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("a.log");

        create_root_only_file(&path).unwrap();

        assert_eq!(mode(&path), ROOT_ONLY_FILE_MODE);
    }

    #[test]
    fn an_existing_file_is_truncated_and_gets_mode_600() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("a.log");
        fs::write(&path, "old content").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        create_root_only_file(&path)
            .unwrap()
            .write_all(b"new")
            .unwrap();

        assert_eq!(mode(&path), ROOT_ONLY_FILE_MODE);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn a_root_only_dir_has_mode_700_and_its_parents_keep_theirs() {
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("log");
        let dir = parent.join("fsck");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, Permissions::from_mode(0o755)).unwrap();

        create_root_only_dir(&dir).unwrap();

        assert_eq!(mode(&dir), ROOT_ONLY_DIR_MODE);
        assert_eq!(mode(&parent), 0o755);
    }

    #[test]
    fn an_existing_dir_gets_mode_700() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("fsck");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();

        create_root_only_dir(&dir).unwrap();

        assert_eq!(mode(&dir), ROOT_ONLY_DIR_MODE);
    }
}
