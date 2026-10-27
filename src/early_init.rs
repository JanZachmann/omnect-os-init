//! Early initialization before logging is available
//!
//! This module mounts essential filesystems (/dev, /proc, /sys, /run)
//! that must be available before any other initialization can occur.

use nix::mount::{MsFlags, mount};

use crate::error::EarlyInitError;
use crate::logging::disable_printk_ratelimit;

pub type Result<T> = std::result::Result<T, EarlyInitError>;

/// An essential filesystem: source and fstype are the same.
struct EssentialMount {
    path: &'static str,
    fstype: &'static str,
    flags: MsFlags,
    data: Option<&'static str>,
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
    },
    EssentialMount {
        path: "/proc",
        fstype: "proc",
        flags: MsFlags::empty(),
        data: None,
    },
    EssentialMount {
        path: "/sys",
        fstype: "sysfs",
        flags: MsFlags::empty(),
        data: None,
    },
    EssentialMount {
        path: "/run",
        fstype: "tmpfs",
        flags: RUN_FLAGS,
        data: Some("mode=0755"),
    },
];

/// Mounts essential filesystems required before any other initialization.
///
/// Must be called as early as possible, before logging or device access.
pub fn mount_essential_filesystems() -> Result<()> {
    for m in &ESSENTIAL_MOUNTS {
        do_mount(m.fstype, m.path, m.fstype, m.flags, m.data)?;
    }

    disable_printk_ratelimit();

    Ok(())
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
}
