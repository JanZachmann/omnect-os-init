use std::fs;
use std::os::unix::fs::{MetadataExt, chown};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{FactoryResetError, Result};
use crate::filesystem::CP_CMD;
use crate::mode::factory_reset::CONTEXT_SEPARATOR;

const SYNC_CMD: &str = "/bin/sync";

/// Result of a restore pass over the full preserve list.
pub enum RestoreResult {
    Success,
    PartialFailure { context: String, error: String },
}

/// Backup all preserve-list paths from rootfs into backup_dir. Returns the
/// subset actually backed up (those whose source existed).
pub fn backup_all(
    rootfs: &Path,
    preserve_list: &[String],
    backup_dir: &Path,
) -> Result<Vec<String>> {
    std::fs::create_dir_all(backup_dir)?;
    let mut backed_up = Vec::new();
    for path in preserve_list {
        if backup_path(rootfs, path, backup_dir)? {
            backed_up.push(path.clone());
        }
    }
    Ok(backed_up)
}

/// Restore all backed-up paths from backup_dir back into rootfs.
///
/// `backed_up` is the manifest returned by `backup_all`. A manifest entry whose
/// backup is missing at restore time is reported as `PartialFailure`. Per-path
/// restore failures are accumulated rather than aborting mid-restore, so as many
/// paths as possible are restored.
pub fn restore_all(
    rootfs: &Path,
    backed_up: &[String],
    backup_dir: &Path,
) -> Result<RestoreResult> {
    let mut error_context: Vec<String> = Vec::new();
    let mut last_error: Option<String> = None;

    for path in backed_up {
        let backup_src = backup_source(rootfs, path, backup_dir);
        if !backup_src.exists() {
            log::warn!("restore: backup for {path} is missing; preserved data lost");
            error_context.push(format!("{}:missing-backup", path.trim_start_matches('/')));
            last_error = Some(format!("backup missing for {path}"));
            continue;
        }
        if let Err(e) = restore_path(rootfs, path, backup_dir) {
            log::warn!("restore failed for {path}: {e}");
            error_context.push(format!("{}:restore", path.trim_start_matches('/')));
            last_error = Some(e.to_string());
        }
    }

    if let Some(error) = last_error {
        Ok(RestoreResult::PartialFailure {
            context: error_context.join(CONTEXT_SEPARATOR),
            error,
        })
    } else {
        Ok(RestoreResult::Success)
    }
}

fn backup_path(rootfs: &Path, path: &str, backup_dir: &Path) -> Result<bool> {
    let src = rootfs.join(path.trim_start_matches('/'));

    if !src.exists() {
        log::info!("backup: {path} does not exist; skipping");
        return Ok(false);
    }

    log::info!("backup: {path}");

    let output = Command::new(CP_CMD)
        .args(["--parents", "-a"])
        .arg(&src)
        .arg(backup_dir)
        .output()
        .map_err(|e| FactoryResetError::BackupFailed {
            path: PathBuf::from(path),
            reason: format!("Failed to run cp: {e}"),
        })?;

    if !output.status.success() {
        return Err(FactoryResetError::BackupFailed {
            path: PathBuf::from(path),
            reason: format!(
                "cp failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        }
        .into());
    }

    run_sync()?;
    Ok(true)
}

/// Path of a preserved entry inside the backup tree. One definition so
/// `restore_all`'s existence check and `restore_path`'s copy source stay in sync.
fn backup_source(rootfs: &Path, path: &str, backup_dir: &Path) -> PathBuf {
    backup_root(rootfs, backup_dir).join(path.trim_start_matches('/'))
}

fn backup_root(rootfs: &Path, backup_dir: &Path) -> PathBuf {
    backup_dir.join(rootfs.strip_prefix("/").unwrap_or(rootfs))
}

/// Creates the missing directories up to `dest_dir` with the owner and mode
/// they have in the backup.
fn create_parent_dirs(rootfs: &Path, dest_dir: &Path, backup_root: &Path) -> std::io::Result<()> {
    let missing: Vec<&Path> = dest_dir
        .ancestors()
        .take_while(|dir| *dir != rootfs && dir.symlink_metadata().is_err())
        .collect();

    for dir in missing.into_iter().rev() {
        let rel = dir.strip_prefix(rootfs).map_err(std::io::Error::other)?;
        let meta = fs::metadata(backup_root.join(rel))?;
        fs::create_dir(dir)?;
        chown(dir, Some(meta.uid()), Some(meta.gid()))?;
        fs::set_permissions(dir, meta.permissions())?;
    }

    if !dest_dir.is_dir() {
        return Err(std::io::Error::other("not a directory"));
    }
    Ok(())
}

fn restore_path(rootfs: &Path, path: &str, backup_dir: &Path) -> Result<()> {
    let path_stripped = path.trim_start_matches('/');
    let backup_src = backup_source(rootfs, path, backup_dir);

    if !backup_src.exists() {
        log::info!("restore: {path} does not exist in backup; skipping");
        return Ok(());
    }

    log::info!("restore: {path}");

    let dest_full = rootfs.join(path_stripped);
    let dest_dir = dest_full.parent().unwrap_or(rootfs);

    create_parent_dirs(rootfs, dest_dir, &backup_root(rootfs, backup_dir)).map_err(|e| {
        FactoryResetError::RestoreFailed {
            path: PathBuf::from(path),
            reason: format!(
                "Cannot recreate {} from the backup: {e}",
                dest_dir.display()
            ),
        }
    })?;

    let output = Command::new(CP_CMD)
        .arg("-a")
        .arg(&backup_src)
        .arg(dest_dir)
        .output()
        .map_err(|e| FactoryResetError::RestoreFailed {
            path: PathBuf::from(path),
            reason: format!("Failed to run cp: {e}"),
        })?;

    if !output.status.success() {
        return Err(FactoryResetError::RestoreFailed {
            path: PathBuf::from(path),
            reason: format!(
                "cp failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        }
        .into());
    }

    run_sync()?;
    Ok(())
}

fn run_sync() -> Result<()> {
    let status = Command::new(SYNC_CMD)
        .status()
        .map_err(|e| FactoryResetError::Io(std::io::Error::other(format!("sync failed: {e}"))))?;
    if !status.success() {
        return Err(FactoryResetError::Io(std::io::Error::other(format!(
            "sync exited with {status}"
        )))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn backup_path_nonexistent_source_silently_skips() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();
        backup_path(&rootfs, "/etc/hostname", &backup).unwrap();
        assert!(!backup.join("etc/hostname").exists());
    }

    #[test]
    fn restore_path_restores_file_from_backup() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        let backup_root = backup.join(rootfs.strip_prefix("/").unwrap_or(rootfs.as_path()));
        let restored = rootfs.join("etc/hostname");
        let backup_src = backup_root.join("etc/hostname");

        fs::create_dir_all(restored.parent().unwrap()).unwrap();
        fs::create_dir_all(backup_src.parent().unwrap()).unwrap();
        fs::write(&backup_src, "restored-hostname").unwrap();

        restore_path(&rootfs, "/etc/hostname", &backup).unwrap();

        assert_eq!(fs::read_to_string(restored).unwrap(), "restored-hostname");
    }

    #[test]
    fn restore_path_creates_parents_like_backup() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        let backup_root = backup_root(&rootfs, &backup);
        fs::create_dir_all(rootfs.join("var")).unwrap();
        fs::set_permissions(rootfs.join("var"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(backup_root.join("var/app/sub")).unwrap();
        fs::write(backup_root.join("var/app/sub/file"), "data").unwrap();
        fs::set_permissions(backup_root.join("var"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            backup_root.join("var/app"),
            fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        fs::set_permissions(
            backup_root.join("var/app/sub"),
            fs::Permissions::from_mode(0o710),
        )
        .unwrap();

        restore_path(&rootfs, "/var/app/sub/file", &backup).unwrap();

        let mode = |p: &str| fs::metadata(rootfs.join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            fs::read_to_string(rootfs.join("var/app/sub/file")).unwrap(),
            "data"
        );
        assert_eq!(mode("var/app"), 0o750);
        assert_eq!(mode("var/app/sub"), 0o710);
        // An existing parent keeps its mode.
        assert_eq!(mode("var"), 0o755);
        // The owner is not checked: both trees belong to the test user.
    }

    #[test]
    fn restore_path_nonexistent_backup_silently_skips() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();

        restore_path(&rootfs, "/etc/hostname", &backup).unwrap();
        assert!(!rootfs.join("etc/hostname").exists());
    }

    #[test]
    fn restore_all_success_on_empty_list() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();

        let result = restore_all(&rootfs, &[], &backup).unwrap();
        assert!(matches!(result, RestoreResult::Success));
    }

    #[test]
    fn restore_all_partial_failure_accumulates_context() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        let backup_root = backup.join(rootfs.strip_prefix("/").unwrap_or(rootfs.as_path()));

        // Backup file exists
        fs::create_dir_all(backup_root.join("etc")).unwrap();
        fs::write(backup_root.join("etc/hostname"), "host").unwrap();

        // Place a regular file at rootfs/etc so create_dir_all(rootfs/etc) fails,
        // triggering RestoreFailed for the path
        fs::create_dir_all(&rootfs).unwrap();
        fs::write(rootfs.join("etc"), "not-a-dir").unwrap();

        let manifest = vec!["/etc/hostname".to_string()];
        let result = restore_all(&rootfs, &manifest, &backup).unwrap();
        assert!(matches!(result, RestoreResult::PartialFailure { .. }));
        if let RestoreResult::PartialFailure { context, .. } = result {
            assert!(context.contains("etc/hostname:restore"));
        }
    }

    #[test]
    fn backup_all_returns_only_paths_that_existed() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::write(rootfs.join("etc/hostname"), "host").unwrap();

        let preserve = vec!["/etc/hostname".to_string(), "/etc/absent".to_string()];
        let manifest = backup_all(&rootfs, &preserve, &backup).unwrap();
        assert_eq!(manifest, vec!["/etc/hostname".to_string()]);
    }

    #[test]
    fn restore_all_reports_partial_failure_when_backed_up_file_vanished() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();

        // Manifest claims /etc/hostname was backed up, but the backup dir is empty
        // (simulates the tmpfs backup lost between backup and restore).
        let manifest = vec!["/etc/hostname".to_string()];
        let result = restore_all(&rootfs, &manifest, &backup).unwrap();
        assert!(matches!(result, RestoreResult::PartialFailure { .. }));
        if let RestoreResult::PartialFailure { context, .. } = result {
            assert!(context.contains("etc/hostname:missing-backup"));
        }
    }
}
