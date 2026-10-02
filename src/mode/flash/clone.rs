//! Flash mode 1: clone the running disk onto a second block device.
//!
//! The clone leaves the destination in the state a freshly flashed image has:
//! a shipped-size data partition, empty `etc` and `data` filesystems, and a
//! default bootloader environment. On an EFI machine the sequence also
//! rewrites the running machine's NVRAM boot entries.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::bootloader::sync_filesystems;
use crate::config::{BuildConstant, build};
use crate::error::FlashError;
#[cfg(feature = "grub")]
use crate::filesystem::MountOptions;
use crate::filesystem::reformat_ext4;
#[cfg(feature = "grub")]
use crate::mode::flash::efi;
use crate::mode::flash::rawio::{self, ByteRange, kb_to_bytes};
use crate::mode::flash::{layout_partition, sfdisk, unmount};
#[cfg(feature = "grub")]
use crate::mode::flash::{scratch_mounts, with_mount};
use crate::partition::device::{
    REASON_NOT_A_BLOCK_DEVICE, SYS_DEV_BLOCK, block_devnum, partition_path, partition_sep_for,
    whole_disk_devnum,
};
use crate::partition::layout::{
    PARTITION_NUM_BOOT, PARTITION_NUM_CERT, PARTITION_NUM_DATA, PARTITION_NUM_ETC,
    PARTITION_NUM_FACTORY, PARTITION_NUM_ROOT_A, PARTITION_NUM_ROOT_B,
};
use crate::partition::{PartitionLayout, PartitionName};

const DEST_DEVICE_WAIT: Duration = Duration::from_secs(30);
const DEST_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);

const E2IMAGE_CMD: &str = "/sbin/e2image";
const E2IMAGE_RAW_FLAG: &str = "-ra";
const E2IMAGE_PROGRESS_FLAG: &str = "-p";
const E2IMAGE_READ_CHUNK: usize = 4096;
/// How much of the `e2image` output is kept for the error.
const E2IMAGE_TAIL_BYTES: usize = 4096;
const E2IMAGE_REASON_LINES: usize = 3;

const DATA_PARTITION_LABEL: &str = "data";
const ETC_PARTITION_LABEL: &str = "etc";

#[cfg(feature = "grub")]
const GRUBENV_SOURCE: &str = "/etc/omnect/grubenv.in";
#[cfg(feature = "grub")]
const GRUBENV_TARGET: &str = "EFI/BOOT/grubenv";

#[cfg(feature = "uboot")]
const UBOOT_ENV_SOURCE: &str = "/etc/omnect/uboot-env.bin";

#[cfg(feature = "gpt")]
const URANDOM_PATH: &str = "/dev/urandom";
#[cfg(feature = "gpt")]
const UUID_BYTES: usize = 16;

const REASON_IDENTICAL_DISK: &str = "identical to the booted disk";
const REASON_SOURCE_PARTITION: &str = "a partition of the booted disk";
const REASON_NOT_A_WHOLE_DISK: &str = "a partition, not a whole disk";
const REASON_UNKNOWN_DISK: &str = "the disk it belongs to is unknown to sysfs";
const REASON_NO_PARTITION_NODE: &str = "the applied partition table produced no block device here";

/// A DOS extended container holds none of these roles, so its node is not
/// required.
const DEST_PARTITION_ROLES: [u32; 7] = [
    PARTITION_NUM_BOOT,
    PARTITION_NUM_ROOT_A,
    PARTITION_NUM_ROOT_B,
    PARTITION_NUM_FACTORY,
    PARTITION_NUM_CERT,
    PARTITION_NUM_ETC,
    PARTITION_NUM_DATA,
];

pub(crate) struct CloneCtx<'a> {
    pub(crate) destination: &'a Path,
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
}

/// The build-time constants as `build.rs` generated them, KB-valued.
struct BuildConstants {
    data_size: Option<u64>,
    bootloader_start: Option<u64>,
    uboot_env1_start: Option<u64>,
    #[cfg(feature = "uboot")]
    uboot_env2_start: Option<u64>,
    #[cfg(feature = "uboot")]
    uboot_env_size: Option<u64>,
}

impl BuildConstants {
    fn from_build() -> Self {
        Self {
            data_size: build::DATA_SIZE,
            bootloader_start: build::BOOTLOADER_START,
            uboot_env1_start: build::UBOOT_ENV1_START,
            #[cfg(feature = "uboot")]
            uboot_env2_start: build::UBOOT_ENV2_START,
            #[cfg(feature = "uboot")]
            uboot_env_size: build::UBOOT_ENV_SIZE,
        }
    }
}

#[cfg(feature = "uboot")]
#[derive(Debug, PartialEq, Eq)]
struct UbootEnv {
    size: u64,
    offsets: Vec<u64>,
}

/// The build-time constants mode 1 needs, validated and in bytes.
#[derive(Debug, PartialEq, Eq)]
struct Constants {
    data_size_kb: u64,
    /// The bootloader a machine keeps outside the boot partition.
    bootloader_area: Option<ByteRange>,
    #[cfg(feature = "uboot")]
    uboot_env: UbootEnv,
}

/// Validate the build-time constants before anything is written.
///
/// `UBOOT_ENV2_START` stays optional: its absence is how a machine says it
/// reserves no second environment bank.
fn required_constants(raw: &BuildConstants) -> Result<Constants, FlashError> {
    let data_size_kb = raw
        .data_size
        .ok_or(FlashError::MissingBuildConstant(BuildConstant::DataSize))?;

    // The bootloader area reaches up to the first U-Boot environment.
    let bootloader_area = match raw.bootloader_start {
        None => None,
        Some(start) => {
            let end = raw
                .uboot_env1_start
                .ok_or(FlashError::MissingBuildConstant(
                    BuildConstant::UbootEnv1Start,
                ))?;
            let len = end
                .checked_sub(start)
                .ok_or_else(|| FlashError::InvalidBuildConstant {
                    name: BuildConstant::UbootEnv1Start,
                    reason: format!("{end} KB lies below the bootloader area start {start} KB"),
                })?;
            Some(ByteRange {
                offset: kb_to_bytes(start, BuildConstant::BootloaderStart)?,
                len: kb_to_bytes(len, BuildConstant::UbootEnv1Start)?,
            })
        }
    };

    #[cfg(feature = "uboot")]
    let uboot_env = {
        let size = raw.uboot_env_size.ok_or(FlashError::MissingBuildConstant(
            BuildConstant::UbootEnvSize,
        ))?;
        let first = raw
            .uboot_env1_start
            .ok_or(FlashError::MissingBuildConstant(
                BuildConstant::UbootEnv1Start,
            ))?;
        let mut offsets = vec![kb_to_bytes(first, BuildConstant::UbootEnv1Start)?];
        if let Some(second) = raw.uboot_env2_start {
            offsets.push(kb_to_bytes(second, BuildConstant::UbootEnv2Start)?);
        }
        UbootEnv {
            size: kb_to_bytes(size, BuildConstant::UbootEnvSize)?,
            offsets,
        }
    };

    Ok(Constants {
        data_size_kb,
        bootloader_area,
        #[cfg(feature = "uboot")]
        uboot_env,
    })
}

/// The path of partition `num` on `destination`.
///
/// The destination receives a copy of the source table, so every role sits at
/// the same index on both disks.
fn destination_partition(destination: &Path, num: u32) -> PathBuf {
    partition_path(destination, partition_sep_for(destination), num)
}

/// Why a destination on `destination_disk` is refused for `source`, if it is.
fn refusal(destination: u64, destination_disk: Option<u64>, source: u64) -> Option<&'static str> {
    if destination == source {
        return Some(REASON_IDENTICAL_DISK);
    }
    match destination_disk {
        None => Some(REASON_UNKNOWN_DISK),
        Some(disk) if disk == source => Some(REASON_SOURCE_PARTITION),
        Some(disk) if disk != destination => Some(REASON_NOT_A_WHOLE_DISK),
        Some(_) => None,
    }
}

/// Device numbers are compared, so every alias of the booted disk is caught.
/// Writing a partition table into a partition of the running disk would
/// damage the source before the next step could notice.
fn validate_destination(destination: &Path, source: &Path) -> Result<(), FlashError> {
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
        Some(reason @ (REASON_IDENTICAL_DISK | REASON_SOURCE_PARTITION)) => {
            Err(invalid(format!("{reason} {}", source.display())))
        }
        Some(reason) => Err(invalid(reason.to_string())),
        None => Ok(()),
    }
}

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
                timeout,
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

fn verify_destination_partitions(destination: &Path) -> Result<(), FlashError> {
    for num in DEST_PARTITION_ROLES {
        let path = destination_partition(destination, num);
        let reason = match fs::metadata(&path) {
            Ok(m) if m.file_type().is_block_device() => continue,
            Ok(_) => REASON_NOT_A_BLOCK_DEVICE.to_string(),
            Err(e) if e.kind() == ErrorKind::NotFound => REASON_NO_PARTITION_NODE.to_string(),
            Err(e) => format!("cannot stat: {e}"),
        };
        return Err(FlashError::InvalidDestination {
            device: path,
            reason,
        });
    }
    Ok(())
}

fn output_tail(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(E2IMAGE_REASON_LINES)..].join("; ")
}

/// Run `e2image` with its output forwarded to the console and its tail kept.
///
/// Progress goes to stderr, some errors go to stdout, so both share one pipe.
fn run_e2image(src: &Path, dst: &Path) -> Result<(), String> {
    let (mut reader, writer) =
        std::io::pipe().map_err(|e| format!("creating the output pipe: {e}"))?;
    let writer_err = writer
        .try_clone()
        .map_err(|e| format!("creating the output pipe: {e}"))?;

    // The temporary `Command` is dropped at the end of this statement, which
    // closes the parent's write ends, so the read loop below sees EOF.
    let mut child = Command::new(E2IMAGE_CMD)
        .args([E2IMAGE_RAW_FLAG, E2IMAGE_PROGRESS_FLAG])
        .arg(src)
        .arg(dst)
        .stdout(writer)
        .stderr(writer_err)
        .spawn()
        .map_err(|e| format!("failed to run {E2IMAGE_CMD}: {e}"))?;

    let mut console = std::io::stderr();
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; E2IMAGE_READ_CHUNK];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let _ = console.write_all(&chunk[..n]);
                tail.extend_from_slice(&chunk[..n]);
                let excess = tail.len().saturating_sub(E2IMAGE_TAIL_BYTES);
                tail.drain(..excess);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // A child still writing would block on a full pipe after a read error.
    drop(reader);

    let status = child
        .wait()
        .map_err(|e| format!("failed to wait for {E2IMAGE_CMD}: {e}"))?;
    if !status.success() {
        return Err(format!(
            "{E2IMAGE_CMD} failed ({status}): {}",
            output_tail(&tail)
        ));
    }
    Ok(())
}

#[cfg(feature = "grub")]
fn copy_grubenv(target: &Path) -> Result<(), FlashError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| FlashError::PathIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::copy(GRUBENV_SOURCE, target).map_err(|e| FlashError::BootEnvWriteFailed {
        device: target.to_path_buf(),
        reason: format!("copying {GRUBENV_SOURCE}: {e}"),
    })?;
    Ok(())
}

/// The side effects of the clone, so a test can pin their order.
trait CloneOps {
    #[cfg(feature = "gpt")]
    fn fresh_uuid(&mut self) -> Result<String, FlashError>;
    /// Wait for `destination` and resolve it to the device node.
    fn resolve_destination(&mut self, destination: &Path) -> Result<PathBuf, FlashError>;
    fn validate_destination(&mut self, destination: &Path, source: &Path)
    -> Result<(), FlashError>;
    fn unmount_rootfs(&mut self, rootfs: &Path) -> Result<(), FlashError>;
    fn dump(&mut self, source: &Path) -> Result<String, FlashError>;
    fn apply(&mut self, destination: &Path, dump: &str) -> Result<(), FlashError>;
    fn verify_partitions(&mut self, destination: &Path) -> Result<(), FlashError>;
    fn copy_range(
        &mut self,
        src: &Path,
        src_offset: u64,
        dst: &Path,
        dst_offset: u64,
        len: Option<u64>,
    ) -> Result<(), FlashError>;
    fn reformat(&mut self, device: &Path, label: &str) -> Result<(), FlashError>;
    fn e2image(&mut self, src: &Path, dst: &Path) -> Result<(), FlashError>;
    #[cfg(feature = "gpt")]
    fn set_part_uuid(&mut self, device: &Path, num: u32, uuid: &str) -> Result<(), FlashError>;
    #[cfg(feature = "grub")]
    fn write_grubenv(&mut self, boot_partition: &Path) -> Result<(), FlashError>;
    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps;
    fn sync(&mut self);
}

#[derive(Default)]
struct RealCloneOps {
    #[cfg(feature = "grub")]
    efi: efi::RealEfiOps,
}

impl CloneOps for RealCloneOps {
    /// A panic in PID 1 is a kernel panic, so a failing random source has to
    /// be an error.
    #[cfg(feature = "gpt")]
    fn fresh_uuid(&mut self) -> Result<String, FlashError> {
        let mut bytes = [0u8; UUID_BYTES];
        fs::File::open(URANDOM_PATH)
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(|source| FlashError::PathIo {
                path: PathBuf::from(URANDOM_PATH),
                source,
            })?;
        Ok(uuid::Builder::from_random_bytes(bytes)
            .into_uuid()
            .to_string())
    }

    fn resolve_destination(&mut self, destination: &Path) -> Result<PathBuf, FlashError> {
        // A destination owned by the source always exists already, so the wait
        // returns at once for it and the refusal is not delayed.
        wait_for_block_device(destination, DEST_DEVICE_WAIT)?;
        fs::canonicalize(destination).map_err(|e| FlashError::InvalidDestination {
            device: destination.to_path_buf(),
            reason: format!("cannot resolve: {e}"),
        })
    }

    fn validate_destination(
        &mut self,
        destination: &Path,
        source: &Path,
    ) -> Result<(), FlashError> {
        validate_destination(destination, source)
    }

    fn unmount_rootfs(&mut self, rootfs: &Path) -> Result<(), FlashError> {
        unmount::unmount_rootfs(rootfs)
    }

    fn dump(&mut self, source: &Path) -> Result<String, FlashError> {
        sfdisk::dump(source)
    }

    fn apply(&mut self, destination: &Path, dump: &str) -> Result<(), FlashError> {
        sfdisk::apply(destination, dump)
    }

    fn verify_partitions(&mut self, destination: &Path) -> Result<(), FlashError> {
        verify_destination_partitions(destination)
    }

    fn copy_range(
        &mut self,
        src: &Path,
        src_offset: u64,
        dst: &Path,
        dst_offset: u64,
        len: Option<u64>,
    ) -> Result<(), FlashError> {
        rawio::copy_range(src, src_offset, dst, dst_offset, len).map(drop)
    }

    fn reformat(&mut self, device: &Path, label: &str) -> Result<(), FlashError> {
        Ok(reformat_ext4(device, label)?)
    }

    fn e2image(&mut self, src: &Path, dst: &Path) -> Result<(), FlashError> {
        run_e2image(src, dst).map_err(|reason| FlashError::CopyFailed {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            reason,
        })
    }

    #[cfg(feature = "gpt")]
    fn set_part_uuid(&mut self, device: &Path, num: u32, uuid: &str) -> Result<(), FlashError> {
        sfdisk::set_part_uuid(device, num, uuid)
    }

    #[cfg(feature = "grub")]
    fn write_grubenv(&mut self, boot_partition: &Path) -> Result<(), FlashError> {
        with_mount(
            boot_partition,
            Path::new(scratch_mounts::CLONE_BOOT),
            MountOptions::vfat(),
            |boot_mount| copy_grubenv(&boot_mount.join(GRUBENV_TARGET)),
        )
    }

    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps {
        &mut self.efi
    }

    fn sync(&mut self) {
        sync_filesystems();
    }
}

/// Clone the running disk onto `ctx.destination`.
pub(crate) fn run_clone(ctx: &CloneCtx<'_>) -> Result<(), FlashError> {
    let constants = required_constants(&BuildConstants::from_build())?;
    clone_with(ctx, &constants, &mut RealCloneOps::default())
}

fn clone_with(
    ctx: &CloneCtx<'_>,
    constants: &Constants,
    ops: &mut dyn CloneOps,
) -> Result<(), FlashError> {
    let source = ctx.layout.device.base.as_path();

    log::info!(
        "flash mode 1: cloning {} onto {}",
        source.display(),
        ctx.destination.display()
    );

    // The clone's boot and rootA partitions get identities of their own, so a
    // host that sees both disks can tell them apart.
    #[cfg(feature = "gpt")]
    let fresh_uuids = [
        (PARTITION_NUM_BOOT, ops.fresh_uuid()?),
        (PARTITION_NUM_ROOT_A, ops.fresh_uuid()?),
    ];

    // Everything below addresses the destination by the resolved path,
    // because `destination_partition` appends a partition index to it and an
    // alias such as a by-id link names no partition of its own.
    let destination = ops.resolve_destination(ctx.destination)?;
    let destination = destination.as_path();
    log::info!("destination resolved to {}", destination.display());

    ops.validate_destination(destination, source)?;

    // `e2image` below must not read a mounted filesystem, and the raw boot
    // copy must not read one either.
    ops.unmount_rootfs(ctx.rootfs)?;

    let dump = ops.dump(source)?;
    let rewritten = sfdisk::rewrite_dump(source, &dump, constants.data_size_kb)?;
    ops.apply(destination, &rewritten)?;
    ops.verify_partitions(destination)?;

    if let Some(area) = &constants.bootloader_area {
        log::info!(
            "copying the {} byte bootloader area at {} bytes",
            area.len,
            area.offset
        );
        ops.copy_range(
            source,
            area.offset,
            destination,
            area.offset,
            Some(area.len),
        )?;
    }

    // Empty `etc` and `data` filesystems are what put the clone into the
    // first-boot condition.
    ops.reformat(
        &destination_partition(destination, PARTITION_NUM_ETC),
        ETC_PARTITION_LABEL,
    )?;
    ops.reformat(
        &destination_partition(destination, PARTITION_NUM_DATA),
        DATA_PARTITION_LABEL,
    )?;

    for (name, num) in [
        (PartitionName::Boot, PARTITION_NUM_BOOT),
        (PartitionName::Factory, PARTITION_NUM_FACTORY),
        (PartitionName::Cert, PARTITION_NUM_CERT),
    ] {
        let src = layout_partition(ctx.layout, name)?;
        let dst = destination_partition(destination, num);
        log::info!("copying {} onto {}", src.display(), dst.display());
        ops.copy_range(src, 0, &dst, 0, None)?;
    }

    let root_src = ctx.layout.root_current();
    let root_dst = destination_partition(destination, PARTITION_NUM_ROOT_A);
    log::info!("imaging {} onto {}", root_src.display(), root_dst.display());
    ops.e2image(&root_src, &root_dst)?;

    #[cfg(feature = "gpt")]
    for (num, uuid) in &fresh_uuids {
        log::info!(
            "assigning partition {num} on {} the UUID {uuid}",
            destination.display()
        );
        ops.set_part_uuid(destination, *num, uuid)?;
    }

    #[cfg(feature = "grub")]
    {
        let boot_partition = destination_partition(destination, PARTITION_NUM_BOOT);
        ops.write_grubenv(&boot_partition)?;
        efi::handle(ops.efi(), destination, &boot_partition)?;
    }

    #[cfg(feature = "uboot")]
    for &offset in &constants.uboot_env.offsets {
        log::info!(
            "writing {UBOOT_ENV_SOURCE} at {offset} bytes on {}",
            destination.display()
        );
        ops.copy_range(
            Path::new(UBOOT_ENV_SOURCE),
            0,
            destination,
            offset,
            Some(constants.uboot_env.size),
        )?;
    }

    log::info!("flash mode 1 finished");
    ops.sync();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::RootDevice;
    use nix::sys::stat::makedev;

    #[test]
    fn destination_partitions_use_the_same_indices_as_the_source() {
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

    #[test]
    fn the_clone_writes_every_role_but_the_dos_extended_container() {
        use crate::partition::layout::*;
        assert_eq!(
            DEST_PARTITION_ROLES,
            [
                PARTITION_NUM_BOOT,
                PARTITION_NUM_ROOT_A,
                PARTITION_NUM_ROOT_B,
                PARTITION_NUM_FACTORY,
                PARTITION_NUM_CERT,
                PARTITION_NUM_ETC,
                PARTITION_NUM_DATA,
            ]
        );
        #[cfg(feature = "dos")]
        assert!(!DEST_PARTITION_ROLES.contains(&PARTITION_NUM_EXTENDED));
    }

    const SDA: u64 = makedev(8, 0);
    const SDA2: u64 = makedev(8, 2);
    const SDB: u64 = makedev(8, 16);
    const SDB1: u64 = makedev(8, 17);

    #[test]
    fn the_booted_disk_itself_is_refused() {
        assert_eq!(refusal(SDA, Some(SDA), SDA), Some(REASON_IDENTICAL_DISK));
    }

    #[test]
    fn a_partition_of_the_booted_disk_is_refused() {
        assert_eq!(refusal(SDA2, Some(SDA), SDA), Some(REASON_SOURCE_PARTITION));
    }

    #[test]
    fn a_partition_of_another_disk_is_refused() {
        assert_eq!(refusal(SDB1, Some(SDB), SDA), Some(REASON_NOT_A_WHOLE_DISK));
    }

    #[test]
    fn another_disk_is_accepted() {
        assert_eq!(refusal(SDB, Some(SDB), SDA), None);
    }

    #[test]
    fn a_destination_sysfs_does_not_list_is_refused() {
        assert_eq!(refusal(SDB, None, SDA), Some(REASON_UNKNOWN_DISK));
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
    fn a_missing_partition_node_and_a_stat_error_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_destination_partitions(&dir.path().join("sdz")).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidDestination { reason, .. }
                if reason == REASON_NO_PARTITION_NODE),
            "got {err}"
        );

        // A path through a regular file fails with ENOTDIR, not ENOENT.
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = verify_destination_partitions(&file.path().join("sdz")).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidDestination { reason, .. }
                if reason.starts_with("cannot stat:")),
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

    #[test]
    fn the_e2image_error_keeps_the_last_lines_of_its_output() {
        let output = b"e2image 1.47.0 (5-Feb-2023)\nScanning inodes...\n\
            Copying 0 / 1042 blocks (0%)\rCopying 512 / 1042 blocks (49%)\r\
            e2image: Input/output error while writing block 600\n";
        assert_eq!(
            output_tail(output),
            "Copying 0 / 1042 blocks (0%); Copying 512 / 1042 blocks (49%); \
             e2image: Input/output error while writing block 600"
        );
    }

    fn raw_constants() -> BuildConstants {
        BuildConstants {
            data_size: Some(4096),
            bootloader_start: None,
            uboot_env1_start: Some(4096),
            #[cfg(feature = "uboot")]
            uboot_env2_start: None,
            #[cfg(feature = "uboot")]
            uboot_env_size: Some(64),
        }
    }

    #[test]
    fn data_size_is_required() {
        let raw = BuildConstants {
            data_size: None,
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::MissingBuildConstant(BuildConstant::DataSize))
        ));
    }

    #[test]
    fn a_bootloader_area_needs_the_first_uboot_env_offset() {
        let raw = BuildConstants {
            bootloader_start: Some(2048),
            uboot_env1_start: None,
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::MissingBuildConstant(
                BuildConstant::UbootEnv1Start
            ))
        ));
    }

    #[test]
    fn a_bootloader_area_ending_before_its_start_is_refused() {
        let raw = BuildConstants {
            bootloader_start: Some(8192),
            uboot_env1_start: Some(4096),
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::InvalidBuildConstant {
                name: BuildConstant::UbootEnv1Start,
                ..
            })
        ));
    }

    #[test]
    fn the_bootloader_area_is_scaled_from_kb_to_bytes() {
        let raw = BuildConstants {
            bootloader_start: Some(2048),
            ..raw_constants()
        };
        assert_eq!(
            required_constants(&raw).unwrap().bootloader_area,
            Some(ByteRange {
                offset: 2_097_152,
                len: 2_097_152
            })
        );
        assert_eq!(
            required_constants(&raw_constants())
                .unwrap()
                .bootloader_area,
            None
        );
    }

    #[test]
    fn a_kb_value_that_overflows_bytes_is_refused() {
        let raw = BuildConstants {
            bootloader_start: Some(u64::MAX / 2),
            uboot_env1_start: Some(u64::MAX),
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::InvalidBuildConstant {
                name: BuildConstant::BootloaderStart,
                ..
            })
        ));
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn uboot_env_size_and_offsets_are_scaled_from_kb_to_bytes() {
        let raw = BuildConstants {
            uboot_env2_start: Some(8192),
            ..raw_constants()
        };
        assert_eq!(
            required_constants(&raw).unwrap().uboot_env,
            UbootEnv {
                size: 65_536,
                offsets: vec![4_194_304, 8_388_608]
            }
        );
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn a_machine_without_a_second_env_bank_gets_one_write() {
        assert_eq!(
            required_constants(&raw_constants()).unwrap().uboot_env,
            UbootEnv {
                size: 65_536,
                offsets: vec![4_194_304]
            }
        );
    }

    #[cfg(feature = "uboot")]
    #[test]
    fn uboot_needs_the_env_size_and_the_first_offset() {
        let raw = BuildConstants {
            uboot_env_size: None,
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::MissingBuildConstant(
                BuildConstant::UbootEnvSize
            ))
        ));
        let raw = BuildConstants {
            uboot_env1_start: None,
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::MissingBuildConstant(
                BuildConstant::UbootEnv1Start
            ))
        ));
    }

    /// Records every clone side effect as one line, in call order, and fails
    /// the first call whose line starts with `fail_on`.
    struct RecordingCloneOps {
        calls: Vec<String>,
        fail_on: Option<&'static str>,
        #[cfg(feature = "gpt")]
        uuids: u32,
    }

    impl RecordingCloneOps {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                fail_on: None,
                #[cfg(feature = "gpt")]
                uuids: 0,
            }
        }

        fn record(&mut self, call: String) -> Result<(), FlashError> {
            let fail = self.fail_on.is_some_and(|prefix| call.starts_with(prefix));
            self.calls.push(call);
            if fail {
                return Err(FlashError::EfiFailed("injected".to_string()));
            }
            Ok(())
        }
    }

    #[cfg(feature = "grub")]
    impl efi::EfiOps for RecordingCloneOps {
        fn mount_efivarfs(&mut self) -> Result<(), FlashError> {
            self.record("mount efivarfs".to_string())
        }

        fn efibootmgr(&mut self, args: &[String]) -> Result<String, FlashError> {
            self.record(
                format!("efibootmgr {}", args.join(" "))
                    .trim_end()
                    .to_string(),
            )?;
            Ok(String::new())
        }

        fn write_entry_dump(
            &mut self,
            boot_partition: &Path,
            _dump: &str,
        ) -> Result<(), FlashError> {
            self.record(format!("write entry dump to {}", boot_partition.display()))
        }
    }

    impl CloneOps for RecordingCloneOps {
        #[cfg(feature = "gpt")]
        fn fresh_uuid(&mut self) -> Result<String, FlashError> {
            self.uuids += 1;
            let uuid = format!("uuid-{}", self.uuids);
            self.record(format!("generate {uuid}"))?;
            Ok(uuid)
        }

        fn resolve_destination(&mut self, destination: &Path) -> Result<PathBuf, FlashError> {
            self.record(format!("resolve {}", destination.display()))?;
            Ok(destination.to_path_buf())
        }

        fn validate_destination(
            &mut self,
            destination: &Path,
            source: &Path,
        ) -> Result<(), FlashError> {
            self.record(format!(
                "validate {} against {}",
                destination.display(),
                source.display()
            ))
        }

        fn unmount_rootfs(&mut self, rootfs: &Path) -> Result<(), FlashError> {
            self.record(format!("unmount {}", rootfs.display()))
        }

        fn dump(&mut self, source: &Path) -> Result<String, FlashError> {
            self.record(format!("dump {}", source.display()))?;
            Ok(sfdisk::tests::SOURCE_DUMP.to_string())
        }

        fn apply(&mut self, destination: &Path, _dump: &str) -> Result<(), FlashError> {
            self.record(format!("apply {}", destination.display()))
        }

        fn verify_partitions(&mut self, destination: &Path) -> Result<(), FlashError> {
            self.record(format!("verify {}", destination.display()))
        }

        fn copy_range(
            &mut self,
            src: &Path,
            src_offset: u64,
            dst: &Path,
            dst_offset: u64,
            len: Option<u64>,
        ) -> Result<(), FlashError> {
            self.record(format!(
                "copy {}@{src_offset} to {}@{dst_offset} len {len:?}",
                src.display(),
                dst.display()
            ))
        }

        fn reformat(&mut self, device: &Path, label: &str) -> Result<(), FlashError> {
            self.record(format!("reformat {} as {label}", device.display()))
        }

        fn e2image(&mut self, src: &Path, dst: &Path) -> Result<(), FlashError> {
            self.record(format!("e2image {} to {}", src.display(), dst.display()))
        }

        #[cfg(feature = "gpt")]
        fn set_part_uuid(&mut self, device: &Path, num: u32, uuid: &str) -> Result<(), FlashError> {
            self.record(format!("set uuid {uuid} on {} {num}", device.display()))
        }

        #[cfg(feature = "grub")]
        fn write_grubenv(&mut self, boot_partition: &Path) -> Result<(), FlashError> {
            self.record(format!("write grubenv to {}", boot_partition.display()))
        }

        #[cfg(feature = "grub")]
        fn efi(&mut self) -> &mut dyn efi::EfiOps {
            self
        }

        fn sync(&mut self) {
            self.calls.push("sync".to_string());
        }
    }

    fn source_layout() -> PartitionLayout {
        PartitionLayout::new(RootDevice {
            base: PathBuf::from("/dev/sda"),
            partition_sep: "",
            root_partition: PathBuf::from("/dev/sda2"),
        })
        .unwrap()
    }

    fn run_recorded(ops: &mut RecordingCloneOps, constants: &Constants) -> Result<(), FlashError> {
        let layout = source_layout();
        let ctx = CloneCtx {
            destination: Path::new("/dev/sdb"),
            layout: &layout,
            rootfs: Path::new("/rootfs"),
        };
        clone_with(&ctx, constants, ops)
    }

    fn part(disk: &str, num: u32) -> String {
        destination_partition(Path::new(disk), num)
            .display()
            .to_string()
    }

    /// The spec 4.1 order: nothing is written before the destination is
    /// validated and the rootfs is unmounted, the table is applied and
    /// verified before any partition is written, and `etc`/`data` are
    /// reformatted before the other partitions are copied.
    #[test]
    fn the_clone_runs_its_steps_in_the_spec_order() {
        let raw = BuildConstants {
            bootloader_start: Some(2048),
            ..raw_constants()
        };
        let constants = required_constants(&raw).unwrap();
        let mut ops = RecordingCloneOps::new();
        run_recorded(&mut ops, &constants).unwrap();

        let mut expected: Vec<String> = Vec::new();
        #[cfg(feature = "gpt")]
        expected.extend(["generate uuid-1".to_string(), "generate uuid-2".to_string()]);
        expected.extend([
            "resolve /dev/sdb".to_string(),
            "validate /dev/sdb against /dev/sda".to_string(),
            "unmount /rootfs".to_string(),
            "dump /dev/sda".to_string(),
            "apply /dev/sdb".to_string(),
            "verify /dev/sdb".to_string(),
            "copy /dev/sda@2097152 to /dev/sdb@2097152 len Some(2097152)".to_string(),
            format!("reformat {} as etc", part("/dev/sdb", PARTITION_NUM_ETC)),
            format!("reformat {} as data", part("/dev/sdb", PARTITION_NUM_DATA)),
        ]);
        for num in [
            PARTITION_NUM_BOOT,
            PARTITION_NUM_FACTORY,
            PARTITION_NUM_CERT,
        ] {
            expected.push(format!(
                "copy {}@0 to {}@0 len None",
                part("/dev/sda", num),
                part("/dev/sdb", num)
            ));
        }
        expected.push(format!(
            "e2image {} to {}",
            part("/dev/sda", PARTITION_NUM_ROOT_A),
            part("/dev/sdb", PARTITION_NUM_ROOT_A)
        ));
        #[cfg(feature = "gpt")]
        expected.extend([
            "set uuid uuid-1 on /dev/sdb 1".to_string(),
            "set uuid uuid-2 on /dev/sdb 2".to_string(),
        ]);
        #[cfg(feature = "uboot")]
        expected.push(
            "copy /etc/omnect/uboot-env.bin@0 to /dev/sdb@4194304 len Some(65536)".to_string(),
        );
        #[cfg(feature = "grub")]
        expected.extend([
            "write grubenv to /dev/sdb1".to_string(),
            "mount efivarfs".to_string(),
            "efibootmgr".to_string(),
            r"efibootmgr -c -d /dev/sdb -p 1 -L omnect_os -l \EFI\BOOT\bootx64.efi".to_string(),
            "efibootmgr -v".to_string(),
            "write entry dump to /dev/sdb1".to_string(),
        ]);
        expected.push("sync".to_string());

        assert_eq!(ops.calls, expected);
    }

    #[test]
    fn a_refused_destination_stops_the_clone_before_anything_is_unmounted_or_written() {
        let constants = required_constants(&raw_constants()).unwrap();
        let mut ops = RecordingCloneOps::new();
        ops.fail_on = Some("validate");
        assert!(run_recorded(&mut ops, &constants).is_err());
        assert!(ops.calls.last().unwrap().starts_with("validate"));
    }

    #[test]
    fn a_table_that_fails_verification_stops_the_clone_before_any_partition_is_written() {
        let constants = required_constants(&raw_constants()).unwrap();
        let mut ops = RecordingCloneOps::new();
        ops.fail_on = Some("verify");
        assert!(run_recorded(&mut ops, &constants).is_err());
        assert!(ops.calls.last().unwrap().starts_with("verify"));
    }
}
