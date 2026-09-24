//! Flash mode 1: clone the running disk onto a second block device.
//!
//! The sequence is destructive on the destination only, so a failure costs the
//! clone and nothing else. The clone leaves the destination in the state a
//! freshly flashed image has: a shipped-size data partition, empty `etc` and
//! `data` filesystems, and a default bootloader environment.
//!
//! There is no destructive write to the source, but three deliberate writes do
//! reach it, each required: clearing the flash triggers in the source bootloader environment, mounting the source
//! `data` partition read-write to persist the run log, and, on an EFI machine,
//! rewriting the running machine's NVRAM boot entries.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::stat::{major, makedev, minor};

use crate::bootloader::sync_filesystems;
use crate::config::build;
use crate::error::FlashError;
#[cfg(feature = "grub")]
use crate::filesystem::MountOptions;
use crate::filesystem::reformat_ext4;
#[cfg(feature = "grub")]
use crate::mode::flash::with_mount;
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

pub(crate) const CONST_DATA_SIZE: &str = "DATA_SIZE";
const CONST_BOOTLOADER_START: &str = "BOOTLOADER_START";
const CONST_UBOOT_ENV1_START: &str = "UBOOT_ENV1_START";
#[cfg(feature = "uboot")]
const CONST_UBOOT_ENV2_START: &str = "UBOOT_ENV2_START";
#[cfg(feature = "uboot")]
const CONST_UBOOT_ENV_SIZE: &str = "UBOOT_ENV_SIZE";

/// Names the operation of a `FlashError::PartitionTable` raised while looking
/// a source partition up in the layout.
const LAYOUT_LOOKUP_OPERATION: &str = "lookup";

/// Why a destination was refused.
const REASON_IDENTICAL_DISK: &str = "identical to the booted disk";
const REASON_SOURCE_PARTITION: &str = "a partition of the booted disk";
const REASON_NOT_A_BLOCK_DEVICE: &str = "not a block device";
const REASON_UNKNOWN_DISK: &str = "the disk it belongs to is unknown to sysfs";

/// Block devices by device number, each a link to the device's sysfs
/// directory.
const SYS_DEV_BLOCK: &str = "/sys/dev/block";

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
    if build::UBOOT_ENV_SIZE.is_none() {
        return Err(FlashError::MissingBuildConstant(CONST_UBOOT_ENV_SIZE));
    }

    // Two independent conditions make the offset mandatory: U-Boot keeps its
    // environment there, and a machine reserving a bootloader area needs it as
    // that area's end. Both are checked here rather than where the area is
    // copied, so a build missing the constant fails before the destination has
    // been repartitioned.
    if (cfg!(feature = "uboot") || build::BOOTLOADER_START.is_some())
        && build::UBOOT_ENV1_START.is_none()
    {
        return Err(FlashError::MissingBuildConstant(CONST_UBOOT_ENV1_START));
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

/// The device number of `path`, which has to be a block device.
fn block_devnum(path: &Path) -> Result<u64, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("cannot stat: {e}"))?;
    if !metadata.file_type().is_block_device() {
        return Err(REASON_NOT_A_BLOCK_DEVICE.to_string());
    }
    Ok(metadata.rdev())
}

/// The device number of the whole disk `devnum` sits on: `devnum` itself for
/// a disk, the parent disk for a partition. `None` when sysfs does not list it.
fn whole_disk_devnum(sys_dev_block: &Path, devnum: u64) -> Option<u64> {
    let node = sys_dev_block.join(format!("{}:{}", major(devnum), minor(devnum)));
    if !node.exists() {
        return None;
    }
    if !node.join("partition").exists() {
        return Some(devnum);
    }
    // The kernel resolves `..` after following the link, so this reads the
    // `dev` file of the parent disk's directory.
    let parent = fs::read_to_string(node.join("../dev")).ok()?;
    let (parent_major, parent_minor) = parent.trim().split_once(':')?;
    Some(makedev(
        parent_major.parse().ok()?,
        parent_minor.parse().ok()?,
    ))
}

/// Why a destination on `destination_disk` is refused for `source`, if it is.
fn refusal(destination: u64, destination_disk: Option<u64>, source: u64) -> Option<&'static str> {
    if destination == source {
        return Some(REASON_IDENTICAL_DISK);
    }
    match destination_disk {
        None => Some(REASON_UNKNOWN_DISK),
        Some(disk) if disk == source => Some(REASON_SOURCE_PARTITION),
        Some(_) => None,
    }
}

/// Refuse a destination that is not a block device, that is the disk the
/// device booted from, or that is one of that disk's partitions.
///
/// Device numbers are compared, so every spelling of the booted disk is
/// caught. Writing a partition table into a partition of the running disk
/// would damage the source before the next step could notice.
pub fn validate_destination(destination: &Path, source: &Path) -> Result<(), FlashError> {
    validate_devices(Path::new(SYS_DEV_BLOCK), destination, source)
}

fn validate_devices(
    sys_dev_block: &Path,
    destination: &Path,
    source: &Path,
) -> Result<(), FlashError> {
    let invalid = |reason: String| FlashError::InvalidDestination {
        device: destination.to_path_buf(),
        reason,
    };

    let destination_dev = block_devnum(destination).map_err(invalid)?;
    let source_dev = block_devnum(source)
        .map_err(|reason| invalid(format!("the booted disk {}: {reason}", source.display())))?;

    match refusal(
        destination_dev,
        whole_disk_devnum(sys_dev_block, destination_dev),
        source_dev,
    ) {
        Some(reason) => Err(invalid(format!("{reason} {}", source.display()))),
        None => Ok(()),
    }
}

/// Poll for `destination` until it exists or `timeout` has passed.
fn wait_for_block_device(destination: &Path, timeout: Duration) -> Result<(), FlashError> {
    let start = Instant::now();
    let mut announced = false;
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
        if !announced {
            log::info!(
                "waiting up to {}s for the destination block device {}",
                timeout.as_secs(),
                destination.display()
            );
            announced = true;
        }
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
    with_mount(
        &destination_partition(destination, PARTITION_NUM_BOOT),
        Path::new(BOOT_ENV_MOUNT_POINT),
        MountOptions::vfat(),
        |boot_mount| copy_grubenv(&boot_mount.join(GRUBENV_TARGET)),
    )
}

/// The byte size of one U-Boot environment and the byte offset of every bank
/// the machine reserves.
#[cfg(feature = "uboot")]
fn uboot_env_banks(constants: &Constants) -> Result<(u64, Vec<u64>), FlashError> {
    let size_kb = constants
        .uboot_env_size
        .ok_or(FlashError::MissingBuildConstant(CONST_UBOOT_ENV_SIZE))?;
    let first_kb = constants
        .uboot_env1_start
        .ok_or(FlashError::MissingBuildConstant(CONST_UBOOT_ENV1_START))?;

    let mut offsets = vec![kb_to_bytes(first_kb, CONST_UBOOT_ENV1_START)?];
    if let Some(second_kb) = constants.uboot_env2_start {
        offsets.push(kb_to_bytes(second_kb, CONST_UBOOT_ENV2_START)?);
    }

    Ok((kb_to_bytes(size_kb, CONST_UBOOT_ENV_SIZE)?, offsets))
}

/// Put the default U-Boot environment in every bank the machine reserves.
#[cfg(feature = "uboot")]
fn write_boot_env(constants: &Constants, destination: &Path) -> Result<(), FlashError> {
    let (size, offsets) = uboot_env_banks(constants)?;

    for offset in offsets {
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

    log::info!(
        "flash mode 1: cloning {} onto {}",
        source.display(),
        ctx.destination.display()
    );

    // A destination owned by the source always exists already, so the wait
    // returns at once for it and the refusal below is not delayed.
    wait_for_block_device(ctx.destination, DEST_DEVICE_WAIT)?;

    // Everything below addresses the destination by the resolved path,
    // because `destination_partition` appends a partition index to it and an
    // alias such as a by-id link names no partition of its own.
    let destination =
        fs::canonicalize(ctx.destination).map_err(|e| FlashError::InvalidDestination {
            device: ctx.destination.to_path_buf(),
            reason: format!("cannot resolve: {e}"),
        })?;
    let destination = destination.as_path();
    log::info!("destination resolved to {}", destination.display());

    validate_destination(destination, source)?;

    // `e2image` below must not read a mounted filesystem, and the raw boot
    // copy must not read one either.
    unmount::unmount_sysroot(ctx.rootfs)?;

    let dump = sfdisk::dump(source)?;
    let rewritten = sfdisk::rewrite_dump(&dump, constants.data_size)?;
    sfdisk::apply(destination, &rewritten)?;
    verify_destination_partitions(destination)?;

    copy_bootloader_area(&constants, source, destination)?;

    // Empty `etc` and `data` filesystems are what put the clone into the
    // first-boot condition.
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
    sync_filesystems();
    Ok(())
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

    // The two tests below pin DEST_PARTITION_ROLES against literal indices on
    // purpose: the list has to stay in step with the reformat calls, the copy
    // loop and the layout constants, and a pin written in terms of those same
    // constants would follow any of them silently.

    #[cfg(feature = "gpt")]
    #[test]
    fn the_roles_the_clone_writes_are_pinned_for_gpt() {
        assert_eq!(DEST_PARTITION_ROLES, [1, 2, 3, 4, 5, 6, 7]);
    }

    #[cfg(feature = "dos")]
    #[test]
    fn the_roles_the_clone_writes_are_pinned_for_dos() {
        assert_eq!(DEST_PARTITION_ROLES, [1, 2, 3, 5, 6, 7, 8]);
        assert!(
            !DEST_PARTITION_ROLES.contains(&crate::partition::layout::PARTITION_NUM_EXTENDED),
            "the extended container holds none of the roles the clone writes"
        );
    }

    const SDA: u64 = makedev(8, 0);
    const SDA2: u64 = makedev(8, 2);
    const SDB: u64 = makedev(8, 16);

    #[test]
    fn the_booted_disk_itself_is_refused() {
        assert_eq!(refusal(SDA, Some(SDA), SDA), Some(REASON_IDENTICAL_DISK));
    }

    #[test]
    fn a_partition_of_the_booted_disk_is_refused() {
        assert_eq!(refusal(SDA2, Some(SDA), SDA), Some(REASON_SOURCE_PARTITION));
    }

    #[test]
    fn another_disk_is_accepted() {
        assert_eq!(refusal(SDB, Some(SDB), SDA), None);
    }

    #[test]
    fn a_destination_sysfs_does_not_list_is_refused() {
        assert_eq!(refusal(SDB, None, SDA), Some(REASON_UNKNOWN_DISK));
    }

    /// A sysfs tree with `mmcblk1` (179:0) and its partition `mmcblk1p2`
    /// (179:2) linked from `dev/block`, the way the kernel lays it out.
    fn fake_sys_dev_block() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let disk = root.path().join("devices/block/mmcblk1");
        let partition = disk.join("mmcblk1p2");
        fs::create_dir_all(&partition).unwrap();
        fs::write(disk.join("dev"), "179:0\n").unwrap();
        fs::write(partition.join("dev"), "179:2\n").unwrap();
        fs::write(partition.join("partition"), "2\n").unwrap();

        let by_number = root.path().join("dev/block");
        fs::create_dir_all(&by_number).unwrap();
        std::os::unix::fs::symlink(&disk, by_number.join("179:0")).unwrap();
        std::os::unix::fs::symlink(&partition, by_number.join("179:2")).unwrap();
        root
    }

    #[test]
    fn a_partition_maps_onto_its_parent_disk() {
        let sys = fake_sys_dev_block();
        let by_number = sys.path().join("dev/block");
        assert_eq!(
            whole_disk_devnum(&by_number, makedev(179, 2)),
            Some(makedev(179, 0))
        );
    }

    #[test]
    fn a_disk_maps_onto_itself() {
        let sys = fake_sys_dev_block();
        let by_number = sys.path().join("dev/block");
        assert_eq!(
            whole_disk_devnum(&by_number, makedev(179, 0)),
            Some(makedev(179, 0))
        );
    }

    #[test]
    fn a_device_sysfs_does_not_list_has_no_disk() {
        let sys = fake_sys_dev_block();
        let by_number = sys.path().join("dev/block");
        assert_eq!(whole_disk_devnum(&by_number, makedev(179, 8)), None);
    }

    #[test]
    fn a_destination_that_is_not_a_block_device_is_refused() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = validate_destination(file.path(), Path::new("/dev/sda")).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidDestination { reason, .. }
                if reason == REASON_NOT_A_BLOCK_DEVICE),
            "got {err}"
        );
    }

    #[test]
    fn a_destination_present_when_the_wait_runs_out_counts_as_found() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(wait_for_block_device(file.path(), Duration::ZERO).is_ok());
    }

    #[test]
    fn a_destination_that_never_appears_times_out() {
        let err = wait_for_block_device(Path::new("/nonexistent/destination"), Duration::ZERO)
            .unwrap_err();
        assert!(
            matches!(err, FlashError::DestinationTimeout { .. }),
            "got {err}"
        );
    }

    #[cfg(feature = "uboot")]
    fn uboot_constants(uboot_env2_start: Option<u64>) -> Constants {
        Constants {
            data_size: 524_288,
            uboot_env1_start: Some(4096),
            uboot_env2_start,
            uboot_env_size: Some(64),
            bootloader_start: None,
        }
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn uboot_env_size_and_offsets_are_scaled_from_kb_to_bytes() {
        assert_eq!(
            uboot_env_banks(&uboot_constants(Some(8192))).unwrap(),
            (65_536, vec![4_194_304, 8_388_608])
        );
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn a_machine_without_a_second_env_bank_gets_one_write() {
        assert_eq!(
            uboot_env_banks(&uboot_constants(None)).unwrap(),
            (65_536, vec![4_194_304])
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
