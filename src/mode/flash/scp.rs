//! Flash mode 2: flash the running disk with a `wic.xz` the operator pushes in
//! over `scp`.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::net::Ipv4Addr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::unistd::{Gid, Uid, chown, mkfifo};

use crate::bootloader::sync_filesystems;
use crate::config::{BuildConstant, build};
use crate::error::FlashError;
use crate::mode::flash::bmap::{self, Bmap, Destination, Source};
use crate::mode::flash::rawio::{self, ByteRange, kb_to_bytes};
#[cfg(feature = "grub")]
use crate::mode::flash::{efi, layout_partition};
use crate::mode::flash::{net, unmount};
use crate::partition::PartitionLayout;
#[cfg(feature = "grub")]
use crate::partition::PartitionName;

const OMNECT_HOME: &str = "/home/omnect";
const OMNECT_USER: &str = "omnect";
const WIC_FIFO_NAME: &str = "wic.xz";
const WIC_BMAP_NAME: &str = "wic.bmap";
#[cfg(not(feature = "flash-mode-2-direct"))]
const WIC_DECODED_NAME: &str = "wic";
const BMAP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const BMAP_CLOSING_TAG: &str = "</bmap>";
/// Enough for the closing tag and the whitespace an editor or generator
/// leaves after it.
const BMAP_TAIL_LEN: u64 = 64;
/// Polls without a size change before a file that is not a bmap is reported.
const BMAP_STALE_POLLS: u32 = 5;

pub(crate) struct ScpCtx<'a> {
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
}

struct BuildConstants {
    boot_start: Option<u64>,
    boot_size: Option<u64>,
    omnect_user_id: Option<u64>,
}

impl BuildConstants {
    fn from_build() -> Self {
        Self {
            boot_start: build::BOOT_START,
            boot_size: build::BOOT_SIZE,
            omnect_user_id: build::OMNECT_USER_ID,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Constants {
    head_bytes: u64,
    omnect_user_id: u32,
}

/// The disk head: everything up to the end of the boot partition.
fn head_kib(boot_start: Option<u64>, boot_size: Option<u64>) -> Result<u64, FlashError> {
    let start = boot_start.ok_or(FlashError::MissingBuildConstant(BuildConstant::BootStart))?;
    let size = boot_size.ok_or(FlashError::MissingBuildConstant(BuildConstant::BootSize))?;
    start
        .checked_add(size)
        .ok_or_else(|| FlashError::InvalidBuildConstant {
            name: BuildConstant::BootSize,
            reason: format!("{start} KB + {size} KB does not fit a KB count"),
        })
}

fn required_constants(raw: &BuildConstants) -> Result<Constants, FlashError> {
    let head_bytes = kb_to_bytes(
        head_kib(raw.boot_start, raw.boot_size)?,
        BuildConstant::BootSize,
    )?;
    let id = raw.omnect_user_id.ok_or(FlashError::MissingBuildConstant(
        BuildConstant::OmnectUserId,
    ))?;
    let omnect_user_id = u32::try_from(id).map_err(|_| FlashError::InvalidBuildConstant {
        name: BuildConstant::OmnectUserId,
        reason: format!("{id} does not fit a user id"),
    })?;
    Ok(Constants {
        head_bytes,
        omnect_user_id,
    })
}

fn bmap_instruction(ip: Ipv4Addr) -> String {
    format!("please run: scp -O <bmap-file> {OMNECT_USER}@{ip}:{WIC_BMAP_NAME}")
}

fn image_instruction(ip: Ipv4Addr) -> String {
    format!("please run: scp -O <wic-image> {OMNECT_USER}@{ip}:{WIC_FIFO_NAME}")
}

/// The FIFO lets the flash read the image while `scp` is still writing it.
fn create_owned_fifo(path: &Path, uid: Uid, gid: Gid) -> Result<(), FlashError> {
    let failed = |errno: Errno| FlashError::PathIo {
        path: path.to_path_buf(),
        source: errno.into(),
    };
    mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(failed)?;
    chown(path, Some(uid), Some(gid)).map_err(failed)
}

/// A writer blocked in `open()` waits for a reader of this inode, so it would
/// hang forever once the path is gone. The short-lived reader lets its `open()`
/// return, and once the reader is closed its writes fail with `EPIPE`.
fn remove_fifo(path: &Path) {
    let reader = OpenOptions::new()
        .read(true)
        .custom_flags(OFlag::O_NONBLOCK.bits())
        .open(path);
    if let Err(e) = &reader
        && e.kind() != ErrorKind::NotFound
    {
        log::warn!("failed to open {} for reading: {e}", path.display());
    }
    remove_file(path);
    drop(reader);
}

fn remove_file(path: &Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            log::warn!("failed to remove {}: {e}", path.display());
        }
        _ => {}
    }
}

/// A half-copied bmap must not end the wait, or the operator would be asked
/// again for no reason. Only the tail is read, so a large file pushed under
/// the bmap name costs no RAM.
fn bmap_is_complete(path: &Path) -> bool {
    path.is_file()
        && read_tail(path)
            .is_ok_and(|tail| tail.trim_ascii_end().ends_with(BMAP_CLOSING_TAG.as_bytes()))
}

fn read_tail(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(BMAP_TAIL_LEN)))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    Ok(tail)
}

fn wait_for_bmap(path: &Path, interval: Duration, sleep: &mut dyn FnMut(Duration)) {
    log::info!("waiting for {}", path.display());
    let mut last_len = None;
    let mut unchanged_polls = 0;
    while !bmap_is_complete(path) {
        let len = std::fs::metadata(path)
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len());
        unchanged_polls = if len.is_some() && len == last_len {
            unchanged_polls + 1
        } else {
            0
        };
        last_len = len;
        if unchanged_polls == BMAP_STALE_POLLS {
            log::warn!(
                "{} has not changed for {} s and does not end with {BMAP_CLOSING_TAG}, \
                 is it the bmap file?",
                path.display(),
                (interval * BMAP_STALE_POLLS).as_secs()
            );
        }
        sleep(interval);
    }
}

trait ScpOps {
    fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError>;
    fn bring_up_network(&mut self) -> Result<Ipv4Addr, FlashError>;
    fn start_dropbear(&mut self) -> Result<(), FlashError>;
    fn create_fifo(&mut self, path: &Path, owner: u32) -> Result<(), FlashError>;
    fn tell_operator(&mut self, message: &str);
    fn wait_for_bmap(&mut self, path: &Path);
    fn read_bmap(&mut self, path: &Path) -> Result<Bmap, FlashError>;
    fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError>;
    fn bmap_copy(
        &mut self,
        bmap: &Bmap,
        source: &Source<'_>,
        destination: &Destination<'_>,
    ) -> Result<(), FlashError>;
    fn zero_range(&mut self, dst: &Path, range: &ByteRange) -> Result<(), FlashError>;
    fn remove(&mut self, path: &Path);
    fn remove_fifo(&mut self, path: &Path);
    fn reread_table(&mut self, disk: &Path) -> Result<(), FlashError>;
    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps;
    fn sync(&mut self);
}

#[derive(Default)]
struct RealScpOps {
    #[cfg(feature = "grub")]
    efi: efi::RealEfiOps,
}

impl ScpOps for RealScpOps {
    fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
        unmount::unmount_target_disk(rootfs, disk)
    }

    fn bring_up_network(&mut self) -> Result<Ipv4Addr, FlashError> {
        net::bring_up()
    }

    fn start_dropbear(&mut self) -> Result<(), FlashError> {
        net::start_dropbear()
    }

    fn create_fifo(&mut self, path: &Path, owner: u32) -> Result<(), FlashError> {
        create_owned_fifo(path, Uid::from_raw(owner), Gid::from_raw(owner))
    }

    fn tell_operator(&mut self, message: &str) {
        log::info!("{message}");
    }

    fn wait_for_bmap(&mut self, path: &Path) {
        wait_for_bmap(path, BMAP_POLL_INTERVAL, &mut thread::sleep);
    }

    fn read_bmap(&mut self, path: &Path) -> Result<Bmap, FlashError> {
        bmap::read(path)
    }

    fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError> {
        File::open(disk)
            .and_then(|mut file| file.seek(SeekFrom::End(0)))
            .map_err(|e| FlashError::InvalidDestination {
                device: disk.to_path_buf(),
                reason: format!("cannot determine size: {e}"),
            })
    }

    fn bmap_copy(
        &mut self,
        bmap: &Bmap,
        source: &Source<'_>,
        destination: &Destination<'_>,
    ) -> Result<(), FlashError> {
        bmap::copy(bmap, source, destination)
    }

    fn zero_range(&mut self, dst: &Path, range: &ByteRange) -> Result<(), FlashError> {
        rawio::zero_range(dst, range)
    }

    fn remove(&mut self, path: &Path) {
        remove_file(path);
    }

    fn remove_fifo(&mut self, path: &Path) {
        remove_fifo(path);
    }

    fn reread_table(&mut self, disk: &Path) -> Result<(), FlashError> {
        rawio::reread_partition_table(disk)
    }

    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps {
        &mut self.efi
    }

    fn sync(&mut self) {
        sync_filesystems();
    }
}

struct Upload {
    fifo: PathBuf,
    bmap: PathBuf,
    #[cfg(not(feature = "flash-mode-2-direct"))]
    decoded: PathBuf,
}

impl Upload {
    fn in_home() -> Self {
        let home = Path::new(OMNECT_HOME);
        Self {
            fifo: home.join(WIC_FIFO_NAME),
            bmap: home.join(WIC_BMAP_NAME),
            #[cfg(not(feature = "flash-mode-2-direct"))]
            decoded: home.join(WIC_DECODED_NAME),
        }
    }
}

pub(crate) fn run_scp(ctx: &ScpCtx<'_>) -> Result<(), FlashError> {
    scp_with(
        ctx,
        &BuildConstants::from_build(),
        &mut RealScpOps::default(),
    )
}

fn scp_with(
    ctx: &ScpCtx<'_>,
    raw: &BuildConstants,
    ops: &mut dyn ScpOps,
) -> Result<(), FlashError> {
    let constants = required_constants(raw)?;
    let disk = ctx.layout.device.base.as_path();
    #[cfg(feature = "grub")]
    let boot_partition = layout_partition(ctx.layout, PartitionName::Boot)?;
    let upload = Upload::in_home();
    let head = ByteRange {
        offset: 0,
        len: constants.head_bytes,
    };

    log::info!(
        "flash mode 2: flashing {} with an image pushed in over scp",
        disk.display()
    );

    ops.unmount(ctx.rootfs, disk)?;
    let ip = ops.bring_up_network()?;
    // Before dropbear, so a client that can log in always finds the FIFO.
    ops.create_fifo(&upload.fifo, constants.omnect_user_id)?;
    ops.start_dropbear()?;

    // The system runs from RAM, so a failed attempt, even one that left the
    // disk unbootable, can be repaired by a new upload as long as the device
    // stays powered.
    let mut disk_written = false;
    while let Err(e) = flash_once(ops, &upload, disk, &head, ip) {
        log::error!("{e}");
        disk_written |= matches!(e, FlashError::DiskPartlyWritten { .. });
        ops.tell_operator("flash failed, asking for the bmap and the image again");
        ops.remove(&upload.bmap);
        #[cfg(not(feature = "flash-mode-2-direct"))]
        ops.remove(&upload.decoded);
        ops.remove_fifo(&upload.fifo);
        if let Err(e) = ops.create_fifo(&upload.fifo, constants.omnect_user_id) {
            // Keeps the run log off a disk that an earlier attempt wrote.
            return Err(if disk_written {
                partly_written(disk, e)
            } else {
                e
            });
        }
    }

    // The new image may move partitions. Everything after this point mounts
    // them, so the kernel must drop the table from before the flash first.
    ops.reread_table(disk)
        .map_err(|source| FlashError::StalePartitionTable {
            disk: disk.to_path_buf(),
            source: Box::new(source),
        })?;

    #[cfg(feature = "grub")]
    efi::handle(ops.efi(), disk, boot_partition)?;

    log::info!("flash mode 2 finished");
    ops.sync();
    Ok(())
}

fn partly_written(disk: &Path, source: FlashError) -> FlashError {
    FlashError::DiskPartlyWritten {
        disk: disk.to_path_buf(),
        source: Box::new(source),
    }
}

/// One upload and flash.
fn flash_once(
    ops: &mut dyn ScpOps,
    upload: &Upload,
    disk: &Path,
    head: &ByteRange,
    ip: Ipv4Addr,
) -> Result<(), FlashError> {
    ops.tell_operator(&bmap_instruction(ip));
    // Unbounded: this waits for a person to start the `scp`.
    ops.wait_for_bmap(&upload.bmap);
    let map = ops.read_bmap(&upload.bmap)?;
    let disk_len = ops.device_len(disk)?;
    if map.image_size() > disk_len {
        return Err(FlashError::InvalidDestination {
            device: disk.to_path_buf(),
            reason: format!(
                "the image needs {} bytes, the device has {disk_len}",
                map.image_size()
            ),
        });
    }
    ops.tell_operator(&image_instruction(ip));

    // The verify pass reads the whole stream first, so a broken transfer
    // fails before the disk is written. The decoded image is held in the
    // initramfs root, so it must fit in RAM.
    #[cfg(not(feature = "flash-mode-2-direct"))]
    let source = {
        log::info!("verifying {}", upload.fifo.display());
        ops.bmap_copy(
            &map,
            &Source::Xz(&upload.fifo),
            &Destination::File(&upload.decoded),
        )?;
        Source::Raw(&upload.decoded)
    };
    #[cfg(feature = "flash-mode-2-direct")]
    let source = Source::Xz(&upload.fifo);

    log::info!(
        "flashing {} onto {}",
        source.path().display(),
        disk.display()
    );
    ops.bmap_copy(&map, &source, &Destination::Device(disk))
        .map_err(|e| partly_written(disk, e))?;

    // Only mapped blocks are written, so bytes of the old image in the rest
    // of the head would survive the flash.
    for range in map.unmapped(head) {
        log::info!(
            "zeroing {} bytes at {} of {}",
            range.len,
            range.offset,
            disk.display()
        );
        ops.zero_range(disk, &range)
            .map_err(|e| partly_written(disk, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::flash::keeps_log;
    use crate::partition::RootDevice;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 7);

    #[test]
    fn the_head_reaches_the_end_of_the_boot_partition() {
        assert_eq!(head_kib(Some(4096), Some(40960)).unwrap(), 45056);
    }

    #[test]
    fn the_head_needs_boot_start_and_boot_size() {
        assert!(matches!(
            head_kib(None, Some(40960)),
            Err(FlashError::MissingBuildConstant(BuildConstant::BootStart))
        ));
        assert!(matches!(
            head_kib(Some(4096), None),
            Err(FlashError::MissingBuildConstant(BuildConstant::BootSize))
        ));
    }

    #[test]
    fn a_head_that_overflows_is_refused() {
        assert!(matches!(
            head_kib(Some(u64::MAX), Some(1)),
            Err(FlashError::InvalidBuildConstant {
                name: BuildConstant::BootSize,
                ..
            })
        ));
        let raw = BuildConstants {
            boot_start: Some(u64::MAX / 2),
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::InvalidBuildConstant {
                name: BuildConstant::BootSize,
                ..
            })
        ));
    }

    fn raw_constants() -> BuildConstants {
        BuildConstants {
            boot_start: Some(4096),
            boot_size: Some(40960),
            omnect_user_id: Some(1000),
        }
    }

    #[test]
    fn the_constants_are_scaled_to_bytes_and_a_user_id() {
        assert_eq!(
            required_constants(&raw_constants()).unwrap(),
            Constants {
                head_bytes: 46_137_344,
                omnect_user_id: 1000,
            }
        );
    }

    #[test]
    fn the_omnect_user_id_is_required() {
        let raw = BuildConstants {
            omnect_user_id: None,
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::MissingBuildConstant(
                BuildConstant::OmnectUserId
            ))
        ));
    }

    #[test]
    fn an_omnect_user_id_beyond_u32_is_refused() {
        let raw = BuildConstants {
            omnect_user_id: Some(u64::from(u32::MAX) + 1),
            ..raw_constants()
        };
        assert!(matches!(
            required_constants(&raw),
            Err(FlashError::InvalidBuildConstant {
                name: BuildConstant::OmnectUserId,
                ..
            })
        ));
    }

    #[test]
    fn the_operator_is_told_both_scp_commands_with_the_address() {
        assert!(bmap_instruction(IP).contains("scp -O <bmap-file> omnect@192.168.0.7:wic.bmap"));
        assert!(image_instruction(IP).contains("scp -O <wic-image> omnect@192.168.0.7:wic.xz"));
    }

    // Without root the FIFO can only go to the current user, so this does not
    // catch a missing `chown`.
    #[test]
    fn the_fifo_is_private_to_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_FIFO_NAME);
        create_owned_fifo(&path, Uid::current(), Gid::current()).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.file_type().is_fifo());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.uid(), Uid::current().as_raw());
        assert_eq!(meta.gid(), Gid::current().as_raw());
    }

    const MAX_TEST_POLLS: usize = 20;
    const PARTIAL_BMAP: &str = "<?xml version=\"1.0\" ?>\n<bmap version=\"2.0\">\n";

    #[test]
    fn a_bmap_is_complete_only_as_a_regular_file_with_its_closing_tag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_BMAP_NAME);
        assert!(!bmap_is_complete(&path), "missing");

        std::fs::create_dir(&path).unwrap();
        assert!(!bmap_is_complete(&path), "directory");
        std::fs::remove_dir(&path).unwrap();

        std::fs::write(&path, PARTIAL_BMAP).unwrap();
        assert!(!bmap_is_complete(&path), "no closing tag");

        std::fs::write(&path, format!("{PARTIAL_BMAP}{BMAP_CLOSING_TAG}\n")).unwrap();
        assert!(bmap_is_complete(&path), "closing tag and trailing newline");
    }

    #[test]
    fn only_the_tail_of_the_bmap_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_BMAP_NAME);
        let long_body = "x".repeat(usize::try_from(BMAP_TAIL_LEN).unwrap() * 4);

        std::fs::write(&path, format!("{BMAP_CLOSING_TAG}{long_body}")).unwrap();
        assert!(!bmap_is_complete(&path), "closing tag before the tail");

        std::fs::write(&path, format!("{long_body}{BMAP_CLOSING_TAG}")).unwrap();
        assert!(
            bmap_is_complete(&path),
            "closing tag at the end of a long file"
        );
    }

    #[test]
    fn the_bmap_wait_polls_until_the_bmap_is_complete_and_logs_once() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();
        crate::logging::capture::start_capture();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_BMAP_NAME);
        let mut sleeps = Vec::new();
        wait_for_bmap(&path, BMAP_POLL_INTERVAL, &mut |d| {
            sleeps.push(d);
            match sleeps.len() {
                2 => std::fs::write(&path, PARTIAL_BMAP).unwrap(),
                3 => std::fs::write(&path, format!("{PARTIAL_BMAP}{BMAP_CLOSING_TAG}")).unwrap(),
                n if n > MAX_TEST_POLLS => panic!("the wait did not end"),
                _ => {}
            }
        });
        assert_eq!(sleeps, [BMAP_POLL_INTERVAL; 3]);

        let waiting = format!("waiting for {}", path.display());
        let lines = crate::logging::capture::take_capture();
        assert_eq!(
            lines.iter().filter(|line| line.contains(&waiting)).count(),
            1,
            "got {lines:?}"
        );
    }

    #[test]
    fn a_file_that_stays_incomplete_is_reported_once() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();
        crate::logging::capture::start_capture();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_BMAP_NAME);
        std::fs::write(&path, "an xz stream").unwrap();
        let complete_at = 2 * usize::try_from(BMAP_STALE_POLLS).unwrap() + 2;
        let mut polls = 0;
        wait_for_bmap(&path, BMAP_POLL_INTERVAL, &mut |_| {
            polls += 1;
            match polls {
                n if n == complete_at => {
                    std::fs::write(&path, format!("{PARTIAL_BMAP}{BMAP_CLOSING_TAG}")).unwrap()
                }
                n if n > MAX_TEST_POLLS => panic!("the wait did not end"),
                _ => {}
            }
        });

        let lines = crate::logging::capture::take_capture();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("is it the bmap file?"))
                .count(),
            1,
            "got {lines:?}"
        );
    }

    const FIXTURE_BMAP: &str = include_str!("testdata/wic.bmap");
    const DISK_LEN: u64 = 64 * 1024 * 1024;

    fn recorded_bmap() -> Bmap {
        Bmap::parse(FIXTURE_BMAP).unwrap()
    }

    /// Lets `passes` calls of `step` succeed, then fails the next `failures`.
    struct Fault {
        step: &'static str,
        passes: usize,
        failures: usize,
    }

    fn fails_always(step: &'static str) -> Vec<Fault> {
        vec![Fault {
            step,
            passes: 0,
            failures: usize::MAX,
        }]
    }

    fn fails_once(step: &'static str) -> Vec<Fault> {
        vec![Fault {
            step,
            passes: 0,
            failures: 1,
        }]
    }

    /// Records every mode 2 side effect as one line, in call order. A call
    /// matches a fault when its line starts or ends with the fault's step.
    struct RecordingScpOps {
        calls: Vec<String>,
        faults: Vec<Fault>,
        disk_len: u64,
    }

    impl RecordingScpOps {
        fn record(&mut self, call: String) -> Result<(), FlashError> {
            let mut fail = false;
            for fault in &mut self.faults {
                if !(call.starts_with(fault.step) || call.ends_with(fault.step)) {
                    continue;
                }
                if fault.passes > 0 {
                    fault.passes -= 1;
                } else if fault.failures > 0 {
                    fault.failures -= 1;
                    fail = true;
                }
            }
            self.calls.push(call);
            if fail {
                return Err(FlashError::EfiFailed("injected".to_string()));
            }
            Ok(())
        }
    }

    #[cfg(feature = "grub")]
    impl efi::EfiOps for RecordingScpOps {
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

    impl ScpOps for RecordingScpOps {
        fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
            self.record(format!(
                "unmount {} and {}",
                rootfs.display(),
                disk.display()
            ))
        }

        fn bring_up_network(&mut self) -> Result<Ipv4Addr, FlashError> {
            self.record("network".to_string())?;
            Ok(IP)
        }

        fn start_dropbear(&mut self) -> Result<(), FlashError> {
            self.record("dropbear".to_string())
        }

        fn create_fifo(&mut self, path: &Path, owner: u32) -> Result<(), FlashError> {
            self.record(format!("fifo {} owned by {owner}", path.display()))
        }

        fn tell_operator(&mut self, message: &str) {
            self.calls.push(format!("tell {message}"));
        }

        fn wait_for_bmap(&mut self, path: &Path) {
            self.calls.push(format!("wait for {}", path.display()));
        }

        fn read_bmap(&mut self, path: &Path) -> Result<Bmap, FlashError> {
            self.record(format!("read bmap {}", path.display()))?;
            Ok(recorded_bmap())
        }

        fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError> {
            self.record(format!("size of {}", disk.display()))?;
            Ok(self.disk_len)
        }

        fn bmap_copy(
            &mut self,
            bmap: &Bmap,
            source: &Source<'_>,
            destination: &Destination<'_>,
        ) -> Result<(), FlashError> {
            assert_eq!(*bmap, recorded_bmap());
            self.record(format!(
                "bmap {} to {}",
                source.path().display(),
                destination.path().display()
            ))
        }

        fn zero_range(&mut self, dst: &Path, range: &ByteRange) -> Result<(), FlashError> {
            self.record(format!(
                "zero {}@{} len {}",
                dst.display(),
                range.offset,
                range.len
            ))
        }

        fn remove(&mut self, path: &Path) {
            self.calls.push(format!("remove {}", path.display()));
        }

        fn remove_fifo(&mut self, path: &Path) {
            self.calls.push(format!("remove fifo {}", path.display()));
        }

        fn reread_table(&mut self, disk: &Path) -> Result<(), FlashError> {
            self.record(format!("reread {}", disk.display()))
        }

        #[cfg(feature = "grub")]
        fn efi(&mut self) -> &mut dyn efi::EfiOps {
            self
        }

        fn sync(&mut self) {
            self.calls.push("sync".to_string());
        }
    }

    fn run_recorded(
        raw: &BuildConstants,
        faults: Vec<Fault>,
    ) -> (Result<(), FlashError>, Vec<String>) {
        run_recorded_on(raw, faults, DISK_LEN)
    }

    fn run_recorded_on(
        raw: &BuildConstants,
        faults: Vec<Fault>,
        disk_len: u64,
    ) -> (Result<(), FlashError>, Vec<String>) {
        let layout = PartitionLayout::new(RootDevice {
            base: PathBuf::from("/dev/sda"),
            partition_sep: "",
            root_partition: PathBuf::from("/dev/sda2"),
        })
        .unwrap();
        let ctx = ScpCtx {
            layout: &layout,
            rootfs: Path::new("/rootfs"),
        };
        let mut ops = RecordingScpOps {
            calls: Vec::new(),
            faults,
            disk_len,
        };
        let result = scp_with(&ctx, raw, &mut ops);
        (result, ops.calls)
    }

    const FLASH_PASS: &str = "to /dev/sda";

    fn setup_calls() -> Vec<String> {
        vec![
            "unmount /rootfs and /dev/sda".to_string(),
            "network".to_string(),
            "fifo /home/omnect/wic.xz owned by 1000".to_string(),
            "dropbear".to_string(),
        ]
    }

    /// One upload and flash, up to and including the zeroed head.
    fn attempt_calls() -> Vec<String> {
        let mut calls = vec![
            "tell please run: scp -O <bmap-file> omnect@192.168.0.7:wic.bmap".to_string(),
            "wait for /home/omnect/wic.bmap".to_string(),
            "read bmap /home/omnect/wic.bmap".to_string(),
            "size of /dev/sda".to_string(),
            "tell please run: scp -O <wic-image> omnect@192.168.0.7:wic.xz".to_string(),
        ];
        #[cfg(not(feature = "flash-mode-2-direct"))]
        calls.extend([
            "bmap /home/omnect/wic.xz to /home/omnect/wic".to_string(),
            "bmap /home/omnect/wic to /dev/sda".to_string(),
        ]);
        #[cfg(feature = "flash-mode-2-direct")]
        calls.push("bmap /home/omnect/wic.xz to /dev/sda".to_string());
        // The head minus the blocks 0-1, 5, 9-11 and 20 that the fixture maps.
        calls.extend([
            "zero /dev/sda@8192 len 12288".to_string(),
            "zero /dev/sda@24576 len 12288".to_string(),
            "zero /dev/sda@49152 len 32768".to_string(),
            "zero /dev/sda@82020 len 46055324".to_string(),
        ]);
        calls
    }

    fn finish_calls() -> Vec<String> {
        let mut calls = vec!["reread /dev/sda".to_string()];
        #[cfg(feature = "grub")]
        calls.extend([
            "mount efivarfs".to_string(),
            "efibootmgr".to_string(),
            r"efibootmgr -c -d /dev/sda -p 1 -L omnect_os -l \EFI\BOOT\bootx64.efi".to_string(),
            "efibootmgr -v".to_string(),
            "write entry dump to /dev/sda1".to_string(),
        ]);
        calls.push("sync".to_string());
        calls
    }

    /// The operator is asked for the image only after the bmap passed its
    /// checks, and the head is zeroed only after the flash.
    #[test]
    fn mode_2_runs_its_steps_in_order() {
        let (result, calls) = run_recorded(&raw_constants(), Vec::new());
        result.unwrap();
        assert_eq!(
            calls,
            [setup_calls(), attempt_calls(), finish_calls()].concat()
        );
    }

    #[test]
    fn a_missing_constant_stops_mode_2_before_anything_is_touched() {
        for raw in [
            BuildConstants {
                boot_start: None,
                ..raw_constants()
            },
            BuildConstants {
                boot_size: None,
                ..raw_constants()
            },
            BuildConstants {
                omnect_user_id: None,
                ..raw_constants()
            },
        ] {
            let (result, calls) = run_recorded(&raw, Vec::new());
            assert!(matches!(result, Err(FlashError::MissingBuildConstant(_))));
            assert!(calls.is_empty(), "touched: {calls:?}");
        }
    }

    #[test]
    fn a_failed_setup_or_finish_step_stops_mode_2_there() {
        let steps = [
            ("unmount", true),
            ("network", true),
            ("fifo", true),
            ("dropbear", true),
            ("reread", false),
            #[cfg(feature = "grub")]
            ("mount efivarfs", true),
            #[cfg(feature = "grub")]
            ("efibootmgr", true),
            #[cfg(feature = "grub")]
            ("write entry dump", true),
        ];
        for (step, log_is_kept) in steps {
            let (result, calls) = run_recorded(&raw_constants(), fails_always(step));
            let last = calls.last().unwrap();
            assert!(
                last.starts_with(step) || last.ends_with(step),
                "{step} must be the last step, got {calls:?}"
            );
            assert!(result.is_err(), "{step}");
            assert_eq!(keeps_log(&result), log_is_kept, "{step}: {result:?}");
        }
    }

    fn retry_calls() -> Vec<String> {
        let mut calls = vec![
            "tell flash failed, asking for the bmap and the image again".to_string(),
            "remove /home/omnect/wic.bmap".to_string(),
        ];
        #[cfg(not(feature = "flash-mode-2-direct"))]
        calls.push("remove /home/omnect/wic".to_string());
        calls.extend([
            "remove fifo /home/omnect/wic.xz".to_string(),
            "fifo /home/omnect/wic.xz owned by 1000".to_string(),
        ]);
        calls
    }

    #[test]
    fn a_failed_attempt_asks_for_bmap_and_image_again() {
        // Without the verify pass, the copy from the FIFO is the flash pass.
        let mut steps = vec!["read bmap", "bmap /home/omnect/wic.xz", "zero"];
        if !cfg!(feature = "flash-mode-2-direct") {
            steps.push(FLASH_PASS);
        }
        for step in steps {
            let (result, calls) = run_recorded(&raw_constants(), fails_once(step));
            result.unwrap_or_else(|e| panic!("{step}: {e}"));

            let attempt = attempt_calls();
            let failed_at = attempt
                .iter()
                .position(|call| call.starts_with(step) || call.ends_with(step))
                .unwrap();
            assert_eq!(
                calls,
                [
                    setup_calls(),
                    attempt[..=failed_at].to_vec(),
                    retry_calls(),
                    attempt,
                    finish_calls()
                ]
                .concat(),
                "{step}"
            );
        }
    }

    fn then_fifo_fails(mut faults: Vec<Fault>, fifo_passes: usize) -> Vec<Fault> {
        faults.push(Fault {
            step: "fifo",
            passes: fifo_passes,
            failures: 1,
        });
        faults
    }

    #[test]
    fn a_failed_fifo_after_a_failed_attempt_stops_mode_2() {
        let mut steps = vec![("read bmap", false), ("zero", true), (FLASH_PASS, true)];
        if !cfg!(feature = "flash-mode-2-direct") {
            steps.push(("bmap /home/omnect/wic.xz", false));
        }
        for (step, disk_written) in steps {
            let (result, calls) =
                run_recorded(&raw_constants(), then_fifo_fails(fails_once(step), 1));
            assert_eq!(
                calls.last().unwrap(),
                "fifo /home/omnect/wic.xz owned by 1000",
                "{step}"
            );
            assert_eq!(
                matches!(result, Err(FlashError::DiskPartlyWritten { .. })),
                disk_written,
                "{step}: {result:?}"
            );
            assert_eq!(keeps_log(&result), !disk_written, "{step}: {result:?}");
        }
    }

    #[test]
    fn a_disk_written_by_an_earlier_attempt_keeps_the_log_off_it() {
        let mut faults = fails_once(FLASH_PASS);
        faults.push(Fault {
            step: "read bmap",
            passes: 1,
            failures: 1,
        });
        let (result, _) = run_recorded(&raw_constants(), then_fifo_fails(faults, 2));
        assert!(
            matches!(result, Err(FlashError::DiskPartlyWritten { .. })),
            "{result:?}"
        );
        assert!(!keeps_log(&result));
    }

    #[test]
    fn mode_2_asks_again_as_often_as_attempts_fail() {
        let faults = vec![Fault {
            step: FLASH_PASS,
            passes: 0,
            failures: 3,
        }];
        let (result, calls) = run_recorded(&raw_constants(), faults);
        result.unwrap();
        let retries = calls
            .iter()
            .filter(|call| call.starts_with("tell flash failed"))
            .count();
        assert_eq!(retries, 3);
        assert!(calls.ends_with(&finish_calls()), "got {calls:?}");
    }

    #[test]
    fn an_image_larger_than_the_disk_is_refused_before_any_write() {
        let image_size = recorded_bmap().image_size();
        let (result, calls) = run_recorded_on(
            &raw_constants(),
            then_fifo_fails(Vec::new(), 1),
            image_size - 1,
        );
        assert!(keeps_log(&result), "{result:?}");
        assert!(
            !calls
                .iter()
                .any(|call| call.starts_with("zero") || call.ends_with(FLASH_PASS)),
            "got {calls:?}"
        );
        let size = calls.iter().position(|call| call == "size of /dev/sda");
        assert_eq!(calls[size.unwrap() + 1], retry_calls()[0], "got {calls:?}");

        let (result, _) = run_recorded_on(&raw_constants(), Vec::new(), image_size);
        result.unwrap();
    }

    #[test]
    fn the_real_ops_read_the_bmap_and_the_disk_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_BMAP_NAME);
        let disk = dir.path().join("sdz");
        let mut ops = RealScpOps::default();

        std::fs::write(&path, FIXTURE_BMAP).unwrap();
        assert_eq!(ops.read_bmap(&path).unwrap(), recorded_bmap());
        std::fs::write(&path, "<bmap").unwrap();
        assert!(matches!(
            ops.read_bmap(&path),
            Err(FlashError::InvalidBmap { .. })
        ));

        assert!(matches!(
            ops.device_len(&disk),
            Err(FlashError::InvalidDestination { .. })
        ));
        std::fs::write(&disk, [0u8; 1024]).unwrap();
        assert_eq!(ops.device_len(&disk).unwrap(), 1024);
    }

    #[test]
    fn remove_ignores_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_FIFO_NAME);
        let mut ops = RealScpOps::default();
        ops.remove(&path);
        create_owned_fifo(&path, Uid::current(), Gid::current()).unwrap();
        ops.remove(&path);
        assert!(!path.exists());
    }

    #[test]
    fn removing_the_fifo_releases_a_writer_blocked_in_open() {
        const WRITER_TIMEOUT: Duration = Duration::from_secs(5);
        const STATE_POLL: Duration = Duration::from_millis(1);
        // More than a pipe holds, so the write cannot end in the pipe buffer.
        const WRITE_LEN: usize = 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIC_FIFO_NAME);
        create_owned_fifo(&path, Uid::current(), Gid::current()).unwrap();

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        thread::spawn(move || {
            tid_tx.send(nix::unistd::gettid()).unwrap();
            let result = OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .and_then(|mut fifo| std::io::Write::write_all(&mut fifo, &[0; WRITE_LEN]));
            result_tx.send(result.map_err(|e| e.kind())).unwrap();
        });
        // Field 3 of the task's stat is its state; `S` means it sleeps in `open()`.
        let stat = format!("/proc/self/task/{}/stat", tid_rx.recv().unwrap());
        while std::fs::read_to_string(&stat)
            .unwrap()
            .rsplit(") ")
            .next()
            .is_none_or(|rest| !rest.starts_with('S'))
        {
            thread::sleep(STATE_POLL);
        }

        remove_fifo(&path);

        assert!(!path.exists());
        assert_eq!(
            result_rx
                .recv_timeout(WRITER_TIMEOUT)
                .expect("the writer still hangs"),
            Err(ErrorKind::BrokenPipe)
        );
    }

    #[cfg(feature = "grub")]
    #[test]
    fn the_efi_step_uses_the_re_read_table() {
        let (_, calls) = run_recorded(&raw_constants(), fails_always("mount efivarfs"));
        let reread = calls.iter().position(|call| call == "reread /dev/sda");
        let efi = calls.iter().position(|call| call == "mount efivarfs");
        assert!(reread.is_some() && reread < efi, "got {calls:?}");
    }
}
