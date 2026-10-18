# Early Module Load Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Correction (2026-10-15):** the code and the spec differ from this plan in these points, and they are the reference:
> - The step ends with one `error` line, not `warn`.
> - `parse_modules_dep` takes the set of listed modules and keeps only their lines.
> - The loader has no `Session` with cloned `deps`. An immutable `Resolved` holds the inputs, and the load state is separate.
> - The step runs after `init_setup::run` and not in a flash mode, not right after `mount_core_partitions`.
> - `param_set_copystring` in the kernel does not strip a trailing newline. The code writes none.

**Goal:** Load the kernel modules listed in `<rootfs>/etc/omnect/early-modules.d/*.conf` (with dependencies and firmware) before `switch_root`, behind the Cargo feature `early-modules`, and enable it for `mx8mm-nxp-bsp` (`imx_sdma`) in meta-omnect.

**Architecture:** New module `src/early_modules/` with three files: config parser, `modules.dep`/`modules.builtin` parser, and the loader. The loader writes `<rootfs>/lib/firmware` to `firmware_class.path` and calls `finit_module` on the rootfs files through a small trait, so tests never load a module. `run_init` calls `early_modules::load(rootfs)` right after `mount_core_partitions` succeeds. The step never returns an error.

**Tech Stack:** Rust 2024, `nix` 0.29 (`kmod` feature, enabled only by `early-modules`), `log`, `tempfile` in tests. meta-omnect (Yocto) for integration.

**Spec:** `docs/superpowers/specs/2026-09-25-early-module-load-design.md` on `upstream/main` (merged with PR #28). The new branch is cut from `origin/feat/flash-mode-3`, which does **not** contain the spec file. Read it with `git show upstream/main:docs/superpowers/specs/2026-09-25-early-module-load-design.md`.

## Context

On i.MX8MM `imx_sdma` is a module (its firmware is in the rootfs). udev loads it only ~6 s after systemd starts, so the TPM on `spi0.1` and `can0` appear late. The legacy initramfs loaded it with `modprobe` from bind mounts. The Rust init needs the same, but generic (config files instead of a fixed name) and without `kmod` or mounts left behind.

## Global Constraints

- Feature: `early-modules = ["core", "nix/kmod"]`, off by default. Images without the feature do not change (also not at the dependency level).
- Config: `<rootfs>/etc/omnect/early-modules.d/*.conf`, sorted file name order. Only files ending in `.conf`.
- Comment: a line whose first non-blank character is `#` is a comment (full-line only, user decision). The rest of a module line goes unchanged (trimmed) to `finit_module`.
- `-` and `_` in module names are equal.
- A module listed twice: first line wins, the second line is a `warn`.
- Parameters only come from the config line; `modprobe.d` options do not apply.
- `MODULE_INIT_COMPRESSED_FILE = 4` (verified in `/usr/include/linux/module.h`), built with `ModuleInitFlags::from_bits_retain`, set only for `.ko.xz`, `.ko.zst`, `.ko.gz`.
- `firmware_class.path` is not restored after the step.
- No path returns an error; every outcome is logged. One final `warn` lists all listed modules that did not load.
- Log lines start with `early modules:` and are short and plain.
- Comments explain only "why" (the `-`/`_` rule, the kernel header constant, why the path is not restored). No comment restates code.
- Commits end with `Signed-off-by: Jan Zachmann <50990105+JanZachmann@users.noreply.github.com>`.
- Gate (each task): `cargo fmt --check`; `cargo clippy --features uboot,gpt,early-modules -- -D warnings` and the same with `--tests,test-utils`; `cargo test --features uboot,gpt,early-modules,test-utils`; `cargo test --features grub,gpt,early-modules,test-utils`; `cargo test --target i686-unknown-linux-gnu --features uboot,gpt,early-modules,test-utils`; `cargo clippy --target armv7-unknown-linux-gnueabihf --features uboot,gpt,early-modules -- -D warnings`; `cargo build --features uboot,gpt` (no feature, no loader code); `cargo audit`.

## Review Focus

1. A config line separated by a tab, or with CRLF line end → name and params parse the same as with a space/LF. (Task 1 test `parses_tab_and_crlf`)
2. Two listed modules share one dependency → the dependency is loaded once; the second time it is found via `/sys/module/<name>/initstate`. (Task 3 test `shared_dependency_loaded_once`)
3. A shared dependency fails → it is not tried again for the second module, and the final summary names both listed modules. (Task 3 test `failed_shared_dependency_not_retried`)
4. Other files in the config dir (`imx-sdma.conf.bak`, `.swp`, a sub-directory) → ignored. Missing config dir → nothing happens. (Task 1 test `ignores_non_conf_files`)
5. `osrelease` with a trailing newline → trimmed before it is used in the path. (Task 3 fixture writes `"6.6.23\n"`)

## File Structure

omnect-os-init (branch `feat/early-modules` from `origin/feat/flash-mode-3`):

- Create `src/early_modules/mod.rs` — public `load(rootfs)`, `ModuleLoader` trait, kernel loader, load order, firmware path, summary.
- Create `src/early_modules/config.rs` — read and parse `*.conf`, `normalize_name`.
- Create `src/early_modules/moddep.rs` — parse `modules.dep` and `modules.builtin`, `module_name`, `is_compressed`.
- Modify `Cargo.toml` — feature `early-modules`.
- Modify `src/lib.rs` — `pub mod early_modules` under cfg, call in `run_init`.
- Modify `README.md` — Features row, Startup Flow node, Runtime Dependencies row, Testing line.
- Modify `project-context.md` — module tree entry, test command.
- Create `docs/superpowers/plans/2026-10-13-early-module-load.md` — copy of this plan (repo convention).

meta-omnect (branch `initramfs-integration`, PR #697):

- Modify `recipes-omnect/omnect-os-init/omnect-os-init.inc` — feature append for `mx8mm-nxp-bsp`; SRCREV in `omnect-os-init_0.1.0.bb` if it pins one.
- Create `recipes-omnect/omnect-base-files/omnect-base-files/etc/omnect/early-modules.d/imx-sdma.conf`.
- Modify `recipes-omnect/omnect-base-files/omnect-base-files.bb` — install it for `mx8mm-nxp-bsp`.
- Create `recipes-kernel/linux/files/module-decompress.cfg`; modify `conf/distro/include/omnect-os-kernel.conf` — add it for `linux-raspberrypi`.

---

### Task 1: Feature and config parser

**Files:**
- Modify: `Cargo.toml` (`[features]`)
- Modify: `src/lib.rs` (module list)
- Create: `src/early_modules/mod.rs` (only `mod config;` for now), `src/early_modules/config.rs`
- Create: `docs/superpowers/plans/2026-10-13-early-module-load.md`

**Interfaces:**
- Produces: `pub(super) struct ModuleEntry { pub name: String, pub params: CString }`; `pub(super) fn normalize_name(name: &str) -> String`; `pub(super) fn read_config_dir(dir: &Path) -> Vec<ModuleEntry>`.

- [ ] **Step 1: Branch**

```bash
git fetch origin feat/flash-mode-3
git switch -c feat/early-modules origin/feat/flash-mode-3
```

- [ ] **Step 2: Feature and module skeleton**

`Cargo.toml`, keep the list sorted:

```toml
early-modules = ["core", "nix/kmod"] # Load the modules listed in the rootfs before switch_root
```

`src/lib.rs`, after `pub mod early_init;`:

```rust
#[cfg(feature = "early-modules")]
pub mod early_modules;
```

`src/early_modules/mod.rs`:

```rust
//! Load the kernel modules listed in the rootfs before `switch_root`.

mod config;
```

- [ ] **Step 3: Write the failing tests** in `src/early_modules/config.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(name: &str, params: &str) -> ModuleEntry {
        ModuleEntry { name: name.to_string(), params: CString::new(params).unwrap() }
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
    }

    #[test]
    fn parses_names_params_comments_and_empty_lines() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "# comment\n\n  imx-sdma\nexample_mod debug=1 mode=fast\n");
        assert_eq!(
            read_config_dir(dir.path()),
            vec![entry("imx_sdma", ""), entry("example_mod", "debug=1 mode=fast")]
        );
    }

    #[test]
    fn hash_inside_a_line_is_a_parameter() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "mod_a label=x#1\n");
        assert_eq!(read_config_dir(dir.path()), vec![entry("mod_a", "label=x#1")]);
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
        assert_eq!(read_config_dir(dir.path()), vec![entry("mod_a", ""), entry("mod_b", "")]);
    }

    #[test]
    fn duplicate_keeps_first_line() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf", "imx_sdma p=1\nimx-sdma p=2\n");
        assert_eq!(read_config_dir(dir.path()), vec![entry("imx_sdma", "p=1")]);
    }

    #[test]
    fn ignores_non_conf_files() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.conf.bak", "mod_a\n");
        write(dir.path(), ".a.conf.swp", "mod_b\n");
        std::fs::create_dir(dir.path().join("sub.conf")).unwrap();
        assert!(read_config_dir(dir.path()).is_empty());
    }

    #[test]
    fn missing_dir_lists_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(read_config_dir(&dir.path().join("missing")).is_empty());
    }
}
```

Note: `sub.conf` is a directory; `read_to_string` fails on it → `warn`, skip. That is the "config file cannot be read" row of spec §8.

- [ ] **Step 4: Run, expect compile failure**

Run: `cargo test --features uboot,gpt,early-modules early_modules::config`
Expected: FAIL, `read_config_dir` not found.

- [ ] **Step 5: Implement** (above the test module)

```rust
//! Read the module list from `early-modules.d/*.conf`.

use std::{collections::HashSet, ffi::CString, fs, io::ErrorKind, path::Path};

use log::warn;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ModuleEntry {
    pub name: String,
    pub params: CString,
}

/// `modprobe` treats `-` and `_` in module names as equal.
pub(super) fn normalize_name(name: &str) -> String {
    name.replace('-', "_")
}

pub(super) fn read_config_dir(dir: &Path) -> Vec<ModuleEntry> {
    let mut files: Vec<_> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
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
            if !seen.insert(name.clone()) {
                warn!("early modules: {name} listed again in {}, line ignored", file.display());
                continue;
            }
            match CString::new(params) {
                Ok(params) => entries.push(ModuleEntry { name, params }),
                Err(e) => warn!("early modules: {name}: bad parameters: {e}"),
            }
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
```

`ModuleEntry`, `normalize_name` and `read_config_dir` are unused until Task 3; add `#[allow(dead_code)]` on `mod config;` only if clippy fails, and remove it in Task 3.

- [ ] **Step 6: Run tests, expect PASS**; then the Global Constraints gate.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/lib.rs src/early_modules docs/superpowers/plans/2026-10-13-early-module-load.md
git commit -s -m "feat(early-modules): add feature and config parser"
```

---

### Task 2: modules.dep and modules.builtin parser

**Files:**
- Create: `src/early_modules/moddep.rs`
- Modify: `src/early_modules/mod.rs` (`mod moddep;`)

**Interfaces:**
- Consumes: `config::normalize_name`.
- Produces: `pub(super) struct ModEntry { pub path: PathBuf, pub deps: Vec<PathBuf> }`; `pub(super) fn parse_modules_dep(text: &str) -> HashMap<String, ModEntry>`; `pub(super) fn parse_modules_builtin(text: &str) -> HashSet<String>`; `pub(super) fn module_name(path: &Path) -> Option<String>`; `pub(super) fn is_compressed(path: &Path) -> bool`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_from_path() {
        assert_eq!(module_name(Path::new("kernel/drivers/dma/imx-sdma.ko")).as_deref(), Some("imx_sdma"));
        assert_eq!(module_name(Path::new("kernel/a/foo.ko.xz")).as_deref(), Some("foo"));
        assert_eq!(module_name(Path::new("kernel/a/foo.ko.zst")).as_deref(), Some("foo"));
        assert_eq!(module_name(Path::new("kernel/a/foo.ko.gz")).as_deref(), Some("foo"));
        assert_eq!(module_name(Path::new("kernel/a/foo.txt")), None);
    }

    #[test]
    fn compressed_only_for_ko_suffix() {
        assert!(is_compressed(Path::new("a/foo.ko.xz")));
        assert!(is_compressed(Path::new("a/foo.ko.zst")));
        assert!(is_compressed(Path::new("a/foo.ko.gz")));
        assert!(!is_compressed(Path::new("a/foo.ko")));
    }

    #[test]
    fn parses_dep_lines_in_listed_order() {
        let map = parse_modules_dep(
            "kernel/a/top.ko.xz: kernel/b/mid.ko.xz kernel/c/low-level.ko.xz\nkernel/c/low-level.ko.xz:\n",
        );
        let top = &map["top"];
        assert_eq!(top.path, PathBuf::from("kernel/a/top.ko.xz"));
        assert_eq!(top.deps, vec![PathBuf::from("kernel/b/mid.ko.xz"), PathBuf::from("kernel/c/low-level.ko.xz")]);
        assert!(map["low_level"].deps.is_empty());
    }

    #[test]
    fn parses_builtin() {
        let set = parse_modules_builtin("kernel/drivers/x/foo-bar.ko\n\nkernel/y/baz.ko\n");
        assert!(set.contains("foo_bar") && set.contains("baz"));
        assert_eq!(set.len(), 2);
    }
}
```

- [ ] **Step 2: Run, expect compile failure**: `cargo test --features uboot,gpt,early-modules early_modules::moddep`

- [ ] **Step 3: Implement**

```rust
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

pub(super) fn module_name(path: &Path) -> Option<String> {
    let file = path.file_name()?.to_str()?;
    let file = COMPRESSION_SUFFIXES
        .iter()
        .find_map(|s| file.strip_suffix(s))
        .unwrap_or(file);
    file.strip_suffix(".ko").map(normalize_name)
}

pub(super) fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| COMPRESSION_SUFFIXES.iter().any(|s| f.strip_suffix(s).is_some_and(|f| f.ends_with(".ko"))))
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
```

- [ ] **Step 4: Run tests, expect PASS**; run the gate.

- [ ] **Step 5: Commit**: `git commit -s -m "feat(early-modules): parse modules.dep and modules.builtin"`

---

### Task 3: Loader

**Files:**
- Modify: `src/early_modules/mod.rs`

**Interfaces:**
- Consumes: Task 1 and Task 2 items.
- Produces: `pub fn load(rootfs: &Path)` (called by Task 4). Internal: `trait ModuleLoader { fn load(&mut self, name: &str, file: &File, params: &CStr, flags: ModuleInitFlags) -> nix::Result<()>; }`, `struct SysPaths<'a> { rootfs, osrelease, firmware_path_param, sys_module: &'a Path }`, `fn load_with(paths: &SysPaths, loader: &mut impl ModuleLoader) -> Vec<String>` (returns the listed modules that did not load).

- [ ] **Step 1: Write the failing tests** (bottom of `mod.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;
    use std::{collections::HashMap, path::PathBuf};
    use tempfile::TempDir;

    const RELEASE: &str = "6.6.23";
    const FW_PARAM_DEFAULT: &str = "";

    struct Fixture {
        _tmp: TempDir,
        rootfs: PathBuf,
        osrelease: PathBuf,
        fw_param: PathBuf,
        sys_module: PathBuf,
    }

    impl Fixture {
        /// `dep` is the content of modules.dep; every module named there gets an empty file.
        fn new(conf: &str, dep: &str, builtin: &str) -> Self {
            let tmp = TempDir::new().unwrap();
            let rootfs = tmp.path().join("rootfs");
            let conf_dir = rootfs.join(CONFIG_DIR);
            fs::create_dir_all(&conf_dir).unwrap();
            fs::write(conf_dir.join("test.conf"), conf).unwrap();
            let mod_dir = rootfs.join(MODULES_DIR).join(RELEASE);
            fs::create_dir_all(&mod_dir).unwrap();
            fs::write(mod_dir.join("modules.dep"), dep).unwrap();
            fs::write(mod_dir.join("modules.builtin"), builtin).unwrap();
            for path in dep.split_whitespace().map(|p| p.trim_end_matches(':')) {
                let file = mod_dir.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, b"").unwrap();
            }
            let osrelease = tmp.path().join("osrelease");
            fs::write(&osrelease, format!("{RELEASE}\n")).unwrap();
            let sys_module = tmp.path().join("sys/module");
            let fw_param = sys_module.join("firmware_class/parameters/path");
            fs::create_dir_all(fw_param.parent().unwrap()).unwrap();
            fs::write(&fw_param, FW_PARAM_DEFAULT).unwrap();
            Self { _tmp: tmp, rootfs, osrelease, fw_param, sys_module }
        }

        fn paths(&self) -> SysPaths<'_> {
            SysPaths {
                rootfs: &self.rootfs,
                osrelease: &self.osrelease,
                firmware_path_param: &self.fw_param,
                sys_module: &self.sys_module,
            }
        }

        fn mark_loaded(&self, name: &str) {
            let dir = self.sys_module.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("initstate"), "live\n").unwrap();
        }
    }

    /// Records each call and, on success, creates `/sys/module/<name>/initstate` as the kernel does.
    struct MockLoader {
        sys_module: PathBuf,
        calls: Vec<(String, String, ModuleInitFlags)>,
        fail: HashMap<&'static str, Errno>,
    }

    impl MockLoader {
        fn new(fx: &Fixture) -> Self {
            Self { sys_module: fx.sys_module.clone(), calls: Vec::new(), fail: HashMap::new() }
        }
        fn names(&self) -> Vec<&str> {
            self.calls.iter().map(|(n, _, _)| n.as_str()).collect()
        }
    }

    impl ModuleLoader for MockLoader {
        fn load(&mut self, name: &str, _file: &File, params: &CStr, flags: ModuleInitFlags) -> nix::Result<()> {
            self.calls.push((name.to_string(), params.to_str().unwrap().to_string(), flags));
            if let Some(e) = self.fail.get(name) {
                return Err(*e);
            }
            let dir = self.sys_module.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("initstate"), "live\n").unwrap();
            Ok(())
        }
    }

    const DEP: &str = "kernel/a/top.ko.xz: kernel/b/mid.ko.xz kernel/c/low.ko.xz\n\
                       kernel/b/mid.ko.xz: kernel/c/low.ko.xz\n\
                       kernel/c/low.ko.xz:\n\
                       kernel/d/plain.ko:\n\
                       kernel/e/other.ko.xz: kernel/c/low.ko.xz\n";

    #[test]
    fn loads_dependencies_in_reverse_order_then_module() {
        let fx = Fixture::new("top\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert_eq!(l.names(), ["low", "mid", "top"]);
    }

    #[test]
    fn writes_rootfs_firmware_path() {
        let fx = Fixture::new("plain\n", DEP, "");
        load_with(&fx.paths(), &mut MockLoader::new(&fx));
        assert_eq!(fs::read_to_string(&fx.fw_param).unwrap(), fx.rootfs.join(FIRMWARE_DIR).to_str().unwrap());
    }

    #[test]
    fn no_listed_module_leaves_firmware_path() {
        let fx = Fixture::new("# nothing\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert!(l.calls.is_empty());
        assert_eq!(fs::read_to_string(&fx.fw_param).unwrap(), FW_PARAM_DEFAULT);
    }

    #[test]
    fn builtin_or_loaded_is_skipped_also_as_dependency() {
        let fx = Fixture::new("top\nplain\n", DEP, "kernel/c/low.ko\n");
        fx.mark_loaded("mid");
        fx.mark_loaded("plain");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert_eq!(l.names(), ["top"]);
    }

    #[test]
    fn failed_dependency_skips_module() {
        let fx = Fixture::new("top\nplain\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        l.fail.insert("mid", Errno::EINVAL);
        assert_eq!(load_with(&fx.paths(), &mut l), ["top"]);
        assert_eq!(l.names(), ["low", "mid", "plain"]);
    }

    #[test]
    fn shared_dependency_loaded_once() {
        let fx = Fixture::new("top\nother\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert_eq!(l.names(), ["low", "mid", "top", "other"]);
    }

    #[test]
    fn failed_shared_dependency_not_retried() {
        let fx = Fixture::new("top\nother\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        l.fail.insert("low", Errno::ENOEXEC);
        assert_eq!(load_with(&fx.paths(), &mut l), ["top", "other"]);
        assert_eq!(l.names(), ["low"]);
    }

    #[test]
    fn listed_dependency_gets_its_parameters() {
        let fx = Fixture::new("top a=1\nlow b=2\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        load_with(&fx.paths(), &mut l);
        let params: Vec<_> = l.calls.iter().map(|(n, p, _)| (n.as_str(), p.as_str())).collect();
        assert_eq!(params, [("low", "b=2"), ("mid", ""), ("top", "a=1")]);
    }

    #[test]
    fn compressed_flag_only_for_compressed_files() {
        let fx = Fixture::new("plain\nlow\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        load_with(&fx.paths(), &mut l);
        assert_eq!(l.calls[0].2, ModuleInitFlags::empty());
        assert_eq!(l.calls[1].2.bits(), MODULE_INIT_COMPRESSED_FILE);
    }

    #[test]
    fn eexist_counts_as_loaded() {
        let fx = Fixture::new("plain\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        l.fail.insert("plain", Errno::EEXIST);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
    }

    #[test]
    fn unknown_module_and_missing_file_are_reported() {
        let fx = Fixture::new("nosuch\nplain\n", DEP, "");
        fs::remove_file(fx.rootfs.join(MODULES_DIR).join(RELEASE).join("kernel/d/plain.ko")).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["nosuch", "plain"]);
        assert!(l.calls.is_empty());
    }

    #[test]
    fn missing_modules_dep_skips_all_and_leaves_path() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::remove_file(fx.rootfs.join(MODULES_DIR).join(RELEASE).join("modules.dep")).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
        assert_eq!(fs::read_to_string(&fx.fw_param).unwrap(), FW_PARAM_DEFAULT);
    }

    #[test]
    fn unwritable_firmware_path_skips_all() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::remove_dir_all(fx.fw_param.parent().unwrap()).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
    }
}
```

- [ ] **Step 2: Run, expect compile failure**: `cargo test --features uboot,gpt,early-modules early_modules::tests`

- [ ] **Step 3: Implement** (`mod.rs`, replacing the skeleton)

```rust
//! Load the kernel modules listed in the rootfs before `switch_root`.

mod config;
mod moddep;

use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, c_uint},
    fs::{self, File},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use log::{info, warn};
use nix::{errno::Errno, kmod::ModuleInitFlags};

use config::ModuleEntry;
use moddep::{ModEntry, is_compressed, module_name, parse_modules_builtin, parse_modules_dep};

const CONFIG_DIR: &str = "etc/omnect/early-modules.d";
const MODULES_DIR: &str = "lib/modules";
const FIRMWARE_DIR: &str = "lib/firmware";
const OSRELEASE: &str = "/proc/sys/kernel/osrelease";
const FIRMWARE_PATH_PARAM: &str = "/sys/module/firmware_class/parameters/path";
const SYS_MODULE: &str = "/sys/module";

/// From `include/uapi/linux/module.h`; nix 0.29 has no constant for it.
const MODULE_INIT_COMPRESSED_FILE: c_uint = 4;

trait ModuleLoader {
    fn load(&mut self, name: &str, file: &File, params: &CStr, flags: ModuleInitFlags) -> nix::Result<()>;
}

struct KernelLoader;

impl ModuleLoader for KernelLoader {
    fn load(&mut self, _name: &str, file: &File, params: &CStr, flags: ModuleInitFlags) -> nix::Result<()> {
        nix::kmod::finit_module(file, params, flags)
    }
}

struct SysPaths<'a> {
    rootfs: &'a Path,
    osrelease: &'a Path,
    firmware_path_param: &'a Path,
    sys_module: &'a Path,
}

/// Best effort: every failure is logged, none is returned.
pub fn load(rootfs: &Path) {
    let paths = SysPaths {
        rootfs,
        osrelease: Path::new(OSRELEASE),
        firmware_path_param: Path::new(FIRMWARE_PATH_PARAM),
        sys_module: Path::new(SYS_MODULE),
    };
    load_with(&paths, &mut KernelLoader);
}

fn load_with(paths: &SysPaths, loader: &mut impl ModuleLoader) -> Vec<String> {
    let entries = config::read_config_dir(&paths.rootfs.join(CONFIG_DIR));
    if entries.is_empty() {
        return Vec::new();
    }
    let failed: Vec<String> = match Session::prepare(paths, &entries, loader) {
        Some(mut session) => entries
            .iter()
            .filter(|e| !session.load_listed(&e.name))
            .map(|e| e.name.clone())
            .collect(),
        None => entries.iter().map(|e| e.name.clone()).collect(),
    };
    if !failed.is_empty() {
        warn!("early modules: not loaded: {}", failed.join(", "));
    }
    failed
}

struct Session<'a, L> {
    loader: &'a mut L,
    sys_module: &'a Path,
    modules_dir: PathBuf,
    deps: HashMap<String, ModEntry>,
    builtin: HashSet<String>,
    params: HashMap<&'a str, &'a CStr>,
    failed: HashSet<String>,
}

impl<'a, L: ModuleLoader> Session<'a, L> {
    fn prepare(paths: &'a SysPaths, entries: &'a [ModuleEntry], loader: &'a mut L) -> Option<Self> {
        let release = read(paths.osrelease)?;
        let modules_dir = paths.rootfs.join(MODULES_DIR).join(release.trim());
        let deps = parse_modules_dep(&read(&modules_dir.join("modules.dep"))?);
        let builtin = parse_modules_builtin(&read(&modules_dir.join("modules.builtin"))?);

        // Not restored: after switch_root this path does not exist, so the
        // firmware loader skips it and uses its default paths.
        let firmware_dir = paths.rootfs.join(FIRMWARE_DIR);
        if let Err(e) = fs::write(paths.firmware_path_param, firmware_dir.as_os_str().as_bytes()) {
            warn!("early modules: cannot set firmware path: {e}");
            return None;
        }

        Some(Self {
            loader,
            sys_module: paths.sys_module,
            modules_dir,
            deps,
            builtin,
            params: entries.iter().map(|e| (e.name.as_str(), e.params.as_c_str())).collect(),
            failed: HashSet::new(),
        })
    }

    fn load_listed(&mut self, name: &str) -> bool {
        if self.is_present(name) {
            return true;
        }
        let Some(entry) = self.deps.get(name) else {
            warn!("early modules: {name} not in modules.dep");
            return false;
        };
        let path = entry.path.clone();
        for dep in entry.deps.clone().iter().rev() {
            let Some(dep_name) = module_name(dep) else {
                warn!("early modules: {name}: bad dependency {}", dep.display());
                return false;
            };
            if !self.load_one(&dep_name, dep) {
                warn!("early modules: {name} skipped, dependency {dep_name} not loaded");
                return false;
            }
        }
        self.load_one(name, &path)
    }

    fn load_one(&mut self, name: &str, rel_path: &Path) -> bool {
        if self.failed.contains(name) {
            return false;
        }
        if self.is_present(name) {
            return true;
        }
        let ok = self.finit(name, rel_path);
        if !ok {
            self.failed.insert(name.to_string());
        }
        ok
    }

    fn finit(&mut self, name: &str, rel_path: &Path) -> bool {
        let path = self.modules_dir.join(rel_path);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                warn!("early modules: cannot open {}: {e}", path.display());
                return false;
            }
        };
        let params = self.params.get(name).copied().unwrap_or(c"");
        let flags = if is_compressed(rel_path) {
            ModuleInitFlags::from_bits_retain(MODULE_INIT_COMPRESSED_FILE)
        } else {
            ModuleInitFlags::empty()
        };
        match self.loader.load(name, &file, params, flags) {
            Ok(()) => {
                info!("early modules: loaded {name}");
                true
            }
            Err(Errno::EEXIST) => {
                info!("early modules: {name} already loaded");
                true
            }
            Err(e) => {
                warn!("early modules: loading {name} failed: {e}");
                false
            }
        }
    }

    fn is_present(&self, name: &str) -> bool {
        let present = self.builtin.contains(name) || self.sys_module.join(name).join("initstate").exists();
        if present {
            info!("early modules: {name} built in or already loaded");
        }
        present
    }
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .inspect_err(|e| warn!("early modules: cannot read {}: {e}", path.display()))
        .ok()
}
```

Notes for the implementer:
- `entry.deps.clone()` avoids a borrow of `self.deps` while `load_one` takes `&mut self`. If clippy or review prefers, move `deps` out of `Session` and pass `&HashMap` to `load_listed` instead of cloning.
- `ModuleInitFlags::bits()` returns `c_uint`; the test compares with `MODULE_INIT_COMPRESSED_FILE` directly.
- Check that the sysfs write of `firmware_class.path` has no trailing newline problem: the kernel `param_set_copystring` strips one trailing newline, and none is written.

- [ ] **Step 4: Run tests, expect PASS**; run the full gate (incl. i686 and armv7 clippy, `cargo audit`).

- [ ] **Step 5: Commit**: `git commit -s -m "feat(early-modules): load listed modules from the rootfs with finit_module"`

---

### Task 4: Wire into run_init and document

**Files:**
- Modify: `src/lib.rs` (`run_init`, right after `let core_result = mount_core_partitions(...)`)
- Modify: `README.md`, `project-context.md`

**Interfaces:**
- Consumes: `early_modules::load(rootfs: &Path)`.

- [ ] **Step 1: Call site** in `run_init`:

```rust
    let core_result = mount_core_partitions(&layout, rootfs, &mut ods_status);

    #[cfg(feature = "early-modules")]
    if core_result.is_ok() {
        early_modules::load(rootfs);
    }
```

Before `open_boot_env`, outside the `skip_init_setup` block, so it runs in every boot mode.

- [ ] **Step 2: README**
  - Features table, after `dos` or at the end before `rtc` (keep the table order used there): ``| `early-modules` | Load the modules listed in `/etc/omnect/early-modules.d/*.conf` of the rootfs before `switch_root` | Implemented |``
  - Startup Flow mermaid: replace `CORE --> BENV["open_boot_env()"]` with
    `CORE -->|"OK + early-modules"| EMOD["early_modules::load()\nbest effort"]`, `EMOD --> BENV["open_boot_env()"]`, `CORE -->|"Fail or no early-modules"| BENV`.
  - Runtime Dependencies: add a short sentence plus a row, since the table is about the initramfs image: ``| rootfs: `/etc/omnect/early-modules.d/*.conf`, `/lib/modules/<release>/modules.dep`, `modules.builtin`, the listed modules and their firmware | `early-modules` | image recipe, kernel |``, followed by one line: "A module listed there gets its parameters only from its config line; `modprobe.d` options do not apply. A line whose first non-blank character is `#` is a comment."
  - Testing: add `cargo test --features uboot,gpt,early-modules,test-utils` and `cargo test --features grub,gpt,early-modules,test-utils`.
- [ ] **Step 3: project-context.md**: in the module tree add `├── early_modules/        # Load modules listed in the rootfs before switch_root (feature = early-modules)`; add the two test lines to the test command list.
- [ ] **Step 4: Gate** (all of Global Constraints), plus the existing README combinations (no regression without the feature).
- [ ] **Step 5: Read every new text line once more with the reader test** (CLAUDE.md "Comments & written communication").
- [ ] **Step 6: Commit**: `git commit -s -m "feat(early-modules): load modules after the rootfs mount"`

---

### Task 5: meta-omnect integration (PR #697, branch `initramfs-integration`)

Work on the meta-omnect branch `initramfs-integration`. The legacy `90-imx_sdma` script is not touched: the CI test initramfs stays on the legacy scripts.

**Files:**
- Modify: `recipes-omnect/omnect-os-init/omnect-os-init.inc`
- Create: `recipes-omnect/omnect-base-files/omnect-base-files/etc/omnect/early-modules.d/imx-sdma.conf`
- Modify: `recipes-omnect/omnect-base-files/omnect-base-files.bb`
- Create: `recipes-kernel/linux/files/module-decompress.cfg`
- Modify: `conf/distro/include/omnect-os-kernel.conf`
- Modify: the omnect-os-init recipe SRCREV, once the omnect-os-init branch is pushed

- [ ] **Step 1: Feature** — in `omnect-os-init.inc` next to the other appends:

```
CARGO_FEATURES:append:mx8mm-nxp-bsp = ",early-modules"
```

- [ ] **Step 2: Config file** `imx-sdma.conf`:

```
imx_sdma
```

`omnect-base-files.bb`:

```
SRC_URI:append:mx8mm-nxp-bsp = " file://etc/omnect/early-modules.d/imx-sdma.conf"
FILES:${PN}:append:mx8mm-nxp-bsp = " /etc/omnect/early-modules.d/imx-sdma.conf"

do_install:append:mx8mm-nxp-bsp() {
    install -m 0644 -D ${UNPACKDIR}/etc/omnect/early-modules.d/imx-sdma.conf ${D}/etc/omnect/early-modules.d/imx-sdma.conf
}
```

The recipe already deploys machine-specific data (`APPEND`), so a machine override fits there.

- [ ] **Step 3: rpi kernel** — `module-decompress.cfg`:

```
CONFIG_MODULE_DECOMPRESS=y
```

`omnect-os-kernel.conf`:

```
SRC_URI:append:pn-linux-raspberrypi = " file://module-decompress.cfg"
```

- [ ] **Step 4: SRCREV** — after the omnect-os-init branch is pushed, point the recipe at its head commit (the recipe is not regenerated with `cargo bitbake` before a release tag exists; follow how #697 pins the current flash-mode-3 commit).
- [ ] **Step 5: Build check** — `bitbake -e omnect-os-init | grep ^CARGO_FEATURES=` for a phytec i.MX8MM machine shows `early-modules`; for rpi4 it does not. `bitbake -c kernel_configcheck linux-raspberrypi` (or check `.config`) shows `CONFIG_MODULE_DECOMPRESS=y`.
- [ ] **Step 6: Commit** (`Signed-off-by` line); push only when the user says so.

---

## Verification

Automated (each task): the Global Constraints gate.

Manual on hardware — **ask the user before any test run on a device**:

- Tauri-L image: `dmesg` shows `imx-sdma … loaded firmware` and the TPM on `spi0.1` before systemd starts; `cat /sys/module/firmware_class/parameters/path` shows `/rootfs/lib/firmware`.
- Same image with `imx-sdma.ko*` removed from the rootfs: boots normally, logs `early modules: not loaded: imx_sdma`.
- rpi4 image: `cargo build` without the feature, so no loader code; `zcat /proc/config.gz | grep MODULE_DECOMPRESS` shows `=y`.

## Out of scope / notes

- The PR for omnect-os-init targets `main`; until #32 is merged it also shows the flash-mode-3 commits. Creating PRs and pushing only on the user's request.
- Customer modules on a device (spec §3 follow-up) are not part of this plan.
