//! Flash modes: deploy a whole disk image from the initramfs, before any
//! rootfs is handed control.

#[cfg(feature = "flash-mode-2")]
pub(crate) mod bmap;
#[cfg(feature = "flash-mode-1")]
pub(crate) mod clone;
pub mod config;
#[cfg(feature = "grub")]
pub(crate) mod efi;
#[cfg(feature = "flash-mode-network")]
pub(crate) mod net;
#[cfg(feature = "flash-mode")]
pub(crate) mod rawio;
#[cfg(feature = "flash-mode-2")]
pub(crate) mod scp;
#[cfg(feature = "flash-mode-1")]
pub(crate) mod sfdisk;
#[cfg(feature = "flash-mode")]
pub(crate) mod unmount;
#[cfg(feature = "flash-mode-3")]
pub(crate) mod url;
#[cfg(feature = "flash-mode-network")]
pub(crate) mod xz;

#[cfg(feature = "flash-mode-network")]
use std::ffi::OsStr;
use std::fs;
#[cfg(feature = "flash-mode-network")]
use std::fs::File;
#[cfg(feature = "flash-mode-network")]
use std::io::Read;
use std::path::Path;
#[cfg(any(feature = "flash-mode-1", feature = "grub"))]
use std::path::PathBuf;
#[cfg(feature = "flash-mode-network")]
use std::process::Command;

use nix::sys::reboot::{RebootMode, reboot};

#[cfg(any(feature = "flash-mode-1", feature = "flash-mode-3"))]
use crate::bootloader::BootEnvKey;
use crate::bootloader::sync_filesystems;
use crate::error::FlashError;
use crate::filesystem::{MountOptions, MountPoint, mount, umount};
use crate::logging::{start_capture, take_capture};
use crate::mode::{BootContext, clear_flash_triggers};
use crate::partition::{PartitionLayout, PartitionName};

/// `PATH` for child processes: PID 1 has no login environment, and tools such
/// as `dhcpcd` run hook scripts that look up their own helpers.
#[cfg(feature = "flash-mode-network")]
const CHILD_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Scratch mount points. They sit outside the rootfs mount, which is unmounted
/// before the disk is written.
pub(crate) mod scratch_mounts {
    /// The data partition, while the run log is written.
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
#[cfg(feature = "flash-mode-2")]
const MODE_2_LOG_FILE: &str = "flash-mode-2.log";
#[cfg(feature = "flash-mode-3")]
const MODE_3_LOG_FILE: &str = "flash-mode-3.log";

/// Writes the run log: the data partition, the file name, the captured lines.
type LogWriter<'a> = &'a mut dyn FnMut(&Path, &str, &[String]) -> Result<(), FlashError>;

type ModeRunner<'a> =
    &'a mut dyn FnMut(&config::FlashConfig, &BootContext<'_>) -> Result<(), FlashError>;

#[cfg(feature = "flash-mode-network")]
pub(crate) const SHA256_LEN: usize = 32;

#[cfg(feature = "flash-mode-network")]
pub(crate) fn sha256_from_hex(text: &str) -> Result<[u8; SHA256_LEN], hex::FromHexError> {
    let mut digest = [0u8; SHA256_LEN];
    hex::decode_to_slice(text, &mut digest)?;
    Ok(digest)
}

/// A file larger than `max_len` comes from a wrong source and would fill RAM.
#[cfg(feature = "flash-mode-network")]
pub(crate) fn read_small_text(
    path: &Path,
    max_len: u64,
    invalid: impl Fn(String) -> FlashError,
) -> Result<String, FlashError> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(max_len + 1).read_to_end(&mut bytes))
        .map_err(|source| FlashError::PathIo {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > max_len {
        return Err(invalid(format!("larger than {max_len} bytes")));
    }
    String::from_utf8(bytes).map_err(|_| invalid("not UTF-8 text".to_string()))
}

#[cfg(feature = "flash-mode-network")]
fn child(cmd: &str) -> Command {
    let mut command = Command::new(cmd);
    command.env("PATH", CHILD_PATH);
    command
}

/// Run `cmd` with inherited stdout and stderr, so the operator sees its
/// output.
#[cfg(feature = "flash-mode-network")]
pub(crate) fn run_inherited(cmd: &str, args: &[&OsStr]) -> Result<(), String> {
    let status = child(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {cmd}: {e}"))?;
    if !status.success() {
        return Err(format!("{cmd} {args:?} failed ({status})"));
    }
    Ok(())
}

fn log_file(mode: config::FlashMode) -> &'static str {
    match mode {
        #[cfg(feature = "flash-mode-1")]
        config::FlashMode::Mode1 => MODE_1_LOG_FILE,
        #[cfg(feature = "flash-mode-2")]
        config::FlashMode::Mode2 => MODE_2_LOG_FILE,
        #[cfg(feature = "flash-mode-3")]
        config::FlashMode::Mode3 => MODE_3_LOG_FILE,
    }
}

#[cfg(feature = "flash-mode-network")]
pub(crate) fn partly_written(disk: &Path, source: FlashError) -> FlashError {
    FlashError::DiskPartlyWritten {
        disk: disk.to_path_buf(),
        source: Box::new(source),
    }
}

/// The data partition is not mounted on a partly written disk, or through a
/// partition table the disk no longer has.
fn keeps_log(outcome: &Result<(), FlashError>) -> bool {
    match outcome {
        #[cfg(feature = "flash-mode-network")]
        Err(FlashError::DiskPartlyWritten { .. } | FlashError::StalePartitionTable { .. }) => false,
        _ => true,
    }
}

fn terminal_action(mode: config::FlashMode) -> RebootMode {
    match mode {
        #[cfg(feature = "flash-mode-1")]
        config::FlashMode::Mode1 => RebootMode::RB_POWER_OFF,
        #[cfg(feature = "flash-mode-2")]
        config::FlashMode::Mode2 => RebootMode::RB_AUTOBOOT,
        #[cfg(feature = "flash-mode-3")]
        config::FlashMode::Mode3 => RebootMode::RB_AUTOBOOT,
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

#[cfg(any(feature = "flash-mode-1", feature = "flash-mode-3"))]
fn env_value<T>(value: &config::EnvValue<T>, key: BootEnvKey) -> Result<&T, FlashError> {
    let invalid = |reason: String| FlashError::InvalidEnvValue { key, reason };
    match value {
        config::EnvValue::Set(value) => Ok(value),
        config::EnvValue::NotSet => Err(invalid("not set".to_string())),
        config::EnvValue::Invalid(reason) => Err(invalid(reason.clone())),
        config::EnvValue::Unreadable(e) => Err(invalid(format!("failed to read env: {e}"))),
    }
}

#[cfg(any(feature = "flash-mode-1", feature = "grub"))]
pub(crate) fn layout_partition(
    layout: &PartitionLayout,
    name: PartitionName,
) -> Result<&Path, FlashError> {
    layout
        .get(name)
        .map(PathBuf::as_path)
        .ok_or_else(|| FlashError::PartitionTable {
            device: layout.device.base.clone(),
            operation: crate::error::PartitionTableOperation::Lookup,
            reason: format!("the layout has no {name} partition"),
        })
}

/// Write the captured log onto the data partition.
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
        log::warn!("flash mode: the layout has no data partition; the run log is not kept");
        return;
    };
    if let Err(e) = write(data_partition, file, lines) {
        log::warn!("flash mode: failed to write the run log to the data partition: {e}");
    }
}

fn run_selected_mode(
    flash_config: &config::FlashConfig,
    ctx: &BootContext<'_>,
) -> Result<(), FlashError> {
    match flash_config.mode {
        #[cfg(feature = "flash-mode-1")]
        config::FlashMode::Mode1 => clone::run_clone(&clone::CloneCtx {
            destination: env_value(&flash_config.devpath, BootEnvKey::FlashModeDevPath)?,
            layout: ctx.layout,
            rootfs: ctx.rootfs,
        }),
        #[cfg(feature = "flash-mode-2")]
        config::FlashMode::Mode2 => scp::run_scp(&scp::ScpCtx {
            layout: ctx.layout,
            rootfs: ctx.rootfs,
        }),
        #[cfg(feature = "flash-mode-3")]
        config::FlashMode::Mode3 => url::run_url(&url::UrlCtx {
            layout: ctx.layout,
            rootfs: ctx.rootfs,
            image_url: env_value(&flash_config.image_url, BootEnvKey::FlashModeUrl)?,
            sha256_url: env_value(&flash_config.sha256_url, BootEnvKey::FlashModeUrlSha256)?,
        }),
    }
}

/// Clear the triggers, run the mode and persist its log according to the mode's
/// policy.
fn run_and_persist(
    ctx: &mut BootContext<'_>,
    flash_config: &config::FlashConfig,
    run_mode: ModeRunner<'_>,
    write: LogWriter<'_>,
) -> Result<(), FlashError> {
    // First, so a failed trigger clear reaches the run log too. kmsg is gone
    // after the power off or reboot, so the file on the data partition is the
    // only record of the run.
    start_capture();

    if let Some(bl) = ctx.boot_env.available_mut() {
        clear_flash_triggers(bl);
    }

    let outcome = run_mode(flash_config, ctx);
    match &outcome {
        Ok(()) => log::info!("flash mode finished"),
        Err(e) => log::error!("flash mode failed: {e}"),
    }
    let lines = take_capture();
    if keeps_log(&outcome) {
        persist_log(ctx.layout, log_file(flash_config.mode), &lines, write);
    }

    // The run log is written after the sequence's own sync. On error the
    // release image halts, and a power cycle must not lose the log.
    sync_filesystems();

    outcome
}

/// Run the selected flash mode.
///
/// The `Ok` path does not return: it ends in the mode's terminal action. Mode 1
/// powers off, because it leaves a clone on a second disk that an operator has
/// to move. Modes 2 and 3 reboot into the new image.
pub(crate) fn run(
    mut ctx: BootContext<'_>,
    flash_config: config::FlashConfig,
) -> crate::Result<()> {
    run_and_persist(
        &mut ctx,
        &flash_config,
        &mut run_selected_mode,
        &mut write_log,
    )?;

    let Err(e) = reboot(terminal_action(flash_config.mode));
    Err(FlashError::Io(e.into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "flash-mode-1")]
    use crate::bootloader::{BootEnv, MockBootEnv};
    #[cfg(all(
        feature = "flash-mode-network",
        not(any(feature = "flash-mode-1", feature = "grub"))
    ))]
    use std::path::PathBuf;

    #[cfg(feature = "flash-mode-network")]
    #[test]
    fn children_get_the_explicit_path() {
        let command = child("/bin/true");
        let path = command
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v);
        assert_eq!(path, Some(OsStr::new(CHILD_PATH)));
    }

    #[cfg(feature = "flash-mode-network")]
    #[test]
    fn a_failing_child_is_reported_with_its_status() {
        let reason = run_inherited("/bin/false", &[]).unwrap_err();
        assert!(reason.contains("/bin/false"), "{reason}");
        assert!(run_inherited("/nonexistent/tool", &[]).is_err());
        assert!(run_inherited("/bin/true", &[]).is_ok());
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn clearing_the_triggers_unsets_the_selector_first() {
        let mut mock = MockBootEnv::new()
            .with_env(BootEnvKey::FlashMode, "1")
            .with_env(BootEnvKey::FlashModeDevPath, "/dev/does-not-exist");
        clear_flash_triggers(&mut mock);
        assert_eq!(
            mock.set_env_calls,
            all_flash_triggers(),
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
        let flash_config = config::FlashConfig::new(config::FlashMode::Mode1);

        // Fails before any device is touched.
        let err =
            run_and_persist(&mut ctx, &flash_config, &mut run_selected_mode, write).unwrap_err();
        let cleared = cleared.lock().unwrap().clone();
        (err, cleared)
    }

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
            all_flash_triggers(),
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

    #[cfg(feature = "flash-mode-2")]
    #[test]
    fn mode_2_writes_its_run_log_under_the_name_operators_look_for() {
        assert_eq!(log_file(config::FlashMode::Mode2), "flash-mode-2.log");
    }

    #[test]
    fn the_persisted_log_is_one_captured_line_per_line() {
        assert_eq!(
            log_contents(&["first".to_string(), "second".to_string()]),
            "first\nsecond\n"
        );
    }

    fn all_flash_triggers() -> Vec<crate::bootloader::BootEnvKey> {
        use crate::bootloader::BootEnvKey;
        [
            BootEnvKey::FlashMode,
            #[cfg(feature = "flash-mode-1")]
            BootEnvKey::FlashModeDevPath,
            #[cfg(feature = "flash-mode-3")]
            BootEnvKey::FlashModeUrl,
            #[cfg(feature = "flash-mode-3")]
            BootEnvKey::FlashModeUrlSha256,
        ]
        .to_vec()
    }

    #[cfg(feature = "flash-mode-network")]
    fn network_modes() -> Vec<config::FlashMode> {
        [
            #[cfg(feature = "flash-mode-2")]
            config::FlashMode::Mode2,
            #[cfg(feature = "flash-mode-3")]
            config::FlashMode::Mode3,
        ]
        .to_vec()
    }

    /// Runs `mode` with every trigger set; returns the outcome, the cleared
    /// keys, and the triggers still set afterwards.
    #[cfg(feature = "flash-mode-network")]
    fn run_network_mode(
        mode: config::FlashMode,
        result: fn() -> Result<(), FlashError>,
        write: LogWriter<'_>,
    ) -> (
        crate::Result<()>,
        Vec<crate::bootloader::BootEnvKey>,
        Vec<crate::bootloader::BootEnvKey>,
    ) {
        use crate::bootloader::{BootEnvState, MockBootEnv};
        use crate::config::Config;
        use crate::runtime::OdsStatus;

        let mock = all_flash_triggers()
            .into_iter()
            .fold(MockBootEnv::new(), |mock, key| mock.with_env(key, "set"));
        let cleared = mock.shared_set_env_calls();
        let cleared_at_start = cleared.clone();
        let config = Config::default();
        let layout = source_layout();
        let mut ctx = BootContext::new(
            &config,
            &layout,
            Path::new("/nonexistent/rootfs"),
            BootEnvState::Available(Box::new(mock)),
            OdsStatus::default(),
        );
        let outcome = run_and_persist(
            &mut ctx,
            &config::FlashConfig::new(mode),
            &mut |_, _| {
                assert_eq!(
                    *cleared_at_start.lock().unwrap(),
                    all_flash_triggers(),
                    "the selector must be cleared before the mode runs"
                );
                result()
            },
            write,
        )
        .map_err(Into::into);
        let bl = ctx.boot_env.available_mut().unwrap();
        let still_set = all_flash_triggers()
            .into_iter()
            .filter(|key| bl.get_env(*key).unwrap().is_some())
            .collect();
        let cleared = cleared.lock().unwrap().clone();
        (outcome, cleared, still_set)
    }

    #[cfg(feature = "flash-mode-network")]
    #[test]
    fn a_written_disk_gets_no_run_log_after_a_failure() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();

        let results: [fn() -> Result<(), FlashError>; 2] = [
            || {
                Err(FlashError::DiskPartlyWritten {
                    disk: PathBuf::from("/dev/sda"),
                    source: Box::new(FlashError::Io(std::io::Error::other("write failed"))),
                })
            },
            || {
                Err(FlashError::StalePartitionTable {
                    disk: PathBuf::from("/dev/sda"),
                    source: Box::new(FlashError::Io(std::io::Error::other("busy"))),
                })
            },
        ];
        for mode in network_modes() {
            for result in results {
                let mut writes = 0;
                let (outcome, cleared, still_set) =
                    run_network_mode(mode, result, &mut |_, _, _| {
                        writes += 1;
                        Ok(())
                    });
                assert!(outcome.is_err());
                assert_eq!(
                    writes, 0,
                    "{mode:?}: the data partition must not be mounted"
                );
                assert_eq!(cleared, all_flash_triggers(), "{mode:?}");
                assert!(still_set.is_empty(), "{mode:?}: {still_set:?}");
            }
        }
    }

    #[cfg(feature = "flash-mode-network")]
    #[test]
    fn a_run_that_fails_before_the_disk_is_written_keeps_its_log() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();

        for mode in network_modes() {
            let mut written: Vec<(String, Vec<String>)> = Vec::new();
            let (outcome, _, still_set) = run_network_mode(
                mode,
                || Err(FlashError::NetworkFailed("no lease".to_string())),
                &mut |_, file, lines| {
                    written.push((file.to_string(), lines.to_vec()));
                    Ok(())
                },
            );
            assert!(outcome.is_err());
            assert!(still_set.is_empty(), "{mode:?}: {still_set:?}");
            let [(file, lines)] = written.as_slice() else {
                panic!("{mode:?}: the run log must be written once, got {written:?}");
            };
            assert_eq!(file, log_file(mode));
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("[ERROR] flash mode failed:")
                        && line.contains("no lease")),
                "{mode:?}: {lines:?}"
            );
        }
    }

    #[cfg(feature = "flash-mode-network")]
    #[test]
    fn a_successful_run_writes_its_log_to_the_data_partition() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();

        for mode in network_modes() {
            let mut written: Vec<(std::path::PathBuf, String, Vec<String>)> = Vec::new();
            let (outcome, cleared, _) =
                run_network_mode(mode, || Ok(()), &mut |partition, file, lines| {
                    written.push((partition.to_path_buf(), file.to_string(), lines.to_vec()));
                    Ok(())
                });
            assert!(outcome.is_ok());
            assert_eq!(cleared, all_flash_triggers());
            let [(partition, file, lines)] = written.as_slice() else {
                panic!("{mode:?}: the run log must be written once, got {written:?}");
            };
            assert_eq!(Some(partition), source_layout().get(PartitionName::Data));
            assert_eq!(file, log_file(mode));
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("flash mode finished")),
                "{mode:?}: {lines:?}"
            );
        }
    }

    #[test]
    fn a_successful_run_ends_in_the_terminal_action_of_its_mode() {
        #[cfg(feature = "flash-mode-1")]
        assert_eq!(
            terminal_action(config::FlashMode::Mode1),
            RebootMode::RB_POWER_OFF
        );
        #[cfg(feature = "flash-mode-2")]
        assert_eq!(
            terminal_action(config::FlashMode::Mode2),
            RebootMode::RB_AUTOBOOT
        );
        #[cfg(feature = "flash-mode-3")]
        assert_eq!(
            terminal_action(config::FlashMode::Mode3),
            RebootMode::RB_AUTOBOOT
        );
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn a_missing_destination_is_reported_with_its_reason() {
        let flash_config = config::FlashConfig::new(config::FlashMode::Mode1);
        let err = env_value(&flash_config.devpath, BootEnvKey::FlashModeDevPath).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidEnvValue { key, reason }
                if *key == BootEnvKey::FlashModeDevPath && reason == "not set"),
            "the absent destination must be named by its env key and reason, got: {err}"
        );
    }

    #[cfg(feature = "flash-mode-3")]
    #[test]
    fn mode_3_writes_its_run_log_under_the_name_operators_look_for() {
        assert_eq!(log_file(config::FlashMode::Mode3), "flash-mode-3.log");
    }

    #[cfg(feature = "flash-mode-3")]
    #[test]
    fn an_unusable_mode_3_url_fails_before_any_work_and_clears_the_triggers() {
        use crate::bootloader::{BootEnvState, MockBootEnv};
        use crate::config::Config;
        use crate::runtime::OdsStatus;

        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mock = MockBootEnv::new()
            .with_env(BootEnvKey::FlashMode, "3")
            .with_env(BootEnvKey::FlashModeUrl, "not base64!!");
        let cleared = mock.shared_set_env_calls();
        let config = Config::default();
        let layout = source_layout();
        let mut ctx = BootContext::new(
            &config,
            &layout,
            Path::new("/nonexistent/rootfs"),
            BootEnvState::Available(Box::new(mock)),
            OdsStatus::default(),
        );
        let flash_config = config::FlashConfig {
            image_url: config::EnvUrl::Invalid("not base64".to_string()),
            ..config::FlashConfig::new(config::FlashMode::Mode3)
        };

        let err = run_and_persist(
            &mut ctx,
            &flash_config,
            &mut run_selected_mode,
            &mut |_, _, _| Ok(()),
        )
        .unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidEnvValue { key, .. } if *key == BootEnvKey::FlashModeUrl),
            "{err}"
        );
        assert_eq!(*cleared.lock().unwrap(), all_flash_triggers());
    }

    #[cfg(feature = "flash-mode-3")]
    #[test]
    fn each_unusable_url_names_its_key_and_reason() {
        let cases = [
            (config::EnvUrl::NotSet, "not set"),
            (
                config::EnvUrl::Invalid("not base64".to_string()),
                "not base64",
            ),
            (
                config::EnvUrl::Unreadable("fw_printenv failed".to_string()),
                "failed to read env: fw_printenv failed",
            ),
        ];
        for (url, expected) in cases {
            let err = env_value(&url, BootEnvKey::FlashModeUrlSha256).unwrap_err();
            assert!(
                matches!(&err, FlashError::InvalidEnvValue { key, reason }
                    if *key == BootEnvKey::FlashModeUrlSha256 && reason == expected),
                "{err}"
            );
        }
        let url = config::Url::new("https://example.invalid/wic.xz").unwrap();
        let set = config::EnvUrl::Set(url.clone());
        assert_eq!(env_value(&set, BootEnvKey::FlashModeUrl).unwrap(), &url);
    }
}
