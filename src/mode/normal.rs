use std::path::Path;

use log::info;

use crate::{
    Result,
    filesystem::{
        drain_fsck_env, mount_remaining_partitions, persist_fsck_results, setup_data_overlay,
        setup_etc_overlay, setup_raw_rootfs_mount,
    },
    mode::BootContext,
    runtime::{ODS_RUNTIME_DIR, create_fs_links, create_ods_runtime_files, switch_root},
};

#[cfg(feature = "resize-data")]
fn resize_succeeded(ods_status: &crate::runtime::OdsStatus) -> bool {
    ods_status.resize_data.is_none()
}

#[cfg(not(feature = "resize-data"))]
fn resize_succeeded(_ods_status: &crate::runtime::OdsStatus) -> bool {
    true
}

/// A failed extra-bootargs sync must not close the first-boot gate: withhold
/// the marker so `first_boot` stays true and the sync retries on the next boot.
/// `extra_bootargs` is `Some` only on failure (set only in that case).
fn extra_bootargs_applied_ok(ods_status: &crate::runtime::OdsStatus) -> bool {
    ods_status.extra_bootargs.is_none()
}

pub(crate) fn write_first_boot_marker(
    first_boot: bool,
    bootloader: &mut crate::bootloader::BootEnvState,
) {
    if first_boot
        && let Some(bl) = bootloader.available_mut()
        && let Err(e) = bl.set_first_boot_done()
    {
        log::warn!("first-boot marker write failed: {e}; will retry next boot");
    }
}

/// Run the normal boot path.
///
/// # Mode obligation: persist fsck results
///
/// This handler (and any future mode handler) is responsible for calling
/// `persist_fsck_results` after `mount_remaining_partitions`, capturing the
/// result before propagating any mount error. Skipping this call means fsck
/// diagnostics for data/factory/cert/etc are silently lost on a failed boot.
pub fn run(ctx: BootContext<'_>) -> Result<()> {
    let BootContext {
        config,
        layout,
        rootfs,
        mut boot_env,
        mut ods_status,
    } = ctx;

    let mount_result = mount_remaining_partitions(layout, rootfs, &mut ods_status);

    // Write fsck diagnostics regardless of bootloader availability so on-disk
    // log files (/data/log/fsck/) are produced even in degraded mode.
    persist_fsck_results(&ods_status, boot_env.available_mut(), rootfs);

    mount_result?;

    setup_raw_rootfs_mount(rootfs)?;
    setup_etc_overlay(rootfs)?;
    setup_data_overlay(rootfs)?;
    create_fs_links(rootfs)?;

    drain_fsck_env(&mut ods_status, boot_env.available_mut());

    create_ods_runtime_files(
        &ods_status,
        boot_env.available_mut(),
        rootfs,
        Path::new(ODS_RUNTIME_DIR),
    )?;

    write_first_boot_marker(
        ods_status.first_boot
            && resize_succeeded(&ods_status)
            && extra_bootargs_applied_ok(&ods_status),
        &mut boot_env,
    );

    info!("omnect-os-initramfs completed successfully");

    switch_root(rootfs, &config.cmdline)
}

#[cfg(test)]
mod marker_writer_tests {
    use super::*;
    use crate::bootloader::{BootEnvKey, BootEnvState, MockBootEnv};
    use crate::error::BootEnvError;

    #[test]
    fn writes_marker_when_first_boot() {
        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        write_first_boot_marker(true, &mut env);
        let bl = env.available().unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::FirstBootDone).unwrap(),
            Some("1".to_string())
        );
    }

    #[test]
    fn extra_bootargs_ok_unless_failed() {
        use crate::runtime::{ExtraBootArgsOutcome, ExtraBootArgsStatus, OdsStatus};
        let mut ods = OdsStatus::new();
        assert!(extra_bootargs_applied_ok(&ods)); // absent → ok

        ods.set_extra_bootargs_status(ExtraBootArgsStatus {
            outcome: ExtraBootArgsOutcome::SetEnvFailed,
            reason: "boom".into(),
        });
        assert!(!extra_bootargs_applied_ok(&ods)); // failure recorded → not ok
    }

    #[test]
    fn skips_marker_when_extra_bootargs_failed() {
        use crate::runtime::{ExtraBootArgsOutcome, ExtraBootArgsStatus, OdsStatus};

        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        ods.first_boot = true;
        ods.set_extra_bootargs_status(ExtraBootArgsStatus {
            outcome: ExtraBootArgsOutcome::SetEnvFailed,
            reason: "test failure".into(),
        });

        write_first_boot_marker(ods.first_boot && extra_bootargs_applied_ok(&ods), &mut env);
        let bl = env.available().unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::FirstBootDone).unwrap(),
            None,
            "marker must not be written when extra-bootargs failed"
        );
    }

    #[test]
    fn does_not_write_when_not_first_boot() {
        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        write_first_boot_marker(false, &mut env);
        let bl = env.available().unwrap();
        assert_eq!(bl.get_env(BootEnvKey::FirstBootDone).unwrap(), None);
    }

    #[test]
    fn no_op_on_degraded_env() {
        let mut env = BootEnvState::Degraded(BootEnvError::CommandFailed {
            command: "boot-env-tool".into(),
            reason: "test".into(),
        });
        // Must not panic; nothing to assert beyond completion.
        write_first_boot_marker(true, &mut env);
    }

    #[test]
    fn logs_warn_and_does_not_abort_when_set_env_fails() {
        // Exercises the log::warn! path: write failure must not abort the boot.
        let mock = MockBootEnv::new().with_set_env_error();
        let mut env = BootEnvState::Available(Box::new(mock));
        // Must not panic or return an error.
        write_first_boot_marker(true, &mut env);
    }

    #[cfg(feature = "resize-data")]
    #[test]
    fn skips_marker_when_resize_failed() {
        use crate::runtime::{OdsStatus, ResizeOutcome, ResizeStatus};

        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        ods.first_boot = true;
        ods.set_resize_status(ResizeStatus {
            outcome: ResizeOutcome::ToolError,
            reason: "test failure".into(),
        });

        write_first_boot_marker(ods.first_boot && resize_succeeded(&ods), &mut env);
        let bl = env.available().unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::FirstBootDone).unwrap(),
            None,
            "marker must not be written when resize failed"
        );
    }

    #[cfg(feature = "resize-data")]
    #[test]
    fn writes_marker_when_resize_succeeded() {
        use crate::runtime::OdsStatus;

        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        ods.first_boot = true;

        write_first_boot_marker(ods.first_boot && resize_succeeded(&ods), &mut env);
        let bl = env.available().unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::FirstBootDone).unwrap(),
            Some("1".to_string()),
            "marker must be written when resize succeeded"
        );
    }
}
