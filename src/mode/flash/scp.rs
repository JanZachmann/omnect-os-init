//! Flash mode 2: flash the running disk with a `wic.xz` the operator pushes in
//! over `scp`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::net::Ipv4Addr;
use std::path::Path;
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::stat::Mode;
use nix::unistd::{Gid, Uid, chown, mkfifo};

use crate::bootloader::sync_filesystems;
use crate::config::{BuildConstant, build};
use crate::error::FlashError;
use crate::mode::flash::bmap::{self, BmapArgs};
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
const WIC_MATERIALIZED_NAME: &str = "wic";
const BMAP_POLL_INTERVAL: Duration = Duration::from_secs(1);
const BMAP_CLOSING_TAG: &str = "</bmap>";
/// Enough for the closing tag and the whitespace an editor or generator
/// leaves after it.
const BMAP_TAIL_LEN: u64 = 64;

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
    zero_head_bytes: u64,
    omnect_user_id: u32,
}

/// The disk head that is zeroed before the flash: everything up to the end
/// of the boot partition.
fn zero_head_kib(boot_start: Option<u64>, boot_size: Option<u64>) -> Result<u64, FlashError> {
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
    let zero_head_bytes = kb_to_bytes(
        zero_head_kib(raw.boot_start, raw.boot_size)?,
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
        zero_head_bytes,
        omnect_user_id,
    })
}

fn bmap_instruction(ip: Ipv4Addr) -> String {
    format!("please run: scp -O <bmap-file> {OMNECT_USER}@{ip}:{WIC_BMAP_NAME}")
}

fn image_instruction(ip: Ipv4Addr) -> String {
    format!("please run: scp -O <wic-image> {OMNECT_USER}@{ip}:{WIC_FIFO_NAME}")
}

/// The FIFO lets `bmaptool` read the image while `scp` is still writing it.
fn create_owned_fifo(path: &Path, uid: Uid, gid: Gid) -> Result<(), FlashError> {
    let failed = |errno: Errno| FlashError::PathIo {
        path: path.to_path_buf(),
        source: errno.into(),
    };
    mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(failed)?;
    chown(path, Some(uid), Some(gid)).map_err(failed)
}

/// A half-copied bmap must not end the wait: `bmaptool` fails on it, and with
/// `flash-mode-2-direct` only after the disk head was zeroed. Only the tail is
/// read, so a large file pushed under the bmap name costs no RAM.
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
    while !bmap_is_complete(path) {
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
    fn bmap_copy(&mut self, args: &BmapArgs<'_>) -> Result<(), FlashError>;
    fn zero_range(&mut self, dst: &Path, range: &ByteRange) -> Result<(), FlashError>;
    #[cfg(not(feature = "flash-mode-2-direct"))]
    fn discard(&mut self, path: &Path);
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

    fn bmap_copy(&mut self, args: &BmapArgs<'_>) -> Result<(), FlashError> {
        bmap::copy(args)
    }

    fn zero_range(&mut self, dst: &Path, range: &ByteRange) -> Result<(), FlashError> {
        rawio::zero_range(dst, range)
    }

    #[cfg(not(feature = "flash-mode-2-direct"))]
    fn discard(&mut self, path: &Path) {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                log::warn!("failed to remove {}: {e}", path.display());
            }
            _ => {}
        }
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

/// Flash the running disk with the image the operator pushes in.
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

    let home = Path::new(OMNECT_HOME);
    let fifo = home.join(WIC_FIFO_NAME);
    let bmap = home.join(WIC_BMAP_NAME);

    log::info!(
        "flash mode 2: flashing {} with an image pushed in over scp",
        disk.display()
    );

    ops.unmount(ctx.rootfs, disk)?;
    let ip = ops.bring_up_network()?;
    // Before dropbear, so a client that can log in always finds the FIFO.
    ops.create_fifo(&fifo, constants.omnect_user_id)?;
    ops.start_dropbear()?;

    ops.tell_operator(&bmap_instruction(ip));
    // Unbounded: this waits for a person to start the `scp`.
    ops.wait_for_bmap(&bmap);
    ops.tell_operator(&image_instruction(ip));

    // The verify pass reads the whole stream first, so a broken transfer
    // fails before the disk is written.
    #[cfg(not(feature = "flash-mode-2-direct"))]
    let source = {
        let image = home.join(WIC_MATERIALIZED_NAME);
        log::info!("verifying {}", fifo.display());
        // The image is held in the initramfs root, so it must fit in RAM; a
        // partial copy is removed so the RAM is free for the rest of the run.
        if let Err(e) = ops.bmap_copy(&BmapArgs {
            bmap: &bmap,
            source: &fifo,
            destination: &image,
        }) {
            ops.discard(&image);
            return Err(e);
        }
        image
    };
    #[cfg(feature = "flash-mode-2-direct")]
    let source = fifo;

    // bmaptool writes only mapped blocks, so bytes of the old image in unmapped
    // ranges of the boot area would survive the flash.
    let partly_written = |source| FlashError::DiskPartlyWritten {
        disk: disk.to_path_buf(),
        source: Box::new(source),
    };
    log::info!(
        "zeroing the first {} bytes of {}",
        constants.zero_head_bytes,
        disk.display()
    );
    let head = ByteRange {
        offset: 0,
        len: constants.zero_head_bytes,
    };
    ops.zero_range(disk, &head).map_err(partly_written)?;

    log::info!("flashing {} onto {}", source.display(), disk.display());
    ops.bmap_copy(&BmapArgs {
        bmap: &bmap,
        source: &source,
        destination: disk,
    })
    .map_err(partly_written)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::RootDevice;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    const IP: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 7);

    #[test]
    fn the_zeroed_head_reaches_the_end_of_the_boot_partition() {
        assert_eq!(zero_head_kib(Some(4096), Some(40960)).unwrap(), 45056);
    }

    #[test]
    fn the_zeroed_head_needs_boot_start_and_boot_size() {
        assert!(matches!(
            zero_head_kib(None, Some(40960)),
            Err(FlashError::MissingBuildConstant(BuildConstant::BootStart))
        ));
        assert!(matches!(
            zero_head_kib(Some(4096), None),
            Err(FlashError::MissingBuildConstant(BuildConstant::BootSize))
        ));
    }

    #[test]
    fn a_zeroed_head_that_overflows_is_refused() {
        assert!(matches!(
            zero_head_kib(Some(u64::MAX), Some(1)),
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
                zero_head_bytes: 46_137_344,
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

    const MAX_TEST_POLLS: usize = 10;
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

    /// Records every mode 2 side effect as one line, in call order, and fails
    /// the first call whose line starts or ends with `fail_on`.
    struct RecordingScpOps {
        calls: Vec<String>,
        fail_on: Option<&'static str>,
    }

    impl RecordingScpOps {
        fn record(&mut self, call: String) -> Result<(), FlashError> {
            let fail = self
                .fail_on
                .is_some_and(|step| call.starts_with(step) || call.ends_with(step));
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

        fn bmap_copy(&mut self, args: &BmapArgs<'_>) -> Result<(), FlashError> {
            self.record(format!(
                "bmap {} {} to {}",
                args.bmap.display(),
                args.source.display(),
                args.destination.display()
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

        #[cfg(not(feature = "flash-mode-2-direct"))]
        fn discard(&mut self, path: &Path) {
            self.calls.push(format!("discard {}", path.display()));
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
        fail_on: Option<&'static str>,
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
            fail_on,
        };
        let result = scp_with(&ctx, raw, &mut ops);
        (result, ops.calls)
    }

    /// The operator is asked for the image only after the bmap arrived, and the
    /// disk head is zeroed right before the flash.
    #[test]
    fn mode_2_runs_its_steps_in_order() {
        let (result, calls) = run_recorded(&raw_constants(), None);
        result.unwrap();

        let mut expected = vec![
            "unmount /rootfs and /dev/sda".to_string(),
            "network".to_string(),
            "fifo /home/omnect/wic.xz owned by 1000".to_string(),
            "dropbear".to_string(),
            "tell please run: scp -O <bmap-file> omnect@192.168.0.7:wic.bmap".to_string(),
            "wait for /home/omnect/wic.bmap".to_string(),
            "tell please run: scp -O <wic-image> omnect@192.168.0.7:wic.xz".to_string(),
        ];
        #[cfg(not(feature = "flash-mode-2-direct"))]
        expected.extend([
            "bmap /home/omnect/wic.bmap /home/omnect/wic.xz to /home/omnect/wic".to_string(),
            "zero /dev/sda@0 len 46137344".to_string(),
            "bmap /home/omnect/wic.bmap /home/omnect/wic to /dev/sda".to_string(),
        ]);
        #[cfg(feature = "flash-mode-2-direct")]
        expected.extend([
            "zero /dev/sda@0 len 46137344".to_string(),
            "bmap /home/omnect/wic.bmap /home/omnect/wic.xz to /dev/sda".to_string(),
        ]);
        expected.push("reread /dev/sda".to_string());
        #[cfg(feature = "grub")]
        expected.extend([
            "mount efivarfs".to_string(),
            "efibootmgr".to_string(),
            r"efibootmgr -c -d /dev/sda -p 1 -L omnect_os -l \EFI\BOOT\bootx64.efi".to_string(),
            "efibootmgr -v".to_string(),
            "write entry dump to /dev/sda1".to_string(),
        ]);
        expected.push("sync".to_string());

        assert_eq!(calls, expected);
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
            let (result, calls) = run_recorded(&raw, None);
            assert!(matches!(result, Err(FlashError::MissingBuildConstant(_))));
            assert!(calls.is_empty(), "touched: {calls:?}");
        }
    }

    #[test]
    fn a_failed_step_stops_mode_2_there() {
        use crate::mode::flash::keeps_log;

        const FLASH_PASS: &str = "to /dev/sda";
        // Without the verify pass, the first `bmap` call is the flash pass.
        let direct = cfg!(feature = "flash-mode-2-direct");
        let steps = [
            ("unmount", true),
            ("network", true),
            ("fifo", true),
            ("dropbear", true),
            ("bmap", !direct),
            ("zero", false),
            (FLASH_PASS, false),
            ("reread", false),
            #[cfg(feature = "grub")]
            ("mount efivarfs", true),
            #[cfg(feature = "grub")]
            ("efibootmgr", true),
            #[cfg(feature = "grub")]
            ("write entry dump", true),
        ];
        for (step, log_is_kept) in steps {
            let (result, calls) = run_recorded(&raw_constants(), Some(step));
            let last = calls
                .iter()
                .rfind(|call| !call.starts_with("discard"))
                .unwrap();
            assert!(
                last.starts_with(step) || last.ends_with(step),
                "{step} must be the last step, got {calls:?}"
            );
            assert!(result.is_err(), "{step}");
            assert_eq!(keeps_log(&result), log_is_kept, "{step}: {result:?}");
        }
    }

    #[test]
    fn a_failed_flash_pass_is_a_partly_written_disk() {
        let (result, _) = run_recorded(&raw_constants(), Some("to /dev/sda"));
        assert!(
            matches!(result, Err(FlashError::DiskPartlyWritten { .. })),
            "{result:?}"
        );
    }

    #[cfg(feature = "grub")]
    #[test]
    fn the_efi_step_uses_the_re_read_table() {
        let (_, calls) = run_recorded(&raw_constants(), Some("mount efivarfs"));
        let reread = calls.iter().position(|call| call == "reread /dev/sda");
        let efi = calls.iter().position(|call| call == "mount efivarfs");
        assert!(reread.is_some() && reread < efi, "got {calls:?}");
    }

    #[cfg(not(feature = "flash-mode-2-direct"))]
    #[test]
    fn a_failed_verify_pass_stops_mode_2_before_the_disk_is_written() {
        let (result, calls) = run_recorded(&raw_constants(), Some("bmap"));
        assert!(result.is_err());
        assert!(
            calls.ends_with(&[
                "bmap /home/omnect/wic.bmap /home/omnect/wic.xz to /home/omnect/wic".to_string(),
                "discard /home/omnect/wic".to_string(),
            ]),
            "got {calls:?}"
        );
        assert!(!calls.iter().any(|call| call.starts_with("zero")));
    }
}
