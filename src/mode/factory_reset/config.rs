use std::path::Path;

use serde_json::Value;

use crate::error::{FactoryResetError, Result};

pub(crate) const FACTORY_RESET_CONFIG_FILE: &str = "etc/omnect/factory-reset.json";
const FACTORY_RESET_CONFIG_DIR: &str = "etc/omnect/factory-reset.d";
const PRESERVE_LIST_MANDATORY: &str = "/etc/omnect/factory-reset.d/";
const KEY_APPLICATIONS: &str = "applications";
const KEY_PATHS: &str = "paths";
const KEY_MODE: &str = "mode";
const KEY_PRESERVE: &str = "preserve";

/// Validated factory-reset mode. A value outside the supported range is
/// rejected when the trigger is parsed, so an unsupported mode never reaches
/// the reset sequence. The discriminant is the on-wire mode number.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Mode1 = 1,
    Mode2 = 2,
    Mode3 = 3,
}

impl TryFrom<u32> for ResetMode {
    type Error = String;
    fn try_from(value: u32) -> std::result::Result<Self, Self::Error> {
        match value {
            v if v == ResetMode::Mode1 as u32 => Ok(ResetMode::Mode1),
            v if v == ResetMode::Mode2 as u32 => Ok(ResetMode::Mode2),
            v if v == ResetMode::Mode3 as u32 => Ok(ResetMode::Mode3),
            _ => Err(format!("factory reset mode {value} is not supported")),
        }
    }
}

#[derive(Debug)]
pub struct FactoryResetConfig {
    pub mode: ResetMode,
    pub preserve: Vec<String>,
}

impl FactoryResetConfig {
    /// Parse the trigger value read from the boot environment.
    ///
    /// `mode` problems and `preserve` problems use different error variants
    /// because they report different statuses; unparsable json counts as a
    /// `mode` problem, since then there is no mode either. `preserve` is
    /// mandatory; an empty array asks to keep nothing beyond the mandatory
    /// path.
    pub fn parse(json: &str) -> std::result::Result<Self, FactoryResetError> {
        let trigger: Value = serde_json::from_str(json).map_err(|e| {
            FactoryResetError::InvalidConfig(format!("Failed to parse factory-reset JSON: {e}"))
        })?;

        let mode = match trigger.get(KEY_MODE) {
            None => Err(format!("the trigger has no '{KEY_MODE}'")),
            Some(value) => value
                .as_u64()
                .and_then(|mode| u32::try_from(mode).ok())
                .ok_or_else(|| format!("'{KEY_MODE}' is not a supported mode number: {value}")),
        }
        .map_err(FactoryResetError::InvalidConfig)?;
        let mode = ResetMode::try_from(mode).map_err(FactoryResetError::InvalidConfig)?;

        let preserve = string_array(&trigger, KEY_PRESERVE).map_err(|e| match e {
            NotAStringArray::Missing => {
                FactoryResetError::MissingField(format!("trigger has no '{KEY_PRESERVE}' key"))
            }
            NotAStringArray::NotAnArray => {
                FactoryResetError::InvalidPreserve(format!("'{KEY_PRESERVE}' must be an array"))
            }
            NotAStringArray::NotOnlyStrings => FactoryResetError::InvalidPreserve(format!(
                "'{KEY_PRESERVE}' must contain only strings"
            )),
        })?;

        Ok(Self {
            mode,
            preserve: preserve.into_iter().map(str::to_string).collect(),
        })
    }
}

/// Why a json value did not yield a list of strings.
enum NotAStringArray {
    Missing,
    NotAnArray,
    NotOnlyStrings,
}

fn string_array<'a>(
    value: &'a Value,
    key: &str,
) -> std::result::Result<Vec<&'a str>, NotAStringArray> {
    let array = value
        .get(key)
        .ok_or(NotAStringArray::Missing)?
        .as_array()
        .ok_or(NotAStringArray::NotAnArray)?;
    array
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<&str>>>()
        .ok_or(NotAStringArray::NotOnlyStrings)
}

/// Reject a preserve-list entry that is empty or contains a `..` component,
/// which would otherwise let backup/restore escape the rootfs tree.
fn validate_preserve_path(path: &str) -> Result<()> {
    if path.trim_start_matches('/').is_empty() {
        return Err(FactoryResetError::InvalidPreserve(
            "preserve path must not be empty".to_string(),
        )
        .into());
    }
    let escapes_rootfs = Path::new(path.trim_start_matches('/'))
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir));
    if escapes_rootfs {
        return Err(FactoryResetError::InvalidPreserve(format!(
            "preserve path '{path}' contains '..' and would escape rootfs"
        ))
        .into());
    }
    Ok(())
}

pub fn build_preserve_list(config: &FactoryResetConfig, rootfs: &Path) -> Result<Vec<String>> {
    let mut list = vec![PRESERVE_LIST_MANDATORY.to_string()];

    let has_non_app_keys = config.preserve.iter().any(|k| k != KEY_APPLICATIONS);

    let config_file = rootfs.join(FACTORY_RESET_CONFIG_FILE);
    let key_config: Option<Value> = if has_non_app_keys {
        let content = std::fs::read_to_string(&config_file).map_err(|e| {
            // A key that names a file which is not there is the same kind of
            // problem as a key missing inside it, and reports the same status.
            // Any other read failure stays an I/O error: the request was
            // usable, the storage was not, and that is worth retrying.
            if e.kind() == std::io::ErrorKind::NotFound {
                FactoryResetError::MissingField(format!("{} does not exist", config_file.display()))
            } else {
                FactoryResetError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to read {}: {e}", config_file.display()),
                ))
            }
        })?;
        let value: Value = serde_json::from_str(&content).map_err(|e| {
            FactoryResetError::InvalidPreserve(format!(
                "Failed to parse {}: {e}",
                config_file.display()
            ))
        })?;
        Some(value)
    } else {
        None
    };

    for key in &config.preserve {
        if key == KEY_APPLICATIONS {
            collect_application_paths(rootfs, &mut list)?;
        } else {
            let value = key_config
                .as_ref()
                .expect("key_config must be Some for non-application keys");
            let paths = string_array(value, key).map_err(|e| {
                let file = config_file.display();
                match e {
                    NotAStringArray::Missing => {
                        FactoryResetError::MissingField(format!("{file}: no '{key}' key"))
                    }
                    NotAStringArray::NotAnArray => FactoryResetError::InvalidPreserve(format!(
                        "{file}: value for key '{key}' must be an array"
                    )),
                    NotAStringArray::NotOnlyStrings => FactoryResetError::InvalidPreserve(format!(
                        "{file}: value for key '{key}' must contain only strings"
                    )),
                }
            })?;
            for path in paths {
                validate_preserve_path(path)?;
                list.push(path.to_string());
            }
        }
    }

    Ok(list)
}

fn collect_application_paths(rootfs: &Path, list: &mut Vec<String>) -> Result<()> {
    let dir = rootfs.join(FACTORY_RESET_CONFIG_DIR);

    if !dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&dir).map_err(|e| {
        FactoryResetError::Io(std::io::Error::new(
            e.kind(),
            format!("Failed to read {}: {e}", dir.display()),
        ))
    })?;

    // Collect per-entry Results and abort on any read error (I/O error, race):
    // silently dropping an application's preserve list would wipe its paths while
    // restore_all still reports Success.
    for entry in entries {
        let entry = entry.map_err(|e| {
            FactoryResetError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read entry in {}: {e}", dir.display()),
            ))
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = std::fs::read_to_string(&path).map_err(|e| {
            FactoryResetError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read {}: {e}", path.display()),
            ))
        })?;

        let value: Value = serde_json::from_str(&content).map_err(|e| {
            FactoryResetError::InvalidPreserve(format!("{}: invalid JSON ({e})", path.display()))
        })?;

        let paths = string_array(&value, KEY_PATHS).map_err(|e| {
            let file = path.display();
            match e {
                NotAStringArray::Missing => {
                    FactoryResetError::MissingField(format!("{file}: no '{KEY_PATHS}' key"))
                }
                NotAStringArray::NotAnArray => FactoryResetError::InvalidPreserve(format!(
                    "{file}: '{KEY_PATHS}' must be an array"
                )),
                NotAStringArray::NotOnlyStrings => FactoryResetError::InvalidPreserve(format!(
                    "{file}: '{KEY_PATHS}' must contain only strings"
                )),
            }
        })?;
        for preserve_path in paths {
            validate_preserve_path(preserve_path)?;
            list.push(preserve_path.to_string());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_mode_and_preserve() {
        let cfg = FactoryResetConfig::parse(r#"{"mode":1,"preserve":[]}"#).unwrap();
        assert_eq!(cfg.mode, ResetMode::Mode1);
        assert!(cfg.preserve.is_empty());
    }

    #[test]
    fn parse_rejects_unsupported_mode() {
        assert!(FactoryResetConfig::parse(r#"{"mode":0,"preserve":[]}"#).is_err());
        assert!(FactoryResetConfig::parse(r#"{"mode":4,"preserve":[]}"#).is_err());
        assert!(FactoryResetConfig::parse(r#"{"mode":5,"preserve":[]}"#).is_err());
        assert!(FactoryResetConfig::parse(r#"{"mode":"2","preserve":[]}"#).is_err());
    }

    #[test]
    fn parse_accepts_mode_1() {
        let cfg = FactoryResetConfig::parse(r#"{"mode":1,"preserve":["applications"]}"#).unwrap();
        assert_eq!(cfg.mode, ResetMode::Mode1);
        assert_eq!(cfg.preserve, vec!["applications"]);
    }

    #[test]
    fn parse_accepts_wipe_modes() {
        let cfg = FactoryResetConfig::parse(r#"{"mode":2,"preserve":[]}"#).unwrap();
        assert_eq!(cfg.mode, ResetMode::Mode2);
        let cfg = FactoryResetConfig::parse(r#"{"mode":3,"preserve":[]}"#).unwrap();
        assert_eq!(cfg.mode, ResetMode::Mode3);
    }

    #[test]
    fn reset_mode_try_from_accepts_one_to_three_only() {
        assert!(ResetMode::try_from(0u32).is_err());
        assert!(ResetMode::try_from(1u32).is_ok());
        assert!(ResetMode::try_from(2u32).is_ok());
        assert!(ResetMode::try_from(3u32).is_ok());
        assert!(ResetMode::try_from(4u32).is_err());
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        assert!(FactoryResetConfig::parse("not json").is_err());
    }

    #[test]
    fn parse_missing_mode_returns_error() {
        assert!(FactoryResetConfig::parse(r#"{"preserve":[]}"#).is_err());
    }

    #[test]
    fn build_preserve_list_empty_preserve() {
        let temp = TempDir::new().unwrap();
        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec![],
        };
        let list = build_preserve_list(&cfg, temp.path()).unwrap();
        assert_eq!(list, vec![PRESERVE_LIST_MANDATORY]);
    }

    #[test]
    fn build_preserve_list_applications_key() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("app.json"),
            r#"{"paths":["/home/user/.config","var/app"]}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["applications".into()],
        };
        let list = build_preserve_list(&cfg, temp.path()).unwrap();
        assert_eq!(list[0], PRESERVE_LIST_MANDATORY);
        assert!(list.contains(&"/home/user/.config".to_string()));
        assert!(list.contains(&"var/app".to_string()));
    }

    #[test]
    fn build_preserve_list_custom_key() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("factory-reset.json"),
            r#"{"network":["/etc/network/interfaces","/etc/wpa_supplicant.conf"]}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["network".into()],
        };
        let list = build_preserve_list(&cfg, temp.path()).unwrap();
        assert!(list.contains(&"/etc/network/interfaces".to_string()));
        assert!(list.contains(&"/etc/wpa_supplicant.conf".to_string()));
    }

    #[test]
    fn build_preserve_list_custom_key_non_array_returns_error() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("factory-reset.json"),
            r#"{"network":"/etc/network/interfaces"}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["network".into()],
        };
        assert!(build_preserve_list(&cfg, temp.path()).is_err());
    }

    #[test]
    fn validate_preserve_path_rejects_empty_string() {
        assert!(validate_preserve_path("").is_err());
        assert!(validate_preserve_path("/").is_err());
    }

    #[test]
    fn validate_preserve_path_rejects_parent_dir_traversal() {
        assert!(validate_preserve_path("../../etc/shadow").is_err());
        assert!(validate_preserve_path("/etc/../../shadow").is_err());
    }

    #[test]
    fn validate_preserve_path_accepts_normal_paths() {
        assert!(validate_preserve_path("/etc/hostname").is_ok());
        assert!(validate_preserve_path("var/app").is_ok());
    }

    #[test]
    fn build_preserve_list_custom_key_rejects_traversal() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("factory-reset.json"),
            r#"{"network":["../../etc/shadow"]}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["network".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::InvalidPreserve(_))
        ));
    }

    #[test]
    fn build_preserve_list_applications_rejects_traversal() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.json"), r#"{"paths":["../outside"]}"#).unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["applications".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::InvalidPreserve(_))
        ));
    }

    #[test]
    fn build_preserve_list_applications_without_usable_paths_is_a_config_error() {
        // A file the caller put there to keep something, which does not say
        // what to keep. Skipping it would wipe those paths and still report
        // success.
        for content in [
            "{}",
            r#"{"path": ""}"#,
            r#"{"paths": ""}"#,
            r#"{"paths": [1]}"#,
        ] {
            let temp = TempDir::new().unwrap();
            let dir = temp.path().join("etc/omnect/factory-reset.d");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("app.json"), content).unwrap();

            let cfg = FactoryResetConfig {
                mode: ResetMode::Mode1,
                preserve: vec!["applications".into()],
            };
            let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
            assert_eq!(
                crate::mode::factory_reset::failure_status_code(&error),
                crate::runtime::FactoryResetStatusCode::ConfigError,
                "{content}"
            );
        }
    }

    #[test]
    fn build_preserve_list_applications_empty_paths_array_is_accepted() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.json"), r#"{"paths": []}"#).unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["applications".into()],
        };
        assert_eq!(
            build_preserve_list(&cfg, temp.path()).unwrap(),
            vec![PRESERVE_LIST_MANDATORY.to_string()]
        );
    }

    #[test]
    fn build_preserve_list_applications_invalid_json_is_a_preserve_error() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.json"), "not json").unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["applications".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::InvalidPreserve(_))
        ));
    }

    #[test]
    fn build_preserve_list_read_error_maps_to_io_not_invalid_config() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        // app.json is itself a directory → read_to_string returns Err(io).
        fs::create_dir_all(dir.join("app.json")).unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["applications".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::Io(_))
        ));
    }

    #[test]
    fn build_preserve_list_missing_config_for_custom_key_is_a_config_error() {
        // A non-application key needs factory-reset.json. An absent file is the
        // same kind of problem as an absent key inside it, so both report
        // ConfigError rather than one of them reading as an I/O failure.
        let temp = TempDir::new().unwrap();
        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["network".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert_eq!(
            crate::mode::factory_reset::failure_status_code(&error),
            crate::runtime::FactoryResetStatusCode::ConfigError
        );
    }

    #[test]
    fn build_preserve_list_unreadable_config_stays_an_io_error() {
        // Only an absent factory-reset.json is a config problem. A path that
        // exists but cannot be read is a storage failure the caller can retry,
        // so it must not be folded into the same status.
        let temp = TempDir::new().unwrap();
        // the config file is itself a directory → read_to_string returns Err(io)
        fs::create_dir_all(temp.path().join(FACTORY_RESET_CONFIG_FILE)).unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["network".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::Io(_))
        ));
        assert_eq!(
            crate::mode::factory_reset::failure_status_code(&error),
            crate::runtime::FactoryResetStatusCode::Error
        );
    }

    #[test]
    fn build_preserve_list_custom_key_non_string_element_is_hard_error() {
        // A non-string array element must not be silently dropped — that would
        // wipe the intended path while restore_all still reports Success.
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("factory-reset.json"),
            r#"{"network":["/etc/network/interfaces", 42]}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["network".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::InvalidPreserve(_))
        ));
    }

    #[test]
    fn build_preserve_list_applications_non_string_element_is_hard_error() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("app.json"), r#"{"paths":["var/app", null]}"#).unwrap();

        let cfg = FactoryResetConfig {
            mode: ResetMode::Mode1,
            preserve: vec!["applications".into()],
        };
        let error = build_preserve_list(&cfg, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            crate::error::InitramfsError::FactoryReset(FactoryResetError::InvalidPreserve(_))
        ));
    }
}
