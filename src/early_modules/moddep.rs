//! Parse `modules.dep` and `modules.builtin`.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use super::config::normalize_name;

const COMPRESSION_SUFFIXES: [&str; 3] = [".xz", ".zst", ".gz"];

pub(super) struct ModEntry {
    pub path: PathBuf,
    pub deps: Vec<PathBuf>,
}

fn split_compression(path: &Path) -> Option<(&str, bool)> {
    let file = path.file_name()?.to_str()?;
    Some(
        COMPRESSION_SUFFIXES
            .iter()
            .find_map(|s| file.strip_suffix(s))
            .map_or((file, false), |f| (f, true)),
    )
}

pub(super) fn module_name(path: &Path) -> Option<String> {
    let (file, _) = split_compression(path)?;
    file.strip_suffix(".ko").map(normalize_name)
}

pub(super) fn is_compressed(path: &Path) -> bool {
    split_compression(path).is_some_and(|(file, compressed)| compressed && file.ends_with(".ko"))
}

pub(super) fn parse_modules_dep(text: &str) -> HashMap<String, ModEntry> {
    text.lines()
        .filter_map(|line| {
            let (path, deps) = line.split_once(':')?;
            let path = PathBuf::from(path.trim());
            let name = module_name(&path)?;
            let deps = deps.split_whitespace().map(PathBuf::from).collect();
            Some((name, ModEntry { path, deps }))
        })
        .collect()
}

pub(super) fn parse_modules_builtin(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| module_name(Path::new(l)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_from_path() {
        assert_eq!(
            module_name(Path::new("kernel/drivers/dma/imx-sdma.ko")).as_deref(),
            Some("imx_sdma")
        );
        assert_eq!(
            module_name(Path::new("kernel/a/foo.ko.xz")).as_deref(),
            Some("foo")
        );
        assert_eq!(
            module_name(Path::new("kernel/a/foo.ko.zst")).as_deref(),
            Some("foo")
        );
        assert_eq!(
            module_name(Path::new("kernel/a/foo.ko.gz")).as_deref(),
            Some("foo")
        );
        assert_eq!(module_name(Path::new("kernel/a/foo.txt")), None);
    }

    #[test]
    fn compressed_only_for_ko_suffix() {
        assert!(is_compressed(Path::new("a/foo.ko.xz")));
        assert!(is_compressed(Path::new("a/foo.ko.zst")));
        assert!(is_compressed(Path::new("a/foo.ko.gz")));
        assert!(!is_compressed(Path::new("a/foo.ko")));
        assert!(!is_compressed(Path::new("a/foo.tar.xz")));
    }

    #[test]
    fn parses_dep_lines_in_listed_order() {
        let map = parse_modules_dep(
            "kernel/a/top.ko.xz: kernel/b/mid.ko.xz kernel/c/low-level.ko.xz\n\
             kernel/c/low-level.ko.xz:\n",
        );
        let top = &map["top"];
        assert_eq!(top.path, PathBuf::from("kernel/a/top.ko.xz"));
        assert_eq!(
            top.deps,
            [
                PathBuf::from("kernel/b/mid.ko.xz"),
                PathBuf::from("kernel/c/low-level.ko.xz")
            ]
        );
        assert!(map["low_level"].deps.is_empty());
    }

    #[test]
    fn parses_builtin() {
        let set = parse_modules_builtin("kernel/drivers/x/foo-bar.ko\n\nkernel/y/baz.ko\n");
        assert_eq!(
            set,
            HashSet::from(["foo_bar".to_string(), "baz".to_string()])
        );
    }
}
