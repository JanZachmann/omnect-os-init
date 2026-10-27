//! Overlayfs setup for etc and home directories
//!
//! This module handles:
//! - Setting up overlayfs for /etc (factory defaults + persistent upper)
//! - Setting up overlayfs for /home (factory defaults + data upper)
//! - Bind mounts for /var/lib and /usr/local
//! - Initial copy of factory etc to upper layer

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix::mount::MsFlags;

use crate::error::FilesystemError;
use crate::filesystem::{
    FsType, MountOptions, MountPoint, Result, mount, mount_bind, mount_bind_private,
};
/// cp command for copying directory contents
pub(crate) const CP_CMD: &str = "/bin/cp";

/// The kernel ignores the mount source for overlayfs, but the mount syscall
/// requires a non-empty string. Using the literal "overlay" is conventional
/// and matches what the `mount` command would produce.
const OVERLAY_MOUNT_SOURCE: &str = "overlay";

/// Directory names for overlay layers
mod overlay_dirs {
    pub const UPPER: &str = "upper";
    pub const WORK: &str = "work";
}

/// Standard paths relative to rootfs
pub(crate) mod paths {
    pub const ETC: &str = "etc";
    pub const HOME: &str = "home";
    pub const VAR_LIB: &str = "var/lib";
    /// Subdirectory on the data partition that is bind-mounted to /usr/local.
    /// The data partition layout omits the "usr/" prefix.
    pub const DATA_LOCAL_DIR: &str = "local";
    pub const USR_LOCAL: &str = "usr/local";
    #[cfg(feature = "persistent-var-log")]
    pub const VAR_LOG: &str = "var/log";
    /// Former location of the persistent /var/log on the data partition.
    #[cfg(feature = "persistent-var-log")]
    pub const DATA_LOG_OLD: &str = "log";
}

/// Mount point paths for partitions relative to rootfs
pub mod mount_points {
    pub const BOOT: &str = "boot";
    pub const CERT_PARTITION: &str = "mnt/cert";
    pub const DATA_PARTITION: &str = "mnt/data";
    pub const ETC_PARTITION: &str = "mnt/etc";
    pub const FACTORY_PARTITION: &str = "mnt/factory";
    pub const ROOT_CURRENT_PRIVATE: &str = "mnt/rootCurrentPrivate";
    pub const VAR_VOLATILE: &str = "var/volatile";
}

/// Setup the etc partition with overlayfs
///
/// Creates an overlay where:
/// - Lower layer: rootfs/etc (read-only from current OS)
/// - Upper layer: mnt/etc/upper (persistent changes)
/// - Work dir: mnt/etc/work
/// - Target: rootfs/etc
///
/// Factory /etc defaults are copied into the upper layer when the upper
/// directory is empty. An empty upper dir indicates either a first boot
/// or a factory reset (both cases require populating config from factory
/// defaults).
pub fn setup_etc_overlay(rootfs_dir: &Path) -> Result<()> {
    setup_etc_overlay_tracked(rootfs_dir, &mut Vec::new())
}

/// Same as `setup_etc_overlay`, but records the overlay target in `mounts`
/// immediately after mounting, so a caller doing manual cleanup (e.g.
/// factory reset) can always unmount exactly what ended up mounted.
pub(crate) fn setup_etc_overlay_tracked(
    rootfs_dir: &Path,
    mounts: &mut Vec<PathBuf>,
) -> Result<()> {
    let etc_mount = rootfs_dir.join(mount_points::ETC_PARTITION);
    let factory_mount = rootfs_dir.join(mount_points::FACTORY_PARTITION);

    // Overlay directories
    let upper_dir = etc_mount.join(overlay_dirs::UPPER);
    let work_dir = etc_mount.join(overlay_dirs::WORK);
    let lower_dir = rootfs_dir.join(paths::ETC);
    let target = rootfs_dir.join(paths::ETC);

    ensure_overlay_dirs(&upper_dir, &work_dir)?;

    // Empty upper dir = first boot or factory reset: populate from factory defaults.
    if is_upper_dir_empty(&upper_dir)? {
        let factory_etc = factory_mount.join(paths::ETC);
        log::info!("First boot or factory reset: copying factory etc to upper layer");
        copy_directory_contents(&factory_etc, &upper_dir)?;
    }

    mount_overlay(&lower_dir, &upper_dir, &work_dir, &target)?;
    mounts.push(target.clone());

    log::info!(
        "Setup etc overlay: lower={}, upper={} -> {}",
        lower_dir.display(),
        upper_dir.display(),
        target.display()
    );

    Ok(())
}

/// Setup the data partition with home overlayfs and bind mounts
///
/// Creates:
/// - Overlay for /home (rootfs/home lower, data/home/upper upper)
/// - Bind mount: data/var/lib -> rootfs/var/lib
/// - Bind mount: data/local -> rootfs/usr/local
/// - Conditional: data/var/log -> rootfs/var/log (compile-time `persistent-var-log` feature)
pub fn setup_data_overlay(rootfs_dir: &Path) -> Result<()> {
    setup_data_overlay_tracked(rootfs_dir, &mut Vec::new())
}

/// Same as `setup_data_overlay`, but records each mount target in `mounts`
/// immediately after it succeeds, so a caller doing manual cleanup (e.g.
/// factory reset) can always unmount exactly what ended up mounted — even
/// if a later sub-mount in this call fails.
pub(crate) fn setup_data_overlay_tracked(
    rootfs_dir: &Path,
    mounts: &mut Vec<PathBuf>,
) -> Result<()> {
    let data_mount = rootfs_dir.join(mount_points::DATA_PARTITION);

    setup_home_overlay(rootfs_dir, &data_mount)?;
    mounts.push(rootfs_dir.join(paths::HOME));

    bind_mount(
        &data_mount.join(paths::VAR_LIB),
        &rootfs_dir.join(paths::VAR_LIB),
    )?;
    mounts.push(rootfs_dir.join(paths::VAR_LIB));

    // The data partition holds this content under a top-level "local" subdir,
    // bind-mounted onto "usr/local".
    bind_mount(
        &data_mount.join(paths::DATA_LOCAL_DIR),
        &rootfs_dir.join(paths::USR_LOCAL),
    )?;
    mounts.push(rootfs_dir.join(paths::USR_LOCAL));

    #[cfg(feature = "persistent-var-log")]
    {
        migrate_var_log(&data_mount);
        bind_mount(
            &data_mount.join(paths::VAR_LOG),
            &rootfs_dir.join(paths::VAR_LOG),
        )?;
        mounts.push(rootfs_dir.join(paths::VAR_LOG));
    }

    Ok(())
}

/// Move the logs from the former data partition location to `var/log`.
///
/// Best effort: a log that cannot be moved stays where it is and does not
/// stop the boot.
// TODO: remove me, as soon as the CR version boots with omnect-os-init
#[cfg(feature = "persistent-var-log")]
fn migrate_var_log(data_mount: &Path) {
    const MIGRATED_LOG_SUFFIX: &str = ".old";

    let old = data_mount.join(paths::DATA_LOG_OLD);
    let new = data_mount.join(paths::VAR_LOG);
    if !old.is_dir() {
        return;
    }

    let entries = match ensure_dir(&new).and_then(|_| Ok(fs::read_dir(&old)?)) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("Cannot move the logs from {}: {e}", old.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let mut dst = new.join(entry.file_name());
        if dst.symlink_metadata().is_ok() {
            let mut name = entry.file_name();
            name.push(MIGRATED_LOG_SUFFIX);
            dst = new.join(name);
        }
        if dst.symlink_metadata().is_ok() {
            log::warn!(
                "Not moving {}: {} exists",
                entry.path().display(),
                dst.display()
            );
            continue;
        }
        if let Err(e) = fs::rename(entry.path(), &dst) {
            log::warn!("Cannot move {}: {e}", entry.path().display());
        }
    }

    match fs::remove_dir(&old) {
        Ok(()) => log::info!("Moved the logs from {} to {}", old.display(), new.display()),
        Err(e) => log::warn!("Cannot remove {}: {e}", old.display()),
    }
}

/// Setup home directory overlay
fn setup_home_overlay(rootfs: &Path, data_mount: &Path) -> Result<()> {
    let home_data = data_mount.join(paths::HOME);
    let upper_dir = home_data.join(overlay_dirs::UPPER);
    let work_dir = home_data.join(overlay_dirs::WORK);
    let lower_dir = rootfs.join(paths::HOME);
    let target = rootfs.join(paths::HOME);

    ensure_dir(&home_data)?;
    ensure_overlay_dirs(&upper_dir, &work_dir)?;

    mount_overlay(&lower_dir, &upper_dir, &work_dir, &target)?;

    log::info!(
        "Setup home overlay: lower={}, upper={} -> {}",
        lower_dir.display(),
        upper_dir.display(),
        target.display()
    );

    Ok(())
}

/// Bind mount source -> target, creating both dirs if needed.
fn bind_mount(source: &Path, target: &Path) -> Result<()> {
    ensure_dir(source)?;
    ensure_dir(target)?;
    mount_bind(source, target)?;
    log::info!("Bind mounted {} -> {}", source.display(), target.display());
    Ok(())
}

/// Mount an overlayfs
fn mount_overlay(lower: &Path, upper: &Path, work: &Path, target: &Path) -> Result<()> {
    let options = format!(
        "lowerdir={},upperdir={},workdir={},index=off,uuid=off",
        lower.display(),
        upper.display(),
        work.display()
    );

    let mount_opts = MountOptions {
        fstype: Some(FsType::Overlay),
        flags: MsFlags::MS_NOATIME | MsFlags::MS_NODIRATIME,
        data: Some(options.clone()),
    };

    mount(MountPoint::new(OVERLAY_MOUNT_SOURCE, target, mount_opts)).map_err(|e| {
        FilesystemError::OverlayFailed {
            target: target.to_path_buf(),
            reason: format!("{e}: options={options}"),
        }
    })
}

/// Ensure overlay directories (upper and work) exist
fn ensure_overlay_dirs(upper: &Path, work: &Path) -> Result<()> {
    ensure_dir(upper)?;
    ensure_dir(work)?;
    Ok(())
}

/// Ensure a directory exists, creating it if necessary
fn ensure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|e| FilesystemError::OverlayFailed {
            target: path.to_path_buf(),
            reason: format!("Failed to create directory: {}", e),
        })?;
    }
    Ok(())
}

/// Check whether the overlay upper directory is empty or absent.
///
/// Returns `true` on first boot (never written) and after factory reset
/// (upper dir wiped). Both cases require populating from factory defaults.
fn is_upper_dir_empty(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    let mut entries = fs::read_dir(path).map_err(|e| FilesystemError::OverlayFailed {
        target: path.to_path_buf(),
        reason: format!("Failed to read directory: {}", e),
    })?;
    Ok(entries.next().is_none())
}

/// Copy contents of one directory to another
fn copy_directory_contents(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        log::warn!("Source directory does not exist: {}", src.display());
        return Ok(());
    }

    // Use cp -a to preserve all attributes
    let output = Command::new(CP_CMD)
        .arg("-a")
        .arg(format!("{}/.", src.display()))
        .arg(dst)
        .output()
        .map_err(|e| FilesystemError::OverlayFailed {
            target: dst.to_path_buf(),
            reason: format!("Failed to execute cp: {}", e),
        })?;

    if !output.status.success() {
        return Err(FilesystemError::OverlayFailed {
            target: dst.to_path_buf(),
            reason: format!("cp failed: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }

    log::debug!("Copied {} -> {}", src.display(), dst.display());

    Ok(())
}

/// Setup raw rootfs bind mount (must be called BEFORE overlays)
///
/// Creates a private bind mount at /mnt/rootCurrentPrivate that provides
/// access to the raw rootfs without overlay modifications.
pub fn setup_raw_rootfs_mount(rootfs_dir: &Path) -> Result<()> {
    let raw_mount = rootfs_dir.join(mount_points::ROOT_CURRENT_PRIVATE);

    ensure_dir(&raw_mount)?;

    // Private bind mount isolates propagation so overlay mounts on top
    // don't bleed into this raw view of rootfs.
    mount_bind_private(rootfs_dir, &raw_mount)?;

    log::info!(
        "Created raw rootfs mount: {} -> {}",
        rootfs_dir.display(),
        raw_mount.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(feature = "persistent-var-log")]
    #[test]
    fn migrate_var_log_moves_old_logs() {
        let data = TempDir::new().unwrap();
        let old = data.path().join(paths::DATA_LOG_OLD);
        let new = data.path().join(paths::VAR_LOG);
        fs::create_dir_all(old.join("journal")).unwrap();
        fs::write(old.join("journal/system.journal"), "j").unwrap();
        fs::write(old.join("app.log"), "old").unwrap();
        // The fsck results are written before the migration runs.
        fs::create_dir_all(new.join("fsck")).unwrap();

        migrate_var_log(data.path());

        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(new.join("journal/system.journal")).unwrap(),
            "j"
        );
        assert_eq!(fs::read_to_string(new.join("app.log")).unwrap(), "old");
        assert!(new.join("fsck").is_dir());
    }

    #[cfg(feature = "persistent-var-log")]
    #[test]
    fn migrate_var_log_moves_a_conflicting_entry_to_old() {
        let data = TempDir::new().unwrap();
        let old = data.path().join(paths::DATA_LOG_OLD);
        let new = data.path().join(paths::VAR_LOG);
        fs::create_dir_all(old.join("journal")).unwrap();
        fs::create_dir_all(new.join("journal")).unwrap();
        fs::write(old.join("journal/system.journal"), "old").unwrap();

        migrate_var_log(data.path());

        assert!(!old.exists());
        assert!(new.join("journal").is_dir());
        assert_eq!(
            fs::read_to_string(new.join("journal.old/system.journal")).unwrap(),
            "old"
        );
    }

    #[cfg(feature = "persistent-var-log")]
    #[test]
    fn migrate_var_log_keeps_an_entry_that_conflicts_twice() {
        let data = TempDir::new().unwrap();
        let old = data.path().join(paths::DATA_LOG_OLD);
        let new = data.path().join(paths::VAR_LOG);
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("app.log"), "old").unwrap();
        fs::write(new.join("app.log"), "new").unwrap();
        fs::write(new.join("app.log.old"), "older").unwrap();

        migrate_var_log(data.path());

        assert_eq!(fs::read_to_string(new.join("app.log")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(new.join("app.log.old")).unwrap(),
            "older"
        );
        assert_eq!(fs::read_to_string(old.join("app.log")).unwrap(), "old");
    }

    #[cfg(feature = "persistent-var-log")]
    #[test]
    fn migrate_var_log_without_old_logs_does_nothing() {
        let data = TempDir::new().unwrap();
        migrate_var_log(data.path());
        assert!(!data.path().join(paths::VAR_LOG).exists());
    }

    #[test]
    fn test_ensure_dir_creates_directory() {
        let temp = TempDir::new().unwrap();
        let new_dir = temp.path().join("test/nested/dir");

        assert!(!new_dir.exists());
        ensure_dir(&new_dir).unwrap();
        assert!(new_dir.exists());
    }

    #[test]
    fn test_ensure_dir_existing() {
        let temp = TempDir::new().unwrap();
        let existing = temp.path();

        assert!(existing.exists());
        ensure_dir(existing).unwrap();
        assert!(existing.exists());
    }

    #[test]
    fn upper_dir_empty_when_absent() {
        use std::path::PathBuf;
        assert!(is_upper_dir_empty(&PathBuf::from("/nonexistent/upper")).unwrap());
    }

    #[test]
    fn upper_dir_empty_when_empty() {
        let temp = TempDir::new().unwrap();
        assert!(is_upper_dir_empty(temp.path()).unwrap());
    }

    #[test]
    fn upper_dir_not_empty_when_populated() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("somefile"), "content").unwrap();
        assert!(!is_upper_dir_empty(temp.path()).unwrap());
    }

    #[test]
    fn test_ensure_overlay_dirs() {
        let temp = TempDir::new().unwrap();
        let upper = temp.path().join("upper");
        let work = temp.path().join("work");

        ensure_overlay_dirs(&upper, &work).unwrap();

        assert!(upper.exists());
        assert!(work.exists());
    }

    #[test]
    fn test_copy_directory_contents() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("file1.txt"), "content1").unwrap();
        fs::create_dir_all(src.join("subdir")).unwrap();
        fs::write(src.join("subdir/file2.txt"), "content2").unwrap();

        copy_directory_contents(&src, &dst).unwrap();

        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("subdir/file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("file1.txt")).unwrap(),
            "content1"
        );
    }

    #[test]
    fn test_copy_directory_nonexistent_source() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("nonexistent");
        let dst = temp.path().join("dst");

        fs::create_dir_all(&dst).unwrap();

        // Should not error on nonexistent source
        let result = copy_directory_contents(&src, &dst);
        assert!(result.is_ok());
    }
}
