use std::path::PathBuf;

use crate::error::FlashError;

/// The `flash-mode-devpath` key as `FlashError::InvalidEnvValue` names it.
#[cfg(feature = "flash-mode-1")]
pub const DEVPATH_KEY: &str = "flash-mode-devpath";

/// Which flash mode the operator selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMode {
    #[cfg(feature = "flash-mode-1")]
    Mode1,
}

/// The selected mode together with the keys that mode needs.
#[derive(Debug, Clone)]
pub struct FlashConfig {
    pub mode: FlashMode,
    #[cfg(feature = "flash-mode-1")]
    pub devpath: Option<PathBuf>,
}

/// Map a `flash-mode` value onto a mode.
///
/// `None` means the value selects nothing — the caller logs it and boots
/// normally rather than failing, because an operator typo must not brick a
/// device.
pub fn parse_mode(value: &str) -> Option<FlashMode> {
    match value {
        #[cfg(feature = "flash-mode-1")]
        "1" => Some(FlashMode::Mode1),
        _ => None,
    }
}

/// Validate the `flash-mode-devpath` value.
#[cfg(feature = "flash-mode-1")]
pub fn parse_devpath(value: Option<&str>) -> Result<PathBuf, FlashError> {
    let path = value.unwrap_or_default().trim();
    if path.is_empty() {
        return Err(FlashError::InvalidEnvValue {
            key: DEVPATH_KEY,
            reason: "not set".into(),
        });
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_accepts_only_the_known_selectors() {
        #[cfg(feature = "flash-mode-1")]
        assert_eq!(parse_mode("1"), Some(FlashMode::Mode1));
        for unknown in ["", " ", "0", "4", "one", "1 "] {
            assert_eq!(
                parse_mode(unknown),
                None,
                "{unknown} must not select a mode"
            );
        }
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn parse_devpath_rejects_absent_and_empty() {
        assert!(parse_devpath(None).is_err());
        assert!(parse_devpath(Some("")).is_err());
        assert!(parse_devpath(Some("   ")).is_err());
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn parse_devpath_takes_the_value_verbatim() {
        // Both legacy backends already return a bare value, so the `cut -d= -f2`
        // the legacy script applies here is dead code and is not ported.
        assert_eq!(
            parse_devpath(Some("/dev/mmcblk2")).unwrap(),
            PathBuf::from("/dev/mmcblk2")
        );
    }
}
