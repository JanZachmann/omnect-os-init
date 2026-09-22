pub mod backup_restore;
pub mod config;
pub mod reformat;
pub mod wipe;

use std::path::{Path, PathBuf};

use log::warn;

use crate::{
    bootloader::BootEnvKey,
    error::{FactoryResetError, FilesystemError, InitramfsError, Result},
    filesystem::{
        FsType, MountOptions, PartitionMountSpec, mount_points, mount_tracked_partition, paths,
        setup_data_overlay_tracked, setup_etc_overlay_tracked, unmount_tracked,
    },
    mode::{BootContext, FactoryResetTrigger, factory_reset::backup_restore::RestoreResult},
    partition::{PartitionLayout, PartitionName},
    runtime::{FactoryResetStatus, FactoryResetStatusCode, OdsStatus},
};

use crate::mode::factory_reset::{
    backup_restore::{backup_all, restore_all},
    config::{FactoryResetConfig, ResetMode, build_preserve_list},
    reformat::reformat_ext4,
    wipe::{wipe_discard, wipe_random},
};

const FACTORY_RESET_BACKUP_DIR: &str = "/tmp/factory_reset/backup";

/// Shared join separator for every note that ends up in the factory-reset status.
pub(crate) const CONTEXT_SEPARATOR: &str = ";";

/// ext4 volume labels applied by `reformat_ext4`.
const DATA_PARTITION_LABEL: &str = "data";
const ETC_PARTITION_LABEL: &str = "etc";

/// Entry point for factory-reset mode.
///
/// Clears the trigger env var, runs the reset sequence, writes status to
/// `ods_status`, and always delegates to Normal boot — never blocks the device.
/// A trigger that could not be parsed skips the sequence and is reported as
/// the failure it is.
pub fn run(mut ctx: BootContext<'_>, trigger: FactoryResetTrigger) -> Result<()> {
    // Failing to clear the trigger (set_env) is non-fatal — log and continue with the reset.
    // If set_env consistently fails the trigger persists and the reset will
    // repeat on every boot until set_env succeeds.
    if let Some(bl) = ctx.boot_env.available_mut()
        && let Err(e) = bl.set_env(BootEnvKey::FactoryReset, None)
    {
        warn!("Failed to clear factory-reset bootloader var: {e}; proceeding anyway");
    }

    let (status, signal) = match trigger {
        FactoryResetTrigger::Rejected(e) => {
            warn!("Factory reset not started: {e}; continuing with Normal boot");
            (aborted_status(&e), None)
        }
        FactoryResetTrigger::Accepted(config) => {
            match run_reset(ctx.layout, ctx.rootfs, &config, &mut ctx.ods_status) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("Factory reset failed: {e}; continuing with Normal boot");
                    (aborted_status(&e), None)
                }
            }
        }
    };
    persist_exhausted_signal(signal.as_ref(), &mut ctx.boot_env);
    ctx.ods_status.set_factory_reset(status);

    crate::mode::normal::run(ctx)
}

/// Status for a reset that never reached the destructive phase: nothing was
/// touched, so `data_wiped` is false and no path was preserved.
fn aborted_status(e: &InitramfsError) -> FactoryResetStatus {
    FactoryResetStatus {
        status: failure_status_code(e),
        error: Some(e.to_string()),
        context: None,
        paths: vec![],
        data_wiped: false,
    }
}

/// Best-effort write of the unrecoverable-failure signal to the bootloader env,
/// so the outcome survives even if the following Normal boot halts before
/// `create_ods_runtime_files`. A degraded env is a no-op.
fn persist_exhausted_signal(
    signal: Option<&ResetFailureSignal>,
    boot_env: &mut crate::bootloader::BootEnvState,
) {
    let Some(sig) = signal else {
        return;
    };
    let Some(bl) = boot_env.available_mut() else {
        warn!("factory-reset failure signal exists but boot env is degraded; cannot persist it");
        return;
    };
    if let Err(e) = bl.save_factory_reset_failure(sig.partition, &sig.reason) {
        warn!("failed to persist factory-reset failure signal: {e}");
    }
}

/// Inner reset sequence. Returns `Err` only for failures before the
/// destructive phase begins (mount, config, backup); failures at or after the
/// wipe are resolved to a status internally instead, so they're never mistaken
/// for a safe abort.
fn run_reset(
    layout: &PartitionLayout,
    rootfs: &Path,
    config: &FactoryResetConfig,
    ods_status: &mut OdsStatus,
) -> Result<(FactoryResetStatus, Option<ResetFailureSignal>)> {
    let mut mounts: Vec<PathBuf> = Vec::new();
    factory_reset_mount(layout, rootfs, ods_status, &mut mounts).inspect_err(|_| {
        let _ = unmount_tracked(&mut mounts);
    })?;

    let preserve_list = build_preserve_list(config, rootfs).inspect_err(|_| {
        let _ = unmount_tracked(&mut mounts);
    })?;

    let backup_dir = PathBuf::from(FACTORY_RESET_BACKUP_DIR);
    let backed_up = backup_all(rootfs, &preserve_list, &backup_dir).inspect_err(|_| {
        let _ = unmount_tracked(&mut mounts);
    })?;

    unmount_tracked(&mut mounts)?;

    let data_dev = layout.partitions.get(&PartitionName::Data).ok_or_else(|| {
        FactoryResetError::MountError("data partition not found in layout".to_string())
    })?;
    let etc_dev = layout.partitions.get(&PartitionName::Etc).ok_or_else(|| {
        FactoryResetError::MountError("etc partition not found in layout".to_string())
    })?;

    let wipe_note = wipe_partitions(config.mode, data_dev, etc_dev, &mut RealWipeOps);

    let (status, signal) = match run_destructive_phase(
        layout,
        rootfs,
        &mut mounts,
        ods_status,
        ReformatTargets {
            data_dev,
            etc_dev,
            preserve_list: &preserve_list,
            backed_up: &backed_up,
            backup_dir: &backup_dir,
        },
    ) {
        Ok(pair) => pair,
        Err(e) => (destructive_phase_failure_status(e, preserve_list), None),
    };

    Ok((apply_wipe_note(status, wipe_note), signal))
}

/// Injectable abstraction over the wipe side effects, see `ReformatRetryOps`.
trait WipeOps {
    fn wipe_random(&mut self, device: &Path) -> Result<()>;
    fn wipe_discard(&mut self, device: &Path) -> Result<()>;
}

struct RealWipeOps;

impl WipeOps for RealWipeOps {
    fn wipe_random(&mut self, device: &Path) -> Result<()> {
        wipe_random(device)
    }

    fn wipe_discard(&mut self, device: &Path) -> Result<()> {
        wipe_discard(device)
    }
}

/// Never fails the reset: a failure on one device does not skip the other, and
/// reformat + restore still run, so the device stays usable. The collected
/// notes end up in the status `error` field.
fn wipe_partitions(
    mode: ResetMode,
    data_dev: &Path,
    etc_dev: &Path,
    ops: &mut dyn WipeOps,
) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    for (partition, device) in [
        (PartitionName::Etc, etc_dev),
        (PartitionName::Data, data_dev),
    ] {
        let wiped = match mode {
            ResetMode::Mode1 => return None,
            ResetMode::Mode2 => ops.wipe_random(device),
            ResetMode::Mode3 => ops.wipe_discard(device),
        };
        if let Err(e) = wiped {
            warn!("factory reset: wipe of {partition} failed; continuing: {e}");
            // Drop the outer "Factory reset error:" prefix, the note already
            // sits in the factory-reset result.
            let reason = match e {
                InitramfsError::FactoryReset(inner) => inner.to_string(),
                other => other.to_string(),
            };
            notes.push(format!("{partition}: {reason}"));
        }
    }

    (!notes.is_empty()).then(|| notes.join(CONTEXT_SEPARATOR))
}

/// Fold a wipe failure into the status the reformat/restore path produced.
///
/// The caller asked for the data to be wiped and it was not, so the outcome is
/// Error even when the rest of the reset succeeded.
fn apply_wipe_note(status: FactoryResetStatus, wipe_note: Option<String>) -> FactoryResetStatus {
    let Some(note) = wipe_note else {
        return status;
    };
    FactoryResetStatus {
        status: FactoryResetStatusCode::Error,
        error: join_context(Some(note), status.error),
        ..status
    }
}

/// Devices and paths needed by `run_destructive_phase`, grouped to keep the
/// function's argument count manageable.
struct ReformatTargets<'a> {
    data_dev: &'a Path,
    etc_dev: &'a Path,
    preserve_list: &'a [String],
    backed_up: &'a [String],
    backup_dir: &'a Path,
}

/// Real `ReformatRetryOps`: `mount_all` runs the full factory mount and, on
/// failure, unmounts what it managed so a failed reset leaves nothing half-mounted.
struct RealReformatOps<'a> {
    layout: &'a PartitionLayout,
    rootfs: &'a Path,
    ods_status: &'a mut OdsStatus,
    mounts: &'a mut Vec<PathBuf>,
}

impl ReformatRetryOps for RealReformatOps<'_> {
    fn reformat(&mut self, device: &Path, label: &str) -> Result<()> {
        reformat_ext4(device, label)
    }

    fn mount_all(&mut self) -> Result<()> {
        factory_reset_mount(self.layout, self.rootfs, self.ods_status, self.mounts).inspect_err(
            |_| {
                let _ = unmount_tracked(self.mounts);
            },
        )
    }
}

fn join_context(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(a), Some(b)) => Some(format!("{a}{CONTEXT_SEPARATOR}{b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Human-readable note for a partition that needed a reformat retry, for the
/// `context` field. Empty `retried` → None.
fn retry_note(retried: &[PartitionName]) -> Option<String> {
    if retried.is_empty() {
        return None;
    }
    let names: Vec<String> = retried.iter().map(|p| p.to_string()).collect();
    Some(format!("{} reformatted twice", names.join(",")))
}

/// Error text for partitions whose `mkfs` failed both times.
fn mkfs_failed_note(reformat_failed: &[PartitionName]) -> String {
    let names: Vec<String> = reformat_failed.iter().map(|p| p.to_string()).collect();
    format!("{}: mkfs failed twice", names.join(","))
}

/// Reformat + restore. `data` and/or `etc` may already be wiped and the backup
/// discarded. Callers must treat any `Err` from this function as data-loss, not
/// a safe no-op abort.
fn run_destructive_phase(
    layout: &PartitionLayout,
    rootfs: &Path,
    mounts: &mut Vec<PathBuf>,
    ods_status: &mut OdsStatus,
    targets: ReformatTargets,
) -> Result<(FactoryResetStatus, Option<ResetFailureSignal>)> {
    let report = {
        let mut ops = RealReformatOps {
            layout,
            rootfs,
            ods_status,
            mounts,
        };
        reformat_and_mount_with_retry(rootfs, &targets, &mut ops)?
    };

    if let Some(signal) = report.exhausted {
        // The reformatted partition never became usable — restore cannot run.
        warn!(
            "factory reset: {} could not be prepared; preserved data lost: {}",
            signal.partition, signal.reason
        );
        let _ = unmount_tracked(mounts);
        return Ok(exhausted_outcome(
            signal,
            &report.retried,
            targets.preserve_list,
        ));
    }

    let restore_result =
        restore_all(rootfs, targets.backed_up, targets.backup_dir).inspect_err(|_| {
            let _ = unmount_tracked(mounts);
        })?;

    unmount_tracked(mounts)?;

    log::info!("factory-reset complete");

    Ok((
        restored_status(
            &report.retried,
            &report.reformat_failed,
            restore_result,
            targets.preserve_list,
        ),
        None,
    ))
}

/// Status for the case where a partition never became usable — restore never
/// runs, so `data_wiped` is the only evidence the preserved data is lost.
fn exhausted_status(
    signal: &ResetFailureSignal,
    retried: &[PartitionName],
    preserve_list: &[String],
) -> FactoryResetStatus {
    FactoryResetStatus {
        status: FactoryResetStatusCode::Error,
        error: Some(signal.reason.clone()),
        context: retry_note(retried),
        paths: preserve_list.to_vec(),
        data_wiped: true,
    }
}

/// Pair the Error status with the signal to persist, so the exhausted branch
/// cannot return one without the other.
fn exhausted_outcome(
    signal: ResetFailureSignal,
    retried: &[PartitionName],
    preserve_list: &[String],
) -> (FactoryResetStatus, Option<ResetFailureSignal>) {
    let status = exhausted_status(&signal, retried, preserve_list);
    (status, Some(signal))
}

/// Status for the case where mounts succeeded and restore ran.
/// `data_wiped` is always `true` since the reformat already happened.
fn restored_status(
    retried: &[PartitionName],
    reformat_failed: &[PartitionName],
    restore: RestoreResult,
    preserve_list: &[String],
) -> FactoryResetStatus {
    let note = retry_note(retried);
    match restore {
        RestoreResult::Success if !reformat_failed.is_empty() => FactoryResetStatus {
            status: FactoryResetStatusCode::Error,
            error: Some(mkfs_failed_note(reformat_failed)),
            context: note,
            paths: preserve_list.to_vec(),
            data_wiped: true,
        },
        RestoreResult::Success if !retried.is_empty() => FactoryResetStatus {
            status: FactoryResetStatusCode::Warning,
            error: None,
            context: note,
            paths: preserve_list.to_vec(),
            data_wiped: true,
        },
        RestoreResult::Success => FactoryResetStatus {
            status: FactoryResetStatusCode::Success,
            error: None,
            context: note,
            paths: preserve_list.to_vec(),
            data_wiped: true,
        },
        RestoreResult::PartialFailure { context, error } => {
            // A double mkfs failure here would otherwise be hidden behind the
            // restore error; keep the suspect-storage signal in `error`.
            let error = if reformat_failed.is_empty() {
                error
            } else {
                format!(
                    "{}{CONTEXT_SEPARATOR}{error}",
                    mkfs_failed_note(reformat_failed)
                )
            };
            FactoryResetStatus {
                status: FactoryResetStatusCode::Error,
                error: Some(error),
                context: join_context(note, Some(context)),
                paths: preserve_list.to_vec(),
                data_wiped: true,
            }
        }
    }
}

/// Build the status for a failure that occurred during or after the
/// destructive phase — `data_wiped: true` and the preserve list are always
/// populated so ODS/cloud can tell this apart from a safe pre-reformat abort
/// and see what was lost.
fn destructive_phase_failure_status(e: InitramfsError, paths: Vec<String>) -> FactoryResetStatus {
    warn!(
        "factory reset failed after the destructive phase began; preserved data may be permanently lost: {e}"
    );
    FactoryResetStatus {
        status: FactoryResetStatusCode::Error,
        error: Some(e.to_string()),
        context: None,
        paths,
        data_wiped: true,
    }
}

/// Status code for a reset that never reached the destructive phase, either
/// because the trigger was unusable or because it failed before the wipe. The
/// config problems are distinguished so ODS/cloud can tell a bad request from a
/// real failure.
fn failure_status_code(e: &InitramfsError) -> FactoryResetStatusCode {
    match e {
        InitramfsError::FactoryReset(FactoryResetError::InvalidConfig(_)) => {
            FactoryResetStatusCode::Invalid
        }
        InitramfsError::FactoryReset(
            FactoryResetError::MissingField(_) | FactoryResetError::InvalidPreserve(_),
        ) => FactoryResetStatusCode::ConfigError,
        _ => FactoryResetStatusCode::Error,
    }
}

/// Mount factory (ro, if present), etc (rw), data (rw) and set up overlays.
///
/// Tracks each mount in `mounts` so `unmount_tracked` can reverse them.
/// Used for both the pre-backup and post-reformat mounts — factory must be
/// present both times so `setup_etc_overlay_tracked` can always reseed an
/// empty etc upper dir from factory defaults.
fn factory_reset_mount(
    layout: &PartitionLayout,
    rootfs: &Path,
    ods_status: &mut OdsStatus,
    mounts: &mut Vec<PathBuf>,
) -> Result<()> {
    mount_tracked_partition(
        layout,
        PartitionMountSpec {
            partition: PartitionName::Factory,
            mount_point: mount_points::FACTORY_PARTITION,
            options: MountOptions::ext4_readonly(),
            fstype: FsType::Ext4,
        },
        rootfs,
        ods_status,
        mounts,
    )?;

    mount_tracked_partition(
        layout,
        PartitionMountSpec {
            partition: PartitionName::Etc,
            mount_point: mount_points::ETC_PARTITION,
            options: MountOptions::ext4_readwrite(),
            fstype: FsType::Ext4,
        },
        rootfs,
        ods_status,
        mounts,
    )?;

    mount_tracked_partition(
        layout,
        PartitionMountSpec {
            partition: PartitionName::Data,
            mount_point: mount_points::DATA_PARTITION,
            options: MountOptions::ext4_readwrite(),
            fstype: FsType::Ext4,
        },
        rootfs,
        ods_status,
        mounts,
    )?;

    setup_etc_overlay_tracked(rootfs, mounts)?;
    setup_data_overlay_tracked(rootfs, mounts)?;

    Ok(())
}

/// Outcome of the reformat + mount, returned so the caller can set the success
/// `context` note and, on a mount failure, the bootloader-env signal.
struct RetryReport {
    /// Partitions whose `mkfs` failed at least once and was retried. Superset
    /// of `reformat_failed`.
    retried: Vec<PartitionName>,
    /// Partitions whose `mkfs` failed both times; the mount was still attempted.
    reformat_failed: Vec<PartitionName>,
    /// Set when the mount failed on a `data`/`etc` partition: the signal to persist.
    exhausted: Option<ResetFailureSignal>,
}

/// Partition and reason for a mount failure the reset could not get past,
/// carried out of the destructive phase to `run()` for the bootloader-env write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResetFailureSignal {
    partition: PartitionName,
    reason: String,
}

/// Injectable abstraction over the destructive-phase reformat/mount side
/// effects, so the control flow is unit-testable without real block devices.
trait ReformatRetryOps {
    fn reformat(&mut self, device: &Path, label: &str) -> Result<()>;
    fn mount_all(&mut self) -> Result<()>;
}

/// Resolve a mount/overlay failure back to the reformatted partition it
/// concerns. For partition mounts `MountFailed.src_path` is the device — match
/// it against the target device paths; a bind-mount source matches neither and
/// yields `None`. `OverlayFailed` carries the mount point — match against the
/// etc/data mount points. Any other error yields `None`, so the caller
/// propagates it without retrying.
///
/// COUPLING: `src_path` equals `targets.data_dev`/`etc_dev` only because both
/// come from the same `layout.partitions` lookup. If a future change resolves
/// one side to a different path form (e.g. `/dev/omnect/data`), this match
/// silently stops firing — the mocked tests cannot catch that.
fn resolve_failed_partition(
    err: &InitramfsError,
    rootfs: &Path,
    targets: &ReformatTargets,
) -> Option<PartitionName> {
    match err {
        InitramfsError::Filesystem(FilesystemError::MountFailed { src_path, .. }) => {
            if src_path == targets.data_dev {
                Some(PartitionName::Data)
            } else if src_path == targets.etc_dev {
                Some(PartitionName::Etc)
            } else {
                None
            }
        }
        InitramfsError::Filesystem(FilesystemError::OverlayFailed { target, .. }) => {
            // Match the overlay upper/work dir under the partition mount point
            // (dir-prep failure, e.g. mnt/etc/upper — by prefix) and the overlay
            // mount target rootfs/etc / rootfs/home (mount-syscall failure — by
            // equality). Any other OverlayFailed target (e.g. a bind-mount dir)
            // yields None.
            if target.starts_with(rootfs.join(mount_points::DATA_PARTITION))
                || *target == rootfs.join(paths::HOME)
            {
                Some(PartitionName::Data)
            } else if target.starts_with(rootfs.join(mount_points::ETC_PARTITION))
                || *target == rootfs.join(paths::ETC)
            {
                Some(PartitionName::Etc)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Reformat `data` and `etc`, then mount. Each `mkfs` is retried once on
/// failure and every failure is logged. The mount is always attempted and
/// decides the outcome: on success `exhausted` is `None`; a mount/overlay
/// failure resolving to `data`/`etc` becomes `RetryReport.exhausted` (a signal,
/// not `Err`), anything else propagates as `Err`. A mount failure is not
/// re-`mkfs`'d — a repeated `mkfs` would produce the same filesystem the mount
/// just rejected.
fn reformat_and_mount_with_retry(
    rootfs: &Path,
    targets: &ReformatTargets,
    ops: &mut dyn ReformatRetryOps,
) -> Result<RetryReport> {
    let mut retried: Vec<PartitionName> = Vec::new();
    let mut reformat_failed: Vec<PartitionName> = Vec::new();

    for (partition, device, label) in [
        (PartitionName::Data, targets.data_dev, DATA_PARTITION_LABEL),
        (PartitionName::Etc, targets.etc_dev, ETC_PARTITION_LABEL),
    ] {
        if let Err(first) = ops.reformat(device, label) {
            warn!("factory reset: mkfs of {partition} failed, retrying once: {first}");
            retried.push(partition);
            if let Err(second) = ops.reformat(device, label) {
                warn!(
                    "factory reset: mkfs of {partition} failed again; attempting the mount anyway: {second}"
                );
                reformat_failed.push(partition);
            }
        }
    }

    match ops.mount_all() {
        Ok(()) => Ok(RetryReport {
            retried,
            reformat_failed,
            exhausted: None,
        }),
        Err(e) => match resolve_failed_partition(&e, rootfs, targets) {
            Some(partition) => Ok(RetryReport {
                retried,
                reformat_failed,
                exhausted: Some(ResetFailureSignal {
                    partition,
                    reason: e.to_string(),
                }),
            }),
            None => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "factory-reset")]
    mod retry_tests {
        use super::*;
        use crate::error::FilesystemError;

        // Programmable ops: each call pops the next scripted result for that
        // method; reformat defaults to Ok on an empty queue, mount_all panics.
        struct ScriptedOps {
            mount_results: std::collections::VecDeque<Result<()>>,
            reformat_results: std::collections::VecDeque<Result<()>>,
            reformatted: Vec<PathBuf>,
        }
        impl ScriptedOps {
            fn new(results: Vec<Result<()>>) -> Self {
                Self {
                    mount_results: results.into_iter().collect(),
                    reformat_results: std::collections::VecDeque::new(),
                    reformatted: vec![],
                }
            }

            fn with_reformat_results(mut self, r: Vec<Result<()>>) -> Self {
                self.reformat_results = r.into_iter().collect();
                self
            }
        }
        impl ReformatRetryOps for ScriptedOps {
            fn reformat(&mut self, device: &Path, _label: &str) -> Result<()> {
                self.reformatted.push(device.to_path_buf());
                self.reformat_results.pop_front().unwrap_or(Ok(()))
            }
            fn mount_all(&mut self) -> Result<()> {
                self.mount_results
                    .pop_front()
                    .expect("mount_all called more times than scripted")
            }
        }

        fn reformat_failed() -> InitramfsError {
            crate::error::FactoryResetError::ReformatFailed {
                device: std::path::PathBuf::from("/dev/sda6"),
                reason: "mkfs failed".into(),
            }
            .into()
        }

        fn targets<'a>(
            data: &'a Path,
            etc: &'a Path,
            preserve: &'a [String],
        ) -> ReformatTargets<'a> {
            ReformatTargets {
                data_dev: data,
                etc_dev: etc,
                preserve_list: preserve,
                backed_up: preserve,
                backup_dir: Path::new("/tmp/does-not-matter"),
            }
        }

        fn mount_failed(src: &Path) -> InitramfsError {
            FilesystemError::MountFailed {
                src_path: src.to_path_buf(),
                target: PathBuf::from("/rootfs/mnt/etc"),
                reason: "bad superblock".into(),
            }
            .into()
        }

        fn overlay_failed(target: &Path) -> InitramfsError {
            FilesystemError::OverlayFailed {
                target: target.to_path_buf(),
                reason: "cannot create upperdir".into(),
            }
            .into()
        }

        #[test]
        fn clean_mount_no_retry() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            let mut ops = ScriptedOps::new(vec![Ok(())]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            assert!(report.retried.is_empty());
            assert!(report.exhausted.is_none());
        }

        #[test]
        fn mkfs_recovers_after_one_retry() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            let mut ops =
                ScriptedOps::new(vec![Ok(())]).with_reformat_results(vec![Err(reformat_failed())]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            assert_eq!(report.retried, vec![PartitionName::Data]);
            assert!(report.reformat_failed.is_empty());
            assert!(report.exhausted.is_none());
        }

        #[test]
        fn mkfs_double_failure_still_attempts_mount_then_signals() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            // Both mkfs attempts on data fail, but the mount is still attempted
            // and here it fails on data → signal.
            let mut ops = ScriptedOps::new(vec![Err(mount_failed(data))])
                .with_reformat_results(vec![Err(reformat_failed()), Err(reformat_failed())]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            assert_eq!(report.retried, vec![PartitionName::Data]);
            assert_eq!(report.reformat_failed, vec![PartitionName::Data]);
            let sig = report.exhausted.expect("must record exhausted");
            assert_eq!(sig.partition, PartitionName::Data);
            assert!(
                ops.mount_results.is_empty(),
                "mount must be attempted even after two failed mkfs"
            );
        }

        #[test]
        fn mkfs_double_failure_but_mount_succeeds_records_reformat_failed() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            // mkfs failed twice, yet the partition mounts. reformat_failed carries
            // it so the status becomes Error (suspect storage), not Warning.
            let mut ops = ScriptedOps::new(vec![Ok(())])
                .with_reformat_results(vec![Err(reformat_failed()), Err(reformat_failed())]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            assert!(report.exhausted.is_none());
            assert_eq!(report.retried, vec![PartitionName::Data]);
            assert_eq!(report.reformat_failed, vec![PartitionName::Data]);
        }

        #[test]
        fn mount_failure_on_etc_signals_without_reformat() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            let mut ops = ScriptedOps::new(vec![Err(mount_failed(etc))]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            let sig = report.exhausted.expect("must record exhausted");
            assert_eq!(sig.partition, PartitionName::Etc);
            assert!(report.retried.is_empty());
            // Only the two initial reformats happened — no mount-triggered re-mkfs.
            assert_eq!(ops.reformatted.len(), 2);
        }

        #[test]
        fn mount_failure_on_data_signals() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            let mut ops = ScriptedOps::new(vec![Err(mount_failed(data))]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            let sig = report.exhausted.expect("must record exhausted");
            assert_eq!(sig.partition, PartitionName::Data);
            assert!(report.retried.is_empty());
        }

        #[test]
        fn overlay_dir_failure_on_etc_signals() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            let etc_overlay_dir = Path::new("/rootfs")
                .join(mount_points::ETC_PARTITION)
                .join("upper");
            let mut ops = ScriptedOps::new(vec![Err(overlay_failed(&etc_overlay_dir))]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            let sig = report.exhausted.expect("must record exhausted");
            assert_eq!(sig.partition, PartitionName::Etc);
            assert!(report.retried.is_empty());
        }

        #[test]
        fn overlay_mount_target_failure_on_home_signals_data() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            let home_overlay_target = Path::new("/rootfs").join(paths::HOME);
            let mut ops = ScriptedOps::new(vec![Err(overlay_failed(&home_overlay_target))]);
            let report = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            )
            .unwrap();
            let sig = report.exhausted.expect("must record exhausted");
            assert_eq!(sig.partition, PartitionName::Data);
        }

        #[test]
        fn unresolvable_failure_propagates_err() {
            let (data, etc) = (Path::new("/dev/sda7"), Path::new("/dev/sda6"));
            // A failure on the factory partition device — matches neither data nor etc.
            let mut ops = ScriptedOps::new(vec![Err(mount_failed(Path::new("/dev/sda4")))]);
            let result = reformat_and_mount_with_retry(
                Path::new("/rootfs"),
                &targets(data, etc, &[]),
                &mut ops,
            );
            assert!(result.is_err());
        }
    }

    #[cfg(feature = "factory-reset")]
    mod context_join_tests {
        use super::*;

        #[test]
        fn retry_note_only() {
            let out = join_context(Some("etc reformatted twice".into()), None);
            assert_eq!(out.as_deref(), Some("etc reformatted twice"));
        }

        #[test]
        fn restore_context_only() {
            let out = join_context(None, Some("etc/hostname:restore".into()));
            assert_eq!(out.as_deref(), Some("etc/hostname:restore"));
        }

        #[test]
        fn both_joined_with_bare_semicolon() {
            let out = join_context(
                Some("etc reformatted twice".into()),
                Some("etc/hostname:restore".into()),
            );
            assert_eq!(
                out.as_deref(),
                Some("etc reformatted twice;etc/hostname:restore")
            );
        }

        #[test]
        fn neither_is_none() {
            assert_eq!(join_context(None, None), None);
        }
    }

    #[cfg(feature = "factory-reset")]
    mod status_assembly_tests {
        use super::*;
        use crate::error::FactoryResetError;

        #[test]
        fn exhausted_status_reports_error_and_wiped_data() {
            let sig = ResetFailureSignal {
                partition: PartitionName::Etc,
                reason: "mkfs retry exhausted".into(),
            };
            let status = exhausted_status(&sig, &[PartitionName::Etc], &["/p".to_string()]);
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert!(status.data_wiped);
            assert_eq!(status.paths, vec!["/p".to_string()]);
            assert_eq!(status.error.as_deref(), Some("mkfs retry exhausted"));
            assert!(
                status
                    .context
                    .as_deref()
                    .is_some_and(|c| c.contains("reformatted twice"))
            );
        }

        #[test]
        fn restored_status_success_no_retry() {
            let status = restored_status(&[], &[], RestoreResult::Success, &["/p".to_string()]);
            assert_eq!(status.status, FactoryResetStatusCode::Success);
            assert!(status.data_wiped);
            assert_eq!(status.context, None);
        }

        #[test]
        fn restored_status_recovered_retry_is_warning() {
            let status = restored_status(
                &[PartitionName::Etc],
                &[],
                RestoreResult::Success,
                &["/p".to_string()],
            );
            assert_eq!(status.status, FactoryResetStatusCode::Warning);
            assert_eq!(status.error, None);
            assert_eq!(
                status.context.as_deref(),
                retry_note(&[PartitionName::Etc]).as_deref()
            );
        }

        #[test]
        fn restored_status_mkfs_failed_twice_is_error() {
            let status = restored_status(
                &[PartitionName::Data],
                &[PartitionName::Data],
                RestoreResult::Success,
                &["/p".to_string()],
            );
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert!(status.data_wiped);
            assert!(
                status
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("mkfs failed twice"))
            );
            assert!(
                status
                    .context
                    .as_deref()
                    .is_some_and(|c| c.contains("reformatted twice"))
            );
        }

        #[test]
        fn restored_status_partial_failure_no_retry() {
            let status = restored_status(
                &[],
                &[],
                RestoreResult::PartialFailure {
                    context: "etc/hostname:restore".into(),
                    error: "cp failed".into(),
                },
                &["/p".to_string()],
            );
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert!(status.data_wiped);
            assert_eq!(status.error.as_deref(), Some("cp failed"));
            assert!(
                status
                    .context
                    .as_deref()
                    .is_some_and(|c| c.contains("etc/hostname:restore"))
            );
        }

        #[test]
        fn restored_status_partial_failure_joins_retry_and_restore_context() {
            let status = restored_status(
                &[PartitionName::Etc],
                &[],
                RestoreResult::PartialFailure {
                    context: "etc/hostname:restore".into(),
                    error: "cp failed".into(),
                },
                &["/p".to_string()],
            );
            let note = retry_note(&[PartitionName::Etc]).expect("retry note");
            assert_eq!(
                status.context.as_deref(),
                Some(format!("{note};etc/hostname:restore").as_str())
            );
        }

        #[test]
        fn destructive_phase_failure_status_reports_error_and_wiped_data() {
            let e = FactoryResetError::MountError("no data partition".into()).into();
            let status = destructive_phase_failure_status(e, vec!["/p".to_string()]);
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert!(status.data_wiped);
            assert!(status.error.is_some());
        }

        #[test]
        fn failure_status_code_distinguishes_config_errors() {
            let invalid: InitramfsError = FactoryResetError::InvalidConfig("x".into()).into();
            let missing: InitramfsError = FactoryResetError::MissingField("x".into()).into();
            let other: InitramfsError = FactoryResetError::MountError("x".into()).into();
            assert_eq!(
                failure_status_code(&invalid),
                FactoryResetStatusCode::Invalid
            );
            assert_eq!(
                failure_status_code(&missing),
                FactoryResetStatusCode::ConfigError
            );
            assert_eq!(failure_status_code(&other), FactoryResetStatusCode::Error);
        }

        #[test]
        fn exhausted_outcome_pairs_status_with_signal() {
            let sig = ResetFailureSignal {
                partition: PartitionName::Data,
                reason: "bad superblock".into(),
            };
            let (status, signal) =
                exhausted_outcome(sig, &[PartitionName::Data], &["/p".to_string()]);
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert!(status.data_wiped);
            let signal = signal.expect("signal must be carried out with the status");
            assert_eq!(signal.partition, PartitionName::Data);
            assert_eq!(signal.reason, "bad superblock");
        }

        #[test]
        fn restored_status_partial_failure_keeps_mkfs_failed_note() {
            let status = restored_status(
                &[PartitionName::Data],
                &[PartitionName::Data],
                RestoreResult::PartialFailure {
                    context: "etc/hostname:restore".into(),
                    error: "cp failed".into(),
                },
                &["/p".to_string()],
            );
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            let error = status.error.expect("error");
            assert!(error.contains("mkfs failed twice"), "{error}");
            assert!(error.contains("cp failed"), "{error}");
        }
    }

    #[cfg(feature = "factory-reset")]
    mod wipe_tests {
        use super::*;
        use crate::error::FactoryResetError;

        const DATA_DEV: &str = "/dev/sda7";
        const ETC_DEV: &str = "/dev/sda6";

        // Records every wiped device with the wipe kind, and fails for the
        // devices listed in `fail`.
        struct ScriptedWipeOps {
            calls: Vec<(&'static str, PathBuf)>,
            fail: Vec<PathBuf>,
        }

        impl ScriptedWipeOps {
            fn failing_on(devices: &[&str]) -> Self {
                Self {
                    calls: vec![],
                    fail: devices.iter().map(PathBuf::from).collect(),
                }
            }

            fn record(&mut self, kind: &'static str, device: &Path) -> Result<()> {
                self.calls.push((kind, device.to_path_buf()));
                if self.fail.iter().any(|d| d == device) {
                    return Err(FactoryResetError::WipeFailed {
                        device: device.to_path_buf(),
                        reason: "no discard support".into(),
                    }
                    .into());
                }
                Ok(())
            }
        }

        impl WipeOps for ScriptedWipeOps {
            fn wipe_random(&mut self, device: &Path) -> Result<()> {
                self.record("random", device)
            }

            fn wipe_discard(&mut self, device: &Path) -> Result<()> {
                self.record("discard", device)
            }
        }

        fn wipe(mode: ResetMode, ops: &mut ScriptedWipeOps) -> Option<String> {
            wipe_partitions(mode, Path::new(DATA_DEV), Path::new(ETC_DEV), ops)
        }

        #[test]
        fn mode1_does_not_wipe() {
            let mut ops = ScriptedWipeOps::failing_on(&[]);
            assert_eq!(wipe(ResetMode::Mode1, &mut ops), None);
            assert!(ops.calls.is_empty());
        }

        #[test]
        fn mode2_overwrites_both_partitions_with_random_data() {
            let mut ops = ScriptedWipeOps::failing_on(&[]);
            assert_eq!(wipe(ResetMode::Mode2, &mut ops), None);
            assert_eq!(
                ops.calls,
                vec![
                    ("random", PathBuf::from(ETC_DEV)),
                    ("random", PathBuf::from(DATA_DEV)),
                ]
            );
        }

        #[test]
        fn mode3_discards_both_partitions() {
            let mut ops = ScriptedWipeOps::failing_on(&[]);
            assert_eq!(wipe(ResetMode::Mode3, &mut ops), None);
            assert_eq!(
                ops.calls,
                vec![
                    ("discard", PathBuf::from(ETC_DEV)),
                    ("discard", PathBuf::from(DATA_DEV)),
                ]
            );
        }

        #[test]
        fn etc_wipe_failure_still_wipes_data() {
            let mut ops = ScriptedWipeOps::failing_on(&[ETC_DEV]);
            let note = wipe(ResetMode::Mode3, &mut ops).expect("failure must produce a note");
            assert_eq!(
                ops.calls,
                vec![
                    ("discard", PathBuf::from(ETC_DEV)),
                    ("discard", PathBuf::from(DATA_DEV)),
                ]
            );
            assert!(note.starts_with("etc: "), "{note}");
            assert!(note.contains(ETC_DEV), "{note}");
            assert!(!note.contains(DATA_DEV), "{note}");
        }

        #[test]
        fn both_wipe_failures_are_joined() {
            let mut ops = ScriptedWipeOps::failing_on(&[ETC_DEV, DATA_DEV]);
            let note = wipe(ResetMode::Mode2, &mut ops).expect("failure must produce a note");
            assert_eq!(note.split(CONTEXT_SEPARATOR).count(), 2, "{note}");
            assert!(note.contains("etc: ") && note.contains("data: "), "{note}");
        }

        #[test]
        fn success_becomes_error_carrying_the_wipe_note() {
            let status = restored_status(&[], &[], RestoreResult::Success, &["/p".to_string()]);
            let status = apply_wipe_note(status, Some("data: wipe failed".into()));
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert_eq!(status.error.as_deref(), Some("data: wipe failed"));
            assert_eq!(status.context, None);
            assert!(status.data_wiped);
        }

        #[test]
        fn reformat_retry_keeps_its_context_note() {
            let status = restored_status(
                &[PartitionName::Etc],
                &[],
                RestoreResult::Success,
                &["/p".to_string()],
            );
            let status = apply_wipe_note(status, Some("data: wipe failed".into()));
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            assert_eq!(status.error.as_deref(), Some("data: wipe failed"));
            assert_eq!(
                status.context.as_deref(),
                retry_note(&[PartitionName::Etc]).as_deref()
            );
        }

        #[test]
        fn restore_partial_failure_joins_both_errors() {
            let status = restored_status(
                &[],
                &[],
                RestoreResult::PartialFailure {
                    context: "1 of 2 paths restored".into(),
                    error: "cp failed".into(),
                },
                &["/p".to_string()],
            );
            let status = apply_wipe_note(status, Some("data: wipe failed".into()));
            assert_eq!(status.status, FactoryResetStatusCode::Error);
            let error = status.error.expect("error");
            assert_eq!(
                error,
                format!("data: wipe failed{CONTEXT_SEPARATOR}cp failed")
            );
            assert_eq!(status.context.as_deref(), Some("1 of 2 paths restored"));
        }

        #[test]
        fn no_wipe_failure_leaves_the_status_untouched() {
            let status = restored_status(
                &[PartitionName::Etc],
                &[],
                RestoreResult::Success,
                &["/p".to_string()],
            );
            let untouched = apply_wipe_note(status.clone(), None);
            assert_eq!(untouched.status, status.status);
            assert_eq!(untouched.error, status.error);
            assert_eq!(untouched.context, status.context);
        }
    }

    #[cfg(feature = "factory-reset")]
    mod trigger_status_tests {
        use super::*;
        use crate::mode::factory_reset::config::ResetMode;

        fn status_of(trigger: &str) -> FactoryResetStatus {
            use crate::bootloader::{BootEnvKey, MockBootEnv};
            use crate::mode::BootMode;

            let bl = MockBootEnv::new().with_env(BootEnvKey::FactoryReset, trigger);
            let BootMode::FactoryReset(FactoryResetTrigger::Rejected(e)) =
                BootMode::detect(Some(&bl)).expect("detect never fails")
            else {
                panic!("trigger must be rejected: {trigger}");
            };
            aborted_status(&e)
        }

        #[test]
        fn a_trigger_without_a_usable_mode_is_invalid() {
            for trigger in [
                r#"{ mode: "1""#,
                "{}",
                r#"{"preserve":[]}"#,
                r#"{"mode":"1","preserve":[]}"#,
                r#"{"mode":-1,"preserve":[]}"#,
                r#"{"mode":1.5,"preserve":[]}"#,
                r#"{"mode":5,"preserve":[]}"#,
            ] {
                assert_eq!(
                    status_of(trigger).status,
                    FactoryResetStatusCode::Invalid,
                    "{trigger}"
                );
            }
        }

        #[test]
        fn a_trigger_with_an_unusable_preserve_is_a_config_error() {
            for trigger in [
                r#"{"mode":1}"#,
                r#"{"mode":1,"pre":["ignored"]}"#,
                r#"{"mode":1,"preserve":""}"#,
                r#"{"mode":1,"preserve":[1]}"#,
            ] {
                assert_eq!(
                    status_of(trigger).status,
                    FactoryResetStatusCode::ConfigError,
                    "{trigger}"
                );
            }
        }

        #[test]
        fn a_rejected_trigger_reports_that_nothing_was_touched() {
            let status = status_of("{}");
            assert!(!status.data_wiped);
            assert!(status.paths.is_empty());
            assert!(status.error.is_some(), "an Error status needs a reason");
            assert_eq!(status.context, None);
        }

        #[test]
        fn a_failed_wipe_is_an_error_status() {
            let e: InitramfsError = FactoryResetError::WipeFailed {
                device: PathBuf::from("/dev/sda7"),
                reason: "no discard support".into(),
            }
            .into();
            assert_eq!(failure_status_code(&e), FactoryResetStatusCode::Error);
        }

        #[test]
        fn a_usable_trigger_parses() {
            let config = FactoryResetConfig::parse(r#"{"mode":1,"preserve":[]}"#).unwrap();
            assert_eq!(config.mode, ResetMode::Mode1);
            assert!(config.preserve.is_empty());
        }
    }

    #[cfg(feature = "factory-reset")]
    mod persist_signal_tests {
        use super::*;
        use crate::bootloader::{BootEnvKey, BootEnvState, MockBootEnv};

        #[test]
        fn writes_bootloader_key_when_signal_present() {
            let sig = ResetFailureSignal {
                partition: PartitionName::Etc,
                reason: "mkfs retry exhausted".into(),
            };
            let mut env = BootEnvState::Available(Box::new(MockBootEnv::new()));
            persist_exhausted_signal(Some(&sig), &mut env);
            let bl = env.available().unwrap();
            assert_eq!(
                bl.get_env(BootEnvKey::FactoryResetLastError).unwrap(),
                Some("etc:mkfs retry exhausted".to_string())
            );
        }

        #[test]
        fn no_write_when_no_signal() {
            let mut env = BootEnvState::Available(Box::new(MockBootEnv::new()));
            persist_exhausted_signal(None, &mut env);
            let bl = env.available().unwrap();
            assert_eq!(bl.get_env(BootEnvKey::FactoryResetLastError).unwrap(), None);
        }

        #[test]
        fn no_panic_on_degraded_env() {
            use crate::error::BootEnvError;
            let sig = ResetFailureSignal {
                partition: PartitionName::Data,
                reason: "mkfs retry exhausted".into(),
            };
            let mut env = BootEnvState::Degraded(BootEnvError::CommandFailed {
                command: "boot-env-tool".into(),
                reason: "test".into(),
            });
            persist_exhausted_signal(Some(&sig), &mut env);
        }

        #[test]
        fn no_propagate_when_set_env_fails() {
            let sig = ResetFailureSignal {
                partition: PartitionName::Data,
                reason: "mkfs retry exhausted".into(),
            };
            let mut env =
                BootEnvState::Available(Box::new(MockBootEnv::new().with_set_env_error()));
            persist_exhausted_signal(Some(&sig), &mut env);
        }
    }
}
