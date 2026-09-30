use std::path::Path;
use std::process::Command;

use crate::error::FilesystemError;

const EXT4_FSTYPE: &str = "ext4";
const MKFS_EXT4_CMD: &str = "/sbin/mkfs.ext4";
const TUNE2FS_CMD: &str = "/sbin/tune2fs";
const MKFS_FORCE_FLAG: &str = "-F";
const MKFS_QUIET_FLAG: &str = "-q";
const TUNE2FS_MAX_MOUNT_COUNT_FLAG: &str = "-c";
const TUNE2FS_CHECK_INTERVAL_FLAG: &str = "-i";
const TUNE2FS_LABEL_FLAG: &str = "-L";
const TUNE2FS_NO_LIMIT: &str = "-1";
const TUNE2FS_ZERO_INTERVAL: &str = "0";

/// Reformat a partition as ext4 and apply omnect tunables.
pub fn reformat_ext4(device: &Path, label: &str) -> crate::filesystem::Result<()> {
    log::info!("Reformatting {} with label={label}", device.display());

    let mkfs = Command::new(MKFS_EXT4_CMD)
        .args([MKFS_FORCE_FLAG, MKFS_QUIET_FLAG])
        .arg(device)
        .output()
        .map_err(|e| FilesystemError::FormatFailed {
            device: device.to_path_buf(),
            fstype: EXT4_FSTYPE.to_string(),
            reason: format!("Failed to run mkfs.ext4: {e}"),
        })?;

    if !mkfs.status.success() {
        return Err(FilesystemError::FormatFailed {
            device: device.to_path_buf(),
            fstype: EXT4_FSTYPE.to_string(),
            reason: format!(
                "mkfs.ext4 failed ({}): {}",
                mkfs.status,
                String::from_utf8_lossy(&mkfs.stderr)
            ),
        });
    }

    let tune = Command::new(TUNE2FS_CMD)
        .arg(device)
        .args([
            TUNE2FS_MAX_MOUNT_COUNT_FLAG,
            TUNE2FS_NO_LIMIT,
            TUNE2FS_CHECK_INTERVAL_FLAG,
            TUNE2FS_ZERO_INTERVAL,
            TUNE2FS_LABEL_FLAG,
            label,
        ])
        .output()
        .map_err(|e| FilesystemError::FormatFailed {
            device: device.to_path_buf(),
            fstype: EXT4_FSTYPE.to_string(),
            reason: format!("Failed to run tune2fs: {e}"),
        })?;

    if !tune.status.success() {
        return Err(FilesystemError::FormatFailed {
            device: device.to_path_buf(),
            fstype: EXT4_FSTYPE.to_string(),
            reason: format!(
                "tune2fs failed ({}): {}",
                tune.status,
                String::from_utf8_lossy(&tune.stderr)
            ),
        });
    }

    log::info!(
        "Reformatted {} with label={label} successfully",
        device.display()
    );
    Ok(())
}
