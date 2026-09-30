//! EFI boot-entry handling after a flash mode writes a disk.
//!
//! A machine that boots via EFI needs its boot entry rebuilt to point at the
//! freshly written loader.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::FlashError;
use crate::filesystem::{FsType, MountOptions, MountPoint, is_path_mounted, mount};
use crate::mode::flash::{scratch_mounts, with_mount};
use crate::partition::layout::PARTITION_NUM_BOOT;

const EFIBOOTMGR_CMD: &str = "/sbin/efibootmgr";
const EFI_BOOT_ENTRY_LABEL: &str = "omnect_os";
const EFI_LOADER_PATH: &str = r"\EFI\BOOT\bootx64.efi";
const EFI_ENTRY_DUMP_FILE: &str = "EFI/BOOT/efibootmgr_entry";
/// `efibootmgr` reads and writes the NVRAM variables through this mount.
const EFIVARFS_MOUNT_POINT: &str = "/sys/firmware/efi/efivars";

const EFIBOOTMGR_CREATE_FLAG: &str = "-c";
const EFIBOOTMGR_DISK_FLAG: &str = "-d";
const EFIBOOTMGR_PART_FLAG: &str = "-p";
const EFIBOOTMGR_LABEL_FLAG: &str = "-L";
const EFIBOOTMGR_LOADER_FLAG: &str = "-l";
const EFIBOOTMGR_BOOTNUM_FLAG: &str = "-b";
const EFIBOOTMGR_DELETE_FLAG: &str = "-B";
const EFIBOOTMGR_VERBOSE_FLAG: &str = "-v";

const BOOT_ENTRY_LINE_PREFIX: &str = "Boot";
const BOOT_ENTRY_ID_LEN: usize = 4;

/// The side effects of the EFI handling, so a test can pin their order.
pub(crate) trait EfiOps {
    fn mount_efivarfs(&mut self) -> Result<(), FlashError>;
    fn efibootmgr(&mut self, args: &[String]) -> Result<String, FlashError>;
    fn write_entry_dump(&mut self, boot_partition: &Path, dump: &str) -> Result<(), FlashError>;
}

#[derive(Default)]
pub(crate) struct RealEfiOps;

impl EfiOps for RealEfiOps {
    fn mount_efivarfs(&mut self) -> Result<(), FlashError> {
        let target = Path::new(EFIVARFS_MOUNT_POINT);
        if !is_path_mounted(target)? {
            mount(MountPoint::new(
                FsType::Efivarfs.as_str(),
                target,
                MountOptions::efivarfs(),
            ))?;
        }
        Ok(())
    }

    fn efibootmgr(&mut self, args: &[String]) -> Result<String, FlashError> {
        let output = Command::new(EFIBOOTMGR_CMD)
            .args(args)
            .output()
            .map_err(|e| FlashError::EfiFailed(format!("failed to run {EFIBOOTMGR_CMD}: {e}")))?;

        if !output.status.success() {
            return Err(FlashError::EfiFailed(format!(
                "{EFIBOOTMGR_CMD} {args:?} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn write_entry_dump(&mut self, boot_partition: &Path, dump: &str) -> Result<(), FlashError> {
        with_mount(
            boot_partition,
            Path::new(scratch_mounts::EFI_BOOT),
            MountOptions::vfat(),
            |boot_mount| {
                let path = boot_mount.join(EFI_ENTRY_DUMP_FILE);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|source| FlashError::PathIo {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                fs::write(&path, dump).map_err(|source| FlashError::PathIo { path, source })
            },
        )
    }
}

pub(crate) fn entry_args(target_disk: &Path) -> Vec<String> {
    vec![
        EFIBOOTMGR_CREATE_FLAG.to_string(),
        EFIBOOTMGR_DISK_FLAG.to_string(),
        target_disk.display().to_string(),
        EFIBOOTMGR_PART_FLAG.to_string(),
        PARTITION_NUM_BOOT.to_string(),
        EFIBOOTMGR_LABEL_FLAG.to_string(),
        EFI_BOOT_ENTRY_LABEL.to_string(),
        EFIBOOTMGR_LOADER_FLAG.to_string(),
        EFI_LOADER_PATH.to_string(),
    ]
}

fn delete_args(id: String) -> Vec<String> {
    vec![
        EFIBOOTMGR_BOOTNUM_FLAG.to_string(),
        id,
        EFIBOOTMGR_DELETE_FLAG.to_string(),
    ]
}

/// The `Boot####` ids of every entry in `efibootmgr`'s default listing,
/// active or not.
///
/// `BootCurrent:`, `BootOrder:` and similar summary lines have no four hex
/// digits right after `Boot` and are skipped.
fn boot_entry_ids(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let id = line
                .strip_prefix(BOOT_ENTRY_LINE_PREFIX)?
                .get(..BOOT_ENTRY_ID_LEN)?;
            id.bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then(|| id.to_string())
        })
        .collect()
}

/// Rebuild the EFI boot entry to point at a freshly flashed `target_disk`,
/// and record the result on its boot partition.
///
/// Every other entry, omnect or not, is deleted. The new entry is created
/// first, so a failure part-way leaves the machine with a boot entry.
/// `efibootmgr -c` takes an unused number, so it is never in the old list.
pub(crate) fn handle(
    ops: &mut (impl EfiOps + ?Sized),
    target_disk: &Path,
    boot_partition: &Path,
) -> Result<(), FlashError> {
    ops.mount_efivarfs()?;
    let old_entries = boot_entry_ids(&ops.efibootmgr(&[])?);
    ops.efibootmgr(&entry_args(target_disk))?;
    for id in old_entries {
        ops.efibootmgr(&delete_args(id))?;
    }

    let dump = ops.efibootmgr(&[EFIBOOTMGR_VERBOSE_FLAG.to_string()])?;
    ops.write_entry_dump(boot_partition, &dump)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
BootCurrent: 0002
BootNext: 0001
Timeout: 1 seconds
BootOrder: 0002,0000,0001
Boot0000* Windows Boot Manager
Boot0001  UEFI: Built-in EFI Shell
Boot0002* omnect_os
";

    /// Records every EFI side effect as one line, in call order.
    #[derive(Default)]
    struct RecordingEfiOps {
        calls: Vec<String>,
    }

    impl EfiOps for RecordingEfiOps {
        fn mount_efivarfs(&mut self) -> Result<(), FlashError> {
            self.calls.push("mount efivarfs".to_string());
            Ok(())
        }

        fn efibootmgr(&mut self, args: &[String]) -> Result<String, FlashError> {
            self.calls.push(
                format!("efibootmgr {}", args.join(" "))
                    .trim_end()
                    .to_string(),
            );
            Ok(if args.is_empty() { LISTING } else { "" }.to_string())
        }

        fn write_entry_dump(
            &mut self,
            boot_partition: &Path,
            _dump: &str,
        ) -> Result<(), FlashError> {
            self.calls
                .push(format!("write entry dump to {}", boot_partition.display()));
            Ok(())
        }
    }

    #[test]
    fn the_new_entry_is_created_before_the_old_ones_are_deleted() {
        let mut ops = RecordingEfiOps::default();
        handle(&mut ops, Path::new("/dev/sdb"), Path::new("/dev/sdb1")).unwrap();
        assert_eq!(
            ops.calls,
            [
                "mount efivarfs",
                "efibootmgr",
                r"efibootmgr -c -d /dev/sdb -p 1 -L omnect_os -l \EFI\BOOT\bootx64.efi",
                "efibootmgr -b 0000 -B",
                "efibootmgr -b 0001 -B",
                "efibootmgr -b 0002 -B",
                "efibootmgr -v",
                "write entry dump to /dev/sdb1",
            ]
        );
    }

    #[test]
    fn boot_entry_ids_lists_every_entry_active_or_not() {
        assert_eq!(
            boot_entry_ids(LISTING),
            vec!["0000".to_string(), "0001".to_string(), "0002".to_string()]
        );
    }
}
