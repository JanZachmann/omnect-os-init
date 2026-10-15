//! Parse `modules.dep` and `modules.builtin`.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use crate::early_modules::config::normalize_name;

const COMPRESSION_SUFFIXES: [&str; 3] = [".xz", ".zst", ".gz"];

pub(super) struct ModEntry {
    pub(super) path: PathBuf,
    /// All transitive dependencies, as depmod writes them; the last one loads
    /// first.
    pub(super) deps: Vec<PathBuf>,
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
    file.strip_suffix(".ko")
        .filter(|n| !n.is_empty())
        .map(normalize_name)
}

pub(super) fn is_compressed(path: &Path) -> bool {
    split_compression(path).is_some_and(|(file, compressed)| compressed && file.ends_with(".ko"))
}

/// depmod writes all transitive dependencies on each line, so only the lines of
/// the listed modules are kept.
pub(super) fn parse_modules_dep(text: &str, listed: &HashSet<&str>) -> HashMap<String, ModEntry> {
    text.lines()
        .filter_map(|line| {
            let (path, deps) = line.split_once(':')?;
            let path = PathBuf::from(path.trim());
            let name = module_name(&path).filter(|n| listed.contains(n.as_str()))?;
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
        assert_eq!(module_name(Path::new("kernel/a/.ko.xz")), None);
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
            &HashSet::from(["top", "low_level"]),
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
    fn skips_dep_lines_without_a_module() {
        let map = parse_modules_dep(
            "no colon\nkernel/a/notes.txt:\nkernel/a/ok.ko:\n",
            &HashSet::from(["notes", "ok"]),
        );
        assert_eq!(map.keys().collect::<Vec<_>>(), ["ok"]);
    }

    #[test]
    fn keeps_only_listed_lines() {
        let map = parse_modules_dep(
            "kernel/a/top.ko: kernel/b/low.ko\nkernel/b/low.ko:\n",
            &HashSet::from(["top"]),
        );
        assert_eq!(map.keys().collect::<Vec<_>>(), ["top"]);
        assert_eq!(map["top"].deps, [PathBuf::from("kernel/b/low.ko")]);
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
