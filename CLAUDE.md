# omnect-os-init

Init process of the omnect OS initramfs, written in Rust. It runs as PID 1,
checks and mounts the partitions, prepares the runtime state for
`omnect-device-service` and hands over to the rootfs with `switch_root`.
README.md lists the features; docs/usage.md describes device behaviour.

## Structure

- `src/main.rs`: binary entry; runs `run_init()` and executes the recovery
  action for a fatal error. `src/recovery.rs` decides that action.
- `src/lib.rs`: `run_init()`, the boot sequence and boot-mode dispatch.
- `src/bootloader/`: `BootEnv` trait, GRUB and U-Boot backends,
  `BootEnvKey` (all bootloader env keys), `classify_boot_env()`.
- `src/mode/`: `BootMode` and its detection; one handler per mode
  (`normal.rs`, `factory_reset/`, `flash/`).
- `src/filesystem/`, `src/partition/`: fsck, mounts, overlays; root device
  detection, partition layout, `/dev/omnect/*` symlinks.
- `src/runtime/`: ODS status JSON, fs-links, `switch_root`.
- `tests/contract/`: expected ODS JSON for factory reset. ODS reads this
  JSON, so a change in its shape is an API change.

## Constraints

- Release image (`release-image`): a fatal error halts in a loop, so the
  device does not reboot again and again. A debug image opens a shell. A
  fatal error while an update is pending reboots instead, on both image
  types. `FsckRequiresReboot` always reboots.
- Degraded boot: if the bootloader env cannot be opened, a release image
  boots on and sets `degraded_boot` in the ODS JSON; a debug image stops.
- A new `InitramfsError` variant must get a class in `recovery_class()`.
- The kernel cmdline keys read are `rootpart` and `bootpart_fsuuid` (GRUB),
  `root` (U-Boot) and `init`.
- Filesystem paths are `const` values, not inline literals. Mount points
  relative to the rootfs are in `filesystem::overlayfs::mount_points`.
- Adding a `BootMode` variant:
  1. Add its keys to `BootEnvKey` and `as_str()`.
  2. Add the variant and read the keys in `BootMode::detect_with()`. An
     absent or blank key, an unreadable env or no bootloader gives `Normal`.
     A present but unusable value still selects the mode, which clears the
     key and reports the failure (see `FactoryResetTrigger::Rejected`).
  3. Add a handler under `src/mode/` and a match arm in `run_init()`.
  4. Test detection with the key set, with no bootloader and with the key
     absent.

## Commands

- `build.rs` panics unless exactly one of `grub`/`uboot` and exactly one of
  `gpt`/`dos` is set.
- The integration tests that use `MockBootEnv` need `test-utils`; without
  it, cargo skips them (`required-features` in Cargo.toml).
- `flash-mode-1` is a default feature. Use `--no-default-features` for a
  build without it, e.g. mode 2 or mode 3 alone.
- Build: `cargo build --features grub,gpt`
- Test: `cargo test --features grub,gpt,test-utils`. The full feature
  matrix and the 32-bit checks are in README.md, section "Testing".
- Lint (the `-W` flags keep items before statements and before the test
  module):

  ```
  cargo clippy --tests --features grub,gpt,factory-reset,test-utils -- \
    -D warnings -W clippy::items_after_statements \
    -W clippy::items_after_test_module
  ```
