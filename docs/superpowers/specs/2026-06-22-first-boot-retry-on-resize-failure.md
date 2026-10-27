# Design: Suppress `FirstBootDone` marker when resize fails

> **Correction (2026-10-26).** The retry also ends with an update: a boot after
> an update is no first boot and sets the marker, so a resize that failed on
> every boot since flashing is not tried again. The marker is in the shared
> env, so a rollback does not bring the retry back. Without a readable boot env
> the resize runs on every boot, whatever `first_boot` says.

**Date:** 2026-06-22
**Status:** Draft
**Scope:** `src/mode/normal.rs` only — one conditional added to `write_first_boot_marker` call.
**Depends on:** Plan D (first-boot detection, `omnect_first_boot_done` sentinel).

---

## 1. Problem

After Plan D, `write_first_boot_marker` is called unconditionally at the end of a successful
`mode::normal::run`, regardless of whether the resize-data init setup step succeeded. If
resize fails (error absorbed by `handle_result`), the marker is still written, so the next
boot sees `first_boot=false` and never retries the resize. The failure is reported once via
`OdsStatus.resize_data` and then lost.

## 2. Design

Suppress the marker write when resize was attempted and failed. The existing
`ods_status.resize_data` field is the signal: `Some(...)` means a failure was absorbed;
`None` means no failure (resize succeeded, or was skipped because `first_boot=false`).

### Change in `mode::normal::run`

```rust
#[cfg(feature = "resize-data")]
let resize_ok = ods_status.resize_data.is_none();
#[cfg(not(feature = "resize-data"))]
let resize_ok = true;

write_first_boot_marker(ods_status.first_boot && resize_ok, &mut boot_env);
```

`write_first_boot_marker` signature is unchanged. No other files change.

### Behavior table

| Scenario | `resize_data` | `first_boot` | Marker written? | Next boot |
|---|---|---|---|---|
| Resize succeeded | `None` | `true` | Yes | `first_boot=false`, done |
| Resize failed (any absorbed outcome) | `Some(...)` | `true` | No | `first_boot=true`, retries |
| Not first boot | `None` | `false` | No (unchanged) | `first_boot=false` |
| Degraded boot | `None` | `false` | No (unchanged) | same |
| `resize-data` feature off | N/A | `true` | Yes | `first_boot=false`, done |
| Marker write fails (best-effort) | `None` | `true` | attempted, logged | `first_boot=true`, retries |

`FsckRequiresReboot` is independent: it propagates before `write_first_boot_marker` is
reached, so it is unaffected.

### Retry semantics

On the next boot after a resize failure:
- `first_boot=true` again → resize is attempted again
- `setup_etc_overlay` also runs the factory-etc copy again (idempotent, accepted)
- If resize succeeds → marker is written → no further retries

Retry loop terminates once resize succeeds or a new image is flashed.

### Feature gate

When `resize-data` is not compiled in, `resize_ok = true` unconditionally, preserving
current behavior for non-resize builds.

---

## 3. Absorbed failure outcomes that block the marker

All three absorbed `ResizeOutcome` variants block the marker write:

- `ToolError` — external tool (parted/sgdisk/resize2fs) failed; transient, retry is sensible
- `SkippedFsck` — fsck reported uncorrected errors; partition may be fixable on retry
- `InvalidLayout` — data partition missing or path problem; permanent, but retry is harmless
  (resize will be skipped again immediately, and the failure reported again via `resize_data`)

---

## 4. Testing

New tests in `mode/normal.rs` `marker_writer_tests`:

- `skips_marker_when_resize_failed` — `first_boot=true`, `ods_status.resize_data=Some(...)` →
  `set_env` must NOT be called
- `writes_marker_when_resize_succeeded` — `first_boot=true`, `ods_status.resize_data=None` →
  `set_env` must be called

Existing tests unchanged. All four feature combos (grub/uboot × gpt/dos) pass for the
non-resize-data fallback.

---

## 5. Out of scope

- Changing `ResizeOutcome` variants or their semantics
- Suppressing the marker for any failure other than resize-data
- ODS-side retry orchestration (ODS consumes `resize_data`; remediation is ODS/cloud-driven)
