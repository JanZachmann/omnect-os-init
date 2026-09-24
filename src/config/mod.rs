//! Configuration module for omnect-os-init
//!
//! Provides a unified `Config` struct loaded once at startup and passed
//! explicitly through the init pipeline. Build-time constants generated from
//! Yocto environment variables are available via the `build` submodule.

use std::collections::HashMap;
use std::fs;

use crate::error::ConfigError;

const OS_RELEASE_PATH: &str = "/etc/os-release";
const MACHINE_FEATURES_PREFIX: &str = "MACHINE_FEATURES=";

/// Build-time constants generated from Yocto environment variables by build.rs.
pub mod build {
    include!(concat!(env!("OUT_DIR"), "/build_config.rs"));
}

/// Parsed kernel command line parameters.
///
/// Handles both `key=value` pairs and bare flags (e.g. `quiet`, `ro`).
/// Bare flags are stored with an empty string value so `get("quiet")` returns
/// `Some("")` when the flag is present.
#[derive(Debug, Clone, Default)]
pub struct CmdlineConfig {
    params: HashMap<String, String>,
}

impl CmdlineConfig {
    /// Load from `/proc/cmdline`.
    pub fn load() -> crate::Result<Self> {
        let raw = fs::read_to_string("/proc/cmdline").map_err(ConfigError::CmdlineReadFailed)?;
        Ok(Self::parse(&raw))
    }

    /// Parses a raw cmdline string; also usable directly in tests.
    ///
    /// Handles `key=value` pairs and bare flags (e.g. `quiet`, `ro`). Values
    /// containing spaces are not supported — the kernel splits the cmdline on
    /// whitespace, so quoted values with spaces arrive as separate tokens.
    /// Bare flags are stored with an empty string value.
    pub fn parse(cmdline: &str) -> Self {
        let mut params = HashMap::new();
        for token in cmdline.split_whitespace() {
            if let Some((key, value)) = token.split_once('=') {
                // The omnect kernel cmdline convention never uses single-quoted values.
                // This strip is purely defensive against the double-quoted root="..."
                // style that some bootloaders emit.
                params.insert(key.to_string(), value.trim_matches('"').to_string());
            } else {
                params.insert(token.to_string(), String::new());
            }
        }
        Self { params }
    }

    /// Get a parameter value by key. Returns `None` if the key is absent.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }
}

/// Unified runtime configuration, loaded once during early init and passed
/// explicitly to all init sub-systems.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Parsed kernel command line.
    pub cmdline: CmdlineConfig,
    /// Space-separated `MACHINE_FEATURES` value from the initramfs-local
    /// `/etc/os-release`. Empty when the file, key, or value is unusable —
    /// the same safe default as a machine that declares no features.
    pub machine_features: String,
}

impl Config {
    /// Load configuration from the running kernel environment.
    ///
    /// Reads `/proc/cmdline` and evaluates compile-time feature flags.
    pub fn load() -> crate::Result<Self> {
        let cmdline = CmdlineConfig::load()?;
        let machine_features = load_machine_features();
        Ok(Self {
            cmdline,
            machine_features,
        })
    }
}

/// Read `MACHINE_FEATURES` from the **initramfs-local** `/etc/os-release`,
/// not `/sysroot/etc/os-release` — a flash mode unmounts `/sysroot` before
/// this value is needed. A file that cannot be read yields an empty string.
fn load_machine_features() -> String {
    fs::read_to_string(OS_RELEASE_PATH)
        .map(|contents| parse_machine_features(&contents))
        .unwrap_or_default()
}

/// Parses `MACHINE_FEATURES="<space-separated features>"` out of the
/// contents of an `/etc/os-release`-shaped file.
///
/// A missing key or a line without a quoted value yields an empty string.
pub fn parse_machine_features(os_release: &str) -> String {
    os_release
        .lines()
        .find(|line| line.starts_with(MACHINE_FEATURES_PREFIX))
        .and_then(|line| line.split('"').nth(1))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmdline_parse_key_value() {
        let cfg = CmdlineConfig::parse("rootpart=2 bootpart_fsuuid=1234-ABCD ro quiet");
        assert_eq!(cfg.get("rootpart"), Some("2"));
        assert_eq!(cfg.get("bootpart_fsuuid"), Some("1234-ABCD"));
    }

    #[test]
    fn test_cmdline_parse_bare_flags() {
        let cfg = CmdlineConfig::parse("rootpart=2 ro quiet");
        assert_eq!(cfg.get("ro"), Some(""));
    }

    #[test]
    fn test_cmdline_parse_missing_key() {
        let cfg = CmdlineConfig::parse("ro quiet");
        assert_eq!(cfg.get("rootpart"), None);
        assert_eq!(cfg.get("bootpart_fsuuid"), None);
    }

    #[test]
    fn test_cmdline_parse_quoted_value() {
        let cfg = CmdlineConfig::parse(r#"root="/dev/mmcblk1p2" ro"#);
        assert_eq!(cfg.get("root"), Some("/dev/mmcblk1p2"));
    }

    #[test]
    fn test_cmdline_default_is_empty() {
        let cfg = CmdlineConfig::default();
        assert_eq!(cfg.get("rootpart"), None);
    }

    #[test]
    fn test_cmdline_duplicate_key_last_wins() {
        // HashMap::insert overwrites; the last occurrence of a key wins.
        // This test pins that contract so a refactor to first-wins is caught.
        let cfg = CmdlineConfig::parse("rootpart=2 rootpart=3");
        assert_eq!(cfg.get("rootpart"), Some("3"));
    }

    #[test]
    fn machine_features_parses_the_quoted_value() {
        let os_release = "ID=omnect\nMACHINE_FEATURES=\"efi usbhost vfat\"\nVERSION=1\n";
        assert_eq!(parse_machine_features(os_release), "efi usbhost vfat");
    }

    #[test]
    fn machine_features_missing_key_is_empty() {
        let os_release = "ID=omnect\nVERSION=1\n";
        assert_eq!(parse_machine_features(os_release), "");
    }

    #[test]
    fn machine_features_unquoted_value_is_empty() {
        let os_release = "MACHINE_FEATURES=efi usbhost\n";
        assert_eq!(parse_machine_features(os_release), "");
    }

    #[test]
    fn machine_features_empty_file_is_empty() {
        assert_eq!(parse_machine_features(""), "");
    }
}
