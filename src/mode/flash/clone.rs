//! Flash mode 1: clone the running disk onto a second block device.
//!
//! The sequence is destructive on the destination and never writes the
//! source, so a failure costs the clone and nothing else. The clone leaves
//! the destination in the state a freshly flashed image has: a shipped-size
//! data partition, empty `etc` and `data` filesystems, and a default
//! bootloader environment.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::build;
use crate::error::FlashError;
use crate::filesystem::reformat_ext4;
#[cfg(feature = "grub")]
use crate::filesystem::{MountOptions, MountPoint, mount, umount};
use crate::mode::flash::{efi, rawio, sfdisk, unmount};
use crate::partition::device::partition_sep_for;
use crate::partition::layout::{
    PARTITION_NUM_BOOT, PARTITION_NUM_CERT, PARTITION_NUM_DATA, PARTITION_NUM_ETC,
    PARTITION_NUM_FACTORY, PARTITION_NUM_ROOT_A, PARTITION_NUM_ROOT_B,
};
use crate::partition::{PartitionLayout, PartitionName, RootDevice};

/// Bound for the destination block device to show up. Inclusive: a device
/// present on the last poll counts as found.
const DEST_DEVICE_WAIT: Duration = Duration::from_secs(30);
const DEST_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);

const E2IMAGE_CMD: &str = "/sbin/e2image";
const E2IMAGE_RAW_FLAG: &str = "-ra";
const E2IMAGE_PROGRESS_FLAG: &str = "-p";

/// ext4 volume labels the clone's `etc` and `data` filesystems get.
const DATA_PARTITION_LABEL: &str = "data";
const ETC_PARTITION_LABEL: &str = "etc";

#[cfg(feature = "grub")]
const GRUBENV_SOURCE: &str = "/etc/omnect/grubenv.in";
#[cfg(feature = "grub")]
const GRUBENV_TARGET: &str = "EFI/BOOT/grubenv";
/// Scratch mount point for the destination boot partition while the default
/// GRUB environment is written.
#[cfg(feature = "grub")]
const BOOT_ENV_MOUNT_POINT: &str = "/tmp/clone-boot";

#[cfg(feature = "uboot")]
const UBOOT_ENV_SOURCE: &str = "/etc/omnect/uboot-env.bin";

const CONST_DATA_SIZE: &str = "DATA_SIZE";
const CONST_BOOTLOADER_START: &str = "BOOTLOADER_START";
const CONST_UBOOT_ENV1_START: &str = "UBOOT_ENV1_START";
#[cfg(feature = "uboot")]
const CONST_UBOOT_ENV2_START: &str = "UBOOT_ENV2_START";
#[cfg(feature = "uboot")]
const CONST_UBOOT_ENV_SIZE: &str = "UBOOT_ENV_SIZE";

/// Names the operation of a `FlashError::PartitionTable` raised while looking
/// a source partition up in the layout.
const LAYOUT_LOOKUP_OPERATION: &str = "lookup";

/// Why a destination was refused. Shared with the tests, which assert on the
/// condition an error reports rather than only on its presence.
const REASON_NO_DESTINATION: &str = "no destination given";
const REASON_IDENTICAL_DISK: &str = "identical to the booted disk";
const REASON_SOURCE_PARTITION: &str = "a partition of the booted disk";
const REASON_NOT_A_BLOCK_DEVICE: &str = "not a block device";

/// The roles the clone writes, and therefore the destination partitions that
/// have to exist once the rewritten table has been applied. A DOS extended
/// container holds none of them, so its node is not required.
const DEST_PARTITION_ROLES: [u32; 7] = [
    PARTITION_NUM_BOOT,
    PARTITION_NUM_ROOT_A,
    PARTITION_NUM_ROOT_B,
    PARTITION_NUM_FACTORY,
    PARTITION_NUM_CERT,
    PARTITION_NUM_ETC,
    PARTITION_NUM_DATA,
];

/// Everything mode 1 needs from the running disk and the image it came from.
pub struct CloneCtx<'a> {
    pub destination: &'a Path,
    pub layout: &'a PartitionLayout,
    pub rootfs: &'a Path,
    pub machine_features: &'a str,
}

/// The build-time constants mode 1 reads, once they are known to be present.
pub struct Constants {
    pub data_size: u64,
    pub uboot_env1_start: Option<u64>,
    pub uboot_env2_start: Option<u64>,
    pub uboot_env_size: Option<u64>,
    pub bootloader_start: Option<u64>,
}

/// Read the build-time constants, failing on any the machine must define.
///
/// `UBOOT_ENV2_START` stays optional: its absence is how a machine says it
/// reserves no second environment bank.
pub fn required_constants() -> Result<Constants, FlashError> {
    let data_size = build::DATA_SIZE.ok_or(FlashError::MissingBuildConstant(CONST_DATA_SIZE))?;

    #[cfg(feature = "uboot")]
    {
        if build::UBOOT_ENV1_START.is_none() {
            return Err(FlashError::MissingBuildConstant(CONST_UBOOT_ENV1_START));
        }
        if build::UBOOT_ENV_SIZE.is_none() {
            return Err(FlashError::MissingBuildConstant(CONST_UBOOT_ENV_SIZE));
        }
    }

    Ok(Constants {
        data_size,
        uboot_env1_start: build::UBOOT_ENV1_START,
        uboot_env2_start: build::UBOOT_ENV2_START,
        uboot_env_size: build::UBOOT_ENV_SIZE,
        bootloader_start: build::BOOTLOADER_START,
    })
}

/// The length in KB of the bootloader area to copy, or `None` when the
/// machine keeps no bootloader outside the boot partition.
///
/// The area reaches up to the first U-Boot environment, so a machine that
/// declares a start without that offset is missing a constant rather than
/// opting out.
pub fn bootloader_area_len(
    bootloader_start: Option<u64>,
    uboot_env1_start: Option<u64>,
) -> Result<Option<u64>, FlashError> {
    let Some(start) = bootloader_start else {
        return Ok(None);
    };

    let end = uboot_env1_start.ok_or(FlashError::MissingBuildConstant(CONST_UBOOT_ENV1_START))?;

    let len = end
        .checked_sub(start)
        .ok_or_else(|| FlashError::InvalidBuildConstant {
            name: CONST_UBOOT_ENV1_START,
            reason: format!("{end} KB lies below the bootloader area start {start} KB"),
        })?;

    Ok(Some(len))
}

/// The destination addressed as a disk the clone's partition roles sit on.
///
/// The clone's rootfs always lands in rootA, which is what makes that the
/// destination's root partition.
fn destination_device(destination: &Path) -> RootDevice {
    let mut device = RootDevice {
        base: destination.to_path_buf(),
        partition_sep: partition_sep_for(destination),
        root_partition: PathBuf::new(),
    };
    device.root_partition = device.partition_path(PARTITION_NUM_ROOT_A);
    device
}

/// The path of partition `num` on `destination`.
///
/// The destination receives a copy of the source table, so every role sits at
/// the same index on both disks.
pub fn destination_partition(destination: &Path, num: u32) -> PathBuf {
    destination_device(destination).partition_path(num)
}

/// Resolve a path as far as the filesystem allows, keeping it as given when
/// it cannot be resolved. An unresolvable path still has to take part in the
/// source comparison; the block-device check rejects it afterwards.
fn resolve(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Refuse a destination that is empty, that is the disk the device booted
/// from, or that is one of that disk's partitions.
///
/// Both sides of the comparison are resolved first, so a symlink or an alias
/// spelling of the running disk is caught too. Writing a partition table into
/// a partition of the running disk would damage the source before the next
/// step could notice, so the partition case is refused here rather than left
/// to the block-device checks.
///
/// Cheap enough to run before the device wait, which is what lets an unset
/// destination be reported by name instead of after the full timeout.
pub fn validate_destination_path(destination: &Path, source: &Path) -> Result<(), FlashError> {
    let invalid = |reason: String| FlashError::InvalidDestination {
        device: destination.to_path_buf(),
        reason,
    };

    if destination.as_os_str().is_empty() {
        return Err(invalid(REASON_NO_DESTINATION.to_string()));
    }

    let resolved_destination = resolve(destination);
    let resolved_source = resolve(source);
    let destination_name = resolved_destination.to_string_lossy();
    let source_name = resolved_source.to_string_lossy();

    if let Some(rest) = destination_name.strip_prefix(source_name.as_ref())
        && unmount::is_disk_or_partition_suffix(rest)
    {
        let what = if rest.is_empty() {
            REASON_IDENTICAL_DISK
        } else {
            REASON_SOURCE_PARTITION
        };
        return Err(invalid(format!("{what} {}", source.display())));
    }

    Ok(())
}

/// Everything `validate_destination_path` refuses, plus a destination that is
/// not a block device.
pub fn validate_destination(destination: &Path, source: &Path) -> Result<(), FlashError> {
    validate_destination_path(destination, source)?;

    let invalid = |reason: String| FlashError::InvalidDestination {
        device: destination.to_path_buf(),
        reason,
    };

    let metadata = fs::metadata(destination).map_err(|e| invalid(format!("cannot stat: {e}")))?;
    if !metadata.file_type().is_block_device() {
        return Err(invalid(REASON_NOT_A_BLOCK_DEVICE.to_string()));
    }

    Ok(())
}

/// Poll for `destination` until it exists or `timeout` has passed.
fn wait_for_block_device(destination: &Path, timeout: Duration) -> Result<(), FlashError> {
    let start = Instant::now();
    loop {
        if destination.exists() {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(FlashError::DestinationTimeout {
                device: destination.to_path_buf(),
                secs: timeout.as_secs(),
            });
        }
        log::info!(
            "waiting for the destination block device {}",
            destination.display()
        );
        thread::sleep(DEST_DEVICE_POLL_INTERVAL);
    }
}

/// Fail unless every role the clone writes has a block device on the
/// destination.
fn verify_destination_partitions(destination: &Path) -> Result<(), FlashError> {
    for num in DEST_PARTITION_ROLES {
        let path = destination_partition(destination, num);
        let is_block = fs::metadata(&path)
            .map(|m| m.file_type().is_block_device())
            .unwrap_or(false);
        if !is_block {
            return Err(FlashError::InvalidDestination {
                device: path,
                reason: "the applied partition table produced no block device here".to_string(),
            });
        }
    }
    Ok(())
}

fn source_partition(layout: &PartitionLayout, name: PartitionName) -> Result<&Path, FlashError> {
    layout
        .get(name)
        .map(PathBuf::as_path)
        .ok_or_else(|| FlashError::PartitionTable {
            device: layout.device.base.clone(),
            operation: LAYOUT_LOOKUP_OPERATION.to_string(),
            reason: format!("the source layout has no {name} partition"),
        })
}

/// Build-time offsets and sizes are KB-valued; block devices are addressed in
/// bytes.
fn kb_to_bytes(kb: u64, name: &'static str) -> Result<u64, FlashError> {
    kb.checked_mul(rawio::KIB)
        .ok_or_else(|| FlashError::InvalidBuildConstant {
            name,
            reason: format!("{kb} KB does not fit a byte offset"),
        })
}

/// Copy the bootloader a machine keeps outside the boot partition, at the
/// same offset on both disks.
fn copy_bootloader_area(
    constants: &Constants,
    source: &Path,
    destination: &Path,
) -> Result<(), FlashError> {
    let len_kb = bootloader_area_len(constants.bootloader_start, constants.uboot_env1_start)?;
    let (Some(start_kb), Some(len_kb)) = (constants.bootloader_start, len_kb) else {
        return Ok(());
    };

    let offset = kb_to_bytes(start_kb, CONST_BOOTLOADER_START)?;
    let len = kb_to_bytes(len_kb, CONST_UBOOT_ENV1_START)?;

    log::info!("copying the {len_kb} KB bootloader area at {offset} bytes");
    rawio::copy_range(source, offset, destination, offset, Some(len))?;
    Ok(())
}

/// Image the running rootfs into the clone's rootA.
fn copy_rootfs(layout: &PartitionLayout, destination: &Path) -> Result<(), FlashError> {
    let src = layout.root_current();
    let dst = destination_partition(destination, PARTITION_NUM_ROOT_A);
    log::info!("imaging {} onto {}", src.display(), dst.display());

    let copy_failed = |reason: String| FlashError::CopyFailed {
        src: src.clone(),
        dst: dst.clone(),
        reason,
    };

    // Inherited stdio: this is the longest step of the sequence, and the
    // progress flag is only worth passing if the output reaches the console.
    let status = Command::new(E2IMAGE_CMD)
        .args([E2IMAGE_RAW_FLAG, E2IMAGE_PROGRESS_FLAG])
        .arg(&src)
        .arg(&dst)
        .status()
        .map_err(|e| copy_failed(format!("failed to run {E2IMAGE_CMD}: {e}")))?;

    if !status.success() {
        return Err(copy_failed(format!("{E2IMAGE_CMD} failed ({status})")));
    }

    Ok(())
}

/// Give the clone's boot and rootA partitions identities of their own, so a
/// host that sees both disks can tell them apart.
#[cfg(feature = "gpt")]
fn assign_fresh_partition_uuids(destination: &Path) -> Result<(), FlashError> {
    for num in [PARTITION_NUM_BOOT, PARTITION_NUM_ROOT_A] {
        let uuid = uuid::Uuid::new_v4().to_string();
        log::info!(
            "assigning partition {num} on {} the UUID {uuid}",
            destination.display()
        );
        sfdisk::set_part_uuid(destination, num, &uuid)?;
    }
    Ok(())
}

/// A DOS partition table carries no per-partition UUID to assign.
#[cfg(feature = "dos")]
fn assign_fresh_partition_uuids(_destination: &Path) -> Result<(), FlashError> {
    Ok(())
}

#[cfg(feature = "grub")]
fn copy_grubenv(target: &Path) -> Result<(), FlashError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(GRUBENV_SOURCE, target).map_err(|e| FlashError::BootEnvWriteFailed {
        device: target.to_path_buf(),
        reason: format!("copying {GRUBENV_SOURCE}: {e}"),
    })?;
    Ok(())
}

/// Put the default GRUB environment on the clone's boot partition.
#[cfg(feature = "grub")]
fn write_boot_env(_constants: &Constants, destination: &Path) -> Result<(), FlashError> {
    let boot = destination_partition(destination, PARTITION_NUM_BOOT);
    fs::create_dir_all(BOOT_ENV_MOUNT_POINT)?;
    mount(MountPoint::new(
        &boot,
        BOOT_ENV_MOUNT_POINT,
        MountOptions::vfat(),
    ))?;

    // The mount must not survive any failure below, so both the success and
    // the error path go through the same unmount call.
    let result = copy_grubenv(&Path::new(BOOT_ENV_MOUNT_POINT).join(GRUBENV_TARGET));
    if let Err(e) = umount(Path::new(BOOT_ENV_MOUNT_POINT)) {
        if result.is_ok() {
            return Err(FlashError::from(e));
        }
        log::warn!("also failed to unmount {BOOT_ENV_MOUNT_POINT} after a boot-env error: {e}");
    }
    result
}

/// Put the default U-Boot environment in every bank the machine reserves.
#[cfg(feature = "uboot")]
fn write_boot_env(constants: &Constants, destination: &Path) -> Result<(), FlashError> {
    let size_kb = constants
        .uboot_env_size
        .ok_or(FlashError::MissingBuildConstant(CONST_UBOOT_ENV_SIZE))?;
    let size = kb_to_bytes(size_kb, CONST_UBOOT_ENV_SIZE)?;
    let first = constants
        .uboot_env1_start
        .ok_or(FlashError::MissingBuildConstant(CONST_UBOOT_ENV1_START))?;

    let mut banks = vec![(CONST_UBOOT_ENV1_START, first)];
    if let Some(second) = constants.uboot_env2_start {
        banks.push((CONST_UBOOT_ENV2_START, second));
    }

    for (name, start_kb) in banks {
        let offset = kb_to_bytes(start_kb, name)?;
        log::info!(
            "writing {UBOOT_ENV_SOURCE} at {offset} bytes on {}",
            destination.display()
        );
        rawio::copy_range(
            Path::new(UBOOT_ENV_SOURCE),
            0,
            destination,
            offset,
            Some(size),
        )?;
    }

    Ok(())
}

/// Clone the running disk onto `ctx.destination`.
pub fn run_clone(ctx: &CloneCtx<'_>) -> Result<(), FlashError> {
    let constants = required_constants()?;
    let source = ctx.layout.device.base.as_path();
    let destination = ctx.destination;

    log::info!(
        "flash mode 1: cloning {} onto {}",
        source.display(),
        destination.display()
    );

    // Ahead of the wait: an unset or source-owned destination is a
    // misconfiguration the operator should hear about at once, not after the
    // full timeout has run down.
    validate_destination_path(destination, source)?;
    wait_for_block_device(destination, DEST_DEVICE_WAIT)?;
    validate_destination(destination, source)?;

    // `e2image` below must not read a mounted filesystem, and the raw boot
    // copy must not read one either.
    unmount::unmount_sysroot(ctx.rootfs)?;

    let dump = sfdisk::dump(source)?;
    let rewritten = sfdisk::rewrite_dump(&dump, constants.data_size)?;
    sfdisk::apply(destination, &rewritten)?;
    verify_destination_partitions(destination)?;

    copy_bootloader_area(&constants, source, destination)?;

    // Reformatting before the remaining copies keeps the legacy order. It is
    // also what puts the clone into the first-boot condition.
    reformat_ext4(
        &destination_partition(destination, PARTITION_NUM_ETC),
        ETC_PARTITION_LABEL,
    )?;
    reformat_ext4(
        &destination_partition(destination, PARTITION_NUM_DATA),
        DATA_PARTITION_LABEL,
    )?;

    for (name, num) in [
        (PartitionName::Boot, PARTITION_NUM_BOOT),
        (PartitionName::Factory, PARTITION_NUM_FACTORY),
        (PartitionName::Cert, PARTITION_NUM_CERT),
    ] {
        let src = source_partition(ctx.layout, name)?;
        let dst = destination_partition(destination, num);
        log::info!("copying {} onto {}", src.display(), dst.display());
        rawio::copy_range(src, 0, &dst, 0, None)?;
    }

    copy_rootfs(ctx.layout, destination)?;
    assign_fresh_partition_uuids(destination)?;

    write_boot_env(&constants, destination)?;

    efi::handle(
        destination,
        &destination_partition(destination, PARTITION_NUM_BOOT),
        ctx.machine_features,
    )?;

    log::info!("flash mode 1 finished");
    rawio::sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_partitions_use_the_same_indices_as_the_source() {
        // The destination gets a copy of the source table, so the roles map
        // onto the same indices on both disks.
        let dst = Path::new("/dev/sda");
        assert_eq!(
            destination_partition(dst, crate::partition::layout::PARTITION_NUM_BOOT),
            PathBuf::from("/dev/sda1")
        );
        let mmc = Path::new("/dev/mmcblk2");
        assert_eq!(
            destination_partition(mmc, crate::partition::layout::PARTITION_NUM_ROOT_A),
            PathBuf::from("/dev/mmcblk2p2")
        );
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn gpt_data_and_etc_land_on_the_gpt_indices() {
        let dst = Path::new("/dev/sda");
        assert_eq!(
            destination_partition(dst, crate::partition::layout::PARTITION_NUM_ETC),
            PathBuf::from("/dev/sda6")
        );
        assert_eq!(
            destination_partition(dst, crate::partition::layout::PARTITION_NUM_DATA),
            PathBuf::from("/dev/sda7")
        );
    }

    #[cfg(feature = "dos")]
    #[test]
    fn dos_data_and_etc_land_on_the_dos_indices() {
        let dst = Path::new("/dev/sda");
        assert_eq!(
            destination_partition(dst, crate::partition::layout::PARTITION_NUM_ETC),
            PathBuf::from("/dev/sda7")
        );
        assert_eq!(
            destination_partition(dst, crate::partition::layout::PARTITION_NUM_DATA),
            PathBuf::from("/dev/sda8")
        );
    }

    /// The `reason` of an `InvalidDestination`, or a panic naming what came
    /// back instead.
    fn refusal_reason(result: Result<(), FlashError>) -> String {
        match result {
            Err(FlashError::InvalidDestination { reason, .. }) => reason,
            other => panic!("expected the destination to be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_destination_equal_to_the_source_is_refused() {
        let src = Path::new("/dev/mmcblk0");
        let reason = refusal_reason(validate_destination(src, src));
        assert!(
            reason.contains(REASON_IDENTICAL_DISK),
            "the refusal must name the identical-disk condition, got: {reason}"
        );
    }

    #[test]
    fn an_alias_spelling_of_the_source_is_resolved_and_refused() {
        // A symlink is the spelling an operator is most likely to reach for,
        // and only resolving both sides catches it.
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk");
        let alias = dir.path().join("alias");
        std::fs::write(&disk, b"").unwrap();
        std::os::unix::fs::symlink(&disk, &alias).unwrap();

        let reason = refusal_reason(validate_destination(&alias, &disk));
        assert!(
            reason.contains(REASON_IDENTICAL_DISK),
            "a symlink to the booted disk must be refused as the disk itself, got: {reason}"
        );
    }

    #[test]
    fn a_partition_of_the_source_is_refused_before_anything_is_written() {
        // Applying a partition table to a partition of the running disk would
        // damage the source, so this must not reach the block-device check.
        for (source, destination) in [
            ("/dev/sda", "/dev/sda2"),
            ("/dev/mmcblk0", "/dev/mmcblk0p2"),
        ] {
            let reason = refusal_reason(validate_destination_path(
                Path::new(destination),
                Path::new(source),
            ));
            assert!(
                reason.contains(REASON_SOURCE_PARTITION),
                "{destination} must be refused as a partition of {source}, got: {reason}"
            );
        }
    }

    #[test]
    fn a_neighbouring_disk_passes_the_path_checks() {
        // The prefix match must not swallow a legitimate destination.
        for (source, destination) in [("/dev/sda", "/dev/sdb"), ("/dev/sda", "/dev/sdab")] {
            assert!(
                validate_destination_path(Path::new(destination), Path::new(source)).is_ok(),
                "{destination} is not part of {source} and must pass"
            );
        }
    }

    #[test]
    fn an_unset_destination_is_refused_by_the_checks_that_run_before_the_wait() {
        let reason = refusal_reason(validate_destination_path(
            Path::new(""),
            Path::new("/dev/sda"),
        ));
        assert!(
            reason.contains(REASON_NO_DESTINATION),
            "an unset destination must be named as such, got: {reason}"
        );
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn the_bootloader_area_copy_needs_the_first_uboot_env_offset() {
        // BOOTLOADER_START alone is not enough: the copy length is
        // UBOOT_ENV1_START - BOOTLOADER_START.
        assert!(bootloader_area_len(Some(2048), None).is_err());
        assert!(bootloader_area_len(None, Some(4096)).unwrap().is_none());
        assert_eq!(
            bootloader_area_len(Some(2048), Some(4096)).unwrap(),
            Some(2048)
        );
    }
}
