//! Flash mode 3: download a `wic.xz` and its sha256 file, check the image and
//! flash it onto the running disk.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use lzma_rust2::xz_uncompressed_size;
use sha2::{Digest, Sha256};

use crate::{
    bootloader::sync_filesystems,
    error::FlashError,
    logging::output_tail,
    mode::flash::{
        SHA256_LEN, child,
        config::Url,
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
const CURL_MAX_FILESIZE_FLAG: &str = "--max-filesize";
const CURL_PROTO_REDIR_FLAG: &str = "--proto-redir";
const CURL_HTTPS_ONLY: &str = "=https";
const CURL_WRITE_OUT_FLAG: &str = "--write-out";
const CURL_EFFECTIVE_URL: &str = "%{url_effective}";
const CURL_OUTPUT_FLAG: &str = "--output";
/// Marks the next argument as the URL, also when it starts with `-`.
const CURL_URL_FLAG: &str = "--url";
#[cfg(not(feature = "rtc"))]
const CURL_INSECURE_FLAG: &str = "--insecure";
const CURL_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A transfer that stays below `CURL_STALL_SPEED` bytes per second for
/// `CURL_STALL_TIME` is aborted, so a stalled server cannot block PID 1.
const CURL_STALL_SPEED: u32 = 1;
const CURL_STALL_TIME: Duration = Duration::from_secs(60);
const CURL_REASON_LINES: usize = 3;
const HTTPS_SCHEME: &str = "https://";

const DOWNLOAD_DIR: &str = "/tmp/flash-mode-3";
const IMAGE_FILE_NAME: &str = "wic.xz";
const SHA256_FILE_NAME: &str = "wic.xz.sha256";
/// Room for a digest and a long file name.
const MAX_SHA256_FILE_LEN: u64 = 4096;
const MEMINFO_PATH: &str = "/proc/meminfo";
const MEM_AVAILABLE_KEY: &str = "MemAvailable:";
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// The download sits in initramfs RAM. What stays free holds the xz decoder,
/// whose dictionary can take 64 MiB, and the rest of the initramfs.
const RAM_RESERVE: u64 = 128 * MIB;

pub(crate) struct UrlCtx<'a> {
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
    pub(crate) image_url: &'a Url,
    pub(crate) sha256_url: &'a Url,
}

/// `max_len` must not be 0: curl reads 0 as no limit.
fn curl_args(url: &Url, target: &Path, max_len: u64) -> Vec<OsString> {
    let mut args: Vec<OsString> = CURL_FLAGS.iter().map(OsString::from).collect();
    #[cfg(not(feature = "rtc"))]
    args.push(CURL_INSECURE_FLAG.into());
    // A redirect to plain HTTP would drop the TLS check.
    let scheme = url.unredacted().get(..HTTPS_SCHEME.len());
    if scheme.is_some_and(|scheme| scheme.eq_ignore_ascii_case(HTTPS_SCHEME)) {
        args.extend([CURL_PROTO_REDIR_FLAG.into(), CURL_HTTPS_ONLY.into()]);
    }
    args.extend([
        CURL_CONNECT_TIMEOUT_FLAG.into(),
        CURL_CONNECT_TIMEOUT.as_secs().to_string().into(),
        CURL_SPEED_LIMIT_FLAG.into(),
        CURL_STALL_SPEED.to_string().into(),
        CURL_SPEED_TIME_FLAG.into(),
        CURL_STALL_TIME.as_secs().to_string().into(),
        CURL_MAX_FILESIZE_FLAG.into(),
        max_len.to_string().into(),
        CURL_WRITE_OUT_FLAG.into(),
        CURL_EFFECTIVE_URL.into(),
        CURL_OUTPUT_FLAG.into(),
        target.into(),
        CURL_URL_FLAG.into(),
        url.unredacted().into(),
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

/// The `MemAvailable` line of `/proc/meminfo`, in bytes.
fn mem_available(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let kib = line
            .strip_prefix(MEM_AVAILABLE_KEY)?
            .trim()
            .strip_suffix("kB")?;
        kib.trim().parse::<u64>().ok()?.checked_mul(KIB)
    })
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

/// Every byte is written, also the zero blocks, so nothing of the old image
/// survives inside the new one. Only an error after both files are open is
/// `DiskPartlyWritten`.
fn write_image(image: &Path, disk: &Path, size: u64) -> Result<(), FlashError> {
    let failed = |reason: String| FlashError::CopyFailed {
        src: image.to_path_buf(),
        dst: disk.to_path_buf(),
        reason,
    };
    let input = File::open(image).map_err(|e| failed(format!("opening source: {e}")))?;
    let mut output =
        open_existing_for_write(disk).map_err(|e| failed(format!("opening destination: {e}")))?;
    decode_into(xz::decoder(input), &mut output, size)
        .and_then(|()| {
            output
                .sync_all()
                .map_err(|e| format!("syncing destination: {e}"))
        })
        .map_err(|reason| partly_written(disk, failed(reason)))
}

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
    fn bring_up_network(&mut self, need: net::Ipv4Need) -> Result<(), FlashError>;
    fn download(&mut self, url: &Url, target: &Path, max_len: u64) -> Result<(), FlashError>;
    fn available_ram(&mut self) -> Result<u64, FlashError>;
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

    fn bring_up_network(&mut self, need: net::Ipv4Need) -> Result<(), FlashError> {
        net::bring_up(need).map(|_| ())
    }

    fn download(&mut self, url: &Url, target: &Path, max_len: u64) -> Result<(), FlashError> {
        let failed = |reason: String| FlashError::DownloadFailed {
            url: url.clone(),
            reason,
        };
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir).map_err(|source| FlashError::PathIo {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let output = child(CURL_CMD)
            .args(curl_args(url, target, max_len))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| failed(format!("failed to run {CURL_CMD}: {e}")))?;
        if !output.status.success() {
            return Err(failed(format!(
                "{CURL_CMD} failed ({}): {}",
                output.status,
                output_tail(&output.stderr, CURL_REASON_LINES)
            )));
        }
        if let Some(effective) = Url::new(&String::from_utf8_lossy(&output.stdout)) {
            log::info!("downloaded {effective}");
        }
        Ok(())
    }

    fn available_ram(&mut self) -> Result<u64, FlashError> {
        let meminfo = fs::read_to_string(MEMINFO_PATH).map_err(|source| FlashError::PathIo {
            path: MEMINFO_PATH.into(),
            source,
        })?;
        mem_available(&meminfo).ok_or_else(|| FlashError::PathIo {
            path: MEMINFO_PATH.into(),
            source: std::io::Error::other(format!("no {MEM_AVAILABLE_KEY} line")),
        })
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
        let file = File::open(image).map_err(|e| invalid(format!("opening: {e}")))?;
        xz_uncompressed_size(file).map_err(|e| invalid(format!("not a valid xz file: {e}")))
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
        ctx.image_url
    );

    #[cfg(not(feature = "rtc"))]
    log::warn!(
        "TLS certificates are not checked without a real-time clock; the sha256 file only \
         detects a corrupted download"
    );

    ops.unmount(ctx.rootfs, disk)?;
    ops.bring_up_network(net::Ipv4Need::Lease)?;

    log::info!("downloading {}", ctx.sha256_url);
    ops.download(ctx.sha256_url, &downloads.sha256, MAX_SHA256_FILE_LEN)?;
    let expected =
        expected_sha256(&ops.read_sha256_file(&downloads.sha256)?).map_err(|reason| {
            FlashError::InvalidImage {
                path: downloads.sha256.clone(),
                reason,
            }
        })?;
    let available = ops.available_ram()?;
    let max_image_len = available
        .checked_sub(RAM_RESERVE)
        .filter(|&len| len > 0)
        .ok_or_else(|| FlashError::DownloadFailed {
            url: ctx.image_url.clone(),
            reason: format!("{available} bytes of RAM are free, {RAM_RESERVE} must stay free"),
        })?;
    log::info!(
        "downloading {} (at most {max_image_len} bytes)",
        ctx.image_url
    );
    ops.download(ctx.image_url, &downloads.image, max_image_len)?;

    let actual = ops.sha256_of(&downloads.image)?;
    if actual != expected {
        return Err(FlashError::InvalidImage {
            path: downloads.image,
            reason: format!(
                "its sha256 is {}, the first digest in the sha256 file is {}",
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
    ops.write_image(&downloads.image, disk, size)?;
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
        mode::flash::{
            keeps_log,
            xz::test_support::{xz, xz_with},
        },
        partition::RootDevice,
    };

    const IMAGE_URL: &str = "https://user:pass@example.invalid/omnect.wic.xz?sv=2024&sig=secret";
    const SHA256_URL: &str = "https://example.invalid/omnect.wic.sha256?sv=2024&sig=secret";
    const IMAGE: &[u8] = b"a small disk image";
    const DISK_LEN: u64 = 1024;
    const IMAGE_RAM: u64 = 64 * MIB;

    fn url(text: &str) -> Url {
        Url::new(text).unwrap()
    }

    fn sha256_line(data: &[u8]) -> String {
        format!(
            "{}  /build/omnect.wic.xz\n",
            hex::encode(Sha256::digest(data))
        )
    }

    fn curl_args_of(url_text: &str) -> Vec<String> {
        curl_args(
            &url(url_text),
            Path::new("/tmp/wic.xz"),
            MAX_SHA256_FILE_LEN,
        )
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
    }

    fn value_after(args: &[String], flag: &str) -> String {
        let at = args.iter().position(|arg| arg == flag).unwrap();
        args[at + 1].clone()
    }

    #[test]
    fn the_curl_command_line_has_every_flag() {
        let args = curl_args_of(IMAGE_URL);
        for flag in ["--fail", "--no-progress-meter", "--location", "--globoff"] {
            assert!(args.contains(&flag.to_string()), "{flag} in {args:?}");
        }
        assert_eq!(value_after(&args, "--connect-timeout"), "30");
        assert_eq!(value_after(&args, "--speed-limit"), "1");
        assert_eq!(value_after(&args, "--speed-time"), "60");
        assert_eq!(value_after(&args, "--max-filesize"), "4096");
        assert_eq!(value_after(&args, "--write-out"), "%{url_effective}");
        assert_eq!(value_after(&args, "--output"), "/tmp/wic.xz");
        assert_eq!(value_after(&args, "--url"), IMAGE_URL);
        assert_eq!(args.last().unwrap(), IMAGE_URL);
    }

    #[test]
    fn an_https_download_follows_only_https_redirects() {
        for https in [IMAGE_URL, "HTTPS://example.invalid/a"] {
            assert_eq!(value_after(&curl_args_of(https), "--proto-redir"), "=https");
        }
        let args = curl_args_of("http://example.invalid/a");
        assert!(!args.contains(&"--proto-redir".to_string()), "{args:?}");
    }

    #[test]
    fn certificates_are_checked_only_with_a_clock() {
        let insecure = curl_args_of(IMAGE_URL).contains(&"--insecure".to_string());
        assert_eq!(insecure, cfg!(not(feature = "rtc")));
    }

    #[test]
    fn the_free_ram_is_read_from_meminfo() {
        let meminfo = "MemTotal:        3884000 kB\nMemFree:  1000 kB\nMemAvailable:    2048 kB\n";
        assert_eq!(mem_available(meminfo), Some(2048 * KIB));
        for bad in [
            "",
            "MemFree: 1000 kB\n",
            "MemAvailable: many kB\n",
            "MemAvailable: 1\n",
        ] {
            assert_eq!(mem_available(bad), None, "{bad:?}");
        }
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
    fn concatenated_streams_are_written_in_full() {
        let first: &[u8] = b"first stream";
        let second: &[u8] = b"second stream";
        let padding = [0u8; 4];
        let file = [&xz(first)[..], &padding, &xz(second), &padding].concat();
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join(IMAGE_FILE_NAME);
        let disk = dir.path().join("sdz");
        fs::write(&image, &file).unwrap();
        fs::write(&disk, vec![0xff; usize::try_from(DISK_LEN).unwrap()]).unwrap();

        // The size read and the decoder must agree, or the write fails late.
        let size = RealUrlOps::default().image_size(&image).unwrap();
        write_image(&image, &disk, size).unwrap();

        let expected = [first, second].concat();
        let written = fs::read(&disk).unwrap();
        assert_eq!(written[..expected.len()], expected[..]);
        assert!(written[expected.len()..].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn a_disk_that_does_not_exist_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join(IMAGE_FILE_NAME);
        fs::write(&image, xz(IMAGE)).unwrap();
        let disk = dir.path().join("sdz");
        let result = write_image(&image, &disk, IMAGE.len() as u64);
        assert!(
            matches!(result, Err(FlashError::CopyFailed { .. })),
            "{result:?}"
        );
        assert!(keeps_log(&result), "nothing was written");
        assert!(!disk.exists());
    }

    #[test]
    fn a_cut_or_broken_stream_leaves_the_disk_partly_written() {
        let mut cut = xz(IMAGE);
        cut.truncate(cut.len() / 2);
        for (case, stream) in [("cut", cut), ("not xz", IMAGE.to_vec())] {
            let dir = tempfile::tempdir().unwrap();
            let image = dir.path().join(IMAGE_FILE_NAME);
            let disk = dir.path().join("sdz");
            fs::write(&image, stream).unwrap();
            fs::write(&disk, vec![0xff; usize::try_from(DISK_LEN).unwrap()]).unwrap();
            let result = write_image(&image, &disk, IMAGE.len() as u64);
            assert!(
                matches!(result, Err(FlashError::DiskPartlyWritten { .. })),
                "{case}: {result:?}"
            );
        }
    }

    #[test]
    fn a_stream_longer_than_its_index_stops_at_the_index_size() {
        let mut out = Cursor::new(Vec::new());
        let short = IMAGE.len() as u64 - 1;
        let err = decode_into(IMAGE, &mut out, short).unwrap_err();
        assert!(err.contains("more than"), "{err}");
        assert!(out.into_inner().len() as u64 <= short);
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
        available_ram: u64,
    }

    impl RecordingUrlOps {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                faults: Vec::new(),
                sha256_file: sha256_line(IMAGE),
                image: IMAGE.to_vec(),
                disk_len: DISK_LEN,
                available_ram: RAM_RESERVE + IMAGE_RAM,
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

        fn bring_up_network(&mut self, need: net::Ipv4Need) -> Result<(), FlashError> {
            self.record(format!("network, waiting for {}", need.describe()))
        }

        fn download(&mut self, url: &Url, target: &Path, max_len: u64) -> Result<(), FlashError> {
            self.record(format!(
                "download {url} to {} (at most {max_len} bytes)",
                target.display()
            ))
        }

        fn available_ram(&mut self) -> Result<u64, FlashError> {
            self.record("free ram".to_string())?;
            Ok(self.available_ram)
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
            self.record(format!("image size of {}", image.display()))?;
            Ok(self.image.len() as u64)
        }

        fn device_len(&mut self, disk: &Path) -> Result<u64, FlashError> {
            self.record(format!("device size of {}", disk.display()))?;
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
        let (image_url, sha256_url) = (url(IMAGE_URL), url(SHA256_URL));
        let ctx = UrlCtx {
            layout: &layout,
            rootfs: Path::new("/rootfs"),
            image_url: &image_url,
            sha256_url: &sha256_url,
        };
        let result = url_with(&ctx, &mut ops);
        (result, ops.calls)
    }

    fn calls_up_to_the_flash() -> Vec<String> {
        vec![
            "unmount /rootfs and /dev/sda".to_string(),
            "network, waiting for a DHCP lease".to_string(),
            "download https://example.invalid/omnect.wic.sha256 to /tmp/flash-mode-3/wic.xz.sha256 \
             (at most 4096 bytes)"
                .to_string(),
            "read /tmp/flash-mode-3/wic.xz.sha256".to_string(),
            "free ram".to_string(),
            format!(
                "download https://example.invalid/omnect.wic.xz to /tmp/flash-mode-3/wic.xz \
                 (at most {IMAGE_RAM} bytes)"
            ),
            "hash /tmp/flash-mode-3/wic.xz".to_string(),
            "image size of /tmp/flash-mode-3/wic.xz".to_string(),
            "device size of /dev/sda".to_string(),
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
            matches!(&err, FlashError::InvalidImage { reason, .. } if reason.contains("first digest in the sha256 file")),
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
        assert_eq!(calls.last().unwrap(), "device size of /dev/sda");
    }

    #[test]
    fn an_image_that_fills_the_disk_exactly_is_flashed() {
        let mut ops = RecordingUrlOps::new();
        ops.disk_len = IMAGE.len() as u64;
        let (result, _) = run_recorded(ops);
        result.unwrap();
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
            "image size of /tmp/flash-mode-3/wic.xz",
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
            step: "image size",
            passes: 0,
        });
        let (result, calls) = run_recorded(ops);
        let err = result.unwrap_err();
        assert!(keeps_log(&Err(err)));
        assert_eq!(
            calls.last().unwrap(),
            "image size of /tmp/flash-mode-3/wic.xz",
            "{calls:?}"
        );
    }

    #[test]
    fn a_failed_setup_or_finish_step_stops_mode_3_there() {
        type IsExpected = fn(&FlashError) -> bool;
        let any: IsExpected = |_| true;
        let steps: &[(&str, bool, IsExpected)] = &[
            ("unmount", true, any),
            ("network", true, any),
            ("reread", false, |e| {
                matches!(e, FlashError::StalePartitionTable { .. })
            }),
            #[cfg(feature = "grub")]
            ("mount efivarfs", true, any),
            #[cfg(feature = "grub")]
            ("efibootmgr", true, any),
            #[cfg(feature = "grub")]
            ("write entry dump", true, any),
        ];
        for &(step, log_is_kept, is_expected) in steps {
            let mut ops = RecordingUrlOps::new();
            ops.faults.push(Fault { step, passes: 0 });
            let (result, calls) = run_recorded(ops);
            assert!(result.is_err(), "{step}");
            assert!(
                calls.last().unwrap().starts_with(step),
                "{step} must be the last step, got {calls:?}"
            );
            assert_eq!(keeps_log(&result), log_is_kept, "{step}: {result:?}");
            assert!(
                is_expected(result.as_ref().unwrap_err()),
                "{step}: {result:?}"
            );
        }
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
        let _ = run_recorded(ops);
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
    }

    #[test]
    fn a_failed_download_names_the_url_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let missing = url("file:///nonexistent/omnect.wic.xz?sig=secret");
        let err = RealUrlOps::default()
            .download(
                &missing,
                &dir.path().join(IMAGE_FILE_NAME),
                MAX_SHA256_FILE_LEN,
            )
            .unwrap_err();
        let printed = err.to_string();
        assert!(
            matches!(err, FlashError::DownloadFailed { .. }),
            "{printed}"
        );
        assert!(
            printed.contains("file:///nonexistent/omnect.wic.xz"),
            "{printed}"
        );
        assert!(!printed.contains("sig="), "{printed}");
        // curl's own message, so the operator need not map its exit code.
        assert!(printed.contains("curl: (37)"), "{printed}");
    }

    fn real_image_size(file: &[u8]) -> Result<u64, FlashError> {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join(IMAGE_FILE_NAME);
        fs::write(&image, file).unwrap();
        RealUrlOps::default().image_size(&image)
    }

    #[test]
    fn the_image_size_is_read_from_the_xz_index() {
        let data: Vec<u8> = (0..3 * 4096 + 17).map(|i| (i % 251) as u8).collect();
        let len = data.len() as u64;
        let several_blocks = xz_with(&data, std::num::NonZeroU64::new(1000));
        assert_eq!(real_image_size(&xz(&data)).unwrap(), len);
        assert_eq!(real_image_size(&several_blocks).unwrap(), len);
        // The decoder accepts concatenated streams, so their sizes add up.
        let two_streams = [xz(&data), xz(&data)].concat();
        assert_eq!(real_image_size(&two_streams).unwrap(), 2 * len);
    }

    #[test]
    fn a_file_that_is_not_xz_has_no_image_size() {
        let mut cut = xz(IMAGE);
        cut.pop();
        let trailing = [xz(IMAGE), b"garbage!".to_vec()].concat();
        for (file, upstream_reason) in [
            (Vec::new(), "too small"),
            (IMAGE.to_vec(), "too small"),
            (cut, "footer"),
            (trailing, "footer"),
            (vec![0x5a; 64], "footer"),
        ] {
            let err = real_image_size(&file).unwrap_err();
            assert!(
                matches!(&err, FlashError::InvalidImage { reason, .. }
                    if reason.starts_with("not a valid xz file") && reason.contains(upstream_reason)),
                "{err}"
            );
        }
    }

    #[test]
    fn a_download_logs_where_it_came_from() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, sha256_line(IMAGE)).unwrap();
        let target = dir.path().join(SHA256_FILE_NAME);

        crate::logging::start_capture();
        let result = RealUrlOps::default().download(
            &url(&format!("file://{}?sig=secret", source.display())),
            &target,
            MAX_SHA256_FILE_LEN,
        );
        let lines = crate::logging::take_capture();

        result.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), sha256_line(IMAGE));
        let expected = format!("downloaded file://{}", source.display());
        assert!(
            lines.iter().any(|line| line.ends_with(&expected)),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| !line.contains("sig=")), "{lines:?}");
    }

    #[test]
    fn too_little_free_ram_stops_before_the_image_download() {
        for available_ram in [0, RAM_RESERVE] {
            let mut ops = RecordingUrlOps::new();
            ops.available_ram = available_ram;
            let (result, calls) = run_recorded(ops);
            let err = result.unwrap_err();
            assert!(
                matches!(&err, FlashError::DownloadFailed { reason, .. } if reason.contains("must stay free")),
                "{err}"
            );
            assert!(keeps_log(&Err(err)));
            assert_eq!(calls.last().unwrap(), "free ram", "{calls:?}");
        }
    }
}
