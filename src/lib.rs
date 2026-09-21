//! omnect-os-init library
//!
//! This library provides the core functionality for the omnect-os init process.
//! It replaces the bash-based initramfs scripts with a type-safe Rust implementation.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};

use crate::{
    config::Config,
    filesystem::{mount_core_partitions, persist_fsck_results},
    mode::{BootContext, BootMode},
    partition::{PartitionLayout, create_omnect_symlinks, detect_root_device},
};

pub mod bootloader;
pub mod config;
pub mod early_init;
pub mod error;
pub mod filesystem;
pub mod init_setup;
pub mod logging;
pub mod mode;
pub mod partition;
pub mod recovery;
pub mod runtime;

// Re-export main types for convenience
#[cfg(any(test, feature = "test-utils"))]
pub use crate::bootloader::MockBootEnv;
pub use crate::bootloader::{
    BootEnv, BootEnvDecision, BootEnvState, classify_boot_env, open_boot_env,
};
pub use crate::early_init::mount_essential_filesystems;
pub use crate::error::{InitramfsError, Result};
pub use crate::logging::KmsgLogger;
pub use crate::runtime::OdsStatus;

/// `true` if `omnect_validate_update` was set in the boot env at the time
/// it was read. Default `false` so a failure before the env is opened (or in
/// degraded mode) is reported as "no update in flight".
///
/// Single writer: [`run_init`] after `apply_boot_env_decision` succeeds.
/// Single reader: `main::handle_fatal_error`.
static UPDATE_PENDING: AtomicBool = AtomicBool::new(false);

/// Mount point for the real rootfs inside the initramfs.
const ROOTFS_DIR: &str = "/rootfs";

pub fn set_update_pending(value: bool) {
    UPDATE_PENDING.store(value, Ordering::Relaxed);
}

pub fn read_update_pending() -> bool {
    UPDATE_PENDING.load(Ordering::Relaxed)
}

/// Derive the update-pending flag from a boot environment.
///
/// Returns `true` only when the env is available *and* `omnect_validate_update`
/// holds a non-empty value; all other cases (degraded env, read error, key
/// absent or empty) return `false` so failures before the env is opened are
/// treated as "no update in flight".
fn update_pending_from_env(env: &BootEnvState) -> bool {
    match env.available() {
        Some(bl) => bl
            .is_flag_set(bootloader::BootEnvKey::ValidateUpdate)
            .unwrap_or_else(|e| {
                warn!("reading omnect_validate_update failed; treating as not pending: {e}");
                false
            }),
        None => false,
    }
}

/// Compute the first-boot flag from the opened boot env.
///
/// `true` unless the marker holds exactly `FIRST_BOOT_DONE`. Repeating the work it
/// gates is a no-op, skipping it can leave the device unresized, so a missing,
/// empty or unexpected value counts as a fresh first boot. A read error or a
/// degraded env yield `false`: under uncertainty, no first-boot side effects.
fn compute_first_boot(env: &bootloader::BootEnvState) -> bool {
    match env.available() {
        Some(bl) => match bl.get_env(bootloader::BootEnvKey::FirstBootDone) {
            Ok(v) => v.as_deref() != Some(bootloader::FIRST_BOOT_DONE),
            Err(e) => {
                warn!("first-boot: get_env failed: {e}; treating as not-first-boot");
                false
            }
        },
        None => false,
    }
}

/// Apply a boot env decision, enforcing the FsckRequiresReboot-wins invariant.
///
/// Persists fsck results to the bootloader environment *before* propagating
/// `core_result`, satisfying the contract documented in `mount_core_partitions`:
/// the diagnostic must be written before the error propagates, or it is lost
/// across the reboot. This is a no-op for degraded boot (`env.available_mut()`
/// returns `None`) and when `ods_status.fsck` is empty.
///
/// The records stay in `ods_status`. `drain_fsck_env` reads the env back into the
/// same map keys later in the boot, so nothing is duplicated, and a persist that
/// failed — the write errors are only logged — still reaches the ODS JSON.
fn apply_boot_env_decision(
    decision: BootEnvDecision,
    core_result: Result<()>,
    ods_status: &mut OdsStatus,
    rootfs: &Path,
) -> Result<BootEnvState> {
    match decision {
        BootEnvDecision::Continue(mut env) => {
            // Must precede core_result? — otherwise a FsckRequiresReboot error
            // propagates before the diagnostic is written to the boot env.
            persist_fsck_results(ods_status, env.available_mut(), rootfs);
            core_result?;
            if let BootEnvState::Degraded(ref e) = env {
                warn!("Boot env unavailable: {e}; booting in degraded mode");
                ods_status.set_degraded_boot(e.to_string());
            }
            Ok(env)
        }
        BootEnvDecision::Abort(err) => {
            core_result?;
            Err(err)
        }
    }
}

pub fn run_init() -> Result<()> {
    info!("omnect-os-initramfs starting");

    let config = Config::load()?;
    let rootfs = Path::new(ROOTFS_DIR);

    info!("Detecting root device...");
    let root_device = detect_root_device(&config.cmdline)?;
    info!(
        "Root device: {} (partition {})",
        root_device.base.display(),
        root_device.root_partition.display()
    );

    let layout = PartitionLayout::new(root_device)?;
    create_omnect_symlinks(&layout)?;

    let mut ods_status = OdsStatus::new();

    // Capture the result rather than propagating immediately: apply_boot_env_decision
    // must persist any fsck diagnostic held in ods_status before the error propagates.
    let core_result = mount_core_partitions(&layout, rootfs, &mut ods_status);

    // Best-effort: open the bootloader environment. The image type determines how
    // to proceed when it is unavailable — see classify_boot_env.
    //
    // Note: if mount_core_partitions returned FsckRequiresReboot, the boot partition
    // may not be mounted (GRUB), causing open_boot_env() to fail.
    // apply_boot_env_decision always propagates core_result before DegradedBoot.
    let is_release = cfg!(feature = "release-image");
    let decision = classify_boot_env(open_boot_env(), is_release);

    let mut bootloader_env =
        apply_boot_env_decision(decision, core_result, &mut ods_status, rootfs)?;

    ods_status.first_boot = compute_first_boot(&bootloader_env);
    if ods_status.first_boot {
        info!("first-boot detected (omnect_first_boot_done not set to the marker value)");
    }

    // Read omnect_validate_update once, before any subsequent fallible step.
    // Stored in a process-global so handle_fatal_error in main.rs can branch
    // on it without threading the value through every return type.
    set_update_pending(update_pending_from_env(&bootloader_env));

    {
        let ctx = init_setup::InitSetupCtx {
            layout: &layout,
            boot_env: &mut bootloader_env,
            ods_status: &mut ods_status,
            rootfs,
            update_pending: read_update_pending(),
        };
        init_setup::run(ctx)?;
    }

    let mut ctx = BootContext::new(&config, &layout, rootfs, bootloader_env, ods_status);

    match BootMode::detect(ctx.boot_env.available_mut())? {
        BootMode::Normal => mode::normal::run(ctx),
        #[cfg(feature = "factory-reset")]
        BootMode::FactoryReset(trigger) => mode::factory_reset::run(ctx, trigger),
        // A flash mode is selected but nothing dispatched it. Clear the
        // trigger so a power cycle boots normally instead of repeating the
        // same selection forever, then report why nothing happened.
        #[cfg(feature = "flash-mode")]
        BootMode::Flash(_) => {
            if let Some(bl) = ctx.boot_env.available_mut() {
                mode::clear_flash_triggers(bl);
            }
            Err(crate::error::FlashError::DispatchUnavailable.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::MockBootEnv;
    use crate::error::{BootEnvError, FilesystemError, InitramfsError};
    use crate::filesystem::FsckExitCode;
    use std::path::PathBuf;

    fn make_available() -> BootEnvDecision {
        BootEnvDecision::Continue(BootEnvState::Available(Box::new(MockBootEnv::new())))
    }

    fn make_degraded() -> BootEnvDecision {
        BootEnvDecision::Continue(BootEnvState::Degraded(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "test".into(),
        }))
    }

    fn make_abort() -> BootEnvDecision {
        BootEnvDecision::Abort(InitramfsError::DegradedBoot(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "test".into(),
        }))
    }

    fn fsck_reboot_err() -> Result<()> {
        Err(InitramfsError::Filesystem(
            FilesystemError::FsckRequiresReboot {
                device: PathBuf::from("/dev/sda1"),
                code: FsckExitCode::REBOOT_REQUIRED,
                output: String::new(),
            },
        ))
    }

    #[test]
    fn available_ok_core_returns_available_env() {
        let mut ods = OdsStatus::new();
        let result = apply_boot_env_decision(make_available(), Ok(()), &mut ods, Path::new("/tmp"));
        assert!(matches!(result, Ok(BootEnvState::Available(_))));
        assert!(ods.degraded_boot.is_none());
    }

    #[test]
    fn degraded_ok_core_sets_degraded_flag() {
        let mut ods = OdsStatus::new();
        let result = apply_boot_env_decision(make_degraded(), Ok(()), &mut ods, Path::new("/tmp"));
        assert!(matches!(result, Ok(BootEnvState::Degraded(_))));
        let degraded = ods
            .degraded_boot
            .as_ref()
            .expect("degraded_boot must be Some after Degraded continue");
        assert_eq!(
            degraded.reason, "Command 'boot-env-tool' failed: test",
            "reason must be the Display of the injected BootEnvError"
        );
    }

    #[test]
    fn fsck_reboot_wins_over_degraded_continue() {
        // FsckRequiresReboot must propagate even when bootloader is also unavailable.
        let mut ods = OdsStatus::new();
        let result = apply_boot_env_decision(
            make_degraded(),
            fsck_reboot_err(),
            &mut ods,
            Path::new("/tmp"),
        );
        assert!(
            matches!(
                result,
                Err(InitramfsError::Filesystem(
                    FilesystemError::FsckRequiresReboot { .. }
                ))
            ),
            "expected FsckRequiresReboot, not DegradedBoot"
        );
        assert!(
            ods.degraded_boot.is_none(),
            "degraded flag must not be set on reboot path"
        );
    }

    #[test]
    fn fsck_reboot_wins_over_abort() {
        // FsckRequiresReboot must propagate even when decision is Abort(DegradedBoot).
        let mut ods = OdsStatus::new();
        let result =
            apply_boot_env_decision(make_abort(), fsck_reboot_err(), &mut ods, Path::new("/tmp"));
        assert!(
            matches!(
                result,
                Err(InitramfsError::Filesystem(
                    FilesystemError::FsckRequiresReboot { .. }
                ))
            ),
            "expected FsckRequiresReboot, not DegradedBoot"
        );
        assert!(
            ods.degraded_boot.is_none(),
            "degraded flag must not be set on reboot path"
        );
    }

    #[test]
    fn persist_runs_before_fsck_reboot_propagates() {
        // Regression test for uboot: on uboot open_boot_env() is infallible,
        // so env is Available. The fsck diagnostic in ods_status.fsck must be
        // persisted to the bootloader env *before* FsckRequiresReboot propagates,
        // or it is lost across the reboot (mount_core_partitions persist-before-propagate contract).
        //
        // The env is moved into the decision and dropped when the error returns, so
        // the call log is read through a handle obtained before the move.
        let mut ods = OdsStatus::new();
        ods.record_fsck_result(
            crate::partition::PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected on pass 1".into(),
        );

        let bl = MockBootEnv::new();
        let saved = bl.saved_fsck_calls();
        let decision = BootEnvDecision::Continue(BootEnvState::Available(Box::new(bl)));
        let result =
            apply_boot_env_decision(decision, fsck_reboot_err(), &mut ods, Path::new("/tmp"));

        assert!(
            matches!(
                result,
                Err(InitramfsError::Filesystem(
                    FilesystemError::FsckRequiresReboot { .. }
                ))
            ),
            "FsckRequiresReboot must still propagate"
        );
        let saved = saved.lock().unwrap();
        assert_eq!(
            saved.iter().map(|(p, _, _)| *p).collect::<Vec<_>>(),
            vec![crate::partition::PartitionName::Boot],
            "persist_fsck_results must run before propagating FsckRequiresReboot \
             (mount_core_partitions persist-before-propagate contract)"
        );
    }

    #[test]
    fn persisted_records_stay_in_ods_status() {
        // drain_fsck_env reads the env back into the same map keys, so keeping the
        // records costs nothing — and a persist whose write failed (errors are only
        // logged) still reaches the ODS JSON.
        let mut ods = OdsStatus::new();
        ods.record_fsck_result(
            crate::partition::PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected on pass 1".into(),
        );

        let result = apply_boot_env_decision(make_available(), Ok(()), &mut ods, Path::new("/tmp"));

        assert!(result.is_ok());
        assert_eq!(ods.fsck.len(), 1, "records must survive the persist step");
    }

    #[test]
    fn update_pending_accessor_roundtrips() {
        // Exercises the public set/read accessors. The chosen default (false) matters so
        // failures before the env is opened report the safe "no update in flight".
        set_update_pending(true);
        assert!(read_update_pending());
        set_update_pending(false);
        assert!(!read_update_pending());
    }

    #[test]
    fn update_pending_false_when_degraded() {
        let env = BootEnvState::Degraded(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "not found".into(),
        });
        assert!(!update_pending_from_env(&env));
    }

    #[test]
    fn update_pending_false_when_key_absent() {
        let env = BootEnvState::Available(Box::new(MockBootEnv::new()));
        assert!(!update_pending_from_env(&env));
    }

    #[test]
    fn update_pending_true_when_key_set() {
        let bl = MockBootEnv::new().with_env(crate::bootloader::BootEnvKey::ValidateUpdate, "1");
        let env = BootEnvState::Available(Box::new(bl));
        assert!(update_pending_from_env(&env));
    }

    #[test]
    fn update_pending_false_when_key_is_empty() {
        // GRUB's rollback path assigns `omnect_validate_update=` and saves it, so
        // the entry survives with an empty value. Treating that as pending would
        // keep a rolled-back device on the reboot-on-fatal path forever.
        let bl = MockBootEnv::new().with_env(crate::bootloader::BootEnvKey::ValidateUpdate, "");
        let env = BootEnvState::Available(Box::new(bl));
        assert!(!update_pending_from_env(&env));
    }

    #[test]
    fn update_pending_false_when_get_env_errors() {
        let bl = MockBootEnv::new().with_get_env_error();
        let env = BootEnvState::Available(Box::new(bl));
        assert!(!update_pending_from_env(&env));
    }
}

#[cfg(test)]
mod first_boot_detection_tests {
    use super::*;
    use crate::bootloader::{BootEnvKey, BootEnvState, MockBootEnv};
    use crate::error::BootEnvError;

    #[test]
    fn marker_absent_yields_first_boot_true() {
        let env: BootEnvState = BootEnvState::Available(Box::new(MockBootEnv::new()));
        assert!(compute_first_boot(&env));
    }

    #[test]
    fn marker_present_yields_first_boot_false() {
        let mock = MockBootEnv::new().with_env(BootEnvKey::FirstBootDone, "1");
        let env: BootEnvState = BootEnvState::Available(Box::new(mock));
        assert!(!compute_first_boot(&env));
    }

    #[test]
    fn empty_marker_yields_first_boot_true() {
        // GRUB keeps an entry whose value was assigned empty; that is not the
        // marker we wrote, so the first-boot work runs again.
        let mock = MockBootEnv::new().with_env(BootEnvKey::FirstBootDone, "");
        let env: BootEnvState = BootEnvState::Available(Box::new(mock));
        assert!(compute_first_boot(&env));
    }

    #[test]
    fn unexpected_marker_value_yields_first_boot_true() {
        let mock = MockBootEnv::new().with_env(BootEnvKey::FirstBootDone, "yes");
        let env: BootEnvState = BootEnvState::Available(Box::new(mock));
        assert!(compute_first_boot(&env));
    }

    #[test]
    fn degraded_env_yields_first_boot_false() {
        // Degraded default: don't trigger first-boot side effects
        // (cloud registration etc.) under uncertainty.
        let env = BootEnvState::Degraded(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "test".into(),
        });
        assert!(!compute_first_boot(&env));
    }

    #[test]
    fn get_env_error_yields_first_boot_false() {
        // Conservative: I/O or parse errors must not trigger first-boot
        // side effects (treat as not-first-boot under uncertainty).
        let mock = MockBootEnv::new().with_get_env_error();
        let env = BootEnvState::Available(Box::new(mock));
        assert!(!compute_first_boot(&env));
    }
}
