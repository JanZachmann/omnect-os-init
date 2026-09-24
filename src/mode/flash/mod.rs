//! Flash modes: deploy a whole disk image from the initramfs, before any
//! rootfs is handed control.
//!
//! The mode is selected through the bootloader environment and runs at most
//! once — the trigger is cleared before any work starts, so a crash mid-flash
//! leads to a normal boot attempt rather than an endless re-entry.

#[cfg(feature = "flash-mode-1")]
pub mod clone;
pub mod config;
pub mod efi;
#[cfg(feature = "flash-mode-1")]
pub mod rawio;
#[cfg(feature = "flash-mode-1")]
pub mod sfdisk;
#[cfg(feature = "flash-mode-1")]
pub mod unmount;

use std::fs;
use std::path::Path;

use nix::sys::reboot::{RebootMode, reboot};

use crate::bootloader::sync_filesystems;
use crate::error::FlashError;
use crate::filesystem::{MountOptions, MountPoint, mount, umount};
use crate::logging::{start_capture, take_capture};
use crate::mode::{BootContext, clear_flash_triggers};
use crate::partition::{PartitionLayout, PartitionName};

/// Scratch mount point for the source data partition while the run log is
/// written. It sits outside the rootfs mount, which mode 1 unmounts before it
/// writes anything.
const FLASH_LOG_MOUNT_POINT: &str = "/tmp/flash-log-data";
#[cfg(feature = "flash-mode-1")]
const MODE_1_LOG_FILE: &str = "flash-mode-1.log";

/// The run-log file name on the data partition, one per mode.
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
/// again on both paths. An unmount failure is returned only when `work`
/// succeeded; otherwise the `work` error wins and the unmount is logged.
pub(crate) fn with_mount<T>(
    source: &Path,
    target: &Path,
    options: MountOptions,
    work: impl FnOnce(&Path) -> Result<T, FlashError>,
) -> Result<T, FlashError> {
    fs::create_dir_all(target)?;
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
///
/// Detection reports an unusable `flash-mode-devpath` and selects the mode
/// anyway, so the absence has to become an error here.
#[cfg(feature = "flash-mode-1")]
fn destination(flash_config: &config::FlashConfig) -> Result<&Path, FlashError> {
    flash_config
        .devpath
        .as_deref()
        .ok_or_else(|| FlashError::InvalidEnvValue {
            key: config::DEVPATH_KEY,
            reason: "no destination device to clone onto".to_string(),
        })
}

/// Write the captured log onto the source data partition.
///
/// The partition is mounted here because nothing else in a flash boot mounts
/// it: `run_init` brings up only the core partitions, and mode 1 unmounts even
/// those before it starts writing.
fn write_log(data_partition: &Path, file: &str, lines: &[String]) -> Result<(), FlashError> {
    with_mount(
        data_partition,
        Path::new(FLASH_LOG_MOUNT_POINT),
        MountOptions::ext4_readwrite(),
        |mount_point| Ok(fs::write(mount_point.join(file), log_contents(lines))?),
    )
}

/// Persist the run log, best-effort.
///
/// Nothing in the clone sequence writes destructively to the source disk,
/// which makes this log the one record that survives a failed clone — but
/// losing it must not change the outcome the operator already has on kmsg.
fn persist_log(layout: &PartitionLayout, file: &str, lines: &[String]) {
    // A run always logs its own outcome, so an empty capture means the capture
    // itself was lost. Writing the empty file anyway would leave the operator
    // unable to tell that from a run that logged nothing.
    if lines.is_empty() {
        log::warn!("flash mode: nothing was captured; no run log is written");
        return;
    }

    let Some(data_partition) = layout.get(PartitionName::Data) else {
        log::warn!("flash mode: the source layout has no data partition; the run log is not kept");
        return;
    };
    if let Err(e) = write_log(data_partition, file, lines) {
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
            machine_features: &ctx.config.machine_features,
        }),
    }
}

/// Run the selected flash mode.
///
/// The `Ok` path does not return: mode 1 ends in a power off, because it
/// leaves a clone on a second disk that an operator has to move, and a reboot
/// would come back up on the source. On `Err` the caller's fatal-error path
/// takes over — a shell in the debug image, a log-and-halt loop in the release
/// image.
pub fn run(mut ctx: BootContext<'_>, flash_config: config::FlashConfig) -> crate::Result<()> {
    // First line, so a failed trigger clear reaches the persisted log too: it
    // means the mode may re-enter on the next boot, which the operator has to
    // learn from the post-mortem. kmsg is gone after the power off, which
    // leaves the file on the source disk as the only record of the run.
    start_capture();

    // Before any work: a crash mid-flash must lead to a normal boot attempt
    // rather than an endless re-entry.
    if let Some(bl) = ctx.boot_env.available_mut() {
        clear_flash_triggers(bl);
    }

    let outcome = run_selected_mode(&flash_config, &ctx);
    match &outcome {
        Ok(()) => log::info!("flash mode finished"),
        Err(e) => log::error!("flash mode failed: {e}"),
    }
    persist_log(ctx.layout, log_file(flash_config.mode), &take_capture());

    outcome?;

    // The run log is written after the sequence's own sync, and reboot(2) does
    // not flush.
    sync_filesystems();

    // reboot(2) returns Result<Infallible, _>, so the Ok side is uninhabited
    // and this let is irrefutable.
    let Err(e) = reboot(RebootMode::RB_POWER_OFF);
    Err(FlashError::Io(e.into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::{BootEnv, BootEnvKey, MockBootEnv};

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

    #[test]
    fn the_log_mount_point_stays_outside_the_rootfs_mount() {
        // Mode 1 unmounts the rootfs before it writes anything, so a target
        // under it would be gone by the time the log is written.
        assert!(!Path::new(FLASH_LOG_MOUNT_POINT).starts_with(crate::ROOTFS_DIR));
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
    fn a_missing_destination_is_reported_as_an_unusable_env_value() {
        // Detection logs an unusable devpath and still selects the mode, so the
        // mode itself has to turn the absence into an error.
        let flash_config = config::FlashConfig {
            mode: config::FlashMode::Mode1,
            devpath: None,
        };
        let err = destination(&flash_config).unwrap_err();
        assert!(
            matches!(err, FlashError::InvalidEnvValue { key, .. } if key == config::DEVPATH_KEY),
            "the absent destination must be named by its env key, got: {err}"
        );
    }
}
