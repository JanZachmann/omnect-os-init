//! Thin wrapper around `bmaptool copy`.

use std::ffi::OsStr;
use std::path::Path;

use crate::error::FlashError;
use crate::mode::flash::run_inherited;

const BMAPTOOL_CMD: &str = "/usr/bin/bmaptool";
const BMAPTOOL_COPY_SUBCOMMAND: &str = "copy";
const BMAPTOOL_BMAP_FLAG: &str = "--bmap";

pub(crate) struct BmapArgs<'a> {
    pub(crate) bmap: &'a Path,
    pub(crate) source: &'a Path,
    pub(crate) destination: &'a Path,
}

fn command_args<'a>(args: &'a BmapArgs<'a>) -> [&'a OsStr; 5] {
    [
        OsStr::new(BMAPTOOL_COPY_SUBCOMMAND),
        OsStr::new(BMAPTOOL_BMAP_FLAG),
        args.bmap.as_os_str(),
        args.source.as_os_str(),
        args.destination.as_os_str(),
    ]
}

/// Copy `source` to `destination`, writing only the ranges the bmap lists.
pub(crate) fn copy(args: &BmapArgs<'_>) -> Result<(), FlashError> {
    copy_with(BMAPTOOL_CMD, args)
}

fn copy_with(cmd: &str, args: &BmapArgs<'_>) -> Result<(), FlashError> {
    run_inherited(cmd, &command_args(args)).map_err(|reason| FlashError::CopyFailed {
        src: args.source.to_path_buf(),
        dst: args.destination.to_path_buf(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BMAP: &str = "/home/omnect/wic.bmap";

    fn args_of(source: &str, destination: &str) -> Vec<String> {
        command_args(&BmapArgs {
            bmap: Path::new(BMAP),
            source: Path::new(source),
            destination: Path::new(destination),
        })
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn the_verify_pass_copies_the_archive_to_a_file() {
        assert_eq!(
            args_of("/home/omnect/wic.xz", "/home/omnect/wic"),
            [
                "copy",
                "--bmap",
                BMAP,
                "/home/omnect/wic.xz",
                "/home/omnect/wic"
            ]
        );
    }

    #[test]
    fn the_flash_pass_copies_the_file_to_the_root_block_device() {
        assert_eq!(
            args_of("/home/omnect/wic", "/dev/sda"),
            ["copy", "--bmap", BMAP, "/home/omnect/wic", "/dev/sda"]
        );
    }

    #[test]
    fn a_tool_that_cannot_run_maps_to_copy_failed() {
        let err = copy_with(
            "/nonexistent/bmaptool",
            &BmapArgs {
                bmap: Path::new(BMAP),
                source: Path::new("/a"),
                destination: Path::new("/b"),
            },
        )
        .unwrap_err();
        let FlashError::CopyFailed { src, dst, reason } = err else {
            panic!("expected CopyFailed, got {err}");
        };
        assert_eq!(src, Path::new("/a"));
        assert_eq!(dst, Path::new("/b"));
        assert!(!reason.is_empty());
    }
}
