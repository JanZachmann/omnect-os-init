# Design: First-Boot Detection

> **Correction (2026-10-26).** A boot with `omnect_validate_update`,
> `omnect_validate_update_failed` or `omnect_bootloader_updated` set, or with
> one of them unreadable, is no first boot. A device updated from an init that
> never wrote the marker therefore does not run the first-boot work again
> (§2.1 "No migration logic" no longer holds). The marker has a second writer:
> `handle_update_validation` sets it on such a boot, before it clears its
> flags (§2.2 "Set once" and §5 "single point" no longer hold).
>
> **Correction (2026-10-27).** With `persistent-var-log`, the init moves the
> logs from `/mnt/data/log`, the location of an older initramfs, to
> `/mnt/data/var/log`. This is a migration of its own (§2.1).

**Date:** 2026-05-27
**Status:** Implemented, partially superseded — see the correction above
**Scope:** omnect-os-init — `bootloader::BootEnvKey` (one variant
add/remove), `lib::run_init` (early detection), `mode::normal::run` (marker
write), `runtime::OdsStatus.first_boot` (new field),
`preflight::resize_data` and `filesystem::resize_data` (read the unified key,
stop writing the old guard).
**Depends on:** Plan A (recovery model), Plan B (fsck/resize policy),
Plan C (degraded-boot signal). This spec changes a clause inside Plan B
(see §6.1).

---

## 1. Problem

Two related defects in today's first-boot handling:

- **Detection is unreliable.** `setup_etc_overlay` infers first-boot from
  `is_directory_empty(&upper_dir)`. The etc-copy is `cp -a`, which populates
  `upper_dir` incrementally; a crash mid-copy leaves a non-empty `upper_dir`,
  and the next boot does not re-detect first boot. The device boots with a
  partially populated `/etc` and never re-copies factory defaults. The
  in-code TODO at `src/filesystem/overlayfs.rs:81` asks for a more robust
  signal but does not specify one.
- **Two markers for one event.** Resize-data already uses
  `BootEnvKey::ResizedData` (`omnect_resized_data`) as its first-boot proxy
  ("absent → must resize"). Layering a second `BootEnvKey::FirstBootDone` on
  top creates two flags for one logical state ("this device has booted at
  least once after flashing"). Two markers means two ways to get out of sync.

---

## 2. Unified sentinel: `omnect_first_boot_done`

### 2.1 Env key

- **New:** `BootEnvKey::FirstBootDone` → `"omnect_first_boot_done"`.
- **Removed:** `BootEnvKey::ResizedData` → `"omnect_resized_data"`.

No migration logic. Devices that OTA from an older initramfs see no
`omnect_first_boot_done` on first boot of the new image, so first-boot
work runs again (etc-copy is idempotent, resize is best-effort and
no-op-ish on an already-full partition). The stale `omnect_resized_data`
env value on field devices becomes dead data — no reader, no harm.

### 2.2 Semantics

- **Absent in the env** (or env unreadable in degraded mode → see §4) →
  `first_boot = true`. This is the device's first successful boot since
  the marker was last clear (i.e. since flashing, or since some future
  factory-reset feature clears it).
- **Present** (any value) → `first_boot = false`.
- Set **once**, at the end of a successful `run_init`. Never read by
  callers other than the detection step (§3) and the resize preflight (§5).

---

## 3. Detection in `run_init`

Read the marker once, early — right after `apply_boot_env_decision` returns
the `BootEnvState`. Set `OdsStatus.first_boot` from the result. Both later
initramfs steps and the ODS JSON share this single value.

```rust
let mut bootloader_env =
    apply_boot_env_decision(decision, core_result, &mut ods_status, rootfs)?;

ods_status.first_boot = compute_first_boot(&bootloader_env);
```

Where `compute_first_boot(env: &BootEnvState) -> bool`:

- `Available(bl)` → `bl.get_env(BootEnvKey::FirstBootDone).unwrap_or(None).is_none()`.
- `Degraded(_)` → `false` (degraded-mode default, §4).
- `get_env` errors that aren't "not present" are conservatively treated as
  "present" (i.e. `first_boot = false`) — same intent as degraded.

---

## 4. Degraded-mode default

When `BootEnvState::Degraded`, the env is unreadable → the marker cannot be
checked. Default: `first_boot = false`. Rationale: ODS-side first-boot effects
(cloud registration, etc.) should not fire under uncertainty; the worst case
of a stale "not first boot" reading is no work done on a degraded boot, which
is consistent with the rest of degraded-mode behavior (Plan C §3.3).

Accepted risk: a fresh device that boots straight into degraded mode never
sets the marker; once the env recovers, the device sees `first_boot = true`
and runs first-boot work then. The pathological case "first boot AND env
permanently broken" is unrecoverable by initramfs alone — same as today.

---

## 5. Marker writer (single point)

The marker is set at the end of a successful boot, **just before
`switch_root`**, in `mode::normal::run`:

```rust
if ods_status.first_boot
    && let Some(bl) = bootloader_env.available_mut()
    && let Err(e) = bl.set_env(BootEnvKey::FirstBootDone, Some("1"))
{
    log::warn!("first-boot marker write failed: {e}; will retry next boot");
}
```

- Best-effort: a write failure is logged and does **not** abort the boot. The
  boot has succeeded; the next boot will retry the set (same work re-runs
  once — idempotent, same accepted cost as the OTA-upgrade case in §2.1).
- Degraded mode: `available_mut()` is `None`, the write is skipped.
- Skipped when `first_boot == false` (idempotent: re-writing the marker is
  harmless but pointless).

---

## 6. Cross-plan changes

### 6.1 Plan B: resize is one-shot, not retry-until-success

Plan B §3.3 ("Idempotency") said: *"Absence of the guard means 'retry next
boot.'"* Under unification, the guard is set after first successful boot
regardless of resize outcome. A resize that was skipped on first boot
(e.g. dirty fsck) is reported once via `ResizeStatus` and is **not retried by
initramfs**. Remediation is ODS/cloud-driven.

Concrete changes inside Plan B's scope:

- `preflight::resize_data` reads `BootEnvKey::FirstBootDone` instead of
  `BootEnvKey::ResizedData`. The semantics flip: today "guard absent → run
  resize"; after this plan "first-boot true → run resize."
- `filesystem::resize_data::resize_if_needed` and its `write_resize_guard`
  helper stop writing the resize guard entirely. The Plan-D marker writer
  (§5) owns the single sentinel.
- Plan B §3.3's idempotency wording is rewritten to reflect "one-shot on
  first boot only" instead of "retried until success."

### 6.2 Other plans

- **Plan A:** unchanged.
- **Plan C:** unchanged; the `degraded_boot` and `first_boot` signals are
  independent ODS JSON fields.

---

## 7. `OdsStatus.first_boot` field

```rust
pub first_boot: bool,
```

- Plain `bool` (not `Option`): every boot has a definite first-boot state.
  `false` is the normal value, not "missing data."
- Serialized **always** (no `skip_serializing_if`). Absence of the key in the
  JSON is itself diagnostic of a bug.
- Default `false` (matches `Default::default()` for `bool`); set in §3.

---

## 8. Code structure

- **`src/bootloader/mod.rs`** — add `BootEnvKey::FirstBootDone` variant and
  its `as_str()` arm (`"omnect_first_boot_done"`); remove
  `BootEnvKey::ResizedData` variant and arm.
- **`src/runtime/omnect_device_service.rs`** — add `first_boot: bool` field
  to `OdsStatus`.
- **`src/lib.rs::run_init`** — call `compute_first_boot` after
  `apply_boot_env_decision`; assign to `ods_status.first_boot`. Helper
  `compute_first_boot` lives in `src/lib.rs` next to `apply_boot_env_decision`
  or in `src/bootloader/mod.rs` near the env key — small enough either way.
- **`src/mode/normal.rs::run`** — add the marker-write block (§5) just
  before `switch_root`.
- **`src/preflight/resize_data.rs`** — switch `get_env(BootEnvKey::ResizedData)`
  to `get_env(BootEnvKey::FirstBootDone)`; the semantics inversion stays inside
  this file.
- **`src/filesystem/resize_data.rs`** — remove the `write_resize_guard`
  helper and the call to it; remove the `omnect_resized_data` set.

No new module. No new external dependency.

---

## 9. Testing

- **Mock env with no marker** → `compute_first_boot(Available(...)) == true`.
- **Mock env with marker present (any value)** → `false`.
- **`BootEnvState::Degraded`** → `false` (degraded default).
- **`get_env` error path** → `false` (defensive default).
- **Successful run with `first_boot == true`** writes
  `BootEnvKey::FirstBootDone` (mock `set_env` call assertion).
- **Successful run with `first_boot == false`** does **not** call `set_env`
  for the key.
- **Marker write failure** is logged and does not abort `mode::normal::run`.
- **`OdsStatus.first_boot` serialization**: JSON always contains
  `"first_boot": true|false`.
- **`BootEnvKey::FirstBootDone.as_str()`** returns `"omnect_first_boot_done"`.
- **`BootEnvKey::ResizedData` is removed** — caught by exhaustive match
  compile errors in `BootEnvKey::as_str`.
- Plan B's existing resize-related tests are updated to use
  `BootEnvKey::FirstBootDone` for the present/absent guard cases.

Action *execution* (the `set_env` call against the real bootloader) is not
unit-tested — Plan A §4 already states this boundary.

---

## 10. Out of scope / follow-ups

- **Atomicity migration of `setup_etc_overlay`.** Tracked as a separate
  follow-up. The etc-copy continues to use the "upper empty" check until
  that follow-up migrates it to the `first_boot` flag and reorders
  copy → `sync` → marker-write so the marker is only set after a durable
  copy.
- **Factory-reset survival.** Design assumes factory-reset will not wipe the
  boot env. Revisit when factory-reset is specified.
- **Two-phase ODS-confirmed sentinel** (the rejected Option C from the
  original first-boot concept doc) — explicitly out of scope.
- **Renaming the env key for clarity** ("omnect_first_boot_done" is already
  clearer than the broadened "omnect_resized_data" would have been;
  considered and chosen).

---

## 11. Open questions

None blocking.
