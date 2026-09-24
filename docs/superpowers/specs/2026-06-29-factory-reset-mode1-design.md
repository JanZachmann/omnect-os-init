# Factory Reset Mode 1 — Design

> **Correction (2026-09-21).** Two statements below have been superseded. A
> trigger the init cannot parse no longer boots on as Normal: it is cleared and
> reported (status 1 for a `mode` problem, status 3 for a `preserve` problem),
> and `preserve` is mandatory rather than defaulted. A file in
> `factory-reset.d` without a usable `paths` array now fails the reset instead
> of being skipped. The payload of `BootMode::FactoryReset` is
> `FactoryResetTrigger`, not `FactoryResetConfig`. See
> `2026-08-11-factory-reset-modes-2-3-4-design.md` §1.2.

**Date:** 2026-06-29
**Status:** Approved

## 1. Overview

Factory reset mode 1 (backup / reformat / restore) is the next boot mode to be ported from the legacy bash scripts into the Rust initramfs binary. When ODS writes a factory-reset trigger to the bootloader environment, the initramfs detects it at next boot, performs the selective-preserve reset sequence, and then continues into Normal boot. The device always boots regardless of reset outcome.

This design covers mode 1 only. Wipe modes 2–4 are a separate future spec.

## 2. Architecture

Factory reset is a new `BootMode::FactoryReset(FactoryResetConfig)` variant alongside the existing `BootMode::Normal`. Detection happens in `BootMode::detect()` by reading `BootEnvKey::FactoryReset` from the bootloader env. When the key is present with a valid JSON value, the initramfs executes the reset sequence and then falls through to Normal boot. All errors are `ContinueDegraded` — the device always boots.

### 2.1 Source of truth for existing types

`FactoryResetStatus` and `FactoryResetStatusCode` are already defined in `src/runtime/omnect_device_service.rs` and embedded in `OdsStatus`. The new implementation writes status via `ods_status.set_factory_reset(status)` rather than a separate `/tmp/factory-reset.json` file.

## 3. Component Changes

### 3.1 `src/bootloader/mod.rs`

Add a new `BootEnvKey` variant, feature-gated:

```rust
#[cfg(feature = "factory-reset")]
/// `factory-reset` — JSON trigger set by ODS to request a factory reset.
/// Value is a JSON string: `{"mode":1,"preserve":["applications","network"]}`.
FactoryReset,
```

`as_str()` mapping: `Self::FactoryReset => Cow::Borrowed("factory-reset")`

### 3.2 `src/error.rs`

Add a new subsystem error enum:

```rust
#[derive(Error, Debug)]
pub enum FactoryResetError {
    #[error("Invalid factory-reset config: {0}")]
    InvalidConfig(String),
    #[error("Missing field in factory-reset config: {0}")]
    MissingField(String),
    #[error("Backup failed for {}: {reason}", path.display())]
    BackupFailed { path: PathBuf, reason: String },
    #[error("Restore failed for {}: {reason}", path.display())]
    RestoreFailed { path: PathBuf, reason: String },
    #[error("Reformat failed for {}: {reason}", device.display())]
    ReformatFailed { device: PathBuf, reason: String },
    #[error("Mount error: {0}")]
    MountError(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
```

Add to `InitramfsError`:

```rust
#[cfg(feature = "factory-reset")]
#[error("Factory reset error: {0}")]
FactoryReset(#[from] FactoryResetError),
```

Add to `recovery_class()`:

```rust
#[cfg(feature = "factory-reset")]
Self::FactoryReset(_) => RecoveryClass::ContinueDegraded,
```

Factory reset errors are always `ContinueDegraded`: a failed reset must not prevent the device from booting.

### 3.3 `src/mode/mod.rs`

Add a new `BootMode` variant:

```rust
#[cfg(feature = "factory-reset")]
FactoryReset(factory_reset::config::FactoryResetConfig),
```

Extend `BootMode::detect()`:

```rust
#[cfg(feature = "factory-reset")]
if let Some(bl) = bl {
    match bl.get_env(BootEnvKey::FactoryReset) {
        Ok(Some(json)) => match factory_reset::config::FactoryResetConfig::parse(&json) {
            Ok(config) => return Ok(Self::FactoryReset(config)),
            Err(e) => warn!("factory-reset: invalid config JSON, booting normally: {e}"),
        },
        Ok(None) => {}
        Err(e) => warn!("factory-reset: failed to read env, booting normally: {e}"),
    }
}
```

Detection falls back to `Normal` on any env-read or parse error — never blocks boot.

### 3.4 `src/mode/factory_reset/` (new module)

Four submodules:

#### `mod.rs` — orchestration

```
pub fn run(mut ctx: BootContext<'_>, config: FactoryResetConfig) -> Result<()>
```

Sequence:
1. Clear `BootEnvKey::FactoryReset` from bootloader env (best-effort; warn on failure).
2. Call `run_reset(&ctx, &config)`.
3. On error: log warning, write error `FactoryResetStatus` to `ctx.ods_status`; proceed to Normal.
4. On success: write success `FactoryResetStatus` to `ctx.ods_status`.
5. Always: `mode::normal::run(ctx)`.

Inner `run_reset()`:
1. Validate `config.mode == 1`; return `Err(FactoryResetError::InvalidConfig)` otherwise.
2. Mount: factory (ro, if present), etc (rw), data (rw) + overlays. Track mounts in a `Vec<PathBuf>` for cleanup.
3. `build_preserve_list()` → list of paths (reads config from mounted rootfs).
4. `backup_all()` → backup to `/tmp/factory_reset/backup/`.
5. `umount` all.
6. `reformat_ext4(data_dev, "data")`.
7. `reformat_ext4(etc_dev, "etc")`.
8. Re-mount: factory (ro, if present), etc (rw), data (rw) + overlays. Factory must
   be included here too — the reformat wiped etc's overlay upper dir, and
   `setup_etc_overlay` needs factory mounted to reseed the now-empty upper dir with
   factory `/etc` defaults before `restore_all` overlays the preserved paths on top.
   (Earlier revision of this spec excluded factory here; that was an oversight —
   omitting it leaves the upper dir permanently unseeded once any preserved path
   populates it, since the empty-upper-dir seed check in `setup_etc_overlay` only
   fires once.)
9. `restore_all()` → `RestoreResult`.
10. `umount` all.

On any error in steps 2–10: return `Err` (cleanup via `inspect_err` + umount).

Access to bootloader: `ctx.boot_env.available_mut()`.

#### `config.rs` — configuration

```rust
pub struct FactoryResetConfig {
    pub mode: u32,
    #[serde(default)]
    pub preserve: Vec<String>,
}
```

`build_preserve_list(config, rootfs)`:
- Mandatory entry: `"/etc/omnect/factory-reset.d/"` (always preserved).
- For `"applications"` in `preserve`: scan `<rootfs>/etc/omnect/factory-reset.d/*.json`, collect `paths` arrays.
- For other keys: read `<rootfs>/etc/omnect/factory-reset.json`, look up the key as a JSON array of strings.

Config files read from the mounted rootfs (`ctx.rootfs`).

#### `backup_restore.rs` — backup and restore

`backup_all(rootfs, preserve_list, backup_dir)`:
- `mkdir -p backup_dir`
- For each path: `cp --parents -a <rootfs>/<path> <backup_dir>`; skip if source doesn't exist.
- `sync` after each copy.

`restore_all(rootfs, preserve_list, backup_dir) -> Result<RestoreResult>`:
- For each path: `mkdir -p <dest_parent>`; `cp -a <backup>/<path> <dest_parent>`.
- Partial failures are logged and accumulated; returns `RestoreResult::PartialFailure` with context string if any path failed, `RestoreResult::Success` otherwise.
- `sync` after each restore.

#### `reformat.rs` — partition reformatting

`reformat_ext4(device, label)`:
1. `mkfs.ext4 -F -q <device>`
2. `tune2fs <device> -c -1 -i 0 -L <label>`

### 3.5 `src/runtime/omnect_device_service.rs`

Remove `copy_factory_reset_status()` and `FACTORY_RESET_STATUS_FILE` — both become dead code once the factory reset mode writes directly to `OdsStatus`. The `factory_reset` field in `OdsStatus` is already serialized into `omnect-os-initramfs.json`, which ODS reads after `switch_root`.

`FactoryResetStatus` and `FactoryResetStatusCode` remain as the canonical types.

### 3.6 `src/lib.rs`

Extend the `BootMode` dispatch in `run_init()`:

```rust
match BootMode::detect(ctx.boot_env.available())? {
    BootMode::Normal => mode::normal::run(ctx),
    #[cfg(feature = "factory-reset")]
    BootMode::FactoryReset(config) => mode::factory_reset::run(ctx, config),
}
```

### 3.7 `Cargo.toml`

Add feature flag:

```toml
[features]
factory-reset = ["core"]
```

## 4. Data Flow

```
run_init()
  → BootMode::detect(bootloader_env)
      → get_env("factory-reset")
      → Some(json) → parse FactoryResetConfig → BootMode::FactoryReset(config)
      → None       → BootMode::Normal
      → parse err  → log warn → BootMode::Normal

BootMode::FactoryReset(config):
  1. clear "factory-reset" bootloader var (best-effort)
  2. mount factory(ro) + etc(rw) + data(rw) + overlays
  3. build preserve_list (reads config files from mounted rootfs)
  4. backup preserve_list → /tmp/factory_reset/backup/
  5. umount
  6. reformat data partition (mkfs.ext4 + tune2fs)
  7. reformat etc partition  (mkfs.ext4 + tune2fs)
  8. re-mount factory(ro) + etc(rw) + data(rw) + overlays
  9. restore preserve_list from backup
  10. umount
  11. ods_status.set_factory_reset(FactoryResetStatus::success(...))
  12. → mode::normal::run(ctx)

On error in any step 2–10:
  → log warning
  → ods_status.set_factory_reset(FactoryResetStatus::error(...))
  → → mode::normal::run(ctx)    ← always boots
```

## 5. Error Handling

| Error source | Handling |
|---|---|
| Bootloader env read failure | Log warn → Normal boot |
| Config JSON parse failure | Log warn → Normal boot |
| Unsupported mode | Log warn → error status → Normal boot |
| Mount failure | Log warn → error status → Normal boot |
| Backup failure | Log warn → error status → Normal boot (umount best-effort) |
| Reformat failure | Log warn → error status → Normal boot |
| Restore partial failure | Log warn → partial-error status → Normal boot |
| Bootloader env clear failure | Log warn → continue |

`FactoryReset(_) → RecoveryClass::ContinueDegraded` — factory reset errors never reach `handle_fatal_error`.

## 6. Status Reporting

`FactoryResetStatus` (already in `OdsStatus`):

```json
{
  "factory_reset": {
    "status": 0,
    "paths": ["/etc/omnect/factory-reset.d/", "/etc/network/interfaces"]
  }
}
```

Error example:

```json
{
  "factory_reset": {
    "status": 2,
    "error": "cp failed (exit 1): ...",
    "context": "etc/hostname:restore",
    "paths": ["/etc/omnect/factory-reset.d/"]
  }
}
```

Serialized into `omnect-os-initramfs.json` by `create_ods_runtime_files()`. No separate tmp file.

## 7. Feature Flag

All factory-reset code is gated behind `#[cfg(feature = "factory-reset")]`. The four test suites remain valid:

```
cargo test --features grub,gpt,factory-reset
cargo test --features grub,dos,factory-reset
cargo test --features uboot,gpt,factory-reset
cargo test --features uboot,dos,factory-reset
```

Existing combinations without `factory-reset` continue to compile and pass.

## 8. Testing

| Test | Kind | Location |
|---|---|---|
| `FactoryResetConfig::parse` — valid JSON | Unit | `config.rs` |
| `FactoryResetConfig::parse` — missing mode field | Unit | `config.rs` |
| `build_preserve_list` — mandatory entry only | Unit | `config.rs` |
| `build_preserve_list` — applications key | Unit | `config.rs` |
| `build_preserve_list` — custom keys | Unit | `config.rs` |
| `backup_all` — nonexistent source silently skipped | Unit | `backup_restore.rs` |
| `restore_all` — nonexistent backup silently skipped | Unit | `backup_restore.rs` |
| `restore_all` — partial failure accumulates context | Unit | `backup_restore.rs` |
| `BootMode::detect` — key absent → Normal | Unit | `mode/mod.rs` |
| `BootMode::detect` — key present valid JSON → FactoryReset | Unit | `mode/mod.rs` |
| `BootMode::detect` — key present invalid JSON → Normal | Unit | `mode/mod.rs` |
| `recovery_class` — FactoryReset → ContinueDegraded | Unit | `error.rs` |
| `factory_reset_umount` — empty list | Unit | `mode/factory_reset/mod.rs` |
| Full reset sequence | Manual (hardware) | — |
