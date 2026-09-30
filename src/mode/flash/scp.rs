//! Flash mode 2: flash the running disk with a `wic.xz` the operator pushes in
//! over `scp`.

use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
#[cfg(feature = "grub")]
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::stat::Mode;
use nix::unistd::{Gid, Uid, chown, mkfifo};

use crate::bootloader::sync_filesystems;
use crate::config::{BuildConstant, build};
use crate::error::FlashError;
#[cfg(feature = "grub")]
use crate::error::PartitionTableOperation;
use crate::mode::flash::bmap::{self, BmapArgs};
#[cfg(feature = "grub")]
use crate::mode::flash::efi;
use crate::mode::flash::rawio::{self, kb_to_bytes};
use crate::mode::flash::{net, unmount};
use crate::partition::PartitionLayout;
#[cfg(feature = "grub")]
use crate::partition::PartitionName;

const OMNECT_HOME: &str = "/home/omnect";
const OMNECT_USER: &str = "omnect";
const WIC_FIFO_NAME: &str = "wic.xz";
const WIC_BMAP_NAME: &str = "wic.bmap";
/// The verify pass writes the mapped, decompressed image here, in RAM.
#[cfg(not(feature = "flash-mode-2-direct"))]
const WIC_MATERIALIZED_NAME: &str = "wic";
const BMAP_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The last element of a bmap file.
const BMAP_CLOSING_TAG: &str = "</bmap>";

pub(crate) struct ScpCtx<'a> {
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
}

/// The build-time constants as `build.rs` generated them, KB-valued.
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

/// The build-time constants mode 2 needs, validated.
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
    format!("please run: scp <bmap-file> {OMNECT_USER}@{ip}:{WIC_BMAP_NAME}")
}

fn image_instruction(ip: Ipv4Addr) -> String {
    format!("please run: scp <wic-image> {OMNECT_USER}@{ip}:{WIC_FIFO_NAME}")
}

#[cfg(feature = "grub")]
fn boot_partition(layout: &PartitionLayout) -> Result<&Path, FlashError> {
    layout
        .get(PartitionName::Boot)
        .map(PathBuf::as_path)
        .ok_or_else(|| FlashError::PartitionTable {
            device: layout.device.base.clone(),
            operation: PartitionTableOperation::Lookup,
            reason: format!("the layout has no {} partition", PartitionName::Boot),
        })
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

/// `scp` creates the file before it writes the content, and with
/// `flash-mode-2-direct` the disk head is zeroed before `bmaptool` reads the
/// bmap, so a half-written bmap must not end the wait.
fn bmap_is_complete(path: &Path) -> bool {
    path.is_file()
        && fs::read(path).is_ok_and(|content| {
            content
                .trim_ascii_end()
                .ends_with(BMAP_CLOSING_TAG.as_bytes())
        })
}

fn wait_for_bmap(path: &Path, interval: Duration, sleep: &mut dyn FnMut(Duration)) {
    log::info!("waiting for {}", path.display());
    while !bmap_is_complete(path) {
        sleep(interval);
    }
}

/// The side effects of mode 2, so a test can pin their order.
trait ScpOps {
    fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError>;
    fn bring_up_network(&mut self) -> Result<Ipv4Addr, FlashError>;
    fn start_dropbear(&mut self) -> Result<(), FlashError>;
    fn create_fifo(&mut self, path: &Path, owner: u32) -> Result<(), FlashError>;
    fn tell_operator(&mut self, message: &str);
    fn wait_for_bmap(&mut self, path: &Path);
    fn bmap_copy(&mut self, args: &BmapArgs<'_>) -> Result<(), FlashError>;
    fn zero_range(&mut self, dst: &Path, offset: u64, len: u64) -> Result<(), FlashError>;
    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps;
    fn sync(&mut self);
}

struct RealScpOps;

#[cfg(feature = "grub")]
impl efi::EfiOps for RealScpOps {
    fn mount_efivarfs(&mut self) -> Result<(), FlashError> {
        efi::RealEfiOps.mount_efivarfs()
    }

    fn efibootmgr(&mut self, args: &[String]) -> Result<String, FlashError> {
        efi::RealEfiOps.efibootmgr(args)
    }

    fn write_entry_dump(&mut self, boot_partition: &Path, dump: &str) -> Result<(), FlashError> {
        efi::RealEfiOps.write_entry_dump(boot_partition, dump)
    }
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

    fn zero_range(&mut self, dst: &Path, offset: u64, len: u64) -> Result<(), FlashError> {
        rawio::zero_range(dst, offset, len)
    }

    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps {
        self
    }

    fn sync(&mut self) {
        sync_filesystems();
    }
}

/// Flash the running disk with the image the operator pushes in.
pub(crate) fn run_scp(ctx: &ScpCtx<'_>) -> Result<(), FlashError> {
    scp_with(ctx, &BuildConstants::from_build(), &mut RealScpOps)
}

fn scp_with(
    ctx: &ScpCtx<'_>,
    raw: &BuildConstants,
    ops: &mut dyn ScpOps,
) -> Result<(), FlashError> {
    let constants = required_constants(raw)?;
    let disk = ctx.layout.device.base.as_path();
    #[cfg(feature = "grub")]
    let boot_partition = boot_partition(ctx.layout)?;

    let home = Path::new(OMNECT_HOME);
    let fifo = home.join(WIC_FIFO_NAME);
    let bmap = home.join(WIC_BMAP_NAME);

    log::info!(
        "flash mode 2: flashing {} with an image pushed in over scp",
        disk.display()
    );

    ops.unmount(ctx.rootfs, disk)?;
    let ip = ops.bring_up_network()?;
    ops.start_dropbear()?;
    ops.create_fifo(&fifo, constants.omnect_user_id)?;

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
        ops.bmap_copy(&BmapArgs {
            bmap: &bmap,
            source: &fifo,
            destination: &image,
        })?;
        image
    };
    #[cfg(feature = "flash-mode-2-direct")]
    let source = fifo;

    // Without this, some devices failed to boot after a flash, on GRUB and on
    // U-Boot. The root cause is unknown.
    log::info!(
        "zeroing the first {} bytes of {}",
        constants.zero_head_bytes,
        disk.display()
    );
    ops.zero_range(disk, 0, constants.zero_head_bytes)?;

    log::info!("flashing {} onto {}", source.display(), disk.display());
    ops.bmap_copy(&BmapArgs {
        bmap: &bmap,
        source: &source,
        destination: disk,
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
        assert!(bmap_instruction(IP).contains("scp <bmap-file> omnect@192.168.0.7:wic.bmap"));
        assert!(image_instruction(IP).contains("scp <wic-image> omnect@192.168.0.7:wic.xz"));
    }

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

    /// Enough polls for the test sequence; more means the wait never ends.
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

    /// Records every mode 2 side effect as one line, in call order.
    struct RecordingScpOps {
        calls: Vec<String>,
    }

    #[cfg(feature = "grub")]
    impl efi::EfiOps for RecordingScpOps {
        fn mount_efivarfs(&mut self) -> Result<(), FlashError> {
            self.calls.push("mount efivarfs".to_string());
            Ok(())
        }

        fn efibootmgr(&mut self, args: &[String]) -> Result<String, FlashError> {
            self.calls.push(
                format!("efibootmgr {}", args.join(" "))
                    .trim_end()
                    .to_string(),
            );
            Ok(String::new())
        }

        fn write_entry_dump(
            &mut self,
            boot_partition: &Path,
            _dump: &str,
        ) -> Result<(), FlashError> {
            self.calls
                .push(format!("write entry dump to {}", boot_partition.display()));
            Ok(())
        }
    }

    impl ScpOps for RecordingScpOps {
        fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
            self.calls.push(format!(
                "unmount {} and {}",
                rootfs.display(),
                disk.display()
            ));
            Ok(())
        }

        fn bring_up_network(&mut self) -> Result<Ipv4Addr, FlashError> {
            self.calls.push("network".to_string());
            Ok(IP)
        }

        fn start_dropbear(&mut self) -> Result<(), FlashError> {
            self.calls.push("dropbear".to_string());
            Ok(())
        }

        fn create_fifo(&mut self, path: &Path, owner: u32) -> Result<(), FlashError> {
            self.calls
                .push(format!("fifo {} owned by {owner}", path.display()));
            Ok(())
        }

        fn tell_operator(&mut self, message: &str) {
            self.calls.push(format!("tell {message}"));
        }

        fn wait_for_bmap(&mut self, path: &Path) {
            self.calls.push(format!("wait for {}", path.display()));
        }

        fn bmap_copy(&mut self, args: &BmapArgs<'_>) -> Result<(), FlashError> {
            self.calls.push(format!(
                "bmap {} {} to {}",
                args.bmap.display(),
                args.source.display(),
                args.destination.display()
            ));
            Ok(())
        }

        fn zero_range(&mut self, dst: &Path, offset: u64, len: u64) -> Result<(), FlashError> {
            self.calls
                .push(format!("zero {}@{offset} len {len}", dst.display()));
            Ok(())
        }

        #[cfg(feature = "grub")]
        fn efi(&mut self) -> &mut dyn efi::EfiOps {
            self
        }

        fn sync(&mut self) {
            self.calls.push("sync".to_string());
        }
    }

    fn run_recorded(raw: &BuildConstants) -> (Result<(), FlashError>, Vec<String>) {
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
        let mut ops = RecordingScpOps { calls: Vec::new() };
        let result = scp_with(&ctx, raw, &mut ops);
        (result, ops.calls)
    }

    /// The spec 5.4 order: the operator is asked for the image only after the
    /// bmap arrived, and the disk head is zeroed right before the flash.
    #[test]
    fn mode_2_runs_its_steps_in_the_spec_order() {
        let (result, calls) = run_recorded(&raw_constants());
        result.unwrap();

        let mut expected = vec![
            "unmount /rootfs and /dev/sda".to_string(),
            "network".to_string(),
            "dropbear".to_string(),
            "fifo /home/omnect/wic.xz owned by 1000".to_string(),
            "tell please run: scp <bmap-file> omnect@192.168.0.7:wic.bmap".to_string(),
            "wait for /home/omnect/wic.bmap".to_string(),
            "tell please run: scp <wic-image> omnect@192.168.0.7:wic.xz".to_string(),
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
            let (result, calls) = run_recorded(&raw);
            assert!(matches!(result, Err(FlashError::MissingBuildConstant(_))));
            assert!(calls.is_empty(), "touched: {calls:?}");
        }
    }
}
