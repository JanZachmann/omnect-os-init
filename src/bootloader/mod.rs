//! BootEnv abstraction module
//!
//! This module provides a trait-based abstraction over different bootloaders
//! (GRUB and U-Boot) to allow unified access to bootloader environment variables.

#[cfg(feature = "grub")]
mod grub;
mod types;
#[cfg(feature = "uboot")]
mod uboot;

use std::borrow::Cow;

use crate::error::BootEnvError;
use crate::filesystem::FsckExitCode;
use crate::partition::PartitionName;

#[cfg(feature = "grub")]
pub use self::grub::GrubBootEnv;
#[cfg(feature = "uboot")]
pub use self::uboot::UBootBootEnv;

pub type Result<T> = std::result::Result<T, BootEnvError>;

/// Value of `omnect_first_boot_done`, shared by its writer and its reader.
pub const FIRST_BOOT_DONE: &str = "1";

/// Upper bound (bytes) on the failure reason stored via `save_factory_reset_failure`.
/// The bootloader env block is small and shared by all variables, so keep it short.
#[cfg(feature = "factory-reset")]
const MAX_FACTORY_RESET_FAILURE_REASON_LEN: usize = 128;

/// Truncate `s` to at most `max_bytes`, never splitting a multi-byte UTF-8
/// character — slicing at a non-boundary panics.
#[cfg(feature = "factory-reset")]
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Flush pending writes to disk.
///
/// Callers that persist state and then reboot or halt need this: neither
/// `reboot(2)` with `RB_AUTOBOOT` nor the halt loop syncs, so an unflushed write
/// is lost exactly where it was meant to survive.
pub(crate) fn sync_filesystems() {
    nix::unistd::sync();
}

/// Decoded fsck result stored in the bootloader environment.
#[derive(Debug, Clone, PartialEq)]
pub struct FsckRecord {
    /// Typed exit code from fsck.
    pub exit_code: FsckExitCode,
    /// Combined stdout + stderr output from fsck.
    pub output: String,
}

/// Typed key for bootloader environment variables.
///
/// Use this instead of raw `&str` keys to prevent typos and make all
/// known env-var names visible in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootEnvKey {
    /// `omnect_validate_update` — OTA update validation state.
    ValidateUpdate,
    /// `omnect_validate_update_failed` — set by the bootloader when it rolled
    /// back to the previous slot because update validation did not complete.
    /// The bootloader clears `omnect_validate_update` when it sets this key.
    ValidateUpdateFailed,
    /// `omnect_bootloader_updated` — whether the bootloader itself was updated.
    BootloaderUpdated,
    /// `omnect_fsck_<partition>` — fsck result for the given partition.
    FsckStatus(PartitionName),
    /// `omnect_first_boot_done` — set to `"1"` after the first successful
    /// `run_init`. Unified first-boot sentinel; read by the resize-data init
    /// setup step and the first-boot detection in `run_init`.
    FirstBootDone,
    /// `omnect_extra_bootargs` — extra kernel cmdline arguments the bootloader
    /// appends. Synced from the boot-partition files by the extra-bootargs
    /// init-setup step.
    ExtraBootArgs,
    #[cfg(feature = "factory-reset")]
    /// `factory-reset` — JSON trigger set by ODS to request a factory reset.
    /// Value format: `{"mode":1,"preserve":["applications","network"]}`.
    /// Cleared by the initramfs as the first step of the reset sequence.
    FactoryReset,
    #[cfg(feature = "factory-reset")]
    /// `omnect_factory_reset_last_error` — records a partition that stayed
    /// unmountable after a reformat-and-retry during the factory-reset
    /// destructive phase, so the failure survives even if the boot that
    /// follows halts before switch_root. Plain text `"<partition>:<reason>"`.
    FactoryResetLastError,
    #[cfg(feature = "flash-mode")]
    /// `flash-mode` — mode selector set by the operator. Cleared by the initramfs
    /// before the selected mode starts work.
    FlashMode,
    #[cfg(feature = "flash-mode-1")]
    /// `flash-mode-devpath` — destination block device for mode 1.
    FlashModeDevPath,
}

impl BootEnvKey {
    /// Returns the env-var name as it is stored in the bootloader environment.
    pub fn as_str(&self) -> Cow<'static, str> {
        match self {
            Self::ValidateUpdate => Cow::Borrowed("omnect_validate_update"),
            Self::ValidateUpdateFailed => Cow::Borrowed("omnect_validate_update_failed"),
            Self::BootloaderUpdated => Cow::Borrowed("omnect_bootloader_updated"),
            Self::FsckStatus(p) => Cow::Owned(format!("omnect_fsck_{p}")),
            Self::FirstBootDone => Cow::Borrowed("omnect_first_boot_done"),
            Self::ExtraBootArgs => Cow::Borrowed("omnect_extra_bootargs"),
            #[cfg(feature = "factory-reset")]
            Self::FactoryReset => Cow::Borrowed("factory-reset"),
            #[cfg(feature = "factory-reset")]
            Self::FactoryResetLastError => Cow::Borrowed("omnect_factory_reset_last_error"),
            #[cfg(feature = "flash-mode")]
            Self::FlashMode => Cow::Borrowed("flash-mode"),
            #[cfg(feature = "flash-mode-1")]
            Self::FlashModeDevPath => Cow::Borrowed("flash-mode-devpath"),
        }
    }
}

/// Trait for boot environment access
///
/// This trait abstracts the differences between GRUB and U-Boot boot
/// environment access, allowing the rest of the codebase to work with
/// bootloader variables in a unified way.
///
/// The minimal required interface is `get_env` and `set_env`. The fsck helpers
/// (`save_fsck_status`, `get_fsck_status`, `clear_fsck_status`) have default
/// implementations that encode/decode via `get_env`/`set_env`. BootEnv
/// backends with custom storage strategies (e.g. GRUB's per-partition files)
/// should override the relevant methods.
pub trait BootEnv: Send + Sync {
    /// Get the value of a bootloader environment variable
    ///
    /// Returns `Ok(None)` if the variable doesn't exist.
    /// Returns `Err` if there was an error accessing the bootloader environment.
    fn get_env(&self, key: BootEnvKey) -> Result<Option<String>>;

    /// Set or delete a bootloader environment variable
    ///
    /// Pass `Some(value)` to set the variable, or `None` to delete it.
    fn set_env(&mut self, key: BootEnvKey, value: Option<&str>) -> Result<()>;

    /// Whether a flag-style variable is set. Any non-empty value counts.
    ///
    /// A GRUB script that assigns `key=` leaves an entry with an empty value
    /// behind, so presence alone is not enough. Applies to the flags the
    /// bootloader writes; `omnect_first_boot_done` is ours and is matched against
    /// [`FIRST_BOOT_DONE`].
    fn is_flag_set(&self, key: BootEnvKey) -> Result<bool> {
        Ok(self.get_env(key)?.is_some_and(|v| !v.is_empty()))
    }

    /// Save fsck result to bootloader environment.
    ///
    /// Stores exit code and full fsck output as gzip+base64 encoded string so the
    /// diagnostic text survives the reboot required after fsck corrects errors.
    fn save_fsck_status(
        &mut self,
        partition: PartitionName,
        code: FsckExitCode,
        output: &str,
    ) -> Result<()> {
        let encoded = types::encode_fsck_output(code.bits(), output);
        self.set_env(BootEnvKey::FsckStatus(partition), Some(&encoded))
    }

    /// Get fsck status from bootloader environment.
    ///
    /// Returns the decoded `FsckRecord` if a value is present,
    /// or `None` if no status was stored for this partition.
    fn get_fsck_status(&self, partition: PartitionName) -> Result<Option<FsckRecord>> {
        Ok(self
            .get_env(BootEnvKey::FsckStatus(partition))?
            .and_then(|v| types::decode_fsck_output(&v)))
    }

    /// Clear fsck status from bootloader environment
    fn clear_fsck_status(&mut self, partition: PartitionName) -> Result<()> {
        self.set_env(BootEnvKey::FsckStatus(partition), None)
    }

    /// Persist an unrecoverable factory-reset reformat/mount failure.
    ///
    /// Stored as plain text `"<partition>:<reason>"`, readable directly with
    /// `fw_printenv`/`grub-editenv list`.
    #[cfg(feature = "factory-reset")]
    fn save_factory_reset_failure(&mut self, partition: PartitionName, reason: &str) -> Result<()> {
        let reason = truncate_on_char_boundary(reason, MAX_FACTORY_RESET_FAILURE_REASON_LEN);
        self.set_env(
            BootEnvKey::FactoryResetLastError,
            Some(&format!("{partition}:{reason}")),
        )
    }
}

/// Opens the appropriate bootloader environment implementation based on the build-time feature flag.
///
/// The bootloader type is a build-time property of the target platform:
/// - `grub` feature: x86-64 EFI targets using GRUB (`grub-editenv`)
/// - `uboot` feature: ARM targets using U-Boot (`fw_printenv`/`fw_setenv`)
///
/// Exactly one of `grub` or `uboot` must be enabled; build.rs enforces this.
pub fn open_boot_env() -> Result<Box<dyn BootEnv>> {
    #[cfg(feature = "grub")]
    return Ok(Box::new(GrubBootEnv::new()?));

    #[cfg(feature = "uboot")]
    return Ok(Box::new(UBootBootEnv::new()?));
}

/// Boot environment state: opened successfully, or unavailable with reason.
pub enum BootEnvState {
    /// Boot environment opened successfully.
    Available(Box<dyn BootEnv>),
    /// Boot environment could not be opened.
    Degraded(BootEnvError),
}

impl BootEnvState {
    /// Returns `true` if the bootloader environment is unavailable.
    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded(_))
    }

    /// Returns a mutable reference to the boot env accessor if available.
    pub fn available_mut(&mut self) -> Option<&mut dyn BootEnv> {
        match self {
            Self::Available(b) => Some(b.as_mut()),
            Self::Degraded(_) => None,
        }
    }

    /// Returns a shared reference to the boot env accessor if available.
    pub fn available(&self) -> Option<&dyn BootEnv> {
        match self {
            Self::Available(b) => Some(b.as_ref()),
            Self::Degraded(_) => None,
        }
    }
}

/// The outcome of `classify_boot_env`.
pub enum BootEnvDecision {
    /// Continue init with this boot env state.
    Continue(BootEnvState),
    /// Abort init with this error — caller passes it to `handle_fatal_error`.
    Abort(crate::error::InitramfsError),
}

/// Decide how to proceed based on the bootloader open result and the image type.
///
/// - `Ok(bl)` → `Continue(Available(bl))` — successful open, both image types.
/// - `Err(e)` + release-image → `Continue(Degraded(e))` — degraded boot continues.
/// - `Err(e)` + debug-image → `Abort(DegradedBoot(e))` — enter debug shell immediately.
pub fn classify_boot_env(
    open_result: std::result::Result<Box<dyn BootEnv>, BootEnvError>,
    is_release_image: bool,
) -> BootEnvDecision {
    match open_result {
        Ok(bl) => BootEnvDecision::Continue(BootEnvState::Available(bl)),
        Err(e) if is_release_image => BootEnvDecision::Continue(BootEnvState::Degraded(e)),
        Err(e) => BootEnvDecision::Abort(crate::error::InitramfsError::DegradedBoot(e)),
    }
}

/// Create a mock boot environment for testing
#[cfg(any(test, feature = "test-utils"))]
pub fn create_mock_bootloader() -> MockBootEnv {
    MockBootEnv::new()
}

/// Mock boot environment for testing
#[cfg(any(test, feature = "test-utils"))]
#[derive(Default)]
pub struct MockBootEnv {
    env: std::collections::HashMap<String, String>,
    /// fsck results stored as typed records — no subprocess encoding needed in tests.
    fsck: std::collections::HashMap<PartitionName, FsckRecord>,
    /// Keys passed to set_env, in call order. Used by tests to verify set_env was/wasn't called.
    pub set_env_calls: Vec<BootEnvKey>,
    /// The same keys, shared so a test can read them after the mock was
    /// moved into a `Box<dyn BootEnv>`.
    shared_set_env_calls: std::sync::Arc<std::sync::Mutex<Vec<BootEnvKey>>>,
    /// When true, `get_env` returns an error instead of looking up the key.
    get_env_errors: bool,
    /// When set, `get_env` succeeds for this many calls, then errors. Lets a
    /// test reach the read-back path: first read ok, read-back fails.
    get_env_ok_calls: Option<usize>,
    /// Count of `get_env` calls seen, for `get_env_ok_calls`.
    get_env_seen: std::sync::atomic::AtomicUsize,
    /// Keys whose `get_env` returns an error while every other key reads fine.
    get_env_error_keys: Vec<BootEnvKey>,
    /// When true, `set_env` returns an error instead of setting the key.
    set_env_errors: bool,
    /// When set, `set_env` stores this fixed value instead of the given one,
    /// simulating a bootloader tool that normalizes the written value.
    set_env_normalize: Option<String>,
    /// Every `save_fsck_status` call, in call order. Shared so a handle
    /// obtained before the mock is moved (e.g. into a `Box<dyn BootEnv>`) can
    /// still be read after the mock itself is dropped.
    saved_fsck_calls: std::sync::Arc<std::sync::Mutex<Vec<(PartitionName, FsckExitCode, String)>>>,
    /// When true, `save_fsck_status` returns an error instead of recording.
    save_fsck_errors: bool,
    /// When true, `get_fsck_status` returns an error instead of looking up the partition.
    get_fsck_errors: bool,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockBootEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(mut self, key: BootEnvKey, value: &str) -> Self {
        self.env.insert(key.as_str().to_string(), value.to_string());
        self
    }

    pub fn with_get_env_error(mut self) -> Self {
        self.get_env_errors = true;
        self
    }

    pub fn with_get_env_error_after(mut self, ok_calls: usize) -> Self {
        self.get_env_ok_calls = Some(ok_calls);
        self
    }

    pub fn with_get_env_error_for(mut self, key: BootEnvKey) -> Self {
        self.get_env_error_keys.push(key);
        self
    }

    pub fn with_set_env_error(mut self) -> Self {
        self.set_env_errors = true;
        self
    }

    pub fn with_set_env_normalize(mut self, stored: &str) -> Self {
        self.set_env_normalize = Some(stored.to_string());
        self
    }

    pub fn with_save_fsck_error(mut self) -> Self {
        self.save_fsck_errors = true;
        self
    }

    pub fn with_get_fsck_error(mut self) -> Self {
        self.get_fsck_errors = true;
        self
    }

    /// A handle to this mock's `set_env` call log, readable even after the
    /// mock has been moved and dropped.
    pub fn shared_set_env_calls(&self) -> std::sync::Arc<std::sync::Mutex<Vec<BootEnvKey>>> {
        std::sync::Arc::clone(&self.shared_set_env_calls)
    }

    /// A handle to this mock's `save_fsck_status` call log, readable even
    /// after the mock has been moved and dropped.
    pub fn saved_fsck_calls(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<(PartitionName, FsckExitCode, String)>>> {
        std::sync::Arc::clone(&self.saved_fsck_calls)
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl BootEnv for MockBootEnv {
    fn get_env(&self, key: BootEnvKey) -> Result<Option<String>> {
        let seen = self
            .get_env_seen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.get_env_errors
            || self.get_env_ok_calls.is_some_and(|ok| seen >= ok)
            || self.get_env_error_keys.contains(&key)
        {
            return Err(crate::error::BootEnvError::CommandFailed {
                command: "mock".into(),
                reason: "injected error".into(),
            });
        }
        Ok(self.env.get(key.as_str().as_ref()).cloned())
    }

    fn set_env(&mut self, key: BootEnvKey, value: Option<&str>) -> Result<()> {
        if self.set_env_errors {
            return Err(crate::error::BootEnvError::CommandFailed {
                command: "mock".into(),
                reason: "injected set_env error".into(),
            });
        }
        self.set_env_calls.push(key);
        self.shared_set_env_calls.lock().unwrap().push(key);
        let to_store = match &self.set_env_normalize {
            Some(forced) => Some(forced.clone()),
            None => value.map(|v| v.to_string()),
        };
        match to_store {
            Some(v) => {
                self.env.insert(key.as_str().to_string(), v);
            }
            None => {
                self.env.remove(key.as_str().as_ref());
            }
        }
        Ok(())
    }

    fn save_fsck_status(
        &mut self,
        partition: PartitionName,
        code: FsckExitCode,
        output: &str,
    ) -> Result<()> {
        if self.save_fsck_errors {
            return Err(crate::error::BootEnvError::CommandFailed {
                command: "mock".into(),
                reason: "injected save_fsck_status error".into(),
            });
        }
        self.saved_fsck_calls
            .lock()
            .unwrap()
            .push((partition, code, output.to_string()));
        self.fsck.insert(
            partition,
            FsckRecord {
                exit_code: code,
                output: output.to_string(),
            },
        );
        Ok(())
    }

    fn get_fsck_status(&self, partition: PartitionName) -> Result<Option<FsckRecord>> {
        if self.get_fsck_errors {
            return Err(crate::error::BootEnvError::CommandFailed {
                command: "mock".into(),
                reason: "injected get_fsck_status error".into(),
            });
        }
        Ok(self.fsck.get(&partition).cloned())
    }

    fn clear_fsck_status(&mut self, partition: PartitionName) -> Result<()> {
        self.fsck.remove(&partition);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod classify_tests {
        use super::*;
        use crate::error::{BootEnvError, InitramfsError};

        fn ok_bootloader() -> std::result::Result<Box<dyn BootEnv>, BootEnvError> {
            Ok(Box::new(MockBootEnv::new()))
        }

        fn err_bootloader() -> std::result::Result<Box<dyn BootEnv>, BootEnvError> {
            Err(BootEnvError::CommandFailed {
                command: "boot-env-tool".into(),
                reason: "not found".into(),
            })
        }

        #[test]
        fn ok_release_image_returns_available_not_degraded() {
            let decision = classify_boot_env(ok_bootloader(), true);
            assert!(matches!(
                decision,
                BootEnvDecision::Continue(BootEnvState::Available(_))
            ));
        }

        #[test]
        fn ok_debug_image_returns_available_not_degraded() {
            let decision = classify_boot_env(ok_bootloader(), false);
            assert!(matches!(
                decision,
                BootEnvDecision::Continue(BootEnvState::Available(_))
            ));
        }

        #[test]
        fn err_release_image_returns_degraded_continue() {
            let decision = classify_boot_env(err_bootloader(), true);
            assert!(matches!(
                decision,
                BootEnvDecision::Continue(BootEnvState::Degraded(_))
            ));
        }

        #[test]
        fn err_debug_image_returns_abort_with_degraded_boot_error() {
            let decision = classify_boot_env(err_bootloader(), false);
            assert!(matches!(
                decision,
                BootEnvDecision::Abort(InitramfsError::DegradedBoot(_))
            ));
        }
    }

    #[test]
    fn is_flag_set_requires_a_non_empty_value() {
        let bl = MockBootEnv::new()
            .with_env(BootEnvKey::ValidateUpdate, "1")
            .with_env(BootEnvKey::ValidateUpdateFailed, "");

        assert!(bl.is_flag_set(BootEnvKey::ValidateUpdate).unwrap());
        assert!(
            !bl.is_flag_set(BootEnvKey::ValidateUpdateFailed).unwrap(),
            "an entry with an empty value means not set"
        );
        assert!(!bl.is_flag_set(BootEnvKey::BootloaderUpdated).unwrap());
    }

    #[test]
    fn test_mock_bootloader_get_set() {
        let mut bl = MockBootEnv::new();

        bl.set_env(BootEnvKey::ValidateUpdate, Some("1")).unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::ValidateUpdate).unwrap(),
            Some("1".to_string())
        );

        bl.set_env(BootEnvKey::ValidateUpdate, None).unwrap();
        assert_eq!(bl.get_env(BootEnvKey::ValidateUpdate).unwrap(), None);
    }

    #[test]
    fn test_mock_bootloader_with_env() {
        let bl = MockBootEnv::new()
            .with_env(BootEnvKey::ValidateUpdate, "1")
            .with_env(BootEnvKey::BootloaderUpdated, "0");

        assert_eq!(
            bl.get_env(BootEnvKey::ValidateUpdate).unwrap(),
            Some("1".to_string())
        );
        assert_eq!(
            bl.get_env(BootEnvKey::BootloaderUpdated).unwrap(),
            Some("0".to_string())
        );
        assert_eq!(
            bl.get_env(BootEnvKey::FsckStatus(PartitionName::Boot))
                .unwrap(),
            None
        );
    }

    #[test]
    fn test_mock_bootloader_fsck_status() {
        use crate::partition::PartitionName;
        let mut bl = MockBootEnv::new();

        bl.save_fsck_status(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected on pass 1",
        )
        .unwrap();

        let retrieved = bl.get_fsck_status(PartitionName::Boot).unwrap();
        assert_eq!(
            retrieved,
            Some(FsckRecord {
                exit_code: FsckExitCode::CORRECTED,
                output: "errors corrected on pass 1".to_string()
            })
        );

        bl.clear_fsck_status(PartitionName::Boot).unwrap();
        assert_eq!(bl.get_fsck_status(PartitionName::Boot).unwrap(), None);
    }

    #[test]
    fn first_boot_done_key_string() {
        // Pin the wire string. ODS / cloud / external tools may match on it
        // so changing it would be a wire-format break.
        assert_eq!(
            BootEnvKey::FirstBootDone.as_str().as_ref(),
            "omnect_first_boot_done"
        );
    }

    #[test]
    fn extra_bootargs_key_string() {
        assert_eq!(
            BootEnvKey::ExtraBootArgs.as_str().as_ref(),
            "omnect_extra_bootargs"
        );
    }

    #[cfg(feature = "factory-reset")]
    #[test]
    fn factory_reset_key_as_str() {
        assert_eq!(BootEnvKey::FactoryReset.as_str().as_ref(), "factory-reset");
    }

    #[cfg(feature = "factory-reset")]
    #[test]
    fn factory_reset_key_roundtrip_via_mock() {
        let mut bl = MockBootEnv::new();
        bl.set_env(BootEnvKey::FactoryReset, Some(r#"{"mode":1}"#))
            .unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::FactoryReset).unwrap(),
            Some(r#"{"mode":1}"#.to_string())
        );
        bl.set_env(BootEnvKey::FactoryReset, None).unwrap();
        assert_eq!(bl.get_env(BootEnvKey::FactoryReset).unwrap(), None);
    }

    #[cfg(feature = "factory-reset")]
    mod truncate_tests {
        use crate::bootloader::truncate_on_char_boundary;

        #[test]
        fn returns_input_when_within_limit() {
            assert_eq!(truncate_on_char_boundary("short", 128), "short");
        }

        #[test]
        fn truncates_ascii_at_limit() {
            assert_eq!(truncate_on_char_boundary("abcdef", 3), "abc");
        }

        #[test]
        fn never_splits_a_multibyte_char() {
            // "é" is 2 bytes; a limit of 2 lands inside it, so the result backs off to "a".
            let s = "aé";
            let out = truncate_on_char_boundary(s, 2);
            assert!(s.is_char_boundary(out.len()));
            assert_eq!(out, "a");
        }
    }

    #[cfg(feature = "factory-reset")]
    mod factory_reset_failure_tests {
        use crate::bootloader::{
            BootEnv, BootEnvKey, MAX_FACTORY_RESET_FAILURE_REASON_LEN, MockBootEnv,
        };
        use crate::partition::PartitionName;

        #[test]
        fn key_as_str_is_stable() {
            assert_eq!(
                BootEnvKey::FactoryResetLastError.as_str().as_ref(),
                "omnect_factory_reset_last_error"
            );
        }

        #[test]
        fn save_factory_reset_failure_round_trips() {
            let mut mock = MockBootEnv::new();
            mock.save_factory_reset_failure(PartitionName::Etc, "mkfs retry exhausted")
                .unwrap();
            assert_eq!(
                mock.get_env(BootEnvKey::FactoryResetLastError).unwrap(),
                Some("etc:mkfs retry exhausted".to_string())
            );
        }

        #[test]
        fn save_factory_reset_failure_truncates_long_reason() {
            let mut mock = MockBootEnv::new();
            let long = "x".repeat(500);
            mock.save_factory_reset_failure(PartitionName::Data, &long)
                .unwrap();
            let stored = mock
                .get_env(BootEnvKey::FactoryResetLastError)
                .unwrap()
                .unwrap();
            assert!(stored.len() <= "data:".len() + MAX_FACTORY_RESET_FAILURE_REASON_LEN);
            assert!(stored.starts_with("data:x"));
        }
    }

    #[cfg(feature = "flash-mode")]
    #[test]
    fn flash_mode_keys_use_the_legacy_hyphenated_names() {
        assert_eq!(BootEnvKey::FlashMode.as_str(), "flash-mode");
        #[cfg(feature = "flash-mode-1")]
        assert_eq!(BootEnvKey::FlashModeDevPath.as_str(), "flash-mode-devpath");
    }
}
