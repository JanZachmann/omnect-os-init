//! xz decoding for the flash modes that receive a `wic.xz` over the network.

use std::io::{self, ErrorKind, Read};

use lzma_rust2::{XzReader, lzma2_get_memory_usage};

/// The largest dictionary of the xz presets (`-9`). The decoder allocates the
/// dictionary that each block header asks for, so a larger one is rejected
/// before it can push the device out of memory.
const MAX_XZ_DICT_SIZE: u32 = 64 * MIB;
const MIB: u32 = 1024 * 1024;

pub(crate) fn decoder<R: Read>(compressed: R) -> XzReader<FullReads<R>> {
    XzReader::new_mem_limit(
        FullReads(compressed),
        true,
        lzma2_get_memory_usage(MAX_XZ_DICT_SIZE),
    )
}

pub(crate) fn read_error(e: &io::Error) -> String {
    match e.kind() {
        ErrorKind::OutOfMemory => format!(
            "decoding needs too much memory, the xz dictionary limit is {} MiB: {e}",
            MAX_XZ_DICT_SIZE / MIB
        ),
        _ => format!("reading source: {e}"),
    }
}

/// The xz decoder treats a short read as a broken stream; a pipe returns only
/// what has arrived, so each read here fills the buffer or reaches the end of
/// input.
pub(crate) struct FullReads<R>(R);

impl<R: Read> Read for FullReads<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.0.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;
    use std::num::NonZeroU64;

    use lzma_rust2::{XzOptions, XzWriter};

    pub(crate) fn xz(data: &[u8]) -> Vec<u8> {
        xz_with(data, None)
    }

    /// `block_size` splits the stream into several blocks, as `xz -T0` does.
    pub(crate) fn xz_with(data: &[u8], block_size: Option<NonZeroU64>) -> Vec<u8> {
        let mut options = XzOptions::with_preset(1);
        options.set_block_size(block_size);
        let mut writer = XzWriter::new(Vec::new(), options).unwrap();
        writer.write_all(data).unwrap();
        writer.finish().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns one byte per read, as a pipe does when the writer is slow, and
    /// fails every other read with `Interrupted`.
    struct Choppy<'a> {
        data: &'a [u8],
        interrupt: bool,
    }

    impl Read for Choppy<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(ErrorKind::Interrupted.into());
            }
            let (Some(slot), Some((&byte, rest))) = (buf.first_mut(), self.data.split_first())
            else {
                return Ok(0);
            };
            *slot = byte;
            self.data = rest;
            Ok(1)
        }
    }

    #[test]
    fn full_reads_fill_the_buffer_until_the_input_ends() {
        let data: Vec<u8> = (0..10).collect();
        let mut reader = FullReads(Choppy {
            data: &data,
            interrupt: false,
        });
        let mut buf = [0u8; 6];
        assert_eq!(reader.read(&mut buf).unwrap(), 6);
        assert_eq!(buf, [0, 1, 2, 3, 4, 5]);
        assert_eq!(reader.read(&mut buf).unwrap(), 4, "the input ends");
        assert_eq!(buf[..4], [6, 7, 8, 9]);
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn an_out_of_memory_read_names_the_dictionary_limit() {
        let reason = read_error(&ErrorKind::OutOfMemory.into());
        assert!(reason.contains("limit is 64 MiB"), "{reason}");
        assert!(read_error(&ErrorKind::UnexpectedEof.into()).starts_with("reading source"));
    }
}
