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
#[cfg(feature = "factory-reset")]
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
pub use self::overlayfs::{
    mount_points, setup_data_overlay, setup_etc_overlay, setup_raw_rootfs_mount,
};
#[cfg(feature = "factory-reset")]
pub(crate) use self::overlayfs::{paths, setup_data_overlay_tracked, setup_etc_overlay_tracked};
// TODO: widen to `any(feature = "factory-reset", feature = "flash-mode-1")` once
// flash-mode-1 has a caller for reformat_ext4; until then that combination has no
// caller and fails the dead-code lint.
#[cfg(feature = "factory-reset")]
pub(crate) use self::reformat::reformat_ext4;

use crate::error::FilesystemError;

pub type Result<T> = std::result::Result<T, FilesystemError>;
