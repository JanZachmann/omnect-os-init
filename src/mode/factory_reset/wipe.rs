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

// Block-device ioctls from <linux/fs.h>. `BLKGETSIZE64` is declared with
// `size_t`, so the request code is derived from the target's `size_t` rather
// than from the `u64` the kernel writes back. `BLKDISCARD` is declared `_IO`
// but takes a pointer to [start, length], which needs the `_bad` variant.
nix::ioctl_read_bad!(
    blkgetsize64,
    nix::request_code_read!(0x12, 114, std::mem::size_of::<nix::libc::size_t>()),
    u64
);
nix::ioctl_write_ptr_bad!(blkdiscard, nix::request_code_none!(0x12, 119), [u64; 2]);

/// Overwrite `device` with random data — factory-reset mode 2.
pub fn wipe_random(device: &Path) -> Result<()> {
    let mut file = open_device(device)?;
    let len = device_size(device, &file)?;

    log::info!("wiping {} with random data ({len} bytes)", device.display());
    overwrite_with_random(&mut file, len).map_err(|e| FactoryResetError::WipeFailed {
        device: device.to_path_buf(),
        reason: format!("random overwrite failed: {e}"),
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

fn device_size(device: &Path, file: &File) -> Result<u64> {
    let mut size: u64 = 0;
    // SAFETY: `size` is a valid u64 the ioctl writes the device size into.
    unsafe { blkgetsize64(file.as_raw_fd(), &mut size) }.map_err(|e| {
        FactoryResetError::WipeFailed {
            device: device.to_path_buf(),
            reason: format!("BLKGETSIZE64 failed: {e}"),
        }
    })?;
    Ok(size)
}

/// Overwrite the first `len` bytes of `target` with data from `/dev/urandom`.
///
/// Split from `wipe_random` so the loop can be tested against a temp file.
/// The last chunk is clamped to the remaining length — a write past the end of
/// a block device fails.
fn overwrite_with_random(target: &mut File, len: u64) -> std::io::Result<()> {
    let mut urandom = File::open(URANDOM_PATH)?;
    let mut buf = vec![0u8; WIPE_CHUNK_SIZE];
    let mut written: u64 = 0;
    let mut next_step = WIPE_PROGRESS_INTERVAL;

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
            next_step = next_step.saturating_add(WIPE_PROGRESS_INTERVAL);
        }
    }
    target.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILLER: u8 = 0xAA;
    const TAIL_LEN: usize = 4096;

    fn filled(len: usize) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&vec![FILLER; len]).unwrap();
        file
    }

    #[test]
    fn overwrite_replaces_every_byte_and_keeps_the_length() {
        // Below one chunk, above one chunk, and an exact multiple of it — the
        // last chunk is clamped to the remaining length in every case.
        for len in [TAIL_LEN, WIPE_CHUNK_SIZE + TAIL_LEN, 2 * WIPE_CHUNK_SIZE] {
            let mut file = filled(len);

            overwrite_with_random(file.as_file_mut(), len as u64).unwrap();

            let wiped = std::fs::read(file.path()).unwrap();
            assert_eq!(wiped.len(), len, "length must not change (len={len})");
            assert!(
                wiped[len - TAIL_LEN..].iter().any(|b| *b != FILLER),
                "the end must be overwritten too (len={len})"
            );
        }
    }

    #[test]
    fn overwrite_of_zero_length_is_a_noop() {
        let mut file = filled(TAIL_LEN);

        overwrite_with_random(file.as_file_mut(), 0).unwrap();

        assert_eq!(std::fs::read(file.path()).unwrap(), vec![FILLER; TAIL_LEN]);
    }
}
