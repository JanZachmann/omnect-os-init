//! Raw byte-range writes to block devices and files.

use std::fs::{File, OpenOptions};
#[cfg(feature = "flash-mode-1")]
use std::io::{ErrorKind, Read};
use std::io::{Seek, SeekFrom, Write};
#[cfg(feature = "flash-mode-2")]
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::config::BuildConstant;
use crate::error::FlashError;

/// Bytes per KiB. Build-time offsets and sizes are KB-valued; block devices
/// are addressed in bytes.
pub const KIB: u64 = 1024;

/// Copy buffer: 1 MiB is enough to keep a block device streaming.
pub const COPY_BUFFER_SIZE: usize = 1024 * 1024;

// BLKRRPART from <linux/fs.h>.
#[cfg(feature = "flash-mode-2")]
const BLK_IOC_MAGIC: u8 = 0x12;
#[cfg(feature = "flash-mode-2")]
const BLKRRPART_NR: u8 = 95;

#[cfg(feature = "flash-mode-2")]
nix::ioctl_none!(blkrrpart, BLK_IOC_MAGIC, BLKRRPART_NR);

#[derive(Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub len: u64,
}

pub(crate) fn chunk_len(left: u64, buf_len: usize) -> usize {
    usize::try_from(left).map_or(buf_len, |left| left.min(buf_len))
}

pub fn kb_to_bytes(kb: u64, name: BuildConstant) -> Result<u64, FlashError> {
    kb.checked_mul(KIB)
        .ok_or_else(|| FlashError::InvalidBuildConstant {
            name,
            reason: format!("{kb} KB does not fit a byte offset"),
        })
}

/// Copy `len` bytes (or the rest of the source when `len` is `None`) from
/// `src_offset` in `src` to `dst_offset` in `dst`.
///
/// A source that ends early is an error: a partition copy that silently wrote
/// less than asked would produce a clone that boots and then fails.
#[cfg(feature = "flash-mode-1")]
pub fn copy_range(
    src: &Path,
    src_offset: u64,
    dst: &Path,
    dst_offset: u64,
    len: Option<u64>,
) -> Result<u64, FlashError> {
    let copy_failed = |reason: String| FlashError::CopyFailed {
        src: src.to_path_buf(),
        dst: dst.to_path_buf(),
        reason,
    };

    let mut src_file =
        std::fs::File::open(src).map_err(|e| copy_failed(format!("opening source: {e}")))?;
    src_file
        .seek(SeekFrom::Start(src_offset))
        .map_err(|e| copy_failed(format!("seeking source: {e}")))?;

    let mut dst_file = open_existing_for_write(dst)
        .map_err(|e| copy_failed(format!("opening destination: {e}")))?;
    dst_file
        .seek(SeekFrom::Start(dst_offset))
        .map_err(|e| copy_failed(format!("seeking destination: {e}")))?;

    let mut buf = vec![0u8; COPY_BUFFER_SIZE];
    let mut copied: u64 = 0;

    loop {
        if let Some(len) = len
            && copied >= len
        {
            break;
        }

        let want = match len {
            Some(len) => chunk_len(len - copied, buf.len()),
            None => buf.len(),
        };

        let n = loop {
            match src_file.read(&mut buf[..want]) {
                Ok(n) => break n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(copy_failed(format!("reading source: {e}"))),
            }
        };

        if n == 0 {
            if let Some(len) = len {
                return Err(copy_failed(format!(
                    "source ended after {copied} bytes, {len} requested"
                )));
            }
            break;
        }

        let mut written = 0;
        while written < n {
            match dst_file.write(&buf[written..n]) {
                Ok(0) => {
                    return Err(copy_failed(format!(
                        "destination stopped accepting data after {written} of {n} bytes in this chunk"
                    )));
                }
                Ok(w) => written += w,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(copy_failed(format!("writing destination: {e}"))),
            }
        }

        copied += n as u64;
    }

    // Write-back errors on the destination are reported here or not at all.
    dst_file
        .sync_all()
        .map_err(|e| copy_failed(format!("syncing destination: {e}")))?;

    Ok(copied)
}

/// The destination must already exist: a mistyped device path has to fail
/// instead of creating a regular file that makes the write look done.
pub(crate) fn open_existing_for_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).open(path)
}

#[cfg(feature = "flash-mode-2")]
pub fn zero_range(dst: &Path, range: &ByteRange) -> Result<(), FlashError> {
    let io_failed = |source| FlashError::PathIo {
        path: dst.to_path_buf(),
        source,
    };

    let mut dst_file = open_existing_for_write(dst).map_err(io_failed)?;
    dst_file
        .seek(SeekFrom::Start(range.offset))
        .map_err(io_failed)?;

    let buf = vec![0u8; COPY_BUFFER_SIZE];
    let mut left = range.len;
    while left > 0 {
        let chunk = chunk_len(left, buf.len());
        dst_file.write_all(&buf[..chunk]).map_err(io_failed)?;
        left -= chunk as u64;
    }

    dst_file.sync_all().map_err(io_failed)
}

/// Make the kernel re-read the partition table of `disk`. Fails while any
/// partition of `disk` is mounted.
#[cfg(feature = "flash-mode-2")]
pub fn reread_partition_table(disk: &Path) -> Result<(), FlashError> {
    let io_failed = |source| FlashError::PathIo {
        path: disk.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(disk).map_err(io_failed)?;
    // SAFETY: BLKRRPART takes no argument; the fd is open for the call.
    unsafe { blkrrpart(file.as_raw_fd()) }.map_err(|errno| io_failed(errno.into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "flash-mode-1")]
    use std::io::Read;
    use std::io::Write;

    fn file_with(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn a_chunk_is_at_most_one_buffer() {
        assert_eq!(chunk_len(3, 8), 3);
        assert_eq!(chunk_len(8, 8), 8);
        assert_eq!(chunk_len(u64::MAX, 8), 8);
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn copy_range_copies_a_bounded_window_at_both_offsets() {
        let src = file_with(b"0123456789");
        let dst = file_with(b"xxxxxxxxxx");

        let n = copy_range(src.path(), 2, dst.path(), 4, Some(3)).unwrap();
        assert_eq!(n, 3);

        let mut out = Vec::new();
        std::fs::File::open(dst.path())
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(&out, b"xxxx234xxx");
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn copy_range_without_a_length_copies_to_the_end_of_the_source() {
        let src = file_with(b"abcdef");
        let dst = file_with(b"..........");
        let n = copy_range(src.path(), 3, dst.path(), 0, None).unwrap();
        assert_eq!(n, 3);

        let mut out = Vec::new();
        std::fs::File::open(dst.path())
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(&out, b"def.......");
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn copy_range_reports_a_short_source_instead_of_writing_less_in_silence() {
        let src = file_with(b"ab");
        let dst = file_with(b"....");
        let err = copy_range(src.path(), 0, dst.path(), 0, Some(4)).unwrap_err();
        assert!(
            matches!(err, FlashError::CopyFailed { .. }),
            "a truncated copy must fail loudly: {err}"
        );
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn copy_range_copies_across_several_buffer_chunks() {
        // Longer than `len`, so a chunk that overshoots shows up past the window.
        const TAIL: usize = 10;
        let len = 2 * COPY_BUFFER_SIZE + 1;
        let bytes: Vec<u8> = (0..len + TAIL).map(|i| (i % 251) as u8).collect();
        let src = file_with(&bytes);
        let dst = file_with(&vec![0xff; len + 2 * TAIL]);

        let n = copy_range(src.path(), 0, dst.path(), 1, Some(len as u64)).unwrap();
        assert_eq!(n, len as u64);

        let out = std::fs::read(dst.path()).unwrap();
        assert_eq!(out[0], 0xff);
        assert_eq!(&out[1..=len], &bytes[..len]);
        assert!(out[len + 1..].iter().all(|&b| b == 0xff));
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn copy_range_does_not_create_a_missing_destination() {
        let src = file_with(b"abc");
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("sdz");
        assert!(copy_range(src.path(), 0, &dst, 0, None).is_err());
        assert!(!dst.exists());
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn copy_range_fails_when_the_source_is_missing() {
        let dst = file_with(b"....");
        assert!(copy_range(Path::new("/nonexistent/src"), 0, dst.path(), 0, Some(1)).is_err());
    }

    #[cfg(feature = "flash-mode-2")]
    #[test]
    fn zero_range_zeroes_exactly_the_asked_range() {
        let dst = file_with(b"xxxxxxxxxx");
        zero_range(dst.path(), &ByteRange { offset: 3, len: 4 }).unwrap();
        assert_eq!(std::fs::read(dst.path()).unwrap(), b"xxx\0\0\0\0xxx");
    }

    #[cfg(feature = "flash-mode-2")]
    #[test]
    fn zero_range_zeroes_across_several_buffer_chunks() {
        const TAIL: usize = 10;
        let len = 2 * COPY_BUFFER_SIZE + 1;
        let dst = file_with(&vec![0xff; len + 2 * TAIL]);

        zero_range(
            dst.path(),
            &ByteRange {
                offset: 1,
                len: len as u64,
            },
        )
        .unwrap();

        let out = std::fs::read(dst.path()).unwrap();
        assert_eq!(out[0], 0xff);
        assert!(out[1..=len].iter().all(|&b| b == 0));
        assert!(out[len + 1..].iter().all(|&b| b == 0xff));
    }

    #[cfg(feature = "flash-mode-2")]
    #[test]
    fn zero_range_does_not_create_a_missing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("sdz");
        assert!(zero_range(&dst, &ByteRange { offset: 0, len: 1 }).is_err());
        assert!(!dst.exists());
    }

    #[cfg(feature = "flash-mode-2")]
    #[test]
    fn reread_partition_table_fails_on_a_regular_file() {
        let disk = file_with(b"");
        assert!(matches!(
            reread_partition_table(disk.path()),
            Err(FlashError::PathIo { path, .. }) if path == disk.path()
        ));
    }
}
