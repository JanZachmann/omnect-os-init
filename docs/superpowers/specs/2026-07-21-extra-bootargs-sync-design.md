# Design: `omnect_extra_bootargs` Sync in the Initramfs

> **Correction (2026-10-26).** The `update_pending == false` gate below is
> removed. `first_boot` is now `false` on every boot after an update, including
> the validation boot of a device that comes from the legacy init, so
> `first_boot` alone gives the same protection.

**Date:** 2026-07-21
**Status:** Approved
**Scope:** omnect-os-init — `init_setup::extra_bootargs`, `error`, `recovery`,
`runtime::OdsStatus`, `bootloader` (`BootEnvKey::ExtraBootArgs`).
**Depends on:** Boot Failure & Recovery Policy — reuses its `RebootToApply`
recovery class.

---

## 1. Problem

On a freshly flashed device the bootloader env `omnect_extra_bootargs` is empty
or stale, while the boot partition already carries the intended arguments in
`omnect_extra_bootargs_omnect` (and optionally `omnect_extra_bootargs_custom`).
The bootloader (GRUB `linux` line, U-Boot `bootargs`) injects
`omnect_extra_bootargs` into the kernel cmdline at boot time, so the running
kernel booted without the intended arguments. These arguments are
security-critical (e.g. LSM/AppArmor settings), so the device must not run a
full session without them.

The legacy bash init (`meta-omnect` `common-sh`) handled this: on first boot it
built the combined value, wrote it to the env, and forced an immediate
`reboot -f` (after `sync`) so the correct arguments applied from the next boot.

The initial Rust port (`src/init_setup/extra_bootargs.rs` on
`feat/sync-extra-bootargs`) syncs the value but:

- runs on **every** boot with no first-boot / update-pending gate,
- never reboots, so the arguments are not applied for the current session,
- never verifies the write or calls `sync()`.

Two defects follow:

- **OTA rollback can break.** During an OTA validation boot the bootloader sets
  `omnect_extra_bootargs` transiently from `omnect_validate_extra_bootargs` and
  deliberately does **not** `save_env` it, so a failed validation rolls back
  cleanly. An every-boot sync would see the new boot files versus the still-old
  persistent env value and persist the new value early — defeating the rollback
  safety.
- **Fresh-flash arguments never apply immediately** — they wait for the next
  unrelated reboot.

## 2. Non-goals / relationship to OTA

The OTA path is **out of scope** and stays as-is. OTA argument changes are
handled entirely by the swupdate handler
(`swupdate_handler_v2_{grub,uboot}.sh`, which sets
`omnect_validate_extra_bootargs`) plus the bootloader validate mechanism (copies
validate → active for the validation boot without `save_env`; persists on
success, reverts on rollback). The general userspace tool
`/usr/bin/omnect_extra_bootargs.sh` covers changes during normal operation.

The initramfs sync covers **only the fresh-flash case** — the one path with no
prior swupdate-handler run.

## 3. Overview

`init_setup::extra_bootargs` runs as the first init-setup step (before
`resize_data`). On the fresh-flash boot only, it builds the combined argument
value, and if it differs from the bootloader env it writes the value, verifies
it, flushes it to disk, and requests a reboot so the arguments apply from the
next boot. Any failure other than the intended reboot is best-effort: it is
logged, recorded in ODS status, and the boot continues without a reboot.

## 4. Gate — when the sync runs

The sync runs only when all hold:

- `first_boot == true`. `first_boot` is derived from the absence of
  `BootEnvKey::FirstBootDone`, which omnect-os-init writes once at the end of
  `normal::run` and never clears. For a device that already ran the Rust init at
  least once, the marker is present, so `first_boot` is `false` on every later
  boot — including OTA validation boots on such devices.
- `update_pending == false`. This is **load-bearing, not defence-in-depth**.
  A device migrating from the legacy bash init via OTA boots its first Rust-init
  boot — the update validation boot — with `omnect_first_boot_done` absent (the
  legacy image never set it), so `first_boot == true` there. In that boot only
  `update_pending` stops the step from persisting bootargs mid-validation, which
  would defeat the rollback safety §1 describes. Do not remove this check. Read
  once into the process-global `UPDATE_PENDING` in `run_init`.
- The bootloader env being `Available` is implied by `first_boot == true`, not a
  separate gate: `compute_first_boot` returns `false` on a degraded env, so a
  degraded env never reaches the write. `run()` keeps a defensive `available_mut`
  check that logs and returns, but it cannot occur in production.

`first_boot` stays `true` across the sync reboot: the marker is written only in
`normal::run`, which runs after `init_setup`, so a reboot in `init_setup`
happens before the marker is written. This is intentional — the gate stays open
until the value converges. The gate is therefore **not** the loop protection
(see §7).

## 5. Building the value

Read `omnect_extra_bootargs_omnect` and the optional
`omnect_extra_bootargs_custom` from the mounted boot partition (`rootfs/boot`),
join them, and squeeze whitespace runs to single spaces (matching the legacy
`awk '{$1=$1};1'`, not only trimming the ends). Missing or empty files
contribute nothing; both absent yields the empty string. The squeeze normalizes
irregular whitespace in hand-edited files so the built value is stable across
boots and the change comparison is reliable.

## 6. Change flow

When the built value differs from the current env value:

1. `set_env(BootEnvKey::ExtraBootArgs, Some(new))`, or `set_env(.., None)` when
   the built value is empty.
2. **Read-back verify:** read the key again; it must equal the built value.
3. **`sync()`** — flush the grubenv write to disk.
4. Request a reboot (§8).

When the value is already current: no-op, no reboot.

The boot partition is mounted read-write before init-setup runs
(`mount_core_partitions`), and `grub-editenv` runs as root, so it can update the
grubenv file.

## 7. Loop and brick protection

Two independent guards, both required:

- **Read-back verify** guards against the reboot loop — if the stored value
  reads back different from what was written, rebooting could never converge
  (`current != new` forever). On a mismatch the step does **not** reboot; it
  records the failure (§9) and returns without a reboot. It does **not** roll
  back the write: `grub-editenv`/`fw_setenv` store the value verbatim, so a
  mismatch means a genuine write fault where the previous value is no better,
  and rolling back would only add a second write per boot. A persistent mismatch
  therefore rewrites every boot and reports the failure until reflash — the same
  accepted retry cost as §9.
- **`sync()`** guards against a lost write. `reboot(2)` with `RB_AUTOBOOT` is a
  hard reboot and does not flush filesystem buffers; without `sync()` the
  grubenv write could be lost across the reboot, so the next boot would see the
  old value and reboot again — an endless loop. The legacy script called `sync`
  before `reboot -f` for the same reason.

Read-back verify alone is not enough: it reads the page cache and can succeed
while nothing is on disk yet. Durability comes from `sync()`.

These two guards **mitigate** the loop; they do not **bound** it. They remove
two specific loop causes (normalization mismatch, lost page-cache write). One
residual remains: if storage acks `sync()` but silently drops the write (worn
eMMC/SD, vfat corruption), the device rewrites-verifies-syncs-reboots forever.
Legacy did not loop here — its first-boot gate closed after one attempt
regardless of the write outcome. This design trades "give up after one try" for
"retry until the value converges" and accepts the residual unbounded-reboot
risk, the same class of accepted risk as the fsck reboot loop. A hard bound
(e.g. a reboot counter in the env) is out of scope here.

## 8. Reboot signaling

The step does not reboot directly — the reboot convention keeps that in
`main.rs`. It reuses the existing non-failure reboot class:

- New `InitramfsError` variant `ExtraBootArgsUpdated`.
- `recovery_class()` maps it to `RecoveryClass::RebootToApply` (the exhaustive
  match makes adding the variant a compile-time obligation).
- `recovery::decide` already maps `RebootToApply → Action::Reboot`, executed in
  `main::handle_fatal_error`.

`extra_bootargs::run` returns `Result<()>`: `Err(ExtraBootArgsUpdated)` after a
verified and synced write, `Ok(())` otherwise. `init_setup::run` and `run_init`
propagate it.

The bootloader does not bound this reboot (a fresh-flash device has no rollback
slot). This matches the existing fsck `RebootToApply` case, which is also
unbounded — only the OTA-rollback reboot is bounded. Read-back verify plus
`sync()` mitigate the loop but do not bound it (§7). The `RebootToApply` doc
comment notes all reboot reasons and that this one carries an accepted residual
loop risk.

## 9. Failure handling — security-first, retry on failure

The arguments are security-critical (§1), so a failed sync must not silently
close the first-boot gate and leave the device running without them forever.

A read error on a boot-partition argument file (any error other than "file
absent") is a failure too: the args are security-relevant, so a flaky read must
not be treated as "no args" and silently drop them.

On a failure (bootargs-file read error, `set_env` error, read-back mismatch, or
current-value read error):
- the step logs, records the failure in ODS, and returns `Ok(())` (no reboot,
  boot continues so the device is reachable for diagnosis/reflash), **and**
- `normal::run` does **not** write the `FirstBootDone` marker on this boot, so
  `first_boot` stays `true` and the sync **retries on the next boot**.

This is how the degraded-env case already behaves (the marker write itself fails
on a degraded env), so a failed sync just extends the same retry behavior.

The rest of the boot proceeds: the step returns `Ok(())`, so `resize_data` still
runs and completes on this boot. Only the first-boot marker is withheld; on the
retry boots resize re-runs and no-ops (it is idempotent).

Accepted cost: if the failure never clears (persistent env-write fault), the
device retries the sync every boot and never completes first-boot marking. It
stays reachable and reports the failure in ODS on every boot, which is the
signal for the cloud to reflash or alert. This is the deliberate
security-over-availability tradeoff for these arguments.

## 10. ODS status — new entry

Following the `resize_data` pattern in both respects: recorded **only on
failure** (`None` on success/no-op/not-run), and carrying a typed failure kind
plus a free-text reason so cloud consumers can match the kind exactly.

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub extra_bootargs: Option<ExtraBootArgsStatus>,

pub struct ExtraBootArgsStatus {
    pub outcome: ExtraBootArgsOutcome,
    pub reason: String, // one-line detail for operator diagnosis
}

// snake_case; failure kinds only — the struct exists only on failure.
pub enum ExtraBootArgsOutcome {
    ReadFailed,       // reading the current value failed (before any write)
    SetEnvFailed,     // writing the env value failed
    ReadBackFailed,   // the read-back after a successful write failed (unverified)
    ReadBackMismatch, // stored value read back different from what was written
}
```

Setter `set_extra_bootargs_status`. `Some` always means the sync failed on this
boot. The successful apply is not recorded — it reboots before `normal::run`
writes the JSON, and the next boot is a no-op (`None`) anyway; the reboot is
visible only in kmsg. A degraded env cannot reach the step (§4), so there is no
degraded status either.

## 11. Testing

- `read_extra_bootargs`: none / omnect-only / custom-only / both / empty; plus
  internal-whitespace squeeze (`"a   b"` → `"a b"`).
- Gate: `first_boot == false` → skip; `update_pending == true` → skip.
- Change path: differing value → `set_env` + read-back + `Err(ExtraBootArgsUpdated)`,
  ODS `extra_bootargs` stays `None`.
- Read-back mismatch (mock stores a different value) → no `Err`, `extra_bootargs`
  outcome is `ReadBackMismatch`, no reboot.
- `current == new` → no-op, no `Err`, `extra_bootargs` stays `None`.
- `recovery_class`: `ExtraBootArgsUpdated → RebootToApply` (exhaustive-match test).
- Marker skip: with `extra_bootargs = Some` (failure), `normal::run` does not
  write `FirstBootDone`; with `None` it does (when `first_boot` and resize allow).

## 12. Comparison to legacy

| Aspect | Legacy (bash) | This design |
|---|---|---|
| Value build | omnect + optional custom, whitespace squeezed | same |
| When | first boot only (`/mnt/etc/upper` missing) | first boot only (`first_boot` env marker) |
| OTA validation boot | not run (first-boot gate) | not run (`!update_pending`; also `first_boot` false on non-migrated devices) |
| On change | `set_env` + `sync` + `reboot -f` | `set_env` + verify + `sync` + `RebootToApply` |
| Durability | `sync` before reboot | `sync` before reboot |
| Loop protection | first-boot gate closed after reboot (gives up) | read-back verify + `sync` (mitigate, retry until converged; accepted residual) |
| Failure | logged | logged + ODS entry (failure only); marker not written, retried next boot |

Note: on a fresh flash the env starts empty while the boot files carry the
arguments, so the first boot always costs one extra reboot to apply them. Same
as legacy. This could be removed later by pre-seeding the env at flash time
(omnect-cli); out of scope here.
