//! Integration tests for the flash modes: trigger detection and clear ordering.

#![cfg(all(feature = "flash-mode", feature = "test-utils"))]

use omnect_os_init::MockBootEnv;
use omnect_os_init::bootloader::BootEnvKey;
use omnect_os_init::mode::BootMode;

#[cfg(feature = "flash-mode-1")]
#[test]
fn a_set_flash_mode_selects_the_flash_boot_mode() {
    use omnect_os_init::mode::flash::config::FlashMode;
    use std::path::Path;

    let mut env = MockBootEnv::new()
        .with_env(BootEnvKey::FlashMode, "1")
        .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2");
    let BootMode::Flash(config) = BootMode::detect(Some(&mut env)).unwrap() else {
        panic!("a set flash-mode must select the flash mode");
    };
    assert_eq!(config.mode, FlashMode::Mode1);
    assert_eq!(config.devpath.as_deref(), Some(Path::new("/dev/mmcblk2")));
}

#[test]
fn an_absent_flash_mode_leaves_the_boot_mode_normal() {
    let mut env = MockBootEnv::new();
    assert!(matches!(
        BootMode::detect(Some(&mut env)).unwrap(),
        BootMode::Normal
    ));
}

#[test]
fn a_boot_env_read_failure_falls_back_to_normal_boot() {
    let mut env = MockBootEnv::new().with_get_env_error();
    assert!(
        matches!(BootMode::detect(Some(&mut env)).unwrap(), BootMode::Normal),
        "an unreadable env must not stop the device from booting"
    );
}

#[cfg(all(feature = "flash-mode-1", feature = "factory-reset"))]
#[test]
fn both_triggers_set_clears_both_and_refuses() {
    let mut env = MockBootEnv::new()
        .with_env(BootEnvKey::FlashMode, "1")
        .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2")
        .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);

    let err = BootMode::detect(Some(&mut env)).unwrap_err();
    assert!(matches!(
        err,
        omnect_os_init::InitramfsError::Flash(
            omnect_os_init::error::FlashError::ConflictingTriggers
        )
    ));

    // Clearing both is what keeps a release image from halting on every power
    // cycle: the next boot is a normal one and the operator re-queues.
    assert!(env.set_env_calls.contains(&BootEnvKey::FlashMode));
    assert!(env.set_env_calls.contains(&BootEnvKey::FactoryReset));
}

#[cfg(feature = "flash-mode-1")]
#[test]
fn an_unknown_selector_value_boots_normally() {
    for value in ["", "0", "4", "banana"] {
        let mut env = MockBootEnv::new().with_env(BootEnvKey::FlashMode, value);
        assert!(
            matches!(BootMode::detect(Some(&mut env)).unwrap(), BootMode::Normal),
            "{value} must boot normally"
        );
    }
}
