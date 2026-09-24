//! EFI boot-entry handling after a flash mode writes a disk.
//!
//! A machine that boots via EFI needs its boot entry rebuilt to point at the
//! freshly written loader. A machine that does not declare the `efi` feature
//! skips this entirely.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::FlashError;
use crate::filesystem::MountOptions;
use crate::mode::flash::with_mount;
use crate::partition::layout::PARTITION_NUM_BOOT;

const EFIBOOTMGR_CMD: &str = "/sbin/efibootmgr";
const EFI_MACHINE_FEATURE: &str = "efi";
const EFI_BOOT_ENTRY_LABEL: &str = "omnect_os";
const EFI_LOADER_PATH: &str = r"\EFI\BOOT\bootx64.efi";
const EFI_ENTRY_DUMP_FILE: &str = "EFI/BOOT/efibootmgr_entry";

/// Scratch mount point for the boot partition while EFI entries are written.
const EFI_MOUNT_POINT: &str = "/tmp/boot";

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

/// Whether `machine_features` declares the `efi` feature.
///
/// Tokenises on whitespace and compares tokens exactly, so a feature such as
/// `efirtc` does not turn EFI handling on.
pub fn has_efi(machine_features: &str) -> bool {
    machine_features
        .split_ascii_whitespace()
        .any(|feature| feature == EFI_MACHINE_FEATURE)
}

/// The `efibootmgr` argument vector that creates the omnect boot entry on
/// `target_disk`.
pub fn entry_args(target_disk: &Path) -> Vec<String> {
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

/// Run `efibootmgr` with `args`, returning stdout on success.
fn run_efibootmgr(args: &[String]) -> Result<String, FlashError> {
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

/// Delete every EFI boot entry, omnect or not — a full rebuild is simpler
/// than reconciling stale entries left by a previous flash or OS.
fn delete_existing_entries() -> Result<(), FlashError> {
    let listing = run_efibootmgr(&[])?;
    for id in boot_entry_ids(&listing) {
        run_efibootmgr(&[
            EFIBOOTMGR_BOOTNUM_FLAG.to_string(),
            id,
            EFIBOOTMGR_DELETE_FLAG.to_string(),
        ])?;
    }
    Ok(())
}

/// Create the omnect boot entry and record the resulting `efibootmgr -v`
/// listing on the boot partition mounted at `boot_mount`.
fn write_boot_entry(target_disk: &Path, boot_mount: &Path) -> Result<(), FlashError> {
    run_efibootmgr(&entry_args(target_disk))?;

    let dump = run_efibootmgr(&[EFIBOOTMGR_VERBOSE_FLAG.to_string()])?;
    let dump_path = boot_mount.join(EFI_ENTRY_DUMP_FILE);
    if let Some(parent) = dump_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dump_path, dump)?;

    Ok(())
}

/// Rebuild the EFI boot entry to point at a freshly flashed `target_disk`.
///
/// A no-op when `machine_features` does not declare `efi`.
pub fn handle(
    target_disk: &Path,
    boot_partition: &Path,
    machine_features: &str,
) -> Result<(), FlashError> {
    if !has_efi(machine_features) {
        return Ok(());
    }

    delete_existing_entries()?;

    with_mount(
        boot_partition,
        Path::new(EFI_MOUNT_POINT),
        MountOptions::vfat(),
        |boot_mount| write_boot_entry(target_disk, boot_mount),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn efi_handling_applies_only_where_the_machine_declares_efi() {
        assert!(has_efi("efi"));
        assert!(has_efi("usbhost efi vfat"));
        assert!(!has_efi(""));
        assert!(!has_efi("usbhost vfat"));
        // A substring of another feature must not enable it.
        assert!(!has_efi("efirtc"));
        assert!(!has_efi("no-efi-here"));
    }

    #[test]
    fn boot_entry_targets_the_first_partition_of_the_target_disk() {
        assert_eq!(
            entry_args(Path::new("/dev/mmcblk2")),
            vec![
                "-c".to_string(),
                "-d".to_string(),
                "/dev/mmcblk2".to_string(),
                "-p".to_string(),
                "1".to_string(),
                "-L".to_string(),
                "omnect_os".to_string(),
                "-l".to_string(),
                r"\EFI\BOOT\bootx64.efi".to_string(),
            ]
        );
    }

    #[test]
    fn boot_entry_ids_lists_every_entry_active_or_not() {
        let listing = "\
BootCurrent: 0002
BootNext: 0001
Timeout: 1 seconds
BootOrder: 0002,0000,0001
Boot0000* Windows Boot Manager
Boot0001  UEFI: Built-in EFI Shell
Boot0002* omnect_os
";
        assert_eq!(
            boot_entry_ids(listing),
            vec!["0000".to_string(), "0001".to_string(), "0002".to_string()]
        );
    }
}
