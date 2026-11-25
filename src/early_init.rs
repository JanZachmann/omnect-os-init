//! Early initialization before logging is available
//!
//! This module mounts essential filesystems (/dev, /proc, /sys, /run)
//! that must be available before any other initialization can occur.

use std::{
    fs::{self, Permissions},
    io,
    os::unix::fs::PermissionsExt,
    path::Path,
};

use nix::mount::{MsFlags, mount};

use crate::error::EarlyInitError;
use crate::logging::disable_printk_ratelimit;

pub type Result<T> = std::result::Result<T, EarlyInitError>;

const INITRAMFS_ROOT: &str = "/";

// Mount point modes as base-files creates them.
const MOUNT_POINT_MODE: u32 = 0o755;
const PSEUDO_FS_MOUNT_POINT_MODE: u32 = 0o555;

/// An essential filesystem: source and fstype are the same.
struct EssentialMount {
    path: &'static str,
    fstype: &'static str,
    flags: MsFlags,
    data: Option<&'static str>,
    /// Mode of the mount point if the init has to create it.
    mode: u32,
}

/// /run moves into the final root, and systemd keeps an existing /run as it is.
const RUN_FLAGS: MsFlags = MsFlags::MS_NOSUID
    .union(MsFlags::MS_NODEV)
    .union(MsFlags::MS_STRICTATIME);

/// Order matters: /dev must be first (needed for /dev/kmsg logging).
const ESSENTIAL_MOUNTS: [EssentialMount; 4] = [
    EssentialMount {
        path: "/dev",
        fstype: "devtmpfs",
        flags: MsFlags::empty(),
        data: None,
        mode: MOUNT_POINT_MODE,
    },
    EssentialMount {
        path: "/proc",
        fstype: "proc",
        flags: MsFlags::empty(),
        data: None,
        mode: PSEUDO_FS_MOUNT_POINT_MODE,
    },
    EssentialMount {
        path: "/sys",
        fstype: "sysfs",
        flags: MsFlags::empty(),
        data: None,
        mode: PSEUDO_FS_MOUNT_POINT_MODE,
    },
    EssentialMount {
        path: "/run",
        fstype: "tmpfs",
        flags: RUN_FLAGS,
        data: Some("mode=0755"),
        mode: MOUNT_POINT_MODE,
    },
];

/// Mounts essential filesystems required before any other initialization.
///
/// Must be called as early as possible, before logging or device access.
/// Creates missing mount points, so the image needs no /proc, /sys or /run.
/// /dev and /dev/console must exist before PID 1 starts: the Rust runtime
/// opens /dev/null for closed standard fds before `main`.
pub fn mount_essential_filesystems() -> Result<()> {
    for m in &ESSENTIAL_MOUNTS {
        create_mount_point(Path::new(INITRAMFS_ROOT), m)?;
        do_mount(m.fstype, m.path, m.fstype, m.flags, m.data)?;
    }

    disable_printk_ratelimit();

    Ok(())
}

/// Creates the mount point of `m` below `root` with `m.mode`. An existing
/// directory keeps its mode; any other existing entry is an error.
fn create_mount_point(root: &Path, m: &EssentialMount) -> Result<()> {
    let path = root.join(m.path.trim_start_matches('/'));
    match fs::create_dir(&path) {
        Ok(()) => fs::set_permissions(&path, Permissions::from_mode(m.mode)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::symlink_metadata(&path).and_then(|meta| {
                if meta.is_dir() {
                    Ok(())
                } else {
                    Err(io::Error::other("exists and is not a directory"))
                }
            })
        }
        Err(e) => Err(e),
    }
    .map_err(|e| EarlyInitError::MountPointFailed {
        target: m.path.to_string(),
        reason: e.to_string(),
    })
}

fn do_mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<()> {
    mount(Some(source), target, Some(fstype), flags, data).map_err(|e| {
        EarlyInitError::MountFailed {
            target: target.to_string(),
            reason: e.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_is_mounted_without_suid_and_devices() {
        let run = ESSENTIAL_MOUNTS.iter().find(|m| m.path == "/run").unwrap();
        assert_eq!(run.fstype, "tmpfs");
        assert!(run.flags.contains(MsFlags::MS_NOSUID | MsFlags::MS_NODEV));
        assert_eq!(run.data, Some("mode=0755"));
    }

    #[test]
    fn dev_is_mounted_first() {
        assert_eq!(ESSENTIAL_MOUNTS[0].path, "/dev");
    }

    #[test]
    fn mount_point_modes_match_base_files() {
        for m in &ESSENTIAL_MOUNTS {
            let expected = match m.path {
                "/proc" | "/sys" => 0o555,
                _ => 0o755,
            };
            assert_eq!(m.mode, expected, "{}", m.path);
        }
    }

    #[test]
    fn missing_mount_points_are_created_with_their_mode() {
        let root = tempfile::tempdir().unwrap();
        for m in &ESSENTIAL_MOUNTS {
            create_mount_point(root.path(), m).unwrap();
            let meta = fs::metadata(root.path().join(m.path.trim_start_matches('/'))).unwrap();
            assert!(meta.is_dir(), "{}", m.path);
            assert_eq!(meta.permissions().mode() & 0o7777, m.mode, "{}", m.path);
        }
    }

    #[test]
    fn existing_mount_point_keeps_its_mode() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path().join("proc");
        fs::create_dir(&proc).unwrap();
        fs::set_permissions(&proc, Permissions::from_mode(0o700)).unwrap();
        let m = ESSENTIAL_MOUNTS.iter().find(|m| m.path == "/proc").unwrap();
        create_mount_point(root.path(), m).unwrap();
        assert_eq!(
            fs::metadata(&proc).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn existing_file_is_not_a_mount_point() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("proc"), "").unwrap();
        let m = ESSENTIAL_MOUNTS.iter().find(|m| m.path == "/proc").unwrap();
        let err = create_mount_point(root.path(), m).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn mount_point_error_names_the_path() {
        let root = tempfile::tempdir().unwrap();
        let missing_root = root.path().join("missing");
        let m = ESSENTIAL_MOUNTS.iter().find(|m| m.path == "/proc").unwrap();
        let err = create_mount_point(&missing_root, m).unwrap_err();
        assert!(err.to_string().contains("/proc"), "{err}");
    }
}
