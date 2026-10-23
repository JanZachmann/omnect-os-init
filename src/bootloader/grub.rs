//! GRUB boot environment implementation
//!
//! This module provides access to GRUB bootloader environment variables
//! using the `grub-editenv` command.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::bootloader::{
    BootEnv, BootEnvKey, FsckRecord, Result, sync_filesystems,
    types::{decode_fsck_output, encode_fsck_output},
};
use crate::error::BootEnvError;
use crate::filesystem::FsckExitCode;
use crate::partition::PartitionName;

/// Command name for GRUB environment manipulation
const GRUB_EDITENV_CMD: &str = "/bin/grub-editenv";

/// Path to the boot partition mount point
const BOOT_DIR_PATH: &str = "/rootfs/boot";

/// Absolute path to the grubenv file
const GRUBENV_PATH: &str = "/rootfs/boot/EFI/BOOT/grubenv";

/// Replaces the fsck output when the encoded record does not fit the grubenv
/// block. Kept short on purpose — it shares the block with every other variable.
const FSCK_OUTPUT_TOO_LARGE: &str = "fsck output too large for the boot env";

/// Constructs the fsck status file path for a non-boot partition on the boot volume.
fn fsck_file_path(partition: PartitionName) -> std::path::PathBuf {
    Path::new(BOOT_DIR_PATH).join(format!("fsck.{partition}"))
}

/// Where a partition's fsck record lives. One mapping shared by
/// `save_fsck_status`, `get_fsck_status` and `clear_fsck_status`, so the
/// partition-to-channel rule can't diverge between them.
enum FsckChannel {
    /// The boot partition's own record: grubenv, via `BootEnvKey::FsckStatus`.
    Env,
    /// Every other partition: a file on the boot partition — grubenv is a
    /// small, fixed-size block that storing multiple large blobs would overflow.
    File,
}

impl FsckChannel {
    fn for_partition(partition: PartitionName) -> Self {
        match partition {
            PartitionName::Boot => Self::Env,
            PartitionName::RootA
            | PartitionName::RootB
            | PartitionName::RootCurrent
            | PartitionName::Factory
            | PartitionName::Cert
            | PartitionName::Etc
            | PartitionName::Data => Self::File,
            #[cfg(feature = "dos")]
            PartitionName::Extended => Self::File,
        }
    }
}

/// Maps an I/O failure on `file_path` into a `BootEnvError`, naming the action
/// (`"read"`, `"write"`, `"remove"`) for diagnosis.
fn io_err(action: &str, file_path: &Path, e: std::io::Error) -> BootEnvError {
    BootEnvError::CommandFailed {
        command: format!("{action} {}", file_path.display()),
        reason: e.to_string(),
    }
}

/// Store the boot partition's encoded fsck record, retrying with the exit code
/// alone when the full record does not fit.
///
/// grubenv is a fixed-size block shared by all variables, and a verbose
/// `fsck.vfat` run can exceed it. Keeping the code without the output leaves the
/// result visible in the ODS JSON instead of dropping the record entirely.
fn write_boot_fsck_record<W>(
    code: FsckExitCode,
    encoded: &str,
    mut write: W,
) -> crate::bootloader::Result<()>
where
    W: FnMut(&str) -> crate::bootloader::Result<()>,
{
    match write(encoded) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("boot fsck record rejected by grubenv ({e}); storing exit code only");
            write(&encode_fsck_output(code.bits(), FSCK_OUTPUT_TOO_LARGE))
        }
    }
}

fn save_fsck_to_file(partition: PartitionName, encoded: &str) -> crate::bootloader::Result<()> {
    let file_path = fsck_file_path(partition);
    fs::write(&file_path, encoded).map_err(|e| io_err("write", &file_path, e))?;
    sync_filesystems();
    Ok(())
}

fn get_fsck_from_file(partition: PartitionName) -> crate::bootloader::Result<Option<FsckRecord>> {
    let file_path = fsck_file_path(partition);
    if !file_path.is_file() {
        return Ok(None);
    }
    let encoded = fs::read_to_string(&file_path).map_err(|e| io_err("read", &file_path, e))?;
    // Remove file after reading; each fsck result is consumed once.
    if let Err(e) = fs::remove_file(&file_path) {
        log::warn!(
            "Failed to remove fsck status file {}: {}",
            file_path.display(),
            e
        );
    }
    Ok(decode_fsck_output(&encoded))
}

fn clear_fsck_file(partition: PartitionName) -> crate::bootloader::Result<()> {
    let file_path = fsck_file_path(partition);
    if file_path.exists() {
        fs::remove_file(&file_path).map_err(|e| io_err("remove", &file_path, e))?;
        sync_filesystems();
    }
    Ok(())
}

/// GRUB boot environment implementation
///
/// Uses `grub-editenv` to read/write environment variables from the grubenv file.
pub struct GrubBootEnv;

impl GrubBootEnv {
    /// Create a new GRUB boot environment accessor.
    ///
    /// # Errors
    /// Returns an error if the grubenv file doesn't exist (indicates a corrupted
    /// boot partition, not a missing file on first boot).
    pub fn new() -> Result<Self> {
        if !Path::new(GRUBENV_PATH).is_file() {
            return Err(BootEnvError::EnvFileNotFound {
                path: GRUBENV_PATH.into(),
            });
        }

        Ok(Self)
    }

    /// Run grub-editenv with the given arguments
    fn run_grub_editenv(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(GRUB_EDITENV_CMD)
            .arg(GRUBENV_PATH)
            .args(args)
            .output()
            .map_err(|e| BootEnvError::CommandFailed {
                command: GRUB_EDITENV_CMD.to_string(),
                reason: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(BootEnvError::CommandExitCode {
                command: GRUB_EDITENV_CMD.to_string(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

impl BootEnv for GrubBootEnv {
    fn get_env(&self, key: BootEnvKey) -> Result<Option<String>> {
        let key_str = key.as_str();
        let output = self.run_grub_editenv(&["list"])?;

        for line in output.lines() {
            if let Some((k, v)) = line.split_once('=')
                && k == key_str.as_ref()
            {
                return Ok(Some(v.to_string()));
            }
        }

        Ok(None)
    }

    fn set_env(&mut self, key: BootEnvKey, value: Option<&str>) -> Result<()> {
        let key_str = key.as_str();
        match value {
            Some(v) => {
                let assignment = format!("{}={}", key_str, v);
                self.run_grub_editenv(&["set", &assignment])?;
            }
            None => {
                self.run_grub_editenv(&["unset", key_str.as_ref()])?;
            }
        }
        // grub-editenv leaves the write in the page cache.
        sync_filesystems();
        Ok(())
    }

    fn save_fsck_status(
        &mut self,
        partition: PartitionName,
        code: FsckExitCode,
        output: &str,
    ) -> Result<()> {
        let encoded = encode_fsck_output(code.bits(), output);

        match FsckChannel::for_partition(partition) {
            FsckChannel::Env => write_boot_fsck_record(code, &encoded, |payload| {
                self.set_env(BootEnvKey::FsckStatus(partition), Some(payload))
            }),
            FsckChannel::File => save_fsck_to_file(partition, &encoded),
        }
    }

    fn get_fsck_status(&self, partition: PartitionName) -> Result<Option<FsckRecord>> {
        match FsckChannel::for_partition(partition) {
            FsckChannel::Env => Ok(self
                .get_env(BootEnvKey::FsckStatus(partition))?
                .and_then(|v| decode_fsck_output(&v))),
            FsckChannel::File => get_fsck_from_file(partition),
        }
    }

    fn clear_fsck_status(&mut self, partition: PartitionName) -> Result<()> {
        match FsckChannel::for_partition(partition) {
            FsckChannel::Env => self.set_env(BootEnvKey::FsckStatus(partition), None),
            FsckChannel::File => clear_fsck_file(partition),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_boot_uses_the_env_channel() {
        assert!(matches!(
            FsckChannel::for_partition(PartitionName::Boot),
            FsckChannel::Env
        ));
        for partition in [
            PartitionName::RootA,
            PartitionName::RootB,
            PartitionName::RootCurrent,
            PartitionName::Factory,
            PartitionName::Cert,
            PartitionName::Etc,
            PartitionName::Data,
        ] {
            assert!(
                matches!(FsckChannel::for_partition(partition), FsckChannel::File),
                "{partition} must use the file channel"
            );
        }
    }

    #[cfg(feature = "dos")]
    #[test]
    fn extended_uses_the_file_channel() {
        assert!(matches!(
            FsckChannel::for_partition(PartitionName::Extended),
            FsckChannel::File
        ));
    }

    fn block_too_small() -> BootEnvError {
        BootEnvError::CommandFailed {
            command: "grub-editenv".into(),
            reason: "environment block too small".into(),
        }
    }

    #[test]
    fn full_record_is_written_once_when_it_fits() {
        let mut seen: Vec<String> = Vec::new();

        let result = write_boot_fsck_record(FsckExitCode::CORRECTED, "encoded", |payload| {
            seen.push(payload.to_string());
            Ok(())
        });

        assert!(result.is_ok());
        assert_eq!(seen, ["encoded"]);
    }

    #[test]
    fn a_record_with_the_reboot_bit_is_written() {
        let mut seen: Vec<String> = Vec::new();

        let result = write_boot_fsck_record(FsckExitCode::from(2), "encoded", |payload| {
            seen.push(payload.to_string());
            Ok(())
        });

        assert!(result.is_ok());
        assert_eq!(seen, ["encoded"]);
    }

    #[test]
    fn rejected_record_falls_back_to_the_exit_code_alone() {
        let code = FsckExitCode::CORRECTED;
        let mut seen: Vec<String> = Vec::new();

        let result = write_boot_fsck_record(code, "too-long", |payload| {
            seen.push(payload.to_string());
            if seen.len() == 1 {
                Err(block_too_small())
            } else {
                Ok(())
            }
        });

        assert!(result.is_ok(), "the fallback write must succeed");
        assert_eq!(seen.len(), 2, "expected one retry, got {seen:?}");
        assert_eq!(seen[0], "too-long");
        assert_eq!(
            seen[1],
            encode_fsck_output(code.bits(), FSCK_OUTPUT_TOO_LARGE),
            "the retry must carry the exit code with the placeholder output"
        );
    }

    #[test]
    fn a_broken_env_still_surfaces_as_an_error() {
        // Both attempts failing means grubenv itself is unusable, not just full.
        let mut seen: Vec<String> = Vec::new();

        let result =
            write_boot_fsck_record(FsckExitCode::ERRORS_UNCORRECTED, "payload", |payload| {
                seen.push(payload.to_string());
                Err(block_too_small())
            });

        assert!(result.is_err());
        assert_eq!(seen.len(), 2, "must not retry more than once");
    }
}
