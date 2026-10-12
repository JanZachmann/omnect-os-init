#[cfg(feature = "flash-mode-1")]
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashMode {
    #[cfg(feature = "flash-mode-1")]
    Mode1,
    #[cfg(feature = "flash-mode-2")]
    Mode2,
    #[cfg(feature = "flash-mode-3")]
    Mode3,
}

impl FlashMode {
    /// The `flash-mode` value that selects the mode.
    #[cfg(all(feature = "flash-mode-network", feature = "factory-reset"))]
    pub fn selector(self) -> &'static str {
        match self {
            #[cfg(feature = "flash-mode-1")]
            Self::Mode1 => "1",
            #[cfg(feature = "flash-mode-2")]
            Self::Mode2 => "2",
            #[cfg(feature = "flash-mode-3")]
            Self::Mode3 => "3",
        }
    }

    /// Modes 2 and 3 write a new image over the disk the device runs from.
    #[cfg(feature = "flash-mode-network")]
    pub fn overwrites_running_disk(self) -> bool {
        match self {
            #[cfg(feature = "flash-mode-1")]
            Self::Mode1 => false,
            #[cfg(feature = "flash-mode-2")]
            Self::Mode2 => true,
            #[cfg(feature = "flash-mode-3")]
            Self::Mode3 => true,
        }
    }
}

/// What `flash-mode-devpath` held when the mode was detected.
#[cfg(feature = "flash-mode-1")]
#[derive(Debug, PartialEq, Eq)]
pub enum Devpath {
    Set(PathBuf),
    NotSet,
    Unreadable(String),
}

/// What a mode 3 URL key held when the mode was detected.
#[cfg(feature = "flash-mode-3")]
#[derive(Debug, PartialEq, Eq)]
pub enum EnvUrl {
    Set(String),
    NotSet,
    Invalid(String),
    Unreadable(String),
}

#[derive(Debug)]
pub struct FlashConfig {
    pub mode: FlashMode,
    #[cfg(feature = "flash-mode-1")]
    pub devpath: Devpath,
    #[cfg(feature = "flash-mode-3")]
    pub image_url: EnvUrl,
    #[cfg(feature = "flash-mode-3")]
    pub sha256_url: EnvUrl,
}

impl FlashConfig {
    /// `mode` with none of its boot-env values read.
    pub fn new(mode: FlashMode) -> Self {
        Self {
            mode,
            #[cfg(feature = "flash-mode-1")]
            devpath: Devpath::NotSet,
            #[cfg(feature = "flash-mode-3")]
            image_url: EnvUrl::NotSet,
            #[cfg(feature = "flash-mode-3")]
            sha256_url: EnvUrl::NotSet,
        }
    }
}

/// `None` means the value selects nothing. Compared exactly, as legacy does.
pub(crate) fn parse_mode(value: &str) -> Option<FlashMode> {
    match value {
        #[cfg(feature = "flash-mode-1")]
        "1" => Some(FlashMode::Mode1),
        #[cfg(feature = "flash-mode-2")]
        "2" => Some(FlashMode::Mode2),
        #[cfg(feature = "flash-mode-3")]
        "3" => Some(FlashMode::Mode3),
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

/// The value is base64. The decoded URL is trimmed, because an
/// `echo | base64` encoder adds a newline.
#[cfg(feature = "flash-mode-3")]
pub(crate) fn parse_url(value: Option<&str>) -> EnvUrl {
    use base64::Engine;

    let Some(encoded) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return EnvUrl::NotSet;
    };
    let decoded = match base64::engine::general_purpose::STANDARD.decode(encoded) {
        Ok(bytes) => bytes,
        Err(e) => return EnvUrl::Invalid(format!("not base64: {e}")),
    };
    match String::from_utf8(decoded) {
        Ok(url) if url.trim().is_empty() => EnvUrl::Invalid("decodes to an empty URL".to_string()),
        Ok(url) => EnvUrl::Set(url.trim().to_string()),
        Err(_) => EnvUrl::Invalid("decodes to text that is not UTF-8".to_string()),
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
        #[cfg(feature = "flash-mode-3")]
        assert_eq!(parse_mode("3"), Some(FlashMode::Mode3));
        for unknown in [
            "", " ", "0", "4", "one", "11", " 1", "1 ", " 2", "2 ", " 3", "3 ",
        ] {
            assert_eq!(
                parse_mode(unknown),
                None,
                "{unknown:?} must not select a mode"
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

    #[cfg(feature = "flash-mode-3")]
    mod mode_3 {
        use super::*;
        use base64::Engine;

        const URL: &str = "https://example.invalid/images/omnect.wic.xz?sv=2024&sig=secret";

        fn encoded(text: &str) -> String {
            base64::engine::general_purpose::STANDARD.encode(text)
        }

        #[test]
        fn a_base64_url_is_decoded() {
            assert_eq!(parse_url(Some(&encoded(URL))), EnvUrl::Set(URL.to_string()));
        }

        #[test]
        fn the_newline_of_an_echo_encoder_is_trimmed() {
            let value = format!(" {}\n", encoded(&format!("{URL}\n")));
            assert_eq!(parse_url(Some(&value)), EnvUrl::Set(URL.to_string()));
        }

        #[test]
        fn an_absent_or_blank_value_is_not_set() {
            for value in [None, Some(""), Some("  ")] {
                assert_eq!(parse_url(value), EnvUrl::NotSet, "{value:?}");
            }
        }

        #[test]
        fn unusable_values_are_invalid() {
            let not_utf8 = base64::engine::general_purpose::STANDARD.encode([0xff_u8, 0xfe]);
            for value in ["not base64!!".to_string(), encoded("  \n"), not_utf8] {
                assert!(
                    matches!(parse_url(Some(&value)), EnvUrl::Invalid(_)),
                    "{value:?}"
                );
            }
        }
    }
}
