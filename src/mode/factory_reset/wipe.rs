use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::error::FactoryResetError;

pub(crate) type WipeResult<T> = std::result::Result<T, FactoryResetError>;

const WIPE_CHUNK_SIZE: usize = 1024 * 1024;
const WIPE_PROGRESS_INTERVAL: u64 = 1024 * 1024 * 1024;

const URANDOM_PATH: &str = "/dev/urandom";

// BLKDISCARD from <linux/fs.h>. It is declared `_IO` but takes a pointer to
// [start, length], which needs the `_bad` variant.
const BLK_IOC_MAGIC: u8 = 0x12;
const BLKDISCARD_NR: u8 = 119;

nix::ioctl_write_ptr_bad!(
    blkdiscard,
    nix::request_code_none!(BLK_IOC_MAGIC, BLKDISCARD_NR),
    [u64; 2]
);

fn wipe_failed(device: &Path, reason: impl std::fmt::Display) -> FactoryResetError {
    FactoryResetError::WipeFailed {
        device: device.to_path_buf(),
        reason: reason.to_string(),
    }
}

/// Overwrite `device` with random data — factory-reset mode 2.
pub fn wipe_random(device: &Path) -> WipeResult<()> {
    let mut file = open_device(device)?;
    let len = device_size(device, &file)?;

    log::info!("wiping {} with random data ({len} bytes)", device.display());
    overwrite_with_random(&mut file, len, WIPE_PROGRESS_INTERVAL)
        .map_err(|e| wipe_failed(device, format!("random overwrite failed: {e}")))?;

    log::info!("wiped {} with random data", device.display());
    Ok(())
}

/// Discard every block of `device` — factory-reset mode 3. Fails on hardware
/// without discard support.
pub fn wipe_discard(device: &Path) -> WipeResult<()> {
    let file = open_device(device)?;
    let len = device_size(device, &file)?;

    log::info!(
        "discarding all blocks of {} ({len} bytes)",
        device.display()
    );
    let range: [u64; 2] = [0, len];
    // SAFETY: `range` outlives the call and holds the two u64 the ioctl reads.
    unsafe { blkdiscard(file.as_raw_fd(), &range) }
        .map_err(|e| wipe_failed(device, format!("BLKDISCARD failed: {e}")))?;

    log::info!("discarded all blocks of {}", device.display());
    Ok(())
}

/// O_EXCL makes the kernel refuse a block device that is still mounted, so a
/// wipe cannot run against a live filesystem even if a caller forgets to
/// unmount first.
fn open_device(device: &Path) -> WipeResult<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_EXCL)
        .open(device)
        .map_err(|e| wipe_failed(device, format!("cannot open device: {e}")))
}

fn device_size(device: &Path, file: &File) -> WipeResult<u64> {
    let mut handle = file;
    let size = handle
        .seek(SeekFrom::End(0))
        .and_then(|size| handle.seek(SeekFrom::Start(0)).map(|_| size))
        .map_err(|e| wipe_failed(device, format!("cannot determine size: {e}")))?;
    Ok(size)
}

/// Overwrite the first `len` bytes of `target` with data from `/dev/urandom`,
/// syncing and logging every `progress_interval` bytes.
///
/// Split from `wipe_random` so a test can set `progress_interval`.
fn overwrite_with_random(
    target: &mut File,
    len: u64,
    progress_interval: u64,
) -> std::io::Result<()> {
    let mut urandom = File::open(URANDOM_PATH)?;
    let mut buf = vec![0u8; WIPE_CHUNK_SIZE];
    let mut written: u64 = 0;
    let mut next_step = progress_interval;

    target.seek(SeekFrom::Start(0))?;
    while written < len {
        let chunk = chunk_len(len, written);
        urandom.read_exact(&mut buf[..chunk])?;
        target.write_all(&buf[..chunk])?;
        written += chunk as u64;

        if written >= next_step {
            // Flush every interval, so after a power loss at most one interval
            // of blocks still holds the old content.
            target.sync_all()?;
            log::info!("wipe progress: {written}/{len} bytes");
            next_step = next_step.saturating_add(progress_interval);
        }
    }
    target.sync_all()
}

/// Clamping happens in u64. Casting the remainder to `usize` first truncates
/// on a 32-bit target, where a remainder that is a multiple of 4 GiB becomes a
/// zero-length chunk the loop never gets past.
fn chunk_len(len: u64, written: u64) -> usize {
    (len - written).min(WIPE_CHUNK_SIZE as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;

    const FILLER: u8 = 0xAA;
    const BLOCK_LEN: usize = 4096;

    fn filled(len: usize) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&vec![FILLER; len]).unwrap();
        file
    }

    fn all_replaced(path: &Path, len: usize) -> bool {
        let wiped = std::fs::read(path).unwrap();
        wiped.len() == len
            && wiped
                .chunks(BLOCK_LEN)
                .all(|block| block.iter().any(|b| *b != FILLER))
    }

    #[test]
    fn overwrite_replaces_every_byte_and_keeps_the_length() {
        // Below one chunk, above one chunk, and an exact multiple of it — the
        // last chunk is clamped to the remaining length in every case.
        for len in [BLOCK_LEN, WIPE_CHUNK_SIZE + BLOCK_LEN, 2 * WIPE_CHUNK_SIZE] {
            let mut file = filled(len);

            overwrite_with_random(file.as_file_mut(), len as u64, WIPE_PROGRESS_INTERVAL).unwrap();

            assert!(all_replaced(file.path(), len), "len={len}");
        }
    }

    #[test]
    fn overwrite_of_zero_length_is_a_noop() {
        let mut file = filled(BLOCK_LEN);

        overwrite_with_random(file.as_file_mut(), 0, WIPE_PROGRESS_INTERVAL).unwrap();

        assert_eq!(std::fs::read(file.path()).unwrap(), vec![FILLER; BLOCK_LEN]);
    }

    #[test]
    fn overwrite_syncs_repeatedly_without_losing_data() {
        // an interval below the chunk size makes every chunk cross it, which is
        // the arithmetic the 1 GiB default never exercises in a test
        let len = 3 * WIPE_CHUNK_SIZE;
        let mut file = filled(len);

        overwrite_with_random(file.as_file_mut(), len as u64, BLOCK_LEN as u64).unwrap();

        assert!(all_replaced(file.path(), len));
    }

    #[test]
    fn overwrite_propagates_a_write_failure() {
        let file = filled(BLOCK_LEN);
        // a read-only handle makes write_all fail; a loop that swallowed the
        // error would report a wipe that never happened
        let mut readonly = File::open(file.path()).unwrap();

        overwrite_with_random(&mut readonly, BLOCK_LEN as u64, WIPE_PROGRESS_INTERVAL).unwrap_err();

        assert_eq!(std::fs::read(file.path()).unwrap(), vec![FILLER; BLOCK_LEN]);
    }

    #[test]
    fn device_size_reports_the_length_and_rewinds() {
        let len = WIPE_CHUNK_SIZE + BLOCK_LEN;
        let file = filled(len);
        let handle = File::open(file.path()).unwrap();

        let size = device_size(file.path(), &handle).unwrap();

        assert_eq!(size, len as u64);
        // the caller writes from the start, so the cursor must not stay at the end
        assert_eq!((&handle).stream_position().unwrap(), 0);
    }

    #[test]
    fn wipe_random_replaces_the_whole_file() {
        // the size comes from a seek, so the entry point runs on a temp file
        let len = WIPE_CHUNK_SIZE + BLOCK_LEN;
        let file = filled(len);

        wipe_random(file.path()).unwrap();

        assert!(all_replaced(file.path(), len));
    }

    #[test]
    fn wipe_random_reports_a_device_it_cannot_open() {
        let err = wipe_random(Path::new("/does/not/exist")).unwrap_err();

        assert!(
            err.to_string().contains("cannot open device"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn wipe_random_writes_random_data_not_a_constant() {
        let len = BLOCK_LEN;
        let first = filled(len);
        let second = filled(len);

        wipe_random(first.path()).unwrap();
        wipe_random(second.path()).unwrap();

        let first = std::fs::read(first.path()).unwrap();
        let second = std::fs::read(second.path()).unwrap();
        assert_ne!(first, second, "two wipes must not write the same bytes");

        let mut seen = first.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert!(
            seen.len() > u8::MAX as usize / 4,
            "only {} distinct byte values, that is not random data",
            seen.len()
        );
    }

    #[test]
    fn wipe_discard_reports_a_device_that_cannot_discard() {
        // a regular file gives ENOTTY, hardware without discard gives
        // EOPNOTSUPP; only the mapping to WipeFailed is shared
        let file = filled(BLOCK_LEN);

        let err = wipe_discard(file.path()).unwrap_err();

        assert!(
            err.to_string().contains("BLKDISCARD failed"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains(&file.path().display().to_string()),
            "the error must name the device: {err}"
        );
    }

    #[test]
    fn wipe_discard_reports_a_device_it_cannot_open() {
        let err = wipe_discard(Path::new("/does/not/exist")).unwrap_err();

        assert!(
            err.to_string().contains("cannot open device"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn chunk_len_clamps_without_truncating() {
        // usize is 64 bit on the host, so only a 32-bit run of this test can
        // fail it.
        for (len, written) in [
            (FOUR_GIB, 0),
            (FOUR_GIB + BLOCK_LEN as u64, BLOCK_LEN as u64),
            (2 * FOUR_GIB, FOUR_GIB),
        ] {
            assert_eq!(chunk_len(len, written), WIPE_CHUNK_SIZE, "{len}/{written}");
        }

        assert_eq!(chunk_len(BLOCK_LEN as u64, 0), BLOCK_LEN);
        assert_eq!(chunk_len(FOUR_GIB, FOUR_GIB - 1), 1);
    }
}
