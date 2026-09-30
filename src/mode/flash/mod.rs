//! Flash modes: deploy a whole disk image from the initramfs, before any
//! rootfs is handed control.

#[cfg(feature = "flash-mode-1")]
pub(crate) mod clone;
pub mod config;
#[cfg(feature = "grub")]
pub(crate) mod efi;
#[cfg(feature = "flash-mode-1")]
pub(crate) mod rawio;
#[cfg(feature = "flash-mode-1")]
pub(crate) mod sfdisk;
#[cfg(feature = "flash-mode")]
pub(crate) mod unmount;

use std::fs;
use std::path::Path;

use nix::sys::reboot::{RebootMode, reboot};

#[cfg(feature = "flash-mode-1")]
use crate::bootloader::BootEnvKey;
use crate::bootloader::sync_filesystems;
use crate::error::FlashError;
use crate::filesystem::{MountOptions, MountPoint, mount, umount};
use crate::logging::{start_capture, take_capture};
use crate::mode::{BootContext, clear_flash_triggers};
use crate::partition::{PartitionLayout, PartitionName};

/// Scratch mount points. They sit outside the rootfs mount, which mode 1
/// unmounts before it writes anything.
pub(crate) mod scratch_mounts {
    /// The source data partition, while the run log is written.
    pub(crate) const LOG_DATA: &str = "/tmp/flash-log-data";
    /// The destination boot partition, while the default GRUB environment is
    /// written.
    #[cfg(all(feature = "flash-mode-1", feature = "grub"))]
    pub(crate) const CLONE_BOOT: &str = "/tmp/clone-boot";
    /// The target boot partition, while the EFI entry dump is written.
    #[cfg(feature = "grub")]
    pub(crate) const EFI_BOOT: &str = "/tmp/boot";
}
#[cfg(feature = "flash-mode-1")]
const MODE_1_LOG_FILE: &str = "flash-mode-1.log";

/// Writes the run log: the data partition, the file name, the captured lines.
type LogWriter<'a> = &'a mut dyn FnMut(&Path, &str, &[String]) -> Result<(), FlashError>;

fn log_file(mode: config::FlashMode) -> &'static str {
    match mode {
        #[cfg(feature = "flash-mode-1")]
        config::FlashMode::Mode1 => MODE_1_LOG_FILE,
    }
}

fn log_contents(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// Mount `source` at `target`, run `work` on the mount point, and unmount it
/// again on success and on error. An unmount failure is returned only when
/// `work` succeeded; otherwise the `work` error wins and the unmount is logged.
pub(crate) fn with_mount<T>(
    source: &Path,
    target: &Path,
    options: MountOptions,
    work: impl FnOnce(&Path) -> Result<T, FlashError>,
) -> Result<T, FlashError> {
    fs::create_dir_all(target).map_err(|source| FlashError::PathIo {
        path: target.to_path_buf(),
        source,
    })?;
    mount(MountPoint::new(source, target, options))?;

    let result = work(target);
    if let Err(e) = umount(target) {
        if result.is_ok() {
            return Err(e.into());
        }
        log::warn!(
            "also failed to unmount {} after an error: {e}",
            target.display()
        );
    }
    result
}

/// The destination mode 1 was given.
#[cfg(feature = "flash-mode-1")]
fn destination(flash_config: &config::FlashConfig) -> Result<&Path, FlashError> {
    let invalid = |reason: String| FlashError::InvalidEnvValue {
        key: BootEnvKey::FlashModeDevPath,
        reason,
    };
    match &flash_config.devpath {
        config::Devpath::Set(path) => Ok(path),
        config::Devpath::NotSet => Err(invalid("not set".to_string())),
        config::Devpath::Unreadable(e) => Err(invalid(format!("failed to read env: {e}"))),
    }
}

/// Write the captured log onto the source data partition.
fn write_log(data_partition: &Path, file: &str, lines: &[String]) -> Result<(), FlashError> {
    with_mount(
        data_partition,
        Path::new(scratch_mounts::LOG_DATA),
        MountOptions::ext4_readwrite(),
        |mount_point| {
            let path = mount_point.join(file);
            fs::write(&path, log_contents(lines))
                .map_err(|source| FlashError::PathIo { path, source })
        },
    )
}

/// Persist the run log, best-effort: losing it must not change the outcome.
fn persist_log(layout: &PartitionLayout, file: &str, lines: &[String], write: LogWriter<'_>) {
    // A run always logs its own outcome, so an empty capture means the capture
    // itself was lost.
    if lines.is_empty() {
        log::warn!("flash mode: nothing was captured; no run log is written");
        return;
    }

    let Some(data_partition) = layout.get(PartitionName::Data) else {
        log::warn!("flash mode: the source layout has no data partition; the run log is not kept");
        return;
    };
    if let Err(e) = write(data_partition, file, lines) {
        log::warn!("flash mode: failed to write the run log to the source disk: {e}");
    }
}

fn run_selected_mode(
    flash_config: &config::FlashConfig,
    ctx: &BootContext<'_>,
) -> Result<(), FlashError> {
    match flash_config.mode {
        #[cfg(feature = "flash-mode-1")]
        config::FlashMode::Mode1 => clone::run_clone(&clone::CloneCtx {
            destination: destination(flash_config)?,
            layout: ctx.layout,
            rootfs: ctx.rootfs,
        }),
    }
}

/// Clear the triggers, run the mode and persist its log, on success and on
/// error.
fn run_and_persist(
    ctx: &mut BootContext<'_>,
    flash_config: &config::FlashConfig,
    write: LogWriter<'_>,
) -> Result<(), FlashError> {
    // First, so a failed trigger clear reaches the run log too. kmsg is gone
    // after the power off, so the file on the source disk is the only record
    // of the run.
    start_capture();

    if let Some(bl) = ctx.boot_env.available_mut() {
        clear_flash_triggers(bl);
    }

    let outcome = run_selected_mode(flash_config, ctx);
    match &outcome {
        Ok(()) => log::info!("flash mode finished"),
        Err(e) => log::error!("flash mode failed: {e}"),
    }
    persist_log(
        ctx.layout,
        log_file(flash_config.mode),
        &take_capture(),
        write,
    );

    // The run log is written after the sequence's own sync. On error the
    // release image halts, and a power cycle must not lose the log.
    sync_filesystems();

    outcome
}

/// Run the selected flash mode.
///
/// The `Ok` path does not return: mode 1 ends in a power off, because it
/// leaves a clone on a second disk that an operator has to move, and a reboot
/// would boot the source disk again.
pub(crate) fn run(
    mut ctx: BootContext<'_>,
    flash_config: config::FlashConfig,
) -> crate::Result<()> {
    run_and_persist(&mut ctx, &flash_config, &mut write_log)?;

    let Err(e) = reboot(RebootMode::RB_POWER_OFF);
    Err(FlashError::Io(e.into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::{BootEnv, MockBootEnv};

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn clearing_the_triggers_unsets_the_selector_first() {
        let mut mock = MockBootEnv::new()
            .with_env(BootEnvKey::FlashMode, "1")
            .with_env(BootEnvKey::FlashModeDevPath, "/dev/does-not-exist");
        clear_flash_triggers(&mut mock);
        assert_eq!(
            mock.set_env_calls,
            vec![BootEnvKey::FlashMode, BootEnvKey::FlashModeDevPath],
            "the selector must be cleared first, so a crash mid-flash cannot re-enter"
        );
        assert_eq!(mock.get_env(BootEnvKey::FlashMode).unwrap(), None);
    }

    #[cfg(feature = "flash-mode-1")]
    fn failing_mode_1(
        layout: &PartitionLayout,
        write: LogWriter<'_>,
    ) -> (FlashError, Vec<BootEnvKey>) {
        use crate::bootloader::BootEnvState;
        use crate::config::Config;
        use crate::runtime::OdsStatus;

        let mock = MockBootEnv::new()
            .with_env(BootEnvKey::FlashMode, "1")
            .with_env(BootEnvKey::FlashModeDevPath, " ");
        let cleared = mock.shared_set_env_calls();
        let config = Config::default();
        let mut ctx = BootContext::new(
            &config,
            layout,
            Path::new("/nonexistent/rootfs"),
            BootEnvState::Available(Box::new(mock)),
            OdsStatus::default(),
        );
        let flash_config = config::FlashConfig {
            mode: config::FlashMode::Mode1,
            devpath: config::Devpath::NotSet,
        };

        // Fails before any device is touched.
        let err = run_and_persist(&mut ctx, &flash_config, write).unwrap_err();
        let cleared = cleared.lock().unwrap().clone();
        (err, cleared)
    }

    #[cfg(feature = "flash-mode-1")]
    fn source_layout() -> PartitionLayout {
        PartitionLayout::new(crate::partition::RootDevice {
            base: "/dev/sda".into(),
            partition_sep: "",
            root_partition: "/dev/sda2".into(),
        })
        .unwrap()
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn a_failing_mode_still_leaves_its_triggers_cleared() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let (err, cleared) = failing_mode_1(&source_layout(), &mut |_, _, _| Ok(()));
        assert!(
            matches!(err, FlashError::InvalidEnvValue { .. }),
            "got {err}"
        );
        assert_eq!(
            cleared,
            vec![BootEnvKey::FlashMode, BootEnvKey::FlashModeDevPath],
            "a failed run must not leave a trigger that re-enters on the next boot"
        );
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn a_failing_mode_persists_a_run_log_that_carries_the_error() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();

        let layout = source_layout();
        let mut written: Vec<(std::path::PathBuf, String, Vec<String>)> = Vec::new();
        failing_mode_1(&layout, &mut |partition, file, lines| {
            written.push((partition.to_path_buf(), file.to_string(), lines.to_vec()));
            Ok(())
        });

        let [(partition, file, lines)] = written.as_slice() else {
            panic!("the run log must be written once, got {written:?}");
        };
        assert_eq!(Some(partition), layout.get(PartitionName::Data));
        assert_eq!(file, "flash-mode-1.log");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[ERROR] flash mode failed:")
                    && line.contains("flash-mode-devpath")),
            "got {lines:?}"
        );
    }

    #[test]
    fn an_empty_capture_writes_no_run_log() {
        let layout = PartitionLayout::new(crate::partition::RootDevice {
            base: "/dev/sda".into(),
            partition_sep: "",
            root_partition: "/dev/sda2".into(),
        })
        .unwrap();
        let mut calls = 0;
        persist_log(&layout, "flash-mode-1.log", &[], &mut |_, _, _| {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 0);
    }

    #[test]
    fn the_scratch_mount_points_stay_outside_the_rootfs_mount() {
        let mount_points: &[&str] = &[
            scratch_mounts::LOG_DATA,
            #[cfg(feature = "grub")]
            scratch_mounts::EFI_BOOT,
            #[cfg(all(feature = "flash-mode-1", feature = "grub"))]
            scratch_mounts::CLONE_BOOT,
        ];
        for mount_point in mount_points {
            assert!(!Path::new(mount_point).starts_with(crate::ROOTFS_DIR));
        }
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn mode_1_writes_its_run_log_under_the_name_operators_look_for() {
        assert_eq!(log_file(config::FlashMode::Mode1), "flash-mode-1.log");
    }

    #[test]
    fn the_persisted_log_is_one_captured_line_per_line() {
        assert_eq!(
            log_contents(&["first".to_string(), "second".to_string()]),
            "first\nsecond\n"
        );
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn a_missing_destination_is_reported_with_its_reason() {
        let flash_config = config::FlashConfig {
            mode: config::FlashMode::Mode1,
            devpath: config::Devpath::NotSet,
        };
        let err = destination(&flash_config).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidEnvValue { key, reason }
                if *key == BootEnvKey::FlashModeDevPath && reason == "not set"),
            "the absent destination must be named by its env key and reason, got: {err}"
        );
    }
}
