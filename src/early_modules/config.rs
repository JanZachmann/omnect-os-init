//! Read the module list from `early-modules.d/*.conf`.

use std::{collections::HashSet, ffi::CString, fs, io::ErrorKind, path::Path};

use log::warn;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ModuleEntry {
    pub(super) name: String,
    pub(super) params: CString,
}

/// `modprobe` treats `-` and `_` in module names as equal.
pub(super) fn normalize_name(name: &str) -> String {
    name.replace('-', "_")
}

pub(super) fn read_config_dir(dir: &Path) -> Vec<ModuleEntry> {
    let mut files: Vec<_> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| {
                e.inspect_err(|e| warn!("early modules: cannot list {}: {e}", dir.display()))
                    .ok()
            })
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "conf"))
            .collect(),
        Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!("early modules: cannot read {}: {e}", dir.display());
            return Vec::new();
        }
    };
    files.sort();

    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for file in files {
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) => {
                warn!("early modules: cannot read {}: {e}", file.display());
                continue;
            }
        };
        for (name, params) in text.lines().filter_map(parse_line) {
            let name = normalize_name(name);
            let params = match CString::new(params) {
                Ok(params) => params,
                Err(e) => {
                    warn!(
                        "early modules: {name} in {}: bad parameters: {e}",
                        file.display()
                    );
                    continue;
                }
            };
            if !seen.insert(name.clone()) {
                warn!(
                    "early modules: {name} listed again in {}, line ignored",
                    file.display()
                );
                continue;
            }
            entries.push(ModuleEntry { name, params });
        }
    }
    entries
}

fn parse_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (name, params) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    Some((name, params.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(name: &str, params: &str) -> ModuleEntry {
        ModuleEntry {
            name: name.to_string(),
            params: CString::new(params).unwrap(),
        }
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
    }

    #[test]
    fn parses_names_params_comments_and_empty_lines() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "a.conf",
            "# comment\n\n  imx-sdma\nexample_mod debug=1 mode=fast\n",
        );
        assert_eq!(
            read_config_dir(dir.path()),
            vec![
                entry("imx_sdma", ""),
                entry("example_mod", "debug=1 mode=fast")
            ]
        );
    }

    #[test]
    fn parses_whitespace_variants() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "a.conf",
            "   \n\t# comment\nmod_a    p=1  q=2\n",
        );
        assert_eq!(
            read_config_dir(dir.path()),
            vec![entry("mod_a", "p=1  q=2")]
        );
    }

    #[test]
    fn hash_inside_a_line_is_a_parameter() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "mod_a label=x#1\n");
        assert_eq!(
            read_config_dir(dir.path()),
            vec![entry("mod_a", "label=x#1")]
        );
    }

    #[test]
    fn parses_tab_and_crlf() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "mod_a\tdebug=1\r\nmod_b\r\n");
        assert_eq!(
            read_config_dir(dir.path()),
            vec![entry("mod_a", "debug=1"), entry("mod_b", "")]
        );
    }

    #[test]
    fn reads_files_in_sorted_order() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "20-b.conf", "mod_b\n");
        write(dir.path(), "10-a.conf", "mod_a\n");
        assert_eq!(
            read_config_dir(dir.path()),
            vec![entry("mod_a", ""), entry("mod_b", "")]
        );
    }

    #[test]
    fn duplicate_keeps_first_line() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "imx_sdma p=1\nimx-sdma p=2\n");
        assert_eq!(read_config_dir(dir.path()), vec![entry("imx_sdma", "p=1")]);
    }

    #[test]
    fn duplicate_in_a_later_file_is_ignored() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "20-b.conf", "mod-a p=2\nmod_b\n");
        write(dir.path(), "10-a.conf", "mod_a p=1\n");
        assert_eq!(
            read_config_dir(dir.path()),
            vec![entry("mod_a", "p=1"), entry("mod_b", "")]
        );
    }

    #[test]
    fn line_with_nul_in_parameters_is_skipped() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "mod_a p=\0x\nmod_a p=2\n");
        assert_eq!(read_config_dir(dir.path()), vec![entry("mod_a", "p=2")]);
    }

    #[test]
    fn unreadable_file_does_not_stop_the_others() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("a.conf")).unwrap();
        std::fs::create_dir(dir.path().join("c.conf")).unwrap();
        write(dir.path(), "b.conf", "mod_b\n");
        assert_eq!(read_config_dir(dir.path()), vec![entry("mod_b", "")]);
    }

    #[test]
    fn config_path_that_is_a_file_lists_nothing() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "early-modules.d", "mod_a\n");
        assert!(read_config_dir(&dir.path().join("early-modules.d")).is_empty());
    }

    #[test]
    fn ignores_non_conf_files() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf.bak", "mod_a\n");
        write(dir.path(), ".a.conf.swp", "mod_b\n");
        assert!(read_config_dir(dir.path()).is_empty());
    }

    #[test]
    fn missing_dir_lists_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(read_config_dir(&dir.path().join("missing")).is_empty());
    }
}
