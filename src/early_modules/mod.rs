//! Load the kernel modules listed in the rootfs before `switch_root`.

mod config;
mod moddep;

use std::{
    collections::{HashMap, HashSet},
    ffi::CStr,
    fs::{self, File},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use log::{error, info, warn};
use nix::{errno::Errno, kmod::ModuleInitFlags};

use crate::early_modules::{
    config::ModuleEntry,
    moddep::{ModEntry, is_compressed, module_name, parse_modules_builtin, parse_modules_dep},
};

const CONFIG_DIR: &str = "etc/omnect/early-modules.d";
const MODULES_DIR: &str = "lib/modules";
const MODULES_DEP: &str = "modules.dep";
const MODULES_BUILTIN: &str = "modules.builtin";
const FIRMWARE_DIR: &str = "lib/firmware";
const OSRELEASE: &str = "/proc/sys/kernel/osrelease";
const FIRMWARE_PATH_PARAM: &str = "/sys/module/firmware_class/parameters/path";
const SYS_MODULE: &str = "/sys/module";
const INITSTATE: &str = "initstate";

/// `MODULE_INIT_COMPRESSED_FILE` from `include/uapi/linux/module.h`; nix has
/// no constant for it.
const COMPRESSED_FILE: ModuleInitFlags = ModuleInitFlags::from_bits_retain(4);

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
fn load_with(paths: &SysPaths, loader: &mut dyn ModuleLoader) -> Vec<String> {
    let entries = config::read_config_dir(&paths.rootfs.join(CONFIG_DIR));
    if entries.is_empty() {
        return Vec::new();
    }
    let failed: Vec<String> = match Resolved::prepare(paths, &entries) {
        Some(resolved) => {
            let mut session = Session {
                resolved: &resolved,
                loader,
                outcome: HashMap::new(),
            };
            entries
                .iter()
                .filter(|e| !session.load_listed(&e.name))
                .map(|e| e.name.clone())
                .collect()
        }
        None => entries
            .iter()
            .filter(|e| !is_loaded(paths.sys_module, &e.name))
            .map(|e| e.name.clone())
            .collect(),
    };
    if !failed.is_empty() {
        error!("early modules: not loaded: {}", failed.join(", "));
    }
    failed
}

struct Resolved<'a> {
    rootfs: &'a Path,
    sys_module: &'a Path,
    modules_dir: PathBuf,
    builtin: HashSet<String>,
    deps: HashMap<String, ModEntry>,
    params: HashMap<&'a str, &'a CStr>,
}

impl<'a> Resolved<'a> {
    fn prepare(paths: &SysPaths<'a>, entries: &'a [ModuleEntry]) -> Option<Self> {
        let release = read(paths.osrelease)?;
        let release = release.trim();
        if release.is_empty() {
            warn!("early modules: {} is empty", paths.osrelease.display());
            return None;
        }
        let modules_dir = paths.rootfs.join(MODULES_DIR).join(release);
        let listed = entries.iter().map(|e| e.name.as_str()).collect();
        let deps = parse_modules_dep(&read(&modules_dir.join(MODULES_DEP))?, &listed);
        let builtin = parse_modules_builtin(&read(&modules_dir.join(MODULES_BUILTIN))?);

        // Not restored: after switch_root the path does not exist, so the
        // firmware loader skips it and uses its default paths.
        let firmware_dir = paths.rootfs.join(FIRMWARE_DIR);
        if let Err(e) = fs::write(
            paths.firmware_path_param,
            firmware_dir.as_os_str().as_bytes(),
        ) {
            warn!(
                "early modules: cannot write {} to {}: {e}",
                firmware_dir.display(),
                paths.firmware_path_param.display()
            );
            return None;
        }

        Some(Self {
            rootfs: paths.rootfs,
            sys_module: paths.sys_module,
            modules_dir,
            builtin,
            deps,
            params: entries
                .iter()
                .map(|e| (e.name.as_str(), e.params.as_c_str()))
                .collect(),
        })
    }
}

struct Session<'a> {
    resolved: &'a Resolved<'a>,
    loader: &'a mut dyn ModuleLoader,
    /// Whether each module seen so far is present.
    outcome: HashMap<String, bool>,
}

impl Session<'_> {
    fn load_listed(&mut self, name: &str) -> bool {
        if let Some(&ok) = self.outcome.get(name) {
            if !ok {
                warn!("early modules: {name} not retried, it failed as a dependency");
            }
            return ok;
        }
        if self.is_present(name) {
            self.outcome.insert(name.to_string(), true);
            return true;
        }
        let resolved = self.resolved;
        let Some(entry) = resolved.deps.get(name) else {
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

    fn load_one(&mut self, name: &str, dep_path: &Path) -> bool {
        if let Some(&ok) = self.outcome.get(name) {
            return ok;
        }
        let ok = self.is_present(name) || self.finit(name, dep_path);
        self.outcome.insert(name.to_string(), ok);
        ok
    }

    fn finit(&mut self, name: &str, dep_path: &Path) -> bool {
        // An absolute path in modules.dep is taken relative to the rootfs, as
        // kmod takes it relative to its root.
        let path = match dep_path.strip_prefix("/") {
            Ok(from_root) => self.resolved.rootfs.join(from_root),
            Err(_) => self.resolved.modules_dir.join(dep_path),
        };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                warn!("early modules: cannot open {}: {e}", path.display());
                return false;
            }
        };
        let params = self.resolved.params.get(name).copied().unwrap_or(c"");
        let compressed = is_compressed(dep_path);
        let flags = if compressed {
            COMPRESSED_FILE
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
                let kind = if compressed {
                    "compressed"
                } else {
                    "uncompressed"
                };
                warn!(
                    "early modules: loading {name} failed: {e} ({}, {kind})",
                    path.display()
                );
                false
            }
        }
    }

    fn is_present(&self, name: &str) -> bool {
        let resolved = self.resolved;
        if !resolved.builtin.contains(name) && !is_loaded(resolved.sys_module, name) {
            return false;
        }
        if resolved.params.get(name).is_some_and(|p| !p.is_empty()) {
            warn!("early modules: {name} built in or already loaded, parameters ignored");
        } else {
            info!("early modules: {name} built in or already loaded");
        }
        true
    }
}

fn is_loaded(sys_module: &Path, name: &str) -> bool {
    sys_module.join(name).join(INITSTATE).exists()
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .inspect_err(|e| warn!("early modules: cannot read {}: {e}", path.display()))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const RELEASE: &str = "6.6.23";
    const FW_PARAM_DEFAULT: &str = "";

    const DEP: &str = "kernel/a/top.ko.xz: kernel/b/mid.ko.xz kernel/c/low.ko.xz\n\
                       kernel/b/mid.ko.xz: kernel/c/low.ko.xz\n\
                       kernel/c/low.ko.xz:\n\
                       kernel/d/plain.ko:\n\
                       kernel/e/other.ko.xz: kernel/c/low.ko.xz\n\
                       kernel/dma/imx-sdma.ko.xz:\n";

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
            fs::write(mod_dir.join(MODULES_DEP), dep).unwrap();
            fs::write(mod_dir.join(MODULES_BUILTIN), builtin).unwrap();
            for path in dep.split_whitespace().map(|p| p.trim_end_matches(':')) {
                let file = match path.strip_prefix('/') {
                    Some(from_root) => rootfs.join(from_root),
                    None => mod_dir.join(path),
                };
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
        fs::write(dir.join(INITSTATE), "live\n").unwrap();
    }

    /// On success it creates `initstate` in sysfs, as the kernel does. Each
    /// call records the firmware path the kernel would search at that time.
    struct MockLoader {
        sys_module: PathBuf,
        fw_param: PathBuf,
        calls: Vec<(String, String, ModuleInitFlags)>,
        fw_paths: Vec<String>,
        fail: HashMap<&'static str, Errno>,
    }

    impl MockLoader {
        fn new(fx: &Fixture) -> Self {
            Self {
                sys_module: fx.sys_module.clone(),
                fw_param: fx.fw_param.clone(),
                calls: Vec::new(),
                fw_paths: Vec::new(),
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
            self.fw_paths
                .push(fs::read_to_string(&self.fw_param).unwrap());
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
    fn writes_rootfs_firmware_path_before_the_first_load() {
        let fx = Fixture::new("top\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        load_with(&fx.paths(), &mut l);
        let fw_dir = fx.rootfs.join(FIRMWARE_DIR);
        let fw_dir = fw_dir.to_str().unwrap();
        assert_eq!(l.fw_paths, [fw_dir; 3]);
        assert_eq!(fx.fw_param(), fw_dir);
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
    fn listed_builtin_or_loaded_module_needs_no_modules_dep_line() {
        let fx = Fixture::new("inside\nlive\n", DEP, "kernel/x/inside.ko\n");
        fx.mark_loaded("live");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert!(l.calls.is_empty());
    }

    #[test]
    fn failed_listed_module_is_reported() {
        let fx = Fixture::new("plain\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        l.fail.insert("plain", Errno::ENOEXEC);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert_eq!(l.names(), ["plain"]);
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
        let flags: HashMap<_, _> = l.calls.iter().map(|(n, _, f)| (n.as_str(), *f)).collect();
        assert_eq!(flags["plain"], ModuleInitFlags::empty());
        assert_eq!(flags["low"], COMPRESSED_FILE);
    }

    #[test]
    fn dash_in_file_name_matches_underscore_in_config() {
        let fx = Fixture::new("imx_sdma p=1\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        let params: Vec<_> = l
            .calls
            .iter()
            .map(|(n, p, _)| (n.as_str(), p.as_str()))
            .collect();
        assert_eq!(params, [("imx_sdma", "p=1")]);
    }

    #[test]
    fn loaded_module_with_dash_in_file_name_is_skipped() {
        let fx = Fixture::new("imx_sdma\n", DEP, "");
        fx.mark_loaded("imx_sdma");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert!(l.calls.is_empty());
    }

    #[test]
    fn listed_module_that_failed_as_dependency_is_not_retried() {
        let fx = Fixture::new("top\nlow\n", DEP, "");
        let mut l = MockLoader::new(&fx);
        l.fail.insert("low", Errno::ENOEXEC);
        assert_eq!(load_with(&fx.paths(), &mut l), ["top", "low"]);
        assert_eq!(l.names(), ["low"]);
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
        fs::remove_file(fx.module_file(MODULES_DEP)).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
        assert_eq!(fx.fw_param(), FW_PARAM_DEFAULT);
    }

    #[test]
    fn failed_prepare_reports_only_modules_not_loaded() {
        let fx = Fixture::new("plain\nlive\n", DEP, "");
        fs::remove_file(fx.module_file(MODULES_DEP)).unwrap();
        fx.mark_loaded("live");
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
    }

    #[test]
    fn missing_modules_builtin_skips_all() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::remove_file(fx.module_file(MODULES_BUILTIN)).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
        assert_eq!(fx.fw_param(), FW_PARAM_DEFAULT);
    }

    #[test]
    fn missing_osrelease_skips_all() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::remove_file(&fx.osrelease).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
        assert_eq!(fx.fw_param(), FW_PARAM_DEFAULT);
    }

    #[test]
    fn empty_osrelease_skips_all() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::write(&fx.osrelease, "\n").unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
        assert_eq!(fx.fw_param(), FW_PARAM_DEFAULT);
    }

    #[test]
    fn bad_dependency_path_skips_module() {
        let fx = Fixture::new("top\n", "kernel/a/top.ko: kernel/b/mid.bin\n", "");
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["top"]);
        assert!(l.calls.is_empty());
    }

    #[test]
    fn absolute_dep_path_starts_at_rootfs() {
        let fx = Fixture::new("top\n", "/opt/mods/top.ko: /opt/mods/low.ko\n", "");
        let mut l = MockLoader::new(&fx);
        assert!(load_with(&fx.paths(), &mut l).is_empty());
        assert_eq!(l.names(), ["low", "top"]);
    }

    #[test]
    fn unwritable_firmware_path_skips_all() {
        let fx = Fixture::new("plain\n", DEP, "");
        fs::remove_dir_all(fx.fw_param.parent().unwrap()).unwrap();
        let mut l = MockLoader::new(&fx);
        assert_eq!(load_with(&fx.paths(), &mut l), ["plain"]);
        assert!(l.calls.is_empty());
    }

    fn capture_log(run: impl FnOnce()) -> Vec<String> {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();
        crate::logging::capture::start_capture();
        run();
        crate::logging::capture::take_capture()
    }

    fn assert_logged(lines: &[String], line: &str) {
        assert!(lines.iter().any(|l| l == line), "{line:?} not in {lines:?}");
    }

    #[test]
    fn logs_each_reason_and_one_error_line() {
        let fx = Fixture::new("nosuch\nplain\ntop\n", DEP, "");
        let plain = fx.module_file("kernel/d/plain.ko");
        fs::remove_file(&plain).unwrap();
        let mut l = MockLoader::new(&fx);
        l.fail.insert("mid", Errno::EINVAL);
        let lines = capture_log(|| {
            load_with(&fx.paths(), &mut l);
        });
        let mid = fx.module_file("kernel/b/mid.ko.xz");
        assert_logged(&lines, "[WARN] early modules: nosuch not in modules.dep");
        assert_logged(
            &lines,
            &format!(
                "[WARN] early modules: cannot open {}: No such file or directory (os error 2)",
                plain.display()
            ),
        );
        assert_logged(
            &lines,
            &format!(
                "[WARN] early modules: loading mid failed: EINVAL: Invalid argument ({}, compressed)",
                mid.display()
            ),
        );
        assert_logged(
            &lines,
            "[WARN] early modules: top skipped, dependency mid not loaded",
        );
        assert_logged(
            &lines,
            "[ERROR] early modules: not loaded: nosuch, plain, top",
        );
    }

    #[test]
    fn logs_ignored_parameters_of_a_builtin_module() {
        let fx = Fixture::new("inside_p p=1\n", DEP, "kernel/x/inside_p.ko\n");
        let mut l = MockLoader::new(&fx);
        let lines = capture_log(|| {
            load_with(&fx.paths(), &mut l);
        });
        assert_logged(
            &lines,
            "[WARN] early modules: inside_p built in or already loaded, parameters ignored",
        );
    }
}
