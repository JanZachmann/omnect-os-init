use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::error::{FactoryResetError, Result};

/// Chunk size for the mode-2 random overwrite.
const WIPE_CHUNK_SIZE: usize = 1024 * 1024;

/// Log and flush the mode-2 overwrite once per this many written bytes.
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

/// Overwrite `device` with random data — factory-reset mode 2.
pub fn wipe_random(device: &Path) -> Result<()> {
    let mut file = open_device(device)?;
    let len = device_size(device, &file)?;

    log::info!("wiping {} with random data ({len} bytes)", device.display());
    overwrite_with_random(&mut file, len, WIPE_PROGRESS_INTERVAL).map_err(|e| {
        FactoryResetError::WipeFailed {
            device: device.to_path_buf(),
            reason: format!("random overwrite failed: {e}"),
        }
    })?;

    log::info!("wiped {} with random data", device.display());
    Ok(())
}

/// Discard every block of `device` — factory-reset mode 3. Fails on hardware
/// without discard support.
pub fn wipe_discard(device: &Path) -> Result<()> {
    let file = open_device(device)?;
    let len = device_size(device, &file)?;

    log::info!(
        "discarding all blocks of {} ({len} bytes)",
        device.display()
    );
    let range: [u64; 2] = [0, len];
    // SAFETY: `range` outlives the call and holds the two u64 the ioctl reads.
    unsafe { blkdiscard(file.as_raw_fd(), &range) }.map_err(|e| FactoryResetError::WipeFailed {
        device: device.to_path_buf(),
        reason: format!("BLKDISCARD failed: {e}"),
    })?;

    log::info!("discarded all blocks of {}", device.display());
    Ok(())
}

fn open_device(device: &Path) -> Result<File> {
    OpenOptions::new().write(true).open(device).map_err(|e| {
        FactoryResetError::WipeFailed {
            device: device.to_path_buf(),
            reason: format!("cannot open device: {e}"),
        }
        .into()
    })
}

/// Size of a block device: seeking to its end reports it, which keeps this
/// testable against a temp file.
fn device_size(device: &Path, file: &File) -> Result<u64> {
    let mut handle = file;
    let size = handle
        .seek(SeekFrom::End(0))
        .and_then(|size| handle.seek(SeekFrom::Start(0)).map(|_| size))
        .map_err(|e| FactoryResetError::WipeFailed {
            device: device.to_path_buf(),
            reason: format!("cannot determine size: {e}"),
        })?;
    Ok(size)
}

/// Overwrite the first `len` bytes of `target` with data from `/dev/urandom`,
/// syncing and logging every `progress_interval` bytes.
///
/// Split from `wipe_random` so the loop can be tested against a temp file.
/// The last chunk is clamped to the remaining length — a write past the end of
/// a block device fails.
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
        // Clamp in u64: casting the remainder first truncates on a 32-bit
        // target, and a remainder that is a multiple of 4 GiB becomes a
        // zero-length chunk the loop never gets past.
        let chunk = (len - written).min(WIPE_CHUNK_SIZE as u64) as usize;
        urandom.read_exact(&mut buf[..chunk])?;
        target.write_all(&buf[..chunk])?;
        written += chunk as u64;

        if written >= next_step {
            // Flush at every step, so a power loss can expose the old content
            // of at most one interval of already overwritten blocks.
            target.sync_all()?;
            log::info!("wipe progress: {written}/{len} bytes");
            next_step = next_step.saturating_add(progress_interval);
        }
    }
    target.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // the size now comes from a seek, so the entry point runs on a temp file
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
}
