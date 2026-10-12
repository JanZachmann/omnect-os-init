//! Flash mode 3: download a `wic.xz` and its sha256 file, check the image and
//! flash it onto the running disk.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};

use crate::{
    bootloader::sync_filesystems,
    error::FlashError,
    mode::flash::{
        SHA256_LEN, child,
        config::redacted,
        net, partly_written,
        rawio::{self, COPY_BUFFER_SIZE, open_existing_for_write},
        read_small_text, sha256_from_hex, unmount, xz,
    },
    partition::PartitionLayout,
};
#[cfg(feature = "grub")]
use crate::{
    mode::flash::{efi, layout_partition},
    partition::PartitionName,
};

const CURL_CMD: &str = "/usr/bin/curl";
/// `--globoff`: brackets and braces in a URL are literal characters.
const CURL_FLAGS: [&str; 4] = ["--fail", "--no-progress-meter", "--location", "--globoff"];
const CURL_CONNECT_TIMEOUT_FLAG: &str = "--connect-timeout";
const CURL_SPEED_LIMIT_FLAG: &str = "--speed-limit";
const CURL_SPEED_TIME_FLAG: &str = "--speed-time";
const CURL_OUTPUT_FLAG: &str = "--output";
/// Marks the next argument as the URL, also when it starts with `-`.
const CURL_URL_FLAG: &str = "--url";
/// Without a reliable clock, the validity of a certificate cannot be checked.
#[cfg(not(feature = "rtc"))]
const CURL_INSECURE_FLAG: &str = "--insecure";
const CURL_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A transfer that stays below `CURL_STALL_SPEED` bytes per second for
/// `CURL_STALL_TIME` is aborted, so a stalled server cannot block PID 1.
const CURL_STALL_SPEED: u32 = 1;
const CURL_STALL_TIME: Duration = Duration::from_secs(60);

const DOWNLOAD_DIR: &str = "/tmp/flash-mode-3";
const IMAGE_FILE_NAME: &str = "wic.xz";
const SHA256_FILE_NAME: &str = "wic.xz.sha256";
/// Room for a digest and a long file name.
const MAX_SHA256_FILE_LEN: u64 = 4096;

pub(crate) struct UrlCtx<'a> {
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
    pub(crate) image_url: &'a str,
    pub(crate) sha256_url: &'a str,
}

fn curl_args(url: &str, target: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = CURL_FLAGS.iter().map(OsString::from).collect();
    #[cfg(not(feature = "rtc"))]
    args.push(CURL_INSECURE_FLAG.into());
    args.extend([
        CURL_CONNECT_TIMEOUT_FLAG.into(),
        CURL_CONNECT_TIMEOUT.as_secs().to_string().into(),
        CURL_SPEED_LIMIT_FLAG.into(),
        CURL_STALL_SPEED.to_string().into(),
        CURL_SPEED_TIME_FLAG.into(),
        CURL_STALL_TIME.as_secs().to_string().into(),
        CURL_OUTPUT_FLAG.into(),
        target.into(),
        CURL_URL_FLAG.into(),
        url.into(),
    ]);
    args
}

/// The digest is the first field of the first line that has one, as in
/// `sha256sum` output.
fn expected_sha256(sha256_file: &str) -> Result<[u8; SHA256_LEN], String> {
    let field = sha256_file
        .split_whitespace()
        .next()
        .ok_or("the sha256 file is empty")?;
    sha256_from_hex(field).map_err(|e| format!("'{field}' is not a sha256 digest: {e}"))
}

fn read_sha256_file(path: &Path) -> Result<String, FlashError> {
    read_small_text(path, MAX_SHA256_FILE_LEN, |reason| {
        FlashError::InvalidImage {
            path: path.to_path_buf(),
            reason: format!("the sha256 file is {reason}"),
        }
    })
}

fn sha256_of(path: &Path) -> Result<[u8; SHA256_LEN], FlashError> {
    let io_failed = |source| FlashError::PathIo {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io_failed)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buf).map_err(io_failed)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buf[..read]);
    }
}

/// Decode `image` onto `disk` and write every byte, also the zero blocks, so
/// nothing of the old image survives inside the new one.
fn write_image(image: &Path, disk: &Path, size: u64) -> Result<(), FlashError> {
    let failed = |reason: String| FlashError::CopyFailed {
        src: image.to_path_buf(),
        dst: disk.to_path_buf(),
        reason,
    };
    let input = File::open(image).map_err(|e| failed(format!("opening source: {e}")))?;
    let mut output =
        open_existing_for_write(disk).map_err(|e| failed(format!("opening destination: {e}")))?;
    decode_into(xz::decoder(input), &mut output, size).map_err(failed)?;
    output
        .sync_all()
        .map_err(|e| failed(format!("syncing destination: {e}")))
}

/// Stops before a byte past `size` is written, so a stream that does not
/// match its index cannot write beyond the checked length.
fn decode_into(mut input: impl Read, output: &mut impl Write, size: u64) -> Result<(), String> {
    let mut buf = vec![0u8; COPY_BUFFER_SIZE];
    let mut written: u64 = 0;
    loop {
        let read = input.read(&mut buf).map_err(|e| xz::read_error(&e))?;
        if read == 0 {
            break;
        }
        written = written
            .checked_add(read as u64)
            .filter(|&total| total <= size)
            .ok_or_else(|| {
                format!("the image decodes to more than the {size} bytes its index records")
            })?;
        output
            .write_all(&buf[..read])
            .map_err(|e| format!("writing destination: {e}"))?;
    }
    if written != size {
        return Err(format!(
            "the image decodes to {written} bytes, its index records {size}"
        ));
    }
    Ok(())
}

trait UrlOps {
    fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError>;
    fn bring_up_network(&mut self) -> Result<(), FlashError>;
    fn download(&mut self, url: &str, target: &Path) -> Result<(), FlashError>;
    fn read_sha256_file(&mut self, path: &Path) -> Result<String, FlashError>;
    fn sha256_of(&mut self, path: &Path) -> Result<[u8; SHA256_LEN], FlashError>;
    fn image_size(&mut self, image: &Path) -> Result<u64, FlashError>;
    fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError>;
    fn write_image(&mut self, image: &Path, disk: &Path, size: u64) -> Result<(), FlashError>;
    fn reread_table(&mut self, disk: &Path) -> Result<(), FlashError>;
    #[cfg(feature = "grub")]
    fn efi(&mut self) -> &mut dyn efi::EfiOps;
    fn sync(&mut self);
}

#[derive(Default)]
struct RealUrlOps {
    #[cfg(feature = "grub")]
    efi: efi::RealEfiOps,
}

impl UrlOps for RealUrlOps {
    fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
        unmount::unmount_target_disk(rootfs, disk)
    }

    fn bring_up_network(&mut self) -> Result<(), FlashError> {
        net::bring_up(net::Ipv4Need::Lease).map(|_| ())
    }

    /// The error names the redacted URL, because the arguments and the full
    /// URL can hold an access token.
    fn download(&mut self, url: &str, target: &Path) -> Result<(), FlashError> {
        let failed = |reason: String| FlashError::DownloadFailed {
            url: redacted(url),
            reason,
        };
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).map_err(|source| FlashError::PathIo {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let status = child(CURL_CMD)
            .args(curl_args(url, target))
            .status()
            .map_err(|e| failed(format!("failed to run {CURL_CMD}: {e}")))?;
        if !status.success() {
            return Err(failed(format!("{CURL_CMD} failed ({status})")));
        }
        Ok(())
    }

    fn read_sha256_file(&mut self, path: &Path) -> Result<String, FlashError> {
        read_sha256_file(path)
    }

    fn sha256_of(&mut self, path: &Path) -> Result<[u8; SHA256_LEN], FlashError> {
        sha256_of(path)
    }

    fn image_size(&mut self, image: &Path) -> Result<u64, FlashError> {
        let invalid = |reason: String| FlashError::InvalidImage {
            path: image.to_path_buf(),
            reason,
        };
        let mut file = File::open(image).map_err(|e| invalid(e.to_string()))?;
        xz::uncompressed_size(&mut file).map_err(invalid)
    }

    fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError> {
        rawio::device_len(disk)
    }

    fn write_image(&mut self, image: &Path, disk: &Path, size: u64) -> Result<(), FlashError> {
        write_image(image, disk, size)
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

struct Downloads {
    image: PathBuf,
    sha256: PathBuf,
}

impl Downloads {
    fn in_download_dir() -> Self {
        let dir = Path::new(DOWNLOAD_DIR);
        Self {
            image: dir.join(IMAGE_FILE_NAME),
            sha256: dir.join(SHA256_FILE_NAME),
        }
    }
}

pub(crate) fn run_url(ctx: &UrlCtx<'_>) -> Result<(), FlashError> {
    url_with(ctx, &mut RealUrlOps::default())
}

fn url_with(ctx: &UrlCtx<'_>, ops: &mut dyn UrlOps) -> Result<(), FlashError> {
    let disk = ctx.layout.device.base.as_path();
    #[cfg(feature = "grub")]
    let boot_partition = layout_partition(ctx.layout, PartitionName::Boot)?;
    let downloads = Downloads::in_download_dir();

    log::info!(
        "flash mode 3: flashing {} with an image downloaded from {}",
        disk.display(),
        redacted(ctx.image_url)
    );

    #[cfg(not(feature = "rtc"))]
    log::warn!(
        "TLS certificates are not checked: without a real-time clock their validity cannot be \
         checked, so the sha256 file only detects a corrupted download, not a changed one"
    );

    ops.unmount(ctx.rootfs, disk)?;
    ops.bring_up_network()?;

    log::info!("downloading {}", redacted(ctx.sha256_url));
    ops.download(ctx.sha256_url, &downloads.sha256)?;
    let expected =
        expected_sha256(&ops.read_sha256_file(&downloads.sha256)?).map_err(|reason| {
            FlashError::InvalidImage {
                path: downloads.sha256.clone(),
                reason,
            }
        })?;
    log::info!("downloading {}", redacted(ctx.image_url));
    ops.download(ctx.image_url, &downloads.image)?;

    let actual = ops.sha256_of(&downloads.image)?;
    if actual != expected {
        return Err(FlashError::InvalidImage {
            path: downloads.image,
            reason: format!(
                "its sha256 is {}, the sha256 file expects {}",
                hex::encode(actual),
                hex::encode(expected)
            ),
        });
    }
    let size = ops.image_size(&downloads.image)?;
    if size == 0 {
        return Err(FlashError::InvalidImage {
            path: downloads.image,
            reason: "the image is empty".to_string(),
        });
    }
    let disk_len = ops.device_len(disk)?;
    if size > disk_len {
        return Err(FlashError::InvalidDestination {
            device: disk.to_path_buf(),
            reason: format!("the image needs {size} bytes, the device has {disk_len}"),
        });
    }

    log::info!(
        "flashing {} onto {}",
        downloads.image.display(),
        disk.display()
    );
    ops.write_image(&downloads.image, disk, size)
        .map_err(|e| partly_written(disk, e))?;

    // The new image may move partitions. Everything after this point mounts
    // them, so the kernel must drop the table from before the flash first.
    ops.reread_table(disk)
        .map_err(|source| FlashError::StalePartitionTable {
            disk: disk.to_path_buf(),
            source: Box::new(source),
        })?;

    #[cfg(feature = "grub")]
    efi::handle(ops.efi(), disk, boot_partition)?;

    log::info!("flash mode 3 finished");
    ops.sync();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{
        mode::flash::{keeps_log, xz::test_support::xz},
        partition::RootDevice,
    };

    const IMAGE_URL: &str = "https://user:pass@example.invalid/omnect.wic.xz?sv=2024&sig=secret";
    const SHA256_URL: &str = "https://example.invalid/omnect.wic.sha256?sv=2024&sig=secret";
    const IMAGE: &[u8] = b"a small disk image";
    const DISK_LEN: u64 = 1024;

    fn sha256_line(data: &[u8]) -> String {
        format!(
            "{}  /build/omnect.wic.xz\n",
            hex::encode(Sha256::digest(data))
        )
    }

    #[test]
    fn curl_fails_on_http_errors_and_stalls_and_writes_the_target() {
        let args: Vec<String> = curl_args(IMAGE_URL, Path::new("/tmp/wic.xz"))
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        for flag in CURL_FLAGS {
            assert!(args.contains(&flag.to_string()), "{flag} in {args:?}");
        }
        let after = |flag: &str| {
            let at = args.iter().position(|arg| arg == flag).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(after(CURL_CONNECT_TIMEOUT_FLAG), "30");
        assert_eq!(after(CURL_SPEED_LIMIT_FLAG), "1");
        assert_eq!(after(CURL_SPEED_TIME_FLAG), "60");
        assert_eq!(after(CURL_OUTPUT_FLAG), "/tmp/wic.xz");
        assert_eq!(after(CURL_URL_FLAG), IMAGE_URL);
        assert_eq!(args.last().unwrap(), IMAGE_URL);
    }

    #[test]
    fn certificates_are_checked_only_with_a_clock() {
        let args = curl_args(IMAGE_URL, Path::new("/tmp/wic.xz"));
        let insecure = args.iter().any(|arg| arg == "--insecure");
        assert_eq!(insecure, cfg!(not(feature = "rtc")));
    }

    #[test]
    fn the_digest_is_the_first_field_of_the_sha256_file() {
        let digest = Sha256::digest(IMAGE);
        assert_eq!(
            expected_sha256(&sha256_line(IMAGE)).unwrap()[..],
            digest[..]
        );
        let bare = hex::encode(digest);
        assert_eq!(
            expected_sha256(&format!("\n{bare}")).unwrap()[..],
            digest[..]
        );
    }

    #[test]
    fn a_sha256_file_without_a_digest_is_refused() {
        for bad in [
            "",
            "\n",
            "not-a-digest  file",
            "abc123  file",
            "<html>404</html>",
        ] {
            assert!(expected_sha256(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_sha256_file_is_read_up_to_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SHA256_FILE_NAME);
        let line = sha256_line(IMAGE);
        fs::write(&path, &line).unwrap();
        assert_eq!(read_sha256_file(&path).unwrap(), line);

        let limit = usize::try_from(MAX_SHA256_FILE_LEN).unwrap();
        fs::write(&path, vec![b'a'; limit]).unwrap();
        assert!(read_sha256_file(&path).is_ok());

        fs::write(&path, vec![b'a'; limit + 1]).unwrap();
        let err = read_sha256_file(&path).unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidImage { reason, .. } if reason.contains("larger than")),
            "{err}"
        );
        assert!(keeps_log(&Err(err)));

        fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            read_sha256_file(&path),
            Err(FlashError::InvalidImage { .. })
        ));
    }

    #[test]
    fn a_file_is_hashed_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        let data = vec![0x5a; 2 * COPY_BUFFER_SIZE + 1];
        fs::write(&path, &data).unwrap();
        assert_eq!(sha256_of(&path).unwrap()[..], Sha256::digest(&data)[..]);
    }

    #[test]
    fn the_whole_image_is_decoded_onto_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join(IMAGE_FILE_NAME);
        let disk = dir.path().join("sdz");
        fs::write(&image, xz(IMAGE)).unwrap();
        fs::write(&disk, vec![0xff; usize::try_from(DISK_LEN).unwrap()]).unwrap();

        write_image(&image, &disk, IMAGE.len() as u64).unwrap();

        let written = fs::read(&disk).unwrap();
        assert_eq!(written[..IMAGE.len()], *IMAGE);
        assert!(written[IMAGE.len()..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn a_disk_that_does_not_exist_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join(IMAGE_FILE_NAME);
        fs::write(&image, xz(IMAGE)).unwrap();
        let disk = dir.path().join("sdz");
        assert!(write_image(&image, &disk, IMAGE.len() as u64).is_err());
        assert!(!disk.exists());
    }

    #[test]
    fn a_stream_longer_than_its_index_stops_at_the_index_size() {
        let mut out = Cursor::new(Vec::new());
        let short = IMAGE.len() as u64 - 1;
        let err = decode_into(IMAGE, &mut out, short).unwrap_err();
        assert!(err.contains("more than"), "{err}");
        assert!(out.into_inner().len() < IMAGE.len());
    }

    #[test]
    fn a_stream_shorter_than_its_index_is_an_error() {
        let err =
            decode_into(IMAGE, &mut Cursor::new(Vec::new()), IMAGE.len() as u64 + 1).unwrap_err();
        assert!(err.contains("its index records"), "{err}");
    }

    /// Lets the first `passes` calls of `step` succeed, then fails every call.
    struct Fault {
        step: &'static str,
        passes: usize,
    }

    /// Records every mode 3 side effect as one line, in call order. A call
    /// matches a fault when its line starts with the fault's step.
    struct RecordingUrlOps {
        calls: Vec<String>,
        faults: Vec<Fault>,
        sha256_file: String,
        image: Vec<u8>,
        disk_len: u64,
    }

    impl RecordingUrlOps {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                faults: Vec::new(),
                sha256_file: sha256_line(IMAGE),
                image: IMAGE.to_vec(),
                disk_len: DISK_LEN,
            }
        }

        fn record(&mut self, call: String) -> Result<(), FlashError> {
            let mut fail = false;
            for fault in &mut self.faults {
                if call.starts_with(fault.step) {
                    if fault.passes > 0 {
                        fault.passes -= 1;
                    } else {
                        fail = true;
                    }
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
    impl efi::EfiOps for RecordingUrlOps {
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

    impl UrlOps for RecordingUrlOps {
        fn unmount(&mut self, rootfs: &Path, disk: &Path) -> Result<(), FlashError> {
            self.record(format!(
                "unmount {} and {}",
                rootfs.display(),
                disk.display()
            ))
        }

        fn bring_up_network(&mut self) -> Result<(), FlashError> {
            self.record("network".to_string())
        }

        fn download(&mut self, url: &str, target: &Path) -> Result<(), FlashError> {
            self.record(format!(
                "download {} to {}",
                redacted(url),
                target.display()
            ))
        }

        fn read_sha256_file(&mut self, path: &Path) -> Result<String, FlashError> {
            self.record(format!("read {}", path.display()))?;
            Ok(self.sha256_file.clone())
        }

        fn sha256_of(&mut self, path: &Path) -> Result<[u8; SHA256_LEN], FlashError> {
            self.record(format!("hash {}", path.display()))?;
            Ok(Sha256::digest(&self.image).into())
        }

        fn image_size(&mut self, image: &Path) -> Result<u64, FlashError> {
            self.record(format!("size of {}", image.display()))?;
            Ok(self.image.len() as u64)
        }

        fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError> {
            self.record(format!("size of {}", disk.display()))?;
            Ok(self.disk_len)
        }

        fn write_image(&mut self, image: &Path, disk: &Path, size: u64) -> Result<(), FlashError> {
            self.record(format!(
                "write {} to {} ({size} bytes)",
                image.display(),
                disk.display()
            ))
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

    fn run_recorded(mut ops: RecordingUrlOps) -> (Result<(), FlashError>, Vec<String>) {
        let layout = PartitionLayout::new(RootDevice {
            base: PathBuf::from("/dev/sda"),
            partition_sep: "",
            root_partition: PathBuf::from("/dev/sda2"),
        })
        .unwrap();
        let ctx = UrlCtx {
            layout: &layout,
            rootfs: Path::new("/rootfs"),
            image_url: IMAGE_URL,
            sha256_url: SHA256_URL,
        };
        let result = url_with(&ctx, &mut ops);
        (result, ops.calls)
    }

    fn calls_up_to_the_flash() -> Vec<String> {
        vec![
            "unmount /rootfs and /dev/sda".to_string(),
            "network".to_string(),
            "download https://example.invalid/omnect.wic.sha256 to /tmp/flash-mode-3/wic.xz.sha256"
                .to_string(),
            "read /tmp/flash-mode-3/wic.xz.sha256".to_string(),
            "download https://example.invalid/omnect.wic.xz to /tmp/flash-mode-3/wic.xz"
                .to_string(),
            "hash /tmp/flash-mode-3/wic.xz".to_string(),
            "size of /tmp/flash-mode-3/wic.xz".to_string(),
            "size of /dev/sda".to_string(),
            format!(
                "write /tmp/flash-mode-3/wic.xz to /dev/sda ({} bytes)",
                IMAGE.len()
            ),
        ]
    }

    #[test]
    fn mode_3_downloads_checks_and_flashes_in_order() {
        let (result, calls) = run_recorded(RecordingUrlOps::new());
        result.unwrap();
        let mut expected = calls_up_to_the_flash();
        expected.push("reread /dev/sda".to_string());
        assert_eq!(calls[..expected.len()], expected[..], "{calls:?}");
        assert_eq!(calls.last().unwrap(), "sync");
        #[cfg(not(feature = "grub"))]
        assert_eq!(calls.len(), expected.len() + 1, "{calls:?}");
        #[cfg(feature = "grub")]
        assert!(
            calls[expected.len()].starts_with("mount efivarfs"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_wrong_checksum_stops_before_the_disk_is_touched() {
        let mut ops = RecordingUrlOps::new();
        ops.sha256_file = sha256_line(b"another image");
        let (result, calls) = run_recorded(ops);
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidImage { reason, .. } if reason.contains("sha256 file expects")),
            "{err}"
        );
        assert!(keeps_log(&Err(err)));
        assert!(calls.last().unwrap().starts_with("hash "), "{calls:?}");
    }

    #[test]
    fn an_image_larger_than_the_disk_is_refused_before_the_write() {
        let mut ops = RecordingUrlOps::new();
        ops.disk_len = IMAGE.len() as u64 - 1;
        let (result, calls) = run_recorded(ops);
        assert!(
            matches!(result, Err(FlashError::InvalidDestination { .. })),
            "{result:?}"
        );
        assert_eq!(calls.last().unwrap(), "size of /dev/sda");
    }

    #[test]
    fn an_empty_image_is_refused_before_the_write() {
        let mut ops = RecordingUrlOps::new();
        ops.image = Vec::new();
        ops.sha256_file = sha256_line(b"");
        let (result, calls) = run_recorded(ops);
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FlashError::InvalidImage { reason, .. } if reason.contains("empty")),
            "{err}"
        );
        assert!(keeps_log(&Err(err)));
        assert_eq!(
            calls.last().unwrap(),
            "size of /tmp/flash-mode-3/wic.xz",
            "{calls:?}"
        );
    }

    #[test]
    fn a_failed_download_is_reported_without_writing() {
        for passes in [0, 1] {
            let mut ops = RecordingUrlOps::new();
            ops.faults.push(Fault {
                step: "download",
                passes,
            });
            let (result, calls) = run_recorded(ops);
            assert!(result.is_err());
            assert!(calls.iter().all(|c| !c.starts_with("write ")), "{calls:?}");
        }
    }

    #[test]
    fn an_unreadable_image_size_stops_before_the_write() {
        let mut ops = RecordingUrlOps::new();
        ops.faults.push(Fault {
            step: "size of",
            passes: 0,
        });
        let (result, calls) = run_recorded(ops);
        let err = result.unwrap_err();
        assert!(keeps_log(&Err(err)));
        assert_eq!(
            calls.last().unwrap(),
            "size of /tmp/flash-mode-3/wic.xz",
            "{calls:?}"
        );
    }

    #[test]
    fn a_failed_unmount_or_network_stops_before_any_download() {
        for step in ["unmount", "network"] {
            let mut ops = RecordingUrlOps::new();
            ops.faults.push(Fault { step, passes: 0 });
            let (result, calls) = run_recorded(ops);
            assert!(result.is_err(), "{step}");
            assert!(calls.last().unwrap().starts_with(step), "{calls:?}");
        }
    }

    #[test]
    fn a_failed_write_marks_the_disk_as_partly_written() {
        let mut ops = RecordingUrlOps::new();
        ops.faults.push(Fault {
            step: "write",
            passes: 0,
        });
        let (result, _) = run_recorded(ops);
        assert!(
            matches!(result, Err(FlashError::DiskPartlyWritten { .. })),
            "{result:?}"
        );
        assert!(!keeps_log(&result));
    }

    #[test]
    fn a_failed_table_reread_is_a_stale_table() {
        let mut ops = RecordingUrlOps::new();
        ops.faults.push(Fault {
            step: "reread",
            passes: 0,
        });
        let (result, _) = run_recorded(ops);
        assert!(
            matches!(result, Err(FlashError::StalePartitionTable { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn no_log_line_carries_the_query_string() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();
        crate::logging::start_capture();
        let mut ops = RecordingUrlOps::new();
        ops.faults.push(Fault {
            step: "download",
            passes: 1,
        });
        let (result, _) = run_recorded(ops);
        let lines = crate::logging::take_capture();
        assert!(
            lines.iter().any(|line| line.contains("example.invalid")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("sig=") && !line.contains("pass")),
            "{lines:?}"
        );
        assert!(!result.unwrap_err().to_string().contains("sig="));
    }
}
