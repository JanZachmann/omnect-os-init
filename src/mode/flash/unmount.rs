//! Taking filesystems down before a disk is imaged.

use std::path::Path;

use crate::bootloader::sync_filesystems;
use crate::error::FlashError;
use crate::filesystem::{is_path_mounted, mount_points, umount};

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
