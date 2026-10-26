# Design: fsck Handling & resize-data Policy

**Date:** 2026-05-27
**Status:** Implemented
**Scope:** omnect-os-init — `filesystem::fsck`, `filesystem::boot_sequence`,
`preflight::resize_data`, `filesystem::resize_data`, `runtime::OdsStatus`.
**Depends on:** Plan A (Boot Failure & Recovery Policy) — this spec applies its
recovery classes (`ContinueDegraded`, `RebootToApply`, `Fatal`).

---

## 1. Problem

Two related defects in the current fsck / resize handling:

- **Lenient-vs-strict asymmetry on the same partition.** Normal mount uses
  `check_filesystem_lenient` (uncorrected errors → mount + record, boot
  continues). resize-data uses strict `check_filesystem` (uncorrected errors →
  `FsckFailed` → fatal; release infinite loop; the resize guard is never set →
  repeats every boot). Identical data-partition corruption is recoverable
  without resize-data, a permanent brick with it.
- **No structured signal to ODS for resize anomalies.** When resize cannot
  proceed, the initramfs either bricks (today) or silently skips (intuitive fix)
  — neither lets the cloud learn about the device's degraded state.

A third, related but **deliberately not fixed**, item: a reboot-required fsck
result on a partition that fsck cannot fully repair will reboot-loop. This is
the accepted choice (Plan A §2.4); we honor fsck's "reboot to apply" signal and
accept the loop risk.

---

## 2. fsck outcome → recovery class

Single table, applied at every fsck call site (`fsck_and_record`). No new behavior
for the corrected / uncorrected / clean paths — Plan B only re-expresses today's
behavior under Plan A's vocabulary so the policy has one source of truth.

Outcomes from `check_filesystem` (raw fsck) are filtered by `check_filesystem_lenient`
before they propagate further. Below, "propagates" means the error reaches
`handle_fatal_error` via Plan A.

| fsck result (bitmask) | Mount path (via lenient wrapper) | Recovery class if it propagates |
|---|---|---|
| clean (0) | mount; nothing recorded | — |
| corrected (1) | mount; record diagnostic | `ContinueDegraded` |
| reboot-required (bit 1: 2, 3, …) | propagates as `FsckRequiresReboot` | **`RebootToApply`** → reboot **unconditionally** (Plan A §2.4) |
| uncorrected, not reboot-required (4, 5, 8, 9, …) | swallowed by lenient: mount + record | does not propagate |
| tool failure / spawn error (UNKNOWN) | swallowed by lenient: mount + record | does not propagate |

Note (2026-10-22): the reboot-required row applies to ext4 only. fsck.fat does
not follow the fsck(8) bitmask: 2 is a usage error, 6 means the device could
not be opened. On `boot` (vfat) every code other than 0 and 1 is handled like
the uncorrected row.

The `mount(2)` syscall remains the real gate for partitions that are too corrupt
to be mounted at all — `MountFailed` is `Fatal`. `FsckFailed` is reachable as a
propagated error only via a **strict** caller (`check_filesystem` directly). The
only strict caller in the boot path is resize-data, whose preflight wrapper
(§3) catches `FsckFailed` and converts it to a `ResizeStatus` indicator — so in
practice `FsckFailed` never reaches `handle_fatal_error`. Its recovery class is
`Fatal` as a defensive default.

### 2.1 Diagnostic persistence (unchanged contract, re-stated for clarity)

`fsck_and_record` records every non-clean result in `OdsStatus.fsck`. The
existing two-channel persistence is kept:

- **Bootloader env** (`omnect_fsck_<partition>`, gzip+base64) — primary channel,
  works even when the data partition is not mounted.
- **`/data/var/log/fsck/<partition>.log`** — best-effort, requires the data
  partition to be mounted.

The persistence path (`persist_fsck_results`) and its call ordering (before the
fsck error propagates) are unchanged. The review's "ordering is the contract,
not a footnote" finding is resolved by Plan A's recovery model making the
ordering an explicit step in `handle_fatal_error`'s decision flow.

### 2.2 Full check after a journal replay (added 2026-10-21)

After a power cut, e2fsck replays the ext4 journal, reports the filesystem
clean and skips the full check. Damage the replay does not repair (e.g. a
directory block with a bad checksum) stays until the kernel sets the error flag
and a later boot checks again. Mount-count and interval checks are off
(`tune2fs -c -1 -i 0`), so they do not catch it either.

- **Trigger:** ext4, the first run printed `recovering journal` and exited 0.
  Any other exit code means e2fsck did not skip, or the result is already
  not clean; the first run is then the result.
- **Second run:** `fsck -y -f`. Its exit code is the result; the outputs of
  both runs are concatenated in `FsckResult.output`. `LC_ALL=C` keeps the
  marker text untranslated.
- **Cost:** the forced check runs after every unclean shutdown of an ext4
  partition, which on an edge device is the normal power-off. Its time grows
  with the partition size and the number of used inodes, largest on the data
  partition with `resize-data`. Measured on a Raspberry Pi 4 SD card: an
  empty data partition took 0.26 s, a 20 GB one with 2.5 M inodes 23-24 s.
  No timeout bounds it. This is accepted: the
  alternative is a corrupted directory that stays unreadable for a whole boot.
  The duration is logged (`full check on <device> took ...`).

---

## 3. resize-data: best-effort + ODS indicator

### 3.1 Behavior change

Every failure mode of `resize_if_needed` is reclassified as `ContinueDegraded`.
The function never returns `Fatal`. The `omnect_resized_data` guard is set only
on full success; on any failure the guard is **not** set, so resize is retried
on the next boot when the underlying condition (e.g. partition fscks clean)
clears.

| Failure | Today | Plan B |
|---|---|---|
| Data partition fails fsck (uncorrected errors) | `Fatal` → release infinite loop, guard never set, repeats every boot (brick) | Skip resize; record indicator; **continue boot**. Not retried — Plan D unified the resize trigger with the first-boot marker, so the marker is set at end of boot regardless of resize outcome. |
| `parted` / `sgdisk` / `resize2fs` / `sync` tool error | `Fatal` | Skip resize; record indicator; continue boot |
| `InvalidDevicePath` / `NonUtf8Path` / data partition absent | `Fatal` (or warn for absent) | Skip resize; record indicator; continue boot |
| **Reboot-required fsck on data during resize pre-check** | reboot (today) | **reboot** (`RebootToApply`, Plan A §2.4) — *not* a resize failure; the indicator below is not set |

### 3.2 ODS indicator (new field in `OdsStatus`)

`OdsStatus` gains a `resize_data: Option<ResizeStatus>` field, serialized in
`/run/omnect-device-service/omnect-os-initramfs.json`. ODS consumes it and can
notify the cloud. Field is `None` on a clean run (resize succeeded or guard
already present), `Some(...)` only when resize was attempted and skipped/failed.

```rust
pub struct ResizeStatus {
    pub outcome: ResizeOutcome,         // SkippedFsck | ToolError | InvalidLayout
    pub reason: String,                 // human-readable detail (one line)
}

pub enum ResizeOutcome { SkippedFsck, ToolError, InvalidLayout }
```

`outcome` is a small enum so ODS can branch deterministically; `reason` is a
free-form summary (the underlying error's `Display`) for logs/cloud upload.

This field parallels the `degraded_boot` signal that **Plan C** will add — both
are "initramfs surfaces an anomaly via the runtime JSON" rather than bricking.
The two should land with consistent serialization style (lowercase `snake_case`
keys, `Option` so absence means "no anomaly").

### 3.3 One-shot semantics (revised after Plan D unification)

Resize is **one-shot on first boot only**. The dedicated `omnect_resized_data`
guard is removed (Plan D §2.1); the unified `BootEnvKey::FirstBootDone` marker
acts as the single first-boot sentinel. Resize-data preflight reads
`FirstBootDone` (Plan D §6.1) and runs iff the marker is absent. The marker
is written by Plan D's writer (`mode::normal::run`, just before `switch_root`)
at the end of any successful boot — **regardless of resize outcome.**

Consequences:
- A resize that was deferred on first boot (e.g. dirty fsck) is reported once
  via `ResizeStatus` and is **not** retried by initramfs on later boots.
  Remediation is ODS/cloud-driven.
- A partial run (`parted` succeeded, `resize2fs` did not) leaves the partition
  geometry at the new size and the filesystem at the old size. Because the
  resize is one-shot, the filesystem stays at the old size until cloud-side
  remediation triggers a re-attempt (out of scope here).
- `filesystem::resize_data::resize_if_needed` no longer writes
  `BootEnvKey::ResizedData`; the `write_resize_guard` helper is removed
  (Plan D §6.1).

---

## 4. Code structure

- **`src/filesystem/fsck.rs`** — no behavior changes; `FsckExitCode` and
  `check_filesystem` / `check_filesystem_lenient` stay as-is.
- **`src/error.rs`** — `InitramfsError::recovery_class` (from Plan A) classifies:
  - `FilesystemError::FsckRequiresReboot` → `RebootToApply`
  - `FilesystemError::FsckFailed` → `Fatal` (defensive default; §2 explains why
    this is unreachable in practice once preflight catches it inside resize)
  - `ResizeDataError` (any variant) → `ContinueDegraded`
- **`src/preflight/resize_data.rs`** — catches every error from
  `filesystem::resize_data::resize_if_needed`, records a `ResizeStatus` on
  `OdsStatus`, returns `Ok(())`. The preflight step itself never propagates a
  resize error.
- **`src/filesystem/resize_data.rs`** — switch the internal `check_filesystem`
  call to one that propagates `FsckRequiresReboot` unchanged but converts
  `FsckFailed` into a structured "skip" outcome surfaced to the caller. No
  behavior change for `parted`/`resize2fs`/etc. — they already return
  `ResizeDataError`; preflight just stops treating that as fatal.
- **`src/runtime/omnect_device_service.rs`** — add the `resize_data` field to
  `OdsStatus` and its `ResizeStatus`/`ResizeOutcome` types. Serialization
  follows the existing pattern (`#[serde(skip_serializing_if = "Option::is_none")]`).

No new module; no new external dependency.

---

## 5. Testing

- **fsck classification truth table** — one assertion per
  `FilesystemError` variant against its `recovery_class()`. Exhaustive `match`
  pins intent.
- **Resize preflight never returns Err.** Three sub-tests (dirty fsck, tool
  error via fault-injected mock, data partition absent) each assert
  `preflight::resize_data::run` returns `Ok(())` and `OdsStatus.resize_data` is
  `Some(...)` with the right `outcome`.
- **Resize guard absence on failure.** After a failed resize, the boot env
  has no `omnect_resized_data` key (mock bootloader assertion).
- **Resize success path.** On a clean run (mocked), the guard is set and
  `OdsStatus.resize_data` is `None`.
- **`FsckRequiresReboot` during resize pre-check** — assert it propagates as
  `RebootToApply` (does not become a `ResizeStatus` indicator). This pins the
  §3.1 distinction.
- **Serialization** — `OdsStatus` with a `Some(ResizeStatus { outcome:
  ResizeOutcome::SkippedFsck, … })` round-trips through `serde_json` and
  contains `"resize_data"` with `"outcome":"skipped_fsck"`.

Action *execution* (the `reboot(2)` call for `RebootToApply`) is not unit-tested
— Plan A §4 already states this boundary.

---

## 6. Out of scope

- **Revisiting lenient mounting of uncorrected-error partitions.** Kept as-is by
  explicit decision; tightening that gate is a separate proposal.
- **Bounding the fsck reboot.** Explicitly out of scope (Plan A §2.4): the
  reboot is unconditional with a documented accepted loop risk.
- **The `degraded_boot` ODS signal.** Owned by Plan C; the `resize_data` signal
  here is designed to share its serialization style but is independent.

---

## 7. Open questions

None blocking. One minor item for the implementation step: pick the exact
`ResizeOutcome` variant set — the three proposed (`SkippedFsck`, `ToolError`,
`InvalidLayout`) cover the current failure modes, but the writing-plans step
should confirm each underlying `ResizeDataError` variant maps to one of them
without leaving an "other" bucket.
