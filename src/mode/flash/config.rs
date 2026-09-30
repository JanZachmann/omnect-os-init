#[cfg(feature = "flash-mode-1")]
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMode {
    #[cfg(feature = "flash-mode-1")]
    Mode1,
    #[cfg(feature = "flash-mode-2")]
    Mode2,
}

/// What `flash-mode-devpath` held when the mode was detected.
#[cfg(feature = "flash-mode-1")]
#[derive(Debug, PartialEq, Eq)]
pub enum Devpath {
    Set(PathBuf),
    NotSet,
    Unreadable(String),
}

#[derive(Debug)]
pub struct FlashConfig {
    pub mode: FlashMode,
    #[cfg(feature = "flash-mode-1")]
    pub devpath: Devpath,
}

/// `None` means the value selects nothing. Compared exactly, as legacy does.
pub(crate) fn parse_mode(value: &str) -> Option<FlashMode> {
    match value {
        #[cfg(feature = "flash-mode-1")]
        "1" => Some(FlashMode::Mode1),
        #[cfg(feature = "flash-mode-2")]
        "2" => Some(FlashMode::Mode2),
        _ => None,
    }
}

/// Trimmed, as legacy word splitting did.
#[cfg(feature = "flash-mode-1")]
pub(crate) fn parse_devpath(value: Option<&str>) -> Devpath {
    match value.map(str::trim) {
        Some(path) if !path.is_empty() => Devpath::Set(PathBuf::from(path)),
        _ => Devpath::NotSet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_accepts_only_the_known_selectors() {
        #[cfg(feature = "flash-mode-1")]
        assert_eq!(parse_mode("1"), Some(FlashMode::Mode1));
        #[cfg(feature = "flash-mode-2")]
        assert_eq!(parse_mode("2"), Some(FlashMode::Mode2));
        for unknown in ["", " ", "0", "4", "one", "11", " 1", "1 ", " 2", "2 "] {
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
        for value in [None, Some(""), Some("   ")] {
            assert_eq!(parse_devpath(value), Devpath::NotSet, "{value:?}");
        }
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn parse_devpath_trims_surrounding_whitespace() {
        for value in ["/dev/mmcblk2", " /dev/mmcblk2", "/dev/mmcblk2 \n"] {
            assert_eq!(
                parse_devpath(Some(value)),
                Devpath::Set(PathBuf::from("/dev/mmcblk2")),
                "{value:?}"
            );
        }
    }
}
