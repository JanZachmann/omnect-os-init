//! Switch root to final rootfs and exec init
//!
//! Implements the switch_root operation using MS_MOVE + chroot to transition
//! from initramfs to the real rootfs. pivot_root(2) is not used because the
//! initramfs root cannot be pivoted (returns EINVAL).

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix::mount::{MsFlags, mount};
use nix::sys::statfs::{FsType, TMPFS_MAGIC, statfs};
use nix::unistd::{chdir, chroot};

use crate::config::CmdlineConfig;
use crate::error::{InitramfsError, Result};

/// Magic number of ramfs (linux/magic.h), not provided by nix. Compared as
/// `u32`, because `f_type` is signed and only 32 bits wide on 32-bit targets.
const RAMFS_MAGIC: u32 = 0x8584_58f6;

/// The old root is reached through its open file descriptor after the switch.
const PROC_SELF_FD: &str = "/proc/self/fd";

/// Default init binary path, used when `init=` is absent from the kernel cmdline.
const DEFAULT_INIT: &str = "/sbin/init";

/// Resolve the init binary path from the kernel cmdline.
///
/// Returns `DEFAULT_INIT` when `init=` is absent, empty, or a bare flag.
/// The filter cannot live in `CmdlineConfig::get` because bare flags (`ro`,
/// `quiet`, etc.) are legitimately stored as empty strings and callers rely
/// on `get("ro") == Some("")` to detect them. The empty-means-absent
/// convention is specific to `init=`, so the filter belongs here.
fn init_path_from_cmdline(cmdline: &CmdlineConfig) -> &str {
    cmdline
        .get("init")
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_INIT)
}

/// Switch root to the new rootfs and exec init
pub fn switch_root(new_root: &Path, cmdline: &CmdlineConfig) -> Result<()> {
    let init_path = init_path_from_cmdline(cmdline);

    log::info!(
        "Switching root to {} with init {}",
        new_root.display(),
        init_path
    );
    if !new_root.exists() {
        return Err(InitramfsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("New root does not exist: {}", new_root.display()),
        )));
    }

    // Verify the init binary exists BEFORE moving any mounts. If init is
    // missing we want to fail while /dev, /proc, /sys, /run are still on
    // the initramfs so the debug shell / fatal-error path still works.
    let init_full_path = resolve_init_path(new_root, init_path)?;

    let initramfs = open_initramfs_root();

    // Ensure target mountpoint directories exist under new_root.
    // MS_MOVE fails with ENOENT if the target directory is missing.
    for dir in &["dev", "proc", "sys", "run"] {
        fs::create_dir_all(new_root.join(dir)).map_err(|e| {
            InitramfsError::Io(std::io::Error::other(format!(
                "Failed to create mountpoint {}/{}: {}",
                new_root.display(),
                dir,
                e
            )))
        })?;
    }

    // Move critical mounts to new root before switching.
    // Track which mounts succeeded so any intermediate failure can be rolled back,
    // preserving the debug/emergency shell environment on the initramfs.
    let critical_mounts = [
        ("/dev", "dev"),
        ("/proc", "proc"),
        ("/sys", "sys"),
        ("/run", "run"), // moved so ODS can read its runtime state after root switch
    ];
    let mut moved: Vec<&str> = Vec::new();
    for (src, name) in &critical_mounts {
        if let Err(e) = move_mount(src, &new_root.join(name)) {
            rollback_critical_mounts(&moved, new_root);
            return Err(e);
        }
        moved.push(name);
    }

    chdir(new_root).map_err(|e| {
        rollback_critical_mounts(&moved, new_root);
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to chdir to new root: {}",
            e
        )))
    })?;

    // MS_MOVE re-mounts the new root at /. This is the correct approach for
    // initramfs: ramfs does not support pivot_root (EINVAL). busybox and
    // systemd use the same MS_MOVE + chroot pattern.
    //
    // On failure: restore all moved mounts so the debug/emergency shell still
    // has access to /dev, /proc, /sys, /run on the initramfs.
    if let Err(e) = mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>) {
        rollback_critical_mounts(&moved, new_root);
        return Err(InitramfsError::Io(std::io::Error::other(format!(
            "Failed to MS_MOVE new root to /: {}",
            e
        ))));
    }

    chroot(".").map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!("Failed to chroot: {}", e)))
    })?;

    chdir("/").map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to chdir to /: {}",
            e
        )))
    })?;

    if let Some((dir, dev)) = initramfs {
        free_initramfs(&dir, dev);
    }

    log::info!("Executing init: {}", init_full_path);

    // exec() replaces the current process - does not return on success.
    // env_clear() ensures no initramfs environment variables leak into the
    // new init (systemd); it sets up its own environment independently.
    // fsck results and ODS runtime state are passed via files (/run, bootloader
    // env, /data/log/fsck/) — not through the process environment.
    let err = Command::new(&init_full_path).env_clear().exec();

    // If we get here, exec failed
    Err(InitramfsError::Io(std::io::Error::other(format!(
        "Failed to exec init: {}",
        err
    ))))
}

/// Opens the initramfs root, so its files can be deleted after the switch.
///
/// Returns `None` when `/` is not a ramfs or tmpfs: then it is a real disk and
/// must stay as it is.
fn open_initramfs_root() -> Option<(File, u64)> {
    match statfs("/") {
        Ok(s) if is_ram_fs(s.filesystem_type()) => {}
        Ok(_) => {
            log::warn!("/ is not a ramfs or tmpfs; the initramfs is kept");
            return None;
        }
        Err(e) => {
            log::warn!("Cannot statfs /: {e}; the initramfs is kept");
            return None;
        }
    }

    match File::open("/").and_then(|dir| Ok((dir.metadata()?.dev(), dir))) {
        Ok((dev, dir)) => Some((dir, dev)),
        Err(e) => {
            log::warn!("Cannot open the initramfs root: {e}; the initramfs is kept");
            None
        }
    }
}

fn is_ram_fs(fs_type: FsType) -> bool {
    fs_type.0 as u32 == RAMFS_MAGIC || fs_type == TMPFS_MAGIC
}

/// Deletes the initramfs files to free their RAM.
fn free_initramfs(dir: &File, dev: u64) {
    let path = PathBuf::from(format!("{PROC_SELF_FD}/{}", dir.as_raw_fd()));
    let mut count = DeleteCount::default();
    delete_contents(&path, dev, &mut count);
    log::info!(
        "Freed the initramfs: {} entries deleted, {} failed",
        count.deleted,
        count.failed
    );
}

#[derive(Debug, Default, PartialEq)]
struct DeleteCount {
    deleted: usize,
    failed: usize,
}

/// Best effort, recursive delete of the contents of `dir`. Stops at entries on
/// another device than `dev`, so mounted filesystems are not touched.
fn delete_contents(dir: &Path, dev: u64, count: &mut DeleteCount) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            count.failed += 1;
            log::warn!("Cannot read {}: {e}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            count.failed += 1;
            continue;
        };
        if meta.dev() != dev {
            continue;
        }
        let result = if meta.is_dir() {
            delete_contents(&path, dev, count);
            fs::remove_dir(&path)
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => count.deleted += 1,
            Err(e) => {
                count.failed += 1;
                log::debug!("Cannot delete {}: {e}", path.display());
            }
        }
    }
}

fn move_mount(source: &str, target: &Path) -> Result<()> {
    use nix::mount::{MsFlags, mount};

    mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_MOVE,
        None::<&str>,
    )
    .map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to move {} → {}: {}",
            source,
            target.display(),
            e
        )))
    })?;

    Ok(())
}

/// Attempt to restore already-moved critical mounts back to the initramfs.
///
/// Called on any failure before MS_MOVE succeeds — at that point `/` is still
/// the initramfs root, so moving from `new_root/{name}` back to `/{name}`
/// restores the debug/emergency shell environment. Best-effort: individual
/// failures are logged and do not abort.
fn rollback_critical_mounts(moved: &[&str], new_root: &Path) {
    for name in moved.iter().rev() {
        let src = new_root.join(name);
        let dst = format!("/{name}");
        if let Err(e) = move_mount(&src.to_string_lossy(), Path::new(&dst)) {
            log::warn!("Failed to restore /{name} to initramfs during rollback: {e}");
        } else {
            log::debug!("Restored /{name} to initramfs");
        }
    }
}

/// Returns true if `path` is a regular file with at least one executable bit set.
///
/// Checking only `is_file()` is insufficient — a non-executable file would cause
/// `exec` to fail after critical mounts have already been moved to the new root.
/// Errors from `metadata()` (e.g. permission denied) propagate immediately, so a
/// failed check reports its real cause.
fn is_executable_file(path: &Path) -> std::io::Result<bool> {
    let m = path.metadata()?;
    Ok(m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Resolve the init binary path within the new root.
///
/// Validates the path and checks that the binary exists and is executable.
/// The caller must supply a valid `init=` kernel cmdline parameter or the
/// default `/sbin/init`. A wrong path is a system configuration error and
/// surfaces immediately.
///
/// Returns an absolute path string (starting with `/`) so that `Command::new`
/// on PID 1 does not fall back to a PATH lookup.
fn resolve_init_path(new_root: &Path, requested_init: &str) -> Result<String> {
    // Reject paths containing ".." to prevent escaping new_root before chroot.
    if requested_init.split('/').any(|c| c == "..") {
        return Err(InitramfsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("init path must not contain '..': {requested_init}"),
        )));
    }

    // Ensure the caller-supplied path is absolute to avoid PATH lookup on exec.
    let init_path = if requested_init.starts_with('/') {
        requested_init.to_string()
    } else {
        format!("/{}", requested_init)
    };

    let full_path = new_root.join(init_path.trim_start_matches('/'));
    if is_executable_file(&full_path)? {
        return Ok(init_path);
    }

    Err(InitramfsError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
            "Init binary not found or not executable at {}: {}",
            new_root.display(),
            init_path
        ),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_executable(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn delete_contents_deletes_the_whole_tree() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("a/b")).unwrap();
        fs::write(temp.path().join("a/b/file"), "x").unwrap();
        fs::write(temp.path().join("top"), "x").unwrap();
        std::os::unix::fs::symlink("/nonexistent", temp.path().join("link")).unwrap();

        let dev = temp.path().metadata().unwrap().dev();
        let mut count = DeleteCount::default();
        delete_contents(temp.path(), dev, &mut count);

        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
        assert_eq!(
            count,
            DeleteCount {
                deleted: 5,
                failed: 0
            }
        );
    }

    #[test]
    fn delete_contents_skips_other_devices() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("a")).unwrap();
        fs::write(temp.path().join("a/file"), "x").unwrap();

        // Every entry is on the tempdir device, so a different device must
        // leave them all, as with a mounted filesystem.
        let other_dev = temp.path().metadata().unwrap().dev() + 1;
        let mut count = DeleteCount::default();
        delete_contents(temp.path(), other_dev, &mut count);

        assert!(temp.path().join("a/file").exists());
        assert_eq!(count, DeleteCount::default());
    }

    #[test]
    fn delete_contents_counts_an_unreadable_dir_as_failed() {
        let temp = TempDir::new().unwrap();
        let mut count = DeleteCount::default();

        delete_contents(&temp.path().join("missing"), 0, &mut count);

        assert_eq!(
            count,
            DeleteCount {
                deleted: 0,
                failed: 1
            }
        );
    }

    #[test]
    fn delete_contents_deletes_a_dir_symlink_but_not_its_target() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("root");
        let target = temp.path().join("target");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(&target, root.join("link")).unwrap();

        let dev = root.metadata().unwrap().dev();
        let mut count = DeleteCount::default();
        delete_contents(&root, dev, &mut count);

        assert!(root.join("link").symlink_metadata().is_err());
        assert!(target.join("file").exists());
        assert_eq!(
            count,
            DeleteCount {
                deleted: 1,
                failed: 0
            }
        );
    }

    #[test]
    fn ramfs_and_tmpfs_are_ram_filesystems() {
        use nix::sys::statfs::EXT4_SUPER_MAGIC;

        assert!(is_ram_fs(FsType(RAMFS_MAGIC as _)));
        assert!(is_ram_fs(TMPFS_MAGIC));
        assert!(!is_ram_fs(EXT4_SUPER_MAGIC));
    }

    #[test]
    fn free_initramfs_deletes_through_the_fd() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("file"), "x").unwrap();
        let dir = File::open(temp.path()).unwrap();
        let dev = dir.metadata().unwrap().dev();

        free_initramfs(&dir, dev);

        assert!(!temp.path().join("file").exists());
    }

    #[test]
    fn test_find_init_default() {
        let temp = TempDir::new().unwrap();
        let sbin = temp.path().join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        write_executable(&sbin.join("init"), "#!/bin/sh");

        let result = resolve_init_path(temp.path(), "/sbin/init");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/sbin/init");
    }

    #[test]
    fn test_find_init_not_found() {
        let temp = TempDir::new().unwrap();
        // No fallback probing: absent init is always an error.
        let result = resolve_init_path(temp.path(), "/sbin/init");
        assert!(result.is_err());
    }

    #[test]
    fn test_find_init_non_executable() {
        // A file without +x should not be accepted as init.
        let temp = TempDir::new().unwrap();
        let sbin = temp.path().join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        fs::write(sbin.join("init"), "#!/bin/sh").unwrap();
        fs::set_permissions(sbin.join("init"), fs::Permissions::from_mode(0o644)).unwrap();

        let result = resolve_init_path(temp.path(), "/sbin/init");
        assert!(result.is_err());
    }

    #[test]
    fn test_find_init_no_fallback_to_systemd() {
        // Requesting /sbin/init when only systemd is present must fail — no silent fallback.
        let temp = TempDir::new().unwrap();
        let systemd_dir = temp.path().join("lib/systemd");
        fs::create_dir_all(&systemd_dir).unwrap();
        write_executable(&systemd_dir.join("systemd"), "#!/bin/sh");

        let result = resolve_init_path(temp.path(), "/sbin/init");
        assert!(result.is_err());
    }

    #[test]
    fn test_find_init_explicit_systemd() {
        // Explicitly requesting systemd as init path must work.
        let temp = TempDir::new().unwrap();
        let systemd_dir = temp.path().join("lib/systemd");
        fs::create_dir_all(&systemd_dir).unwrap();
        write_executable(&systemd_dir.join("systemd"), "#!/bin/sh");

        let result = resolve_init_path(temp.path(), "/lib/systemd/systemd");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/lib/systemd/systemd");
    }

    #[test]
    fn test_cmdline_init_override_used() {
        // init= on the kernel cmdline must take precedence over DEFAULT_INIT.
        let temp = TempDir::new().unwrap();
        let systemd_dir = temp.path().join("lib/systemd");
        fs::create_dir_all(&systemd_dir).unwrap();
        write_executable(&systemd_dir.join("systemd"), "#!/bin/sh");

        let cmdline = CmdlineConfig::parse("init=/lib/systemd/systemd ro quiet");
        let init_path = init_path_from_cmdline(&cmdline);
        let result = resolve_init_path(temp.path(), init_path);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/lib/systemd/systemd");
    }

    #[test]
    fn test_cmdline_init_absent_falls_back_to_default() {
        let temp = TempDir::new().unwrap();
        let sbin = temp.path().join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        write_executable(&sbin.join("init"), "#!/bin/sh");

        let cmdline = CmdlineConfig::parse("ro quiet");
        let init_path = init_path_from_cmdline(&cmdline);
        let result = resolve_init_path(temp.path(), init_path);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/sbin/init");
    }

    #[test]
    fn test_cmdline_init_bare_flag_falls_back_to_default() {
        // bare `init` token (no =) stores Some("") — must fall back to DEFAULT_INIT
        let temp = TempDir::new().unwrap();
        let sbin = temp.path().join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        write_executable(&sbin.join("init"), "#!/bin/sh");

        let cmdline = CmdlineConfig::parse("init ro quiet");
        let init_path = init_path_from_cmdline(&cmdline);
        let result = resolve_init_path(temp.path(), init_path);
        assert!(
            result.is_ok(),
            "bare init token must fall back to DEFAULT_INIT"
        );
        assert_eq!(result.unwrap(), "/sbin/init");
    }

    #[test]
    fn test_cmdline_init_empty_value_falls_back_to_default() {
        // `init=` (empty value) stores Some("") — must fall back to DEFAULT_INIT
        let temp = TempDir::new().unwrap();
        let sbin = temp.path().join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        write_executable(&sbin.join("init"), "#!/bin/sh");

        let cmdline = CmdlineConfig::parse("init= ro quiet");
        let init_path = init_path_from_cmdline(&cmdline);
        let result = resolve_init_path(temp.path(), init_path);
        assert!(
            result.is_ok(),
            "empty init= value must fall back to DEFAULT_INIT"
        );
        assert_eq!(result.unwrap(), "/sbin/init");
    }
}
