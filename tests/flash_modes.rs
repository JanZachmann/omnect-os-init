//! Integration tests for the flash modes, through the public API.

#![cfg(all(feature = "flash-mode", feature = "test-utils"))]

use omnect_os_init::MockBootEnv;
use omnect_os_init::bootloader::BootEnvKey;
use omnect_os_init::mode::BootMode;

#[test]
fn a_boot_env_read_failure_falls_back_to_normal_boot() {
    let mut env = MockBootEnv::new()
        .with_env(BootEnvKey::FlashMode, "1")
        .with_get_env_error();
    assert!(
        matches!(BootMode::detect(Some(&mut env)).unwrap(), BootMode::Normal),
        "an unreadable env must not stop the device from booting"
    );
}
