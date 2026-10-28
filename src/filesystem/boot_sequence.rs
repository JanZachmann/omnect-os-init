//! Boot-sequence mount and fsck orchestration
//!
//! These functions coordinate partition mounting and fsck result persistence
//! during initramfs startup. Kept in the library crate so they can be unit-tested
//! with mock bootloaders and temporary directories.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use nix::mount::MsFlags;

use crate::bootloader::BootEnv;
use crate::error::{BootEnvError, FilesystemError, InitramfsError, PartitionError};
use crate::filesystem::{
    FsType, FsckExitCode, MountOptions, MountPoint, check_filesystem_lenient, create_root_only_dir,
    create_root_only_file, is_path_mounted, mount, mount_points, mount_tmpfs,
};
use crate::partition::{PartitionLayout, PartitionName};
use crate::runtime::OdsStatus;

/// Subdirectory within the data partition where fsck logs are written.
const FSCK_LOG_SUBDIR: &str = "log/fsck";

/// Every partition passed to `fsck_and_record`, which is what `drain_fsck_env`
/// looks for in the boot env. Adding a fsck'd partition without listing it here
/// leaves its record undrained.
const FSCK_PARTITIONS: [PartitionName; 5] = [
    PartitionName::Boot,
    PartitionName::Factory,
    PartitionName::Cert,
    PartitionName::Etc,
    PartitionName::Data,
];

/// Run fsck on a partition and record the result (including output) in `ods_status`.
///
/// Records the result even when fsck reports errors (exit ≥ 4) and does not fail
/// the boot: proceeding despite fsck errors is preferable to an unrecoverable
/// brick on an embedded device without physical access. The full fsck result is
/// persisted via `OdsStatus` (→ boot env + `/data/log/fsck/<partition>.log`)
/// so ODS can act on it at runtime — independent of `OdsStatus.degraded_boot`,
/// which is reserved for the env-unavailable condition.
///
/// Intercepts `FsckRequiresReboot` to save the output before propagating, ensuring
/// it is available for persistence even when mounting is aborted early.
pub fn fsck_and_record(
    dev: &Path,
    name: PartitionName,
    ods_status: &mut OdsStatus,
    fstype: FsType,
) -> std::result::Result<(), FilesystemError> {
    match check_filesystem_lenient(dev, fstype) {
        Ok(r) => {
            ods_status.record_fsck_result(name, r.exit_code, r.output);
            Ok(())
        }
        Err(FilesystemError::FsckRequiresReboot {
            device,
            code,
            ref output,
        }) => {
            ods_status.record_fsck_result(name, code, output.clone());
            Err(FilesystemError::FsckRequiresReboot {
                device,
                code,
                output: output.clone(),
            })
        }
        Err(e) => Err(e),
    }
}

/// Mount the core partitions required before the bootloader environment can be opened.
///
/// Mounts rootCurrent (read-only) and boot (read-write). Must be called before
/// `open_boot_env()` — the boot partition must be present when the bootloader
/// environment is opened. `mount_remaining_partitions` must be called afterward
/// to complete the full partition mount sequence.
///
/// # Errors
///
/// Boot's fsck result is in `ods_status` before boot is mounted. On any error
/// the caller must persist `ods_status` first, or the result is lost on the
/// next boot.
pub fn mount_core_partitions(
    layout: &PartitionLayout,
    rootfs: &Path,
    ods_status: &mut OdsStatus,
) -> crate::error::Result<()> {
    let root_dev = layout
        .partitions
        .get(&PartitionName::RootCurrent)
        .ok_or_else(|| {
            InitramfsError::Partition(PartitionError::DeviceDetection(
                "rootCurrent not found in partition map; cannot mount rootfs".to_string(),
            ))
        })?;

    // The mount target must exist before mount(2) is called. The directory is
    // not baked into the initramfs image — create it here on every boot.
    fs::create_dir_all(rootfs).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to create rootfs mount point {}: {}",
            rootfs.display(),
            e
        )))
    })?;

    // rootCurrent is mounted directly without fsck: the kernel's own ext4 journal
    // replay is the correct recovery mechanism. Running fsck -y before mount can
    // interfere with journal replay and cause EUCLEAN on a filesystem that the kernel
    // could have mounted cleanly.
    mount(MountPoint::new(
        root_dev,
        rootfs,
        MountOptions::ext4_readonly().noatime().nodiratime(),
    ))?;
    log::info!("Mounted rootfs at {}", rootfs.display());

    // vfat is mounted read-write without noatime/nodiratime: GRUB needs to write
    // grubenv on the boot partition; atime writes are acceptable on vfat.
    if let Some(boot_dev) = layout.partitions.get(&PartitionName::Boot) {
        let boot_mount = rootfs.join(mount_points::BOOT);
        if is_path_mounted(&boot_mount)? {
            // Boot already mounted at this stage is a logic error: mount_core_partitions
            // is called exactly once per boot. If boot is already present something has
            // gone wrong in the boot sequence.
            return Err(InitramfsError::Filesystem(FilesystemError::MountFailed {
                src_path: boot_dev.clone(),
                target: boot_mount,
                reason: "boot partition already mounted at start of mount_core_partitions"
                    .to_string(),
            }));
        }
        fsck_and_record(boot_dev, PartitionName::Boot, ods_status, FsType::Vfat)?;
        mount(MountPoint::new(boot_dev, &boot_mount, MountOptions::vfat()))?;
    }

    Ok(())
}

/// Describes one partition mount for `mount_tracked_partition`.
#[cfg(feature = "factory-reset")]
pub(crate) struct PartitionMountSpec<'a> {
    pub partition: PartitionName,
    pub mount_point: &'a str,
    pub options: MountOptions,
    pub fstype: FsType,
}

/// Mount `spec.partition` at `rootfs/spec.mount_point`, if present in
/// `layout`: run fsck, mount it, and track the mount path in `mounts` for
/// later cleanup via `unmount_tracked`.
///
/// A no-op when the partition is absent from the layout.
#[cfg(feature = "factory-reset")]
pub(crate) fn mount_tracked_partition(
    layout: &PartitionLayout,
    spec: PartitionMountSpec,
    rootfs: &Path,
    ods_status: &mut OdsStatus,
    mounts: &mut Vec<PathBuf>,
) -> crate::error::Result<()> {
    let Some(dev) = layout.partitions.get(&spec.partition) else {
        return Ok(());
    };
    let mount_path = rootfs.join(spec.mount_point);
    fsck_and_record(dev, spec.partition, ods_status, spec.fstype)?;
    mount(MountPoint::new(dev, &mount_path, spec.options))?;
    mounts.push(mount_path);
    Ok(())
}

/// Mount the remaining partitions after the boot env is opened.
///
/// Mounts factory, cert, etc, data, and var/volatile. Must be called after
/// `mount_core_partitions` and after `open_boot_env`. Each mount is skipped
/// with a warning if already mounted, guarding against a leaked mount (e.g.
/// from an aborted factory reset) turning into a fatal `MountFailed`.
pub fn mount_remaining_partitions(
    layout: &PartitionLayout,
    rootfs: &Path,
    ods_status: &mut OdsStatus,
) -> crate::error::Result<()> {
    if let Some(factory_dev) = layout.partitions.get(&PartitionName::Factory) {
        let factory_mount = rootfs.join(mount_points::FACTORY_PARTITION);
        if is_path_mounted(&factory_mount)? {
            log::warn!(
                "factory already mounted at {}; skipping (unexpected — likely a leaked mount)",
                factory_mount.display()
            );
        } else {
            fsck_and_record(
                factory_dev,
                PartitionName::Factory,
                ods_status,
                FsType::Ext4,
            )?;
            mount(MountPoint::new(
                factory_dev,
                &factory_mount,
                MountOptions::ext4_readonly().noatime().nodiratime(),
            ))?;
        }
    }

    if let Some(cert_dev) = layout.partitions.get(&PartitionName::Cert) {
        let cert_mount = rootfs.join(mount_points::CERT_PARTITION);
        if is_path_mounted(&cert_mount)? {
            log::warn!(
                "cert already mounted at {}; skipping (unexpected — likely a leaked mount)",
                cert_mount.display()
            );
        } else {
            fsck_and_record(cert_dev, PartitionName::Cert, ods_status, FsType::Ext4)?;
            mount(MountPoint::new(
                cert_dev,
                &cert_mount,
                MountOptions::ext4_readwrite().noatime().nodiratime(),
            ))?;
        }
    }

    // Mount etc partition (for overlay upper)
    if let Some(etc_dev) = layout.partitions.get(&PartitionName::Etc) {
        let etc_mount = rootfs.join(mount_points::ETC_PARTITION);
        if is_path_mounted(&etc_mount)? {
            log::warn!(
                "etc already mounted at {}; skipping (unexpected — likely a leaked mount)",
                etc_mount.display()
            );
        } else {
            fsck_and_record(etc_dev, PartitionName::Etc, ods_status, FsType::Ext4)?;
            mount(MountPoint::new(
                etc_dev,
                &etc_mount,
                MountOptions::ext4_readwrite().noatime().nodiratime(),
            ))?;
        }
    }

    if let Some(data_dev) = layout.partitions.get(&PartitionName::Data) {
        let data_mount = rootfs.join(mount_points::DATA_PARTITION);
        if is_path_mounted(&data_mount)? {
            log::warn!(
                "data already mounted at {}; skipping (unexpected — likely a leaked mount)",
                data_mount.display()
            );
        } else {
            fsck_and_record(data_dev, PartitionName::Data, ods_status, FsType::Ext4)?;
            mount(MountPoint::new(
                data_dev,
                &data_mount,
                MountOptions::ext4_readwrite().noatime().nodiratime(),
            ))?;
        }
    }

    // /var/volatile provides a writable mount for volatile data under the read-only rootfs
    let var_volatile = rootfs.join(mount_points::VAR_VOLATILE);
    mount_tmpfs(&var_volatile, MsFlags::empty(), None)?;

    // The initramfs /run tmpfs (mounted by mount_essential_filesystems) is moved
    // into the new root by switch_root using MS_MOVE, so /run is left alone here:
    // mounting a second tmpfs at /rootfs/run would cause EBUSY and lose any files
    // written there (e.g. ODS runtime state).

    Ok(())
}

/// Write one partition's fsck log and make it durable, directory entry included.
///
/// The `FsckRequiresReboot` path continues with `reboot(2)`, which does not sync,
/// so this is flushed here rather than left to whatever happens to sync next.
fn write_fsck_log(
    log_dir: &Path,
    partition: PartitionName,
    output: &str,
) -> std::io::Result<PathBuf> {
    create_root_only_dir(log_dir)?;
    let log_path = log_dir.join(format!("{partition}.log"));
    let mut file = create_root_only_file(&log_path)?;
    file.write_all(output.as_bytes())?;
    file.sync_all()?;
    // Flushing the file does not make its name durable — that lives in the directory.
    fs::File::open(log_dir)?.sync_all()?;
    Ok(log_path)
}

/// Persist fsck results after all partitions are mounted.
///
/// For each partition with a non-zero fsck exit code:
/// - Stores the gzip+base64 encoded exit code and full output in the bootloader
///   environment (grubenv / uboot-env), so the record survives a reboot that
///   happens before the ODS status JSON is written. `drain_fsck_env` consumes it.
///   Skipped with a warning when `bootloader` is `None` (env unavailable).
/// - Writes the full output to `/data/log/fsck/<partition>.log` (written
///   to /rootfs/mnt/data/log/fsck/; visible as `/data/log/fsck/`
///   after switch_root) so ODS and operators can inspect it after boot.
///   Written regardless of boot env availability when the data partition is mounted.
pub fn persist_fsck_results(
    ods_status: &OdsStatus,
    mut bootloader: Option<&mut dyn BootEnv>,
    rootfs_dir: &Path,
) {
    // The data partition log is best-effort: it is only mounted when
    // mount_remaining_partitions() succeeds fully, so it may not be available here.
    let log_dir = rootfs_dir
        .join(mount_points::DATA_PARTITION)
        .join(FSCK_LOG_SUBDIR);
    let data_mounted =
        is_path_mounted(&rootfs_dir.join(mount_points::DATA_PARTITION)).unwrap_or(false);

    for (partition, fsck) in &ods_status.fsck {
        // `record_fsck_result` already drops clean results, but `OdsStatus::fsck` is a
        // public map — this guard is what stops a direct insert from putting a clean
        // record into the fixed-size env block.
        let exit_code = FsckExitCode::from(fsck.code);
        if exit_code.is_clean() {
            continue;
        }

        match &mut bootloader {
            Some(bl) => {
                if let Err(e) = bl.save_fsck_status(*partition, exit_code, &fsck.output) {
                    log::warn!(
                        "Failed to save fsck status for {} to bootloader env: {}",
                        partition,
                        e
                    );
                }
            }
            None => log::warn!(
                "Bootloader env unavailable; fsck status for {partition} not saved to it"
            ),
        }

        if !fsck.output.is_empty() {
            if !data_mounted {
                log::warn!(
                    "Data partition not mounted; skipping fsck log write for {}",
                    partition
                );
                continue;
            }
            match write_fsck_log(&log_dir, *partition, &fsck.output) {
                Ok(path) => log::info!("Wrote fsck log: {}", path.display()),
                Err(e) => log::warn!("Failed to write fsck log for {partition}: {e}"),
            }
        }
    }
}

/// Move fsck records from the boot env into `ods_status` and clear them there.
///
/// The boot env carries a record across a reboot: a boot that aborts between
/// fsck and the ODS status write leaves it there, and this brings it into the
/// JSON on the boot that follows.
///
/// Call after `persist_fsck_results` and before the ODS status JSON is written.
/// A no-op in degraded boot, where `ods_status` still holds this boot's records.
pub fn drain_fsck_env(ods_status: &mut OdsStatus, bootloader: Option<&mut dyn BootEnv>) {
    let Some(bl) = bootloader else {
        return;
    };

    for partition in FSCK_PARTITIONS {
        let record = match bl.get_fsck_status(partition) {
            Ok(r) => r,
            Err(e @ BootEnvError::InvalidValue { .. }) => {
                // Otherwise it stays in the env on every later boot.
                log::warn!("Dropping the fsck record for {partition}: {e}");
                if let Err(e) = bl.clear_fsck_status(partition) {
                    log::warn!("Failed to clear fsck record for {partition} in boot env: {e}");
                }
                continue;
            }
            Err(e) => {
                log::warn!("Failed to read fsck record for {partition} from boot env: {e}");
                continue;
            }
        };
        let Some(record) = record else {
            continue;
        };

        ods_status.record_fsck_result(partition, record.exit_code, record.output);
        if let Err(e) = bl.clear_fsck_status(partition) {
            log::warn!("Failed to clear fsck record for {partition} in boot env: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::MockBootEnv;
    use crate::partition::PartitionName;
    use crate::runtime::OdsStatus;
    use tempfile::TempDir;

    // ---- helpers -------------------------------------------------------

    fn make_ods_with(partition: PartitionName, code: FsckExitCode, output: &str) -> OdsStatus {
        let mut s = OdsStatus::new();
        s.record_fsck_result(partition, code, output.to_string());
        s
    }

    /// Insert past `record_fsck_result`, which drops clean results. Needed to reach
    /// the `is_clean()` guard in `persist_fsck_results` at all.
    fn insert_raw(ods: &mut OdsStatus, partition: PartitionName, code: i32, output: &str) {
        ods.fsck.insert(
            partition,
            crate::runtime::FsckStatus {
                code,
                output: output.to_string(),
            },
        );
    }
    // ---- tests ---------------------------------------------------------

    #[test]
    fn write_fsck_log_writes_the_file_and_returns_its_path() {
        let temp = TempDir::new().unwrap();

        let path = write_fsck_log(temp.path(), PartitionName::Data, "errors corrected").unwrap();

        assert_eq!(path, temp.path().join("data.log"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "errors corrected");
    }

    #[test]
    fn write_fsck_log_makes_the_dir_and_file_root_only() {
        use std::os::unix::fs::MetadataExt;
        let temp = TempDir::new().unwrap();
        let log_dir = temp.path().join("fsck");

        let path = write_fsck_log(&log_dir, PartitionName::Data, "errors corrected").unwrap();

        assert_eq!(fs::metadata(&log_dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn test_persist_zero_code_not_saved() {
        // Exercises the is_clean() guard: a clean record that bypassed
        // record_fsck_result must still not reach the boot env.
        let mut ods = OdsStatus::new();
        insert_raw(&mut ods, PartitionName::Boot, 0, "clean");
        assert_eq!(ods.fsck.len(), 1, "the guard must have something to skip");

        let temp = TempDir::new().unwrap();
        let mut bl = MockBootEnv::new();

        persist_fsck_results(&ods, Some(&mut bl), temp.path());

        assert!(
            bl.saved_fsck_calls().lock().unwrap().is_empty(),
            "zero exit code must not be persisted"
        );
    }

    #[test]
    fn test_persist_nonzero_calls_save_fsck_status() {
        // Non-zero exit code must call save_fsck_status with correct args.
        let ods = make_ods_with(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected",
        );
        let temp = TempDir::new().unwrap();
        let mut bl = MockBootEnv::new();

        persist_fsck_results(&ods, Some(&mut bl), temp.path());

        let saved = bl.saved_fsck_calls();
        let saved = saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].0, PartitionName::Boot);
        assert_eq!(saved[0].1, FsckExitCode::CORRECTED);
        assert_eq!(saved[0].2, "errors corrected");
    }

    #[test]
    fn test_persist_empty_output_still_calls_bootloader_but_no_log_dir() {
        // Empty output: boot env is still called (code != 0), but no log dir is created.
        let ods = make_ods_with(PartitionName::Data, FsckExitCode::ERRORS_UNCORRECTED, "");
        let temp = TempDir::new().unwrap();
        let mut bl = MockBootEnv::new();

        persist_fsck_results(&ods, Some(&mut bl), temp.path());

        assert_eq!(bl.saved_fsck_calls().lock().unwrap().len(), 1);
        // No log dir should be created for empty output.
        assert!(!temp.path().join("mnt/data/log/fsck").exists());
    }

    #[test]
    fn test_persist_multiple_partitions_only_nonzero_saved() {
        // Mix of zero and non-zero codes — only non-zero ones reach save_fsck_status.
        // The clean entries are inserted raw so they are present in the map and the
        // is_clean() guard is what excludes them.
        let mut ods = OdsStatus::new();
        insert_raw(&mut ods, PartitionName::Boot, 0, "clean");
        insert_raw(&mut ods, PartitionName::Etc, 0, "clean");
        ods.record_fsck_result(
            PartitionName::Data,
            FsckExitCode::CORRECTED,
            "errors corrected".to_string(),
        );
        ods.record_fsck_result(
            PartitionName::Cert,
            FsckExitCode::ERRORS_UNCORRECTED,
            "uncorrected errors".to_string(),
        );
        assert_eq!(ods.fsck.len(), 4, "all four must be in the map");

        let temp = TempDir::new().unwrap();
        let mut bl = MockBootEnv::new();

        persist_fsck_results(&ods, Some(&mut bl), temp.path());

        let saved = bl.saved_fsck_calls();
        let saved = saved.lock().unwrap();
        assert_eq!(saved.len(), 2);
        let saved_partitions: std::collections::HashSet<PartitionName> =
            saved.iter().map(|(p, _, _)| *p).collect();
        assert!(saved_partitions.contains(&PartitionName::Data));
        assert!(saved_partitions.contains(&PartitionName::Cert));
        assert!(!saved_partitions.contains(&PartitionName::Boot));
        assert!(!saved_partitions.contains(&PartitionName::Etc));
    }

    #[test]
    fn test_drain_moves_record_from_env_into_ods_and_clears_it() {
        // The round trip that gets a core-partition record into the JSON:
        // apply_boot_env_decision persists it to the env and drops it from
        // ods_status; the drain brings it back.
        let mut bl = MockBootEnv::new();
        bl.save_fsck_status(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected on pass 1",
        )
        .unwrap();
        let mut ods = OdsStatus::new();

        drain_fsck_env(&mut ods, Some(&mut bl));

        let record = ods.fsck.get(&PartitionName::Boot).unwrap();
        assert_eq!(record.code, 1);
        assert_eq!(record.output, "errors corrected on pass 1");
        assert_eq!(
            bl.get_fsck_status(PartitionName::Boot).unwrap(),
            None,
            "a reported record must not be drained again on the next boot"
        );
    }

    #[test]
    fn test_drain_keeps_this_boot_results_and_adds_stale_ones() {
        let mut bl = MockBootEnv::new();
        bl.save_fsck_status(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "stale from a reboot",
        )
        .unwrap();
        let mut ods = make_ods_with(
            PartitionName::Data,
            FsckExitCode::ERRORS_UNCORRECTED,
            "uncorrected errors",
        );

        drain_fsck_env(&mut ods, Some(&mut bl));

        assert_eq!(ods.fsck.len(), 2);
        assert_eq!(ods.fsck.get(&PartitionName::Data).unwrap().code, 4);
        assert_eq!(ods.fsck.get(&PartitionName::Boot).unwrap().code, 1);
    }

    #[test]
    fn test_drain_without_bootloader_is_a_no_op() {
        // Degraded boot: this boot's records stay in ods_status for the JSON.
        let mut ods = make_ods_with(
            PartitionName::Data,
            FsckExitCode::CORRECTED,
            "errors corrected",
        );

        drain_fsck_env(&mut ods, None);

        assert_eq!(ods.fsck.len(), 1);
    }

    #[test]
    fn test_drain_clears_an_undecodable_record() {
        let mut ods = OdsStatus::new();
        let mut bl = MockBootEnv::new().with_undecodable_fsck(PartitionName::Data);

        drain_fsck_env(&mut ods, Some(&mut bl));

        assert!(ods.fsck.is_empty());
        assert_eq!(bl.get_fsck_status(PartitionName::Data).unwrap(), None);
    }

    #[test]
    fn test_drain_read_failure_does_not_abort() {
        let mut ods = OdsStatus::new();
        let mut bl = MockBootEnv::new().with_get_fsck_error();

        // Must not panic.
        drain_fsck_env(&mut ods, Some(&mut bl));

        assert!(ods.fsck.is_empty());
    }

    #[test]
    fn test_persist_bootloader_save_failure_does_not_abort() {
        // A failing boot env write must not panic or propagate — it is non-fatal.
        let ods = make_ods_with(
            PartitionName::Boot,
            FsckExitCode::REBOOT_REQUIRED,
            "reboot required",
        );
        let temp = TempDir::new().unwrap();
        let mut bl = MockBootEnv::new().with_save_fsck_error();

        // Must not panic.
        persist_fsck_results(&ods, Some(&mut bl), temp.path());
    }

    #[test]
    fn test_persist_data_not_mounted_no_log_dir_created() {
        // When data partition is not mounted (normal in tests), no log dir is created.
        let ods = make_ods_with(PartitionName::Boot, FsckExitCode::CORRECTED, "some output");
        let temp = TempDir::new().unwrap();
        let mut bl = MockBootEnv::new();

        persist_fsck_results(&ods, Some(&mut bl), temp.path());

        // BootEnv was still called.
        assert_eq!(bl.saved_fsck_calls().lock().unwrap().len(), 1);
        // But log dir must not be created (data not mounted).
        assert!(!temp.path().join("mnt/data/log/fsck").exists());
    }

    #[test]
    fn test_persist_none_bootloader_skips_save_fsck_status() {
        // In degraded mode persist_fsck_results is called with None. The function
        // must not attempt any boot env write. Passing None (not a mock) means
        // the type system statically prevents any save_fsck_status call — this
        // test documents the contract and catches any future refactor that
        // re-introduces an implicit fallback or null-object pattern.
        let ods = make_ods_with(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected",
        );
        let temp = TempDir::new().unwrap();

        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();
        crate::logging::capture::start_capture();
        // Must not panic; no boot env write; no log dir (data not mounted).
        persist_fsck_results(&ods, None, temp.path());
        let lines = crate::logging::capture::take_capture();
        assert!(!temp.path().join("mnt/data/log/fsck").exists());
        let line = "[WARN] Bootloader env unavailable; fsck status for boot not saved to it";
        assert!(lines.iter().any(|l| l == line), "{line:?} not in {lines:?}");
    }

    #[test]
    #[cfg(feature = "factory-reset")]
    fn mount_tracked_partition_is_noop_when_absent_from_layout() {
        use crate::partition::RootDevice;
        use std::collections::HashMap;

        let layout = PartitionLayout {
            partitions: HashMap::new(),
            device: RootDevice {
                base: PathBuf::from("/dev/sda"),
                partition_sep: "",
                root_partition: PathBuf::from("/dev/sda2"),
            },
        };
        let mut ods_status = OdsStatus::new();
        let mut mounts: Vec<PathBuf> = Vec::new();

        let result = mount_tracked_partition(
            &layout,
            PartitionMountSpec {
                partition: PartitionName::Factory,
                mount_point: mount_points::FACTORY_PARTITION,
                options: MountOptions::ext4_readonly(),
                fstype: FsType::Ext4,
            },
            Path::new("/nonexistent"),
            &mut ods_status,
            &mut mounts,
        );

        assert!(result.is_ok());
        assert!(mounts.is_empty());
    }
}
