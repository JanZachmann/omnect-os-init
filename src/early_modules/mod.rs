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

/// From `include/uapi/linux/module.h`; nix has no constant for it.
const MODULE_INIT_COMPRESSED_FILE: c_uint = 4;

trait ModuleLoader {
    fn load(
        &mut self,
        name: &str,
        file: &File,
        params: &CStr,
        flags: ModuleInitFlags,
    ) -> nix::Result<()>;
}

struct KernelLoader;

impl ModuleLoader for KernelLoader {
    fn load(
        &mut self,
        _name: &str,
        file: &File,
        params: &CStr,
        flags: ModuleInitFlags,
    ) -> nix::Result<()> {
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

/// Returns the listed modules that did not load.
fn load_with(paths: &SysPaths, loader: &mut impl ModuleLoader) -> Vec<String> {
    let entries = config::read_config_dir(&paths.rootfs.join(CONFIG_DIR));
    if entries.is_empty() {
        return Vec::new();
    }
    let failed: Vec<String> = match Session::prepare(paths, &entries, loader) {
        Some((mut session, deps)) => entries
            .iter()
            .filter(|e| !session.load_listed(&deps, &e.name))
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
    builtin: HashSet<String>,
    params: HashMap<&'a str, &'a CStr>,
    failed: HashSet<String>,
}

impl<'a, L: ModuleLoader> Session<'a, L> {
    fn prepare(
        paths: &'a SysPaths,
        entries: &'a [ModuleEntry],
        loader: &'a mut L,
    ) -> Option<(Self, HashMap<String, ModEntry>)> {
        let release = read(paths.osrelease)?;
        let modules_dir = paths.rootfs.join(MODULES_DIR).join(release.trim());
        let deps = parse_modules_dep(&read(&modules_dir.join("modules.dep"))?);
        let builtin = parse_modules_builtin(&read(&modules_dir.join("modules.builtin"))?);

        // Not restored: after switch_root the path does not exist, so the
        // firmware loader skips it and uses its default paths.
        let firmware_dir = paths.rootfs.join(FIRMWARE_DIR);
        if let Err(e) = fs::write(
            paths.firmware_path_param,
            firmware_dir.as_os_str().as_bytes(),
        ) {
            warn!("early modules: cannot set firmware path: {e}");
            return None;
        }

        let session = Self {
            loader,
            sys_module: paths.sys_module,
            modules_dir,
            builtin,
            params: entries
                .iter()
                .map(|e| (e.name.as_str(), e.params.as_c_str()))
                .collect(),
            failed: HashSet::new(),
        };
        Some((session, deps))
    }

    fn load_listed(&mut self, deps: &HashMap<String, ModEntry>, name: &str) -> bool {
        if self.is_present(name) {
            return true;
        }
        let Some(entry) = deps.get(name) else {
            warn!("early modules: {name} not in modules.dep");
            return false;
        };
        for dep in entry.deps.iter().rev() {
            let Some(dep_name) = module_name(dep) else {
                warn!("early modules: {name}: bad dependency {}", dep.display());
                return false;
            };
            if !self.load_one(&dep_name, dep) {
                warn!("early modules: {name} skipped, dependency {dep_name} not loaded");
                return false;
            }
        }
        self.load_one(name, &entry.path)
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
        let present =
            self.builtin.contains(name) || self.sys_module.join(name).join("initstate").exists();
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

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;
    use std::{collections::HashMap, fs, path::PathBuf};
    use tempfile::TempDir;

    const RELEASE: &str = "6.6.23";
    const FW_PARAM_DEFAULT: &str = "";

    const DEP: &str = "kernel/a/top.ko.xz: kernel/b/mid.ko.xz kernel/c/low.ko.xz\n\
                       kernel/b/mid.ko.xz: kernel/c/low.ko.xz\n\
                       kernel/c/low.ko.xz:\n\
                       kernel/d/plain.ko:\n\
                       kernel/e/other.ko.xz: kernel/c/low.ko.xz\n";

    struct Fixture {
        _tmp: TempDir,
        rootfs: PathBuf,
        osrelease: PathBuf,
        fw_param: PathBuf,
        sys_module: PathBuf,
    }

    impl Fixture {
        /// Every module named in `dep` gets an empty file in the rootfs.
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
            Self {
                _tmp: tmp,
                rootfs,
                osrelease,
                fw_param,
                sys_module,
            }
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
            mark_loaded(&self.sys_module, name);
        }

        fn module_file(&self, rel: &str) -> PathBuf {
            self.rootfs.join(MODULES_DIR).join(RELEASE).join(rel)
        }

        fn fw_param(&self) -> String {
            fs::read_to_string(&self.fw_param).unwrap()
        }
    }

    fn mark_loaded(sys_module: &Path, name: &str) {
        let dir = sys_module.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("initstate"), "live\n").unwrap();
    }

    /// On success it creates `initstate` in sysfs, as the kernel does.
    struct MockLoader {
        sys_module: PathBuf,
        calls: Vec<(String, String, ModuleInitFlags)>,
        fail: HashMap<&'static str, Errno>,
    }

    impl MockLoader {
        fn new(fx: &Fixture) -> Self {
            Self {
                sys_module: fx.sys_module.clone(),
                calls: Vec::new(),
                fail: HashMap::new(),
            }
        }

        fn names(&self) -> Vec<&str> {
            self.calls.iter().map(|(n, _, _)| n.as_str()).collect()
        }
    }

    impl ModuleLoader for MockLoader {
        fn load(
            &mut self,
            name: &str,
            _file: &File,
            params: &CStr,
            flags: ModuleInitFlags,
        ) -> nix::Result<()> {
            self.calls.push((
                name.to_string(),
                params.to_str().unwrap().to_string(),
                flags,
            ));
            if let Some(e) = self.fail.get(name) {
                return Err(*e);
            }
            mark_loaded(&self.sys_module, name);
            Ok(())
        }
    }

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
        assert_eq!(
            fx.fw_param(),
            fx.rootfs.join(FIRMWARE_DIR).to_str().unwrap()
        );
    }

    #[test]
    fn no_listed_module_leaves_firmware_path() {
        let fx = Fixture::new("# nothing\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert!(l.calls.is_empty());
        assert_eq!(fx.fw_param(), FW_PARAM_DEFAULT);
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
        let params: Vec<_> = l
            .calls
            .iter()
            .map(|(n, p, _)| (n.as_str(), p.as_str()))
            .collect();
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
        fs::remove_file(fx.module_file("kernel/d/plain.ko")).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["nosuch", "plain"]);
        assert!(l.calls.is_empty());
    }

    #[test]
    fn missing_modules_dep_skips_all_and_leaves_path() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::remove_file(fx.module_file("modules.dep")).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
        assert_eq!(fx.fw_param(), FW_PARAM_DEFAULT);
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
