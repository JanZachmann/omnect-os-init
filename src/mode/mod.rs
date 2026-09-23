use std::path::Path;

use crate::{
    BootEnv, BootEnvState, Result, config::Config, partition::PartitionLayout, runtime::OdsStatus,
};

#[cfg(feature = "factory-reset")]
use crate::bootloader::BootEnvKey;

pub mod normal;

#[cfg(feature = "factory-reset")]
pub mod factory_reset;

/// Runtime context passed to the active boot-mode handler.
pub struct BootContext<'a> {
    pub(crate) config: &'a Config,
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
    pub(crate) boot_env: BootEnvState,
    pub(crate) ods_status: OdsStatus,
}

impl<'a> BootContext<'a> {
    pub(crate) fn new(
        config: &'a Config,
        layout: &'a PartitionLayout,
        rootfs: &'a Path,
        boot_env: BootEnvState,
        ods_status: OdsStatus,
    ) -> Self {
        Self {
            config,
            layout,
            rootfs,
            boot_env,
            ods_status,
        }
    }
}

/// A factory-reset trigger that was set in the boot environment.
///
/// A trigger the init cannot use still has to be answered: it is cleared and
/// its failure is reported, so the caller learns the reset did not run
/// instead of waiting for a result that never arrives.
#[cfg(feature = "factory-reset")]
pub enum FactoryResetTrigger {
    Accepted(factory_reset::config::FactoryResetConfig),
    Rejected(crate::error::FactoryResetError),
}

/// The detected boot mode to execute.
pub enum BootMode {
    Normal,
    #[cfg(feature = "factory-reset")]
    FactoryReset(FactoryResetTrigger),
}

impl BootMode {
    /// Detect the boot mode from the boot environment.
    ///
    /// A `factory-reset` bootloader env key with a non-blank value returns
    /// `FactoryReset`, whether or not that value can be used. A blank value and
    /// an env that cannot be read both fall back to `Normal`. Never blocks
    /// boot.
    pub fn detect(_bl: Option<&dyn BootEnv>) -> Result<Self> {
        #[cfg(feature = "factory-reset")]
        if let Some(bl) = _bl {
            match bl.get_env(BootEnvKey::FactoryReset) {
                // Devices provisioned before this init carry the key set to an
                // empty value, and GRUB reports that as a present key where
                // U-Boot reports `None`. It is not a request to reset.
                Ok(Some(json)) if json.trim().is_empty() => {}
                Ok(Some(json)) => match factory_reset::config::FactoryResetConfig::parse(&json) {
                    Ok(config) => {
                        return Ok(Self::FactoryReset(FactoryResetTrigger::Accepted(config)));
                    }
                    Err(e) => {
                        log::warn!("factory-reset: unusable trigger, reporting it: {e}");
                        return Ok(Self::FactoryReset(FactoryResetTrigger::Rejected(e)));
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    log::warn!("factory-reset: failed to read env, booting normally: {e}");
                }
            }
        }

        Ok(Self::Normal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::create_mock_bootloader;

    #[test]
    fn detect_normal_with_live_bootloader() {
        let mock = create_mock_bootloader();
        let mode = BootMode::detect(Some(&mock)).unwrap();
        assert!(matches!(mode, BootMode::Normal));
    }

    #[test]
    fn detect_normal_degraded_boot_no_bootloader() {
        let mode = BootMode::detect(None).unwrap();
        assert!(matches!(mode, BootMode::Normal));
    }

    #[cfg(feature = "factory-reset")]
    mod factory_reset_detect_tests {
        use super::*;
        use crate::bootloader::BootEnvKey;

        #[test]
        fn detect_normal_when_factory_reset_key_absent() {
            let mock = create_mock_bootloader();
            let mode = BootMode::detect(Some(&mock)).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }

        #[test]
        fn detect_factory_reset_when_key_present_valid_json() {
            let mock = create_mock_bootloader()
                .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);
            let mode = BootMode::detect(Some(&mock)).unwrap();
            let BootMode::FactoryReset(FactoryResetTrigger::Accepted(config)) = mode else {
                panic!("a usable trigger must be accepted");
            };
            assert_eq!(
                config.mode,
                crate::mode::factory_reset::config::ResetMode::Mode1
            );
            assert!(config.preserve.is_empty());
        }

        #[test]
        fn detect_rejects_an_unusable_trigger_instead_of_ignoring_it() {
            for trigger in [
                "not-json",
                r#"{ "mode": "#,
                "{}",
                r#"{"mode":4,"preserve":[]}"#,
                r#"{"mode":1}"#,
                r#"{"mode":1,"preserve":""}"#,
            ] {
                let mock = create_mock_bootloader().with_env(BootEnvKey::FactoryReset, trigger);
                let mode = BootMode::detect(Some(&mock)).unwrap();
                assert!(
                    matches!(
                        mode,
                        BootMode::FactoryReset(FactoryResetTrigger::Rejected(_))
                    ),
                    "trigger {trigger} must be rejected, not ignored"
                );
            }
        }

        #[test]
        fn detect_normal_when_the_trigger_is_blank() {
            for trigger in ["", " ", "\n"] {
                let mock = create_mock_bootloader().with_env(BootEnvKey::FactoryReset, trigger);
                let mode = BootMode::detect(Some(&mock)).unwrap();
                assert!(
                    matches!(mode, BootMode::Normal),
                    "a blank trigger must not start a reset: {trigger:?}"
                );
            }
        }

        #[test]
        fn detect_normal_when_bootloader_unavailable() {
            let mode = BootMode::detect(None).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }

        #[test]
        fn detect_normal_when_get_env_fails() {
            let mock = create_mock_bootloader().with_get_env_error();
            let mode = BootMode::detect(Some(&mock)).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }
    }
}
