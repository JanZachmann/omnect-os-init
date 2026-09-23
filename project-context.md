# Project Context

## 1. Architecture & Tech Stack
- **Role:** Initramfs init process for omnect Secure OS
- **Runtime:** Runs as PID 1 in initramfs before switch_root
- **Language:** Rust (safety-critical, no_std-friendly patterns)
- **Target:** Embedded Linux (x86-64 EFI with GRUB, ARM with U-Boot)

## 2. Module Structure

```
src/
├── main.rs                  # Binary entry point
├── lib.rs                   # Library exports + run_init() + apply_boot_env_decision()
├── error.rs                 # Error type hierarchy
├── early_init.rs            # Mount /dev, /proc, /sys, /run before logging
├── bootloader/
│   ├── mod.rs               # BootEnv trait, BootEnvState, classify_boot_env()
│   ├── grub.rs              # GRUB implementation (grub-editenv)
│   ├── uboot.rs             # U-Boot implementation (fw_printenv/fw_setenv)
│   └── types.rs             # BootloaderType enum
├── config/
│   └── mod.rs               # /proc/cmdline parser; build-time constants via build.rs
├── filesystem/
│   ├── mod.rs               # Public API
│   ├── boot_sequence.rs     # Mount + fsck orchestration (testable with mock bootloaders)
│   ├── fsck.rs              # e2fsck wrapper (all exit codes handled)
│   ├── mount.rs             # Mount primitives (RAII, idempotency checks)
│   ├── overlayfs.rs         # /etc overlay, /home overlay, bind mounts
│   └── resize_data.rs       # Data partition auto-resize on first boot (feature = resize-data)
├── logging/
│   ├── mod.rs               # KmsgLogger initializer
│   └── kmsg.rs              # /dev/kmsg writer with kernel log levels
├── mode/
│   ├── mod.rs               # BootMode enum, FactoryResetTrigger, BootContext, detect()
│   ├── normal.rs            # Normal boot handler (post-mount overlays → switch_root)
│   └── factory_reset/       # Factory reset (feature = factory-reset)
│       ├── mod.rs           # Reset sequence, status assembly, trigger rejection
│       ├── config.rs        # Trigger parsing, preserve list from etc/omnect
│       ├── backup_restore.rs # Preserve-list backup to initramfs RAM and restore
│       ├── reformat.rs      # mkfs.ext4 + tune2fs
│       └── wipe.rs          # Mode 2 random overwrite, mode 3 BLKDISCARD
├── partition/
│   ├── mod.rs               # Public API
│   ├── device.rs            # Root device detection (GRUB: blkid/fsuuid, U-Boot: root=)
│   ├── layout.rs            # GPT/DOS partition map builder
│   └── symlinks.rs          # /dev/omnect/* symlink creation
├── init_setup/
│   ├── mod.rs               # Init setup step runner
│   ├── extra_bootargs.rs    # omnect_extra_bootargs sync
│   └── resize_data.rs       # resize-data step: guard check + degraded-mode dispatch
└── runtime/
    ├── mod.rs               # Public API
    ├── fs_link.rs           # fs-link symlink creation
    ├── omnect_device_service.rs  # ODS JSON status file writer
    └── switch_root.rs       # MS_MOVE + chroot transition to real rootfs; execs init
```

## 3. Build & Test Commands
- **Build:** `cargo build` / `cargo build --release`
- **Check:** `cargo check`
- **Format:** `cargo fmt -- --check`
- **Lint:** `cargo clippy --tests --features <grub|uboot> -- -D warnings -W clippy::items_after_statements -W clippy::items_after_test_module`
- **Test:** `test-utils` must be included for the `degraded_boot` integration test; the
  `factory_reset` one additionally needs the `factory-reset` feature. Base combinations:
  ```
  cargo test --features grub,gpt,test-utils
  cargo test --features grub,dos,test-utils
  cargo test --features uboot,gpt,test-utils
  cargo test --features uboot,dos,test-utils
  cargo test --features grub,gpt,resize-data,test-utils
  cargo test --features grub,dos,resize-data,test-utils
  cargo test --features uboot,gpt,resize-data,test-utils
  cargo test --features uboot,dos,resize-data,test-utils
  cargo test --features grub,gpt,release-image,test-utils
  cargo test --features grub,dos,release-image,test-utils
  cargo test --features uboot,gpt,release-image,test-utils
  cargo test --features uboot,dos,release-image,test-utils
  cargo test --features grub,gpt,resize-data,release-image,test-utils
  cargo test --features uboot,gpt,resize-data,release-image,test-utils
  ```
- **Audit:** `cargo audit`

## 4. Feature Flags
| Feature | Purpose |
|---------|---------|
| `core` | Default, required functionality |
| `grub` | GRUB bootloader support (mutually exclusive with `uboot`) |
| `uboot` | U-Boot bootloader support (mutually exclusive with `grub`) |
| `gpt` | GPT partition table (primary partitions 1-7; mutually exclusive with `dos`) |
| `dos` | DOS/MBR partition table (extended at slot 4, logical 5-8; mutually exclusive with `gpt`) |
| `persistent-var-log` | Persistent `/var/log` mount |
| `release-image` | Release behaviour: loop on fatal error; continue booting in degraded mode |
| `resize-data` | Expand data partition + filesystem to fill disk on first boot |
| `factory-reset` | Factory reset, modes 1-3: backup → wipe (2 and 3) → reformat → restore |
| `test-utils` | Expose `MockBootEnv` for integration tests — never enabled in production builds |

## 5. Runtime Constraints
- **Heap allocation is used freely** (`String`, `PathBuf`, `HashMap`); the OS image provides a standard allocator
- **Read-only rootfs:** All state goes to `/data` or bootloader env
- **Logging:** Available only after `/dev` is mounted
- **Exit behavior:**
  - Release image + normal error: infinite loop (prevent reboot loops)
  - Release image + degraded boot (bootloader unavailable): continue booting; set `degraded_boot: true` in ODS JSON
  - Debug image + degraded boot: abort immediately before init setup; spawn debug shell
  - `FsckRequiresReboot`: always triggers a reboot regardless of degraded state

## 6. Key Patterns
- **Error handling:** `thiserror` for typed errors, `Result<T>` everywhere
- **Bootloader abstraction:** `dyn BootEnv` trait for GRUB/U-Boot
- **Degraded boot:** `BootEnvState` is either `Available(Box<dyn BootEnv>)` or `Degraded(BootEnvError)`. `classify_boot_env()` decides which based on the open result and `is_release`. `apply_boot_env_decision()` enforces the invariant that `FsckRequiresReboot` always propagates before `DegradedBoot`.
- **Compression:** fsck exit code and full output stored in bootloader env as gzip+base64(`"exit_code\noutput"`); full output also written to `/data/var/log/fsck/<partition>.log`
- **Idempotent mounts:** `is_mounted()` check before mounting
- **No magic path strings:** All filesystem paths must be `const` values. Group related paths in a dedicated `pub mod mount_points` (or equivalent) rather than using inline string literals.
- **File organization:** `use`, `const`, `static`, and `type` declarations must appear at the top of the file, before any `fn`, `impl`, `struct`, or `enum` definitions. Exceptions: `use super::*` and imports inside `#[cfg(test)] mod tests` blocks are placed within those blocks.

## 7. Integration Points
- **Kernel cmdline:** `rootpart=` (GRUB: root partition number), `bootpart_fsuuid=` (GRUB: boot partition UUID), `root=` (U-Boot: full root device path), `init=` (optional init binary override), `quiet` (suppress console output); `rootblk=` is parsed for device symlink naming only — no logic reads it
- **Device symlinks:** Creates `/dev/omnect/{boot,rootfs,data,...}`
- **ODS:** Prepares runtime files for `omnect-device-service`

## 8. Planned Features (not yet implemented)

### BootMode variants
The `BootMode` enum (`src/mode/mod.rs`) has the following implemented variants:
- `Normal` — standard boot path; also used when the bootloader is unavailable (degraded boot)
- `FactoryReset(FactoryResetTrigger)` — backup → wipe (modes 2 and 3) → reformat → restore
  (feature `factory-reset`). The trigger is carried even when its value is unusable, as
  `FactoryResetTrigger::Rejected`, so the value is cleared and the failure reported instead
  of the boot continuing in silence.

Data partition resize (feature = `resize-data`) is handled as an init setup step in
`src/init_setup/resize_data.rs`, not as a separate `BootMode` variant. It runs before
`BootMode::detect()` and handles both the live-bootloader (guard check) and degraded-boot
(no guard, resize runs every boot) cases.

The following variants are planned:
- `FlashMode(FlashKind)` — enables in-field OS flashing

When implementing a new variant:
1. Add the variant to `BootMode` and update `BootMode::detect()` to read the relevant bootloader env key. If the key is absent or the bootloader is unavailable, `detect()` must return `Normal` (degraded boot). A key that is present but unusable belongs to its own mode, which clears it and reports the failure — see `FactoryResetTrigger`.
2. Add typed payload structs as needed (define them in `src/mode/mod.rs` near the `BootMode` enum).
3. Add `BootEnvKey` entries for the detection keys.
4. Add a handler module under `src/mode/` mirroring `src/mode/normal.rs`.
5. Cover in tests: env-var present + live bootloader, env-var present + no bootloader (degraded fallback to `Normal`), env-var absent.

## 9. Documentation Standards

### Source-code comments and doc-strings
- **Explain "why", not "what":** The code shows what it does; comments explain constraints, non-obvious rationale, or invariants.
- **No history in comments:** Do not reference previous implementations ("legacy bash", "previously this was…"), PR numbers, or merge order.
- **No forward scaffolding in comments:** Do not describe features not yet implemented in the same comment block. Track planned work in section 8 of this file instead.
- **Concise doc-strings:** A doc-string should be as long as it needs to be and no longer. Avoid restating the function signature or obvious behaviour.