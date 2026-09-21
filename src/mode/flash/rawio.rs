//! In-process replacements for the `dd` calls in the legacy flash scripts.
//!
//! Every one of them is a read and a write at a byte offset, so a seek plus a
//! buffered copy covers all of them. Callers follow with `sync_all`, which the
//! legacy scripts got for free when `dd` returned.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::FlashError;

/// Bytes per KiB. Build-time offsets and sizes are KB-valued; block devices
/// are addressed in bytes.
pub const KIB: u64 = 1024;

/// Copy buffer. Large enough to keep a block device streaming, small enough
/// for an initramfs that shares RAM with the image it is flashing.
pub const COPY_BUFFER_SIZE: usize = 1024 * 1024;

/// Copy `len` bytes (or the rest of the source when `len` is `None`) from
/// `src_offset` in `src` to `dst_offset` in `dst`.
///
/// A source that ends early is an error: a partition copy that silently wrote
/// less than asked would produce a clone that boots and then fails.
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

    // Never `.create(true)`: the destination is an existing block device or
    // file, and creating one where a typo expected a device would turn a
    // mistake into a silent no-op that looks like success.
    let mut dst_file = OpenOptions::new()
        .write(true)
        .open(dst)
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
            Some(len) => usize::try_from(len - copied)
                .unwrap_or(buf.len())
                .min(buf.len()),
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
                Ok(w) => written += w,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(copy_failed(format!("writing destination: {e}"))),
            }
        }

        copied += n as u64;
    }

    Ok(copied)
}

/// Flush every filesystem buffer to disk.
pub fn sync_all() -> Result<(), FlashError> {
    nix::unistd::sync();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn file_with(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

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

    #[test]
    fn copy_range_fails_when_the_source_is_missing() {
        let dst = file_with(b"....");
        assert!(copy_range(Path::new("/nonexistent/src"), 0, dst.path(), 0, Some(1)).is_err());
    }
}
