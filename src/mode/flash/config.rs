#[cfg(feature = "flash-mode-3")]
use std::fmt;
#[cfg(feature = "flash-mode-1")]
use std::path::PathBuf;

#[cfg(feature = "flash-mode-3")]
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

#[cfg(feature = "flash-mode-3")]
const SCHEME_SEPARATOR: &str = "://";

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

/// What a boot-env key held when the mode was detected.
#[cfg(any(feature = "flash-mode-1", feature = "flash-mode-3"))]
#[derive(Debug, PartialEq, Eq)]
pub enum EnvValue<T> {
    Set(T),
    NotSet,
    Invalid(String),
    Unreadable(String),
}

#[cfg(feature = "flash-mode-1")]
pub type Devpath = EnvValue<PathBuf>;

#[cfg(feature = "flash-mode-3")]
pub type EnvUrl = EnvValue<Url>;

/// `Display` and `Debug` print the URL without user info, query string and
/// fragment, which can hold credentials or an access token.
#[cfg(feature = "flash-mode-3")]
#[derive(Clone, PartialEq, Eq)]
pub struct Url(String);

#[cfg(feature = "flash-mode-3")]
impl Url {
    /// Trimmed; `None` for a blank `url`.
    pub fn parse(url: &str) -> Option<Self> {
        let url = url.trim();
        (!url.is_empty()).then(|| Self(url.to_string()))
    }

    /// The full URL, for the curl command line only.
    pub(crate) fn unredacted(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "flash-mode-3")]
impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redacted(&self.0))
    }
}

#[cfg(feature = "flash-mode-3")]
impl fmt::Debug for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Url").field(&redacted(&self.0)).finish()
    }
}

#[cfg(feature = "flash-mode-3")]
fn redacted(url: &str) -> String {
    // curl also takes a URL without a scheme, user info included.
    let (scheme, rest) = url
        .find(SCHEME_SEPARATOR)
        .map_or(("", url), |at| url.split_at(at + SCHEME_SEPARATOR.len()));
    // User info goes first: a `?` or `#` in a password starts no query string.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let host_start = rest[..authority_end].rfind('@').map_or(0, |at| at + 1);
    let rest = &rest[host_start..];
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    format!("{scheme}{rest}")
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

/// Line breaks are ignored, as `base64 -d` does: `base64` wraps its output at
/// 76 columns. Trimmed after decoding: `echo | base64` encodes a trailing
/// newline.
#[cfg(feature = "flash-mode-3")]
pub(crate) fn parse_url(value: Option<&str>) -> EnvUrl {
    let encoded: String = value
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if encoded.is_empty() {
        return EnvUrl::NotSet;
    }
    let decoded = match BASE64.decode(encoded) {
        Ok(bytes) => bytes,
        Err(e) => return EnvUrl::Invalid(format!("not base64: {e}")),
    };
    match String::from_utf8(decoded) {
        Ok(url) => Url::parse(&url).map_or_else(
            || EnvUrl::Invalid("decodes to an empty URL".to_string()),
            EnvUrl::Set,
        ),
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

    #[test]
    fn each_mode_parses_from_its_selector() {
        let modes = [
            #[cfg(feature = "flash-mode-1")]
            FlashMode::Mode1,
            #[cfg(feature = "flash-mode-2")]
            FlashMode::Mode2,
            #[cfg(feature = "flash-mode-3")]
            FlashMode::Mode3,
        ];
        for mode in modes {
            assert_eq!(parse_mode(mode.selector()), Some(mode));
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

        const URL: &str = "https://example.invalid/images/omnect.wic.xz?sv=2024&sig=secret";
        const BASE64_WRAP_COLUMNS: usize = 76;

        fn encoded(text: impl AsRef<[u8]>) -> String {
            BASE64.encode(text)
        }

        fn url(text: &str) -> Url {
            Url::parse(text).unwrap()
        }

        #[test]
        fn a_base64_url_is_decoded() {
            assert_eq!(parse_url(Some(&encoded(URL))), EnvUrl::Set(url(URL)));
        }

        #[test]
        fn the_newline_of_an_echo_encoder_is_trimmed() {
            let value = format!(" {}\n", encoded(format!("{URL}\n")));
            assert_eq!(parse_url(Some(&value)), EnvUrl::Set(url(URL)));
        }

        #[test]
        fn line_breaks_of_a_wrapping_encoder_are_ignored() {
            let value = encoded(URL)
                .as_bytes()
                .chunks(BASE64_WRAP_COLUMNS)
                .map(|line| format!("{}\n", std::str::from_utf8(line).unwrap()))
                .collect::<String>();
            assert!(value.trim_end().contains('\n'), "{value:?}");
            assert_eq!(parse_url(Some(&value)), EnvUrl::Set(url(URL)));
        }

        #[test]
        fn a_blank_url_does_not_parse() {
            for blank in ["", " ", "\n"] {
                assert_eq!(Url::parse(blank), None, "{blank:?}");
            }
            assert_eq!(Url::parse(" a \n").unwrap().unredacted(), "a");
        }

        #[test]
        fn an_absent_or_blank_value_is_not_set() {
            for value in [None, Some(""), Some("  ")] {
                assert_eq!(parse_url(value), EnvUrl::NotSet, "{value:?}");
            }
        }

        #[test]
        fn a_url_loses_its_credentials_and_query_in_logs() {
            assert_eq!(
                redacted(URL),
                "https://example.invalid/images/omnect.wic.xz"
            );
            assert_eq!(redacted("http://host/a#token"), "http://host/a");
            assert_eq!(redacted("http://host/a"), "http://host/a");
            assert_eq!(redacted("https://user:pass@host/a"), "https://host/a");
            assert_eq!(redacted("https://user:pass@host"), "https://host");
            assert_eq!(redacted("host/a?sig=secret"), "host/a");
            assert_eq!(redacted("user:pass@host/a?sig=secret"), "host/a");
            assert_eq!(redacted("https://user:se?cret@host/x"), "https://host/x");
            assert_eq!(redacted("https://user:se#cret@host"), "https://host");
            assert_eq!(redacted("https://host?sig=secret"), "https://host");
        }

        #[test]
        fn a_url_prints_redacted() {
            let url = url("https://user:pass@example.invalid/a?sig=secret");
            assert_eq!(url.to_string(), "https://example.invalid/a");
            assert_eq!(format!("{url:?}"), r#"Url("https://example.invalid/a")"#);
        }

        #[test]
        fn a_debug_print_does_not_show_the_token() {
            let config = FlashConfig {
                image_url: EnvUrl::Set(url("https://user:pass@example.invalid/a?sig=secret")),
                ..FlashConfig::new(FlashMode::Mode3)
            };
            let printed = format!("{config:?}");
            assert!(printed.contains("example.invalid/a"), "{printed}");
            assert!(
                !printed.contains("secret") && !printed.contains("pass"),
                "{printed}"
            );
        }

        #[test]
        fn unusable_values_are_invalid() {
            for value in [
                "not base64!!".to_string(),
                encoded("  \n"),
                encoded([0xff_u8, 0xfe]),
            ] {
                assert!(
                    matches!(parse_url(Some(&value)), EnvUrl::Invalid(_)),
                    "{value:?}"
                );
            }
        }
    }
}
