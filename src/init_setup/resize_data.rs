//! Init setup step: data partition auto-resize
//!
//! Runs on the first boot (when `OdsStatus.first_boot` is `true`), expanding
//! the data partition via `filesystem::resize_data`.
//!
//! On a degraded boot (boot env unavailable): runs resize regardless, since
//! the first-boot flag defaults to `false` and the degraded arm runs
//! unconditionally. Only reached on release-images; debug-images abort in
//! lib.rs before init setup executes.
//!
//! All `ResizeData` failure modes except `FsckRequiresReboot` are absorbed as
//! best-effort: `OdsStatus.resize_data` is populated and boot continues.
//! `FsckRequiresReboot` is propagated so the recovery policy in `error.rs`
//! triggers a controlled reboot. Non-resize errors (e.g. a transient
//! `get_env` failure) are also propagated to preserve their recovery class.

use crate::error::{FilesystemError, InitramfsError, ResizeDataError, Result};
use crate::init_setup::InitSetupCtx;
use crate::runtime::{OdsStatus, ResizeOutcome, ResizeStatus};

pub fn run(ctx: &mut InitSetupCtx<'_, '_, '_, '_>) -> Result<()> {
    handle_result(attempt(ctx), ctx.ods_status)
}

fn handle_result(result: Result<()>, ods_status: &mut OdsStatus) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        // FsckRequiresReboot: a reboot to fix the partition is the correct remedy.
        Err(
            e @ InitramfsError::ResizeData(ResizeDataError::Filesystem(
                FilesystemError::FsckRequiresReboot { .. },
            )),
        ) => Err(e),
        // Absorb only ResizeData errors — non-resize errors (e.g. Bootloader
        // from a transient get_env failure) propagate to preserve their
        // recovery class.
        Err(InitramfsError::ResizeData(inner)) => {
            let reason = inner.to_string();
            let outcome = outcome_for(&inner);
            log::warn!("resize-data init setup skipped: {reason}");
            ods_status.set_resize_status(ResizeStatus { outcome, reason });
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn attempt(ctx: &mut InitSetupCtx<'_, '_, '_, '_>) -> Result<()> {
    // Use the already-computed flag rather than re-reading the boot env.
    // A redundant get_env call here could brick on a transient error
    // (Bootloader → Fatal) while compute_first_boot already rode through it.
    let first_boot = ctx.ods_status.first_boot;
    match ctx.boot_env.available_mut() {
        Some(_) => {
            if !first_boot {
                log::debug!("resize-data init setup: not first boot; skipping resize");
                return Ok(());
            }
            crate::filesystem::resize_data::resize_if_needed(ctx.layout)
        }
        None => {
            log::warn!("resize-data: running without bootloader guard (degraded boot)");
            crate::filesystem::resize_data::resize_if_needed(ctx.layout)
        }
    }
}

fn outcome_for(e: &ResizeDataError) -> ResizeOutcome {
    match e {
        ResizeDataError::Filesystem(FilesystemError::FsckFailed { .. }) => {
            ResizeOutcome::SkippedFsck
        }
        ResizeDataError::InvalidDevicePath(_)
        | ResizeDataError::ExtendedPartitionNotFound(_)
        | ResizeDataError::NonUtf8Path(_) => ResizeOutcome::InvalidLayout,
        ResizeDataError::CommandFailed { .. } | ResizeDataError::Io(_) => ResizeOutcome::ToolError,
        // Any other FilesystemError (e.g. MountFailed) is a tool-level failure;
        // FsckRequiresReboot never reaches here — propagated by handle_result.
        ResizeDataError::Filesystem(_) => ResizeOutcome::ToolError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::{BootEnvState, MockBootEnv};
    use crate::error::BootEnvError;
    use crate::init_setup::InitSetupCtx;
    use crate::partition::{PartitionLayout, RootDevice};
    use crate::runtime::OdsStatus;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn empty_layout() -> PartitionLayout {
        PartitionLayout {
            partitions: HashMap::new(),
            device: RootDevice {
                base: PathBuf::from("/dev/sda"),
                partition_sep: "",
                root_partition: PathBuf::from("/dev/sda2"),
            },
        }
    }

    fn layout_with_data() -> PartitionLayout {
        let mut partitions = HashMap::new();
        partitions.insert(
            crate::partition::PartitionName::Data,
            PathBuf::from("/dev/sda8"),
        );
        PartitionLayout {
            partitions,
            device: RootDevice {
                base: PathBuf::from("/dev/sda"),
                partition_sep: "",
                root_partition: PathBuf::from("/dev/sda2"),
            },
        }
    }

    #[test]
    fn skips_when_not_first_boot() {
        // layout_with_data: if the skip check is bypassed, resize_if_needed
        // will attempt to spawn sgdisk/parted (not in test env) and return Err.
        let layout = layout_with_data();
        let mut env = BootEnvState::Available(Box::new(MockBootEnv::new()));
        let mut ods = OdsStatus::new();
        ods.first_boot = false; // not first boot → skip resize
        let mut ctx = InitSetupCtx {
            layout: &layout,
            boot_env: &mut env,
            ods_status: &mut ods,
            rootfs: Path::new("/rootfs"),
        };
        assert!(run(&mut ctx).is_ok());
        assert!(
            ods.resize_data.is_none(),
            "non-first-boot path must not record a resize_data status"
        );
    }

    #[test]
    fn first_boot_with_available_env_attempts_resize() {
        // first_boot = true + Available env + data partition present.
        // resize_if_needed will fail (no real tools in test env); the error is
        // absorbed by handle_result and recorded in ods_status.resize_data.
        // This confirms the Some(_) arm does NOT skip on first boot.
        let layout = layout_with_data();
        let mut env = BootEnvState::Available(Box::new(MockBootEnv::new()));
        let mut ods = OdsStatus::new();
        ods.first_boot = true;
        let mut ctx = InitSetupCtx {
            layout: &layout,
            boot_env: &mut env,
            ods_status: &mut ods,
            rootfs: Path::new("/rootfs"),
        };
        let result = run(&mut ctx);
        assert!(
            result.is_ok(),
            "tool errors must be absorbed; got: {result:?}"
        );
        assert!(
            ctx.ods_status.resize_data.is_some(),
            "ods_status.resize_data must be set after a tool error on first boot"
        );
    }

    #[test]
    fn degraded_env_runs_resize_without_guard() {
        // Uses empty_layout: resize_if_needed returns Ok immediately (no data
        // partition), so no real sgdisk/parted is invoked. The purpose of this
        // test is to verify the Degraded arm is dispatched correctly — not to
        // test the resize commands themselves (those need real disk tooling and
        // are not run in unit tests).
        let layout = empty_layout();
        let mut env = BootEnvState::Degraded(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "test".into(),
        });
        let mut ods = OdsStatus::new();
        let mut ctx = InitSetupCtx {
            layout: &layout,
            boot_env: &mut env,
            ods_status: &mut ods,
            rootfs: Path::new("/rootfs"),
        };
        assert!(run(&mut ctx).is_ok());
    }

    #[test]
    fn degraded_env_with_data_layout_absorbs_tool_error() {
        // Uses layout_with_data() + BootEnvState::Degraded. resize_if_needed
        // is called with None boot_env (degraded arm). With a real device path
        // (/dev/sda8) the resize commands would fail — plan-b absorbs the
        // error and returns Ok, recording the indicator in ods_status.
        let layout = layout_with_data();
        let mut env = BootEnvState::Degraded(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "test".into(),
        });
        let mut ods = OdsStatus::new();
        let mut ctx = InitSetupCtx {
            layout: &layout,
            boot_env: &mut env,
            ods_status: &mut ods,
            rootfs: Path::new("/rootfs"),
        };
        // With a data partition present and no real tooling, resize_if_needed
        // will fail. The absorb logic turns the error into Ok + indicator.
        let result = run(&mut ctx);
        assert!(
            result.is_ok(),
            "tool errors must be absorbed; got: {result:?}"
        );
        assert!(
            ctx.ods_status.resize_data.is_some(),
            "ods_status.resize_data must be set after a tool error"
        );
    }

    #[test]
    fn fsck_requires_reboot_propagates_through_handle_result() {
        use crate::error::{FilesystemError, InitramfsError, ResizeDataError};
        use crate::filesystem::FsckExitCode;
        // Calls handle_result() directly so the test covers the run()-level
        // decision logic without needing a real disk.
        let err = InitramfsError::ResizeData(ResizeDataError::Filesystem(
            FilesystemError::FsckRequiresReboot {
                device: std::path::PathBuf::from("/dev/sda5"),
                code: FsckExitCode::REBOOT_REQUIRED,
                output: String::new(),
            },
        ));
        let mut ods = OdsStatus::new();
        let result = handle_result(Err(err), &mut ods);
        assert!(result.is_err(), "FsckRequiresReboot must propagate as Err");
        assert!(
            ods.resize_data.is_none(),
            "FsckRequiresReboot must not record a resize_data indicator"
        );
    }

    #[test]
    fn non_resize_data_error_propagates() {
        use crate::error::{BootEnvError, InitramfsError};
        // A transient get_env failure produces InitramfsError::Bootloader, which
        // must propagate so its Fatal recovery class is preserved.
        let err = InitramfsError::Bootloader(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "transient failure".into(),
        });
        let mut ods = OdsStatus::new();
        let result = handle_result(Err(err), &mut ods);
        assert!(
            result.is_err(),
            "non-ResizeData errors must propagate as Err"
        );
        assert!(
            ods.resize_data.is_none(),
            "non-ResizeData errors must not set a resize_data indicator"
        );
    }

    #[test]
    fn invalid_layout_error_maps_to_invalid_layout_outcome() {
        use crate::error::ResizeDataError;
        let e = ResizeDataError::InvalidDevicePath(std::path::PathBuf::from("/dev/sda"));
        assert_eq!(outcome_for(&e), ResizeOutcome::InvalidLayout);
    }

    #[test]
    fn fsck_failed_maps_to_skipped_fsck_outcome() {
        use crate::error::{FilesystemError, ResizeDataError};
        use crate::filesystem::FsckExitCode;
        let e = ResizeDataError::Filesystem(FilesystemError::FsckFailed {
            device: std::path::PathBuf::from("/dev/sda5"),
            code: FsckExitCode::ERRORS_UNCORRECTED,
            output: "some errors".into(),
        });
        assert_eq!(outcome_for(&e), ResizeOutcome::SkippedFsck);
    }

    #[test]
    fn command_failed_maps_to_tool_error_outcome() {
        use crate::error::ResizeDataError;
        let e = ResizeDataError::CommandFailed {
            command: "resize2fs".into(),
            code: 1,
            output: "failed".into(),
        };
        assert_eq!(outcome_for(&e), ResizeOutcome::ToolError);
    }

    #[test]
    fn io_error_maps_to_tool_error_outcome() {
        use crate::error::ResizeDataError;
        let e = ResizeDataError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(outcome_for(&e), ResizeOutcome::ToolError);
    }
}
