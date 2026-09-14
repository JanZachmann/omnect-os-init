# Factory Reset Modes 2, 3, 4 — Design

**Date:** 2026-08-11
**Status:** In review (PR #23)

## 1. Overview

Factory reset mode 1 (backup / reformat / restore) is implemented. This design
adds the wipe modes from the legacy bash script:

| Mode | Meaning                            | Implementation (this design)          |
| ---- | ---------------------------------- | ------------------------------------- |
| 2    | overwrite `etc` and `data` with random data (slow, better privacy) | native Rust write loop over the whole device |
| 3    | discard all blocks of `etc` and `data` (fast, needs hardware discard support) | `BLKDISCARD` ioctl |
| 4    | custom wipe hook                   | run `/opt/factory_reset/custom-wipe`  |

The wipe runs between backup/unmount and reformat. A wipe failure never
aborts the reset: reformat + restore still run, so the device stays usable.
The result is reported as an error, because the caller asked for the data to
be wiped and it was not.

### 1.1 Deliberate changes vs the legacy script

These must be listed in the PR description:

- **Mode 2:** the legacy `dd` seeked past the first 2048 bytes (partition
  alignment plus the ext4 superblock at offset 1024). The Rust init finds
  partitions by partition number, never by label, and the filesystem is
  re-created afterwards anyway, so the wipe covers the whole device from
  byte 0. Same privacy or better.
- **Mode 3:** legacy did mount → `rm -rf *` → `fstrim` → unmount, which trims
  only the blocks freed by `rm` and leaves fs-journal remnants. New: one
  `BLKDISCARD` ioctl discards every block of the partition. This also fixes a
  legacy bug where a failed mount silently counted as a successful wipe.
  On hardware without discard support the ioctl fails cleanly and the reset
  reports an error (legacy `fstrim` had the same hardware requirement).
  Discard remains a hint on some disks — the "no total privacy guarantee"
  note in the meta-omnect README stays true.
- **Mode 4:** unchanged contract. Same hook path, no arguments, partitions
  unmounted at call time — existing customer bbappends keep working.
- **No dependency changes in the initramfs image:** no `fstrim`/`blkdiscard`
  binary needed; `dd` no longer used for the wipe.

### 1.2 Userland (ODS) contract

No change. ODS already sends numeric modes 1–4 in the trigger
(`Serialize_repr`), and the result schema (status codes 0–4, optional
`error`/`context`, `paths`, `data_wiped`) is unchanged — ODS PR #207 parses
it. The wipe-failure note travels in the existing free-text `error` field.
ODS branches on no status value except an unrecognised one, so the status a
wipe failure reports only changes what the cloud sees.
The only observable delta is intended: a mode-2/3/4 trigger now performs a
reset (status 0/2) instead of failing with status 1.

## 2. Component Changes

### 2.1 `src/mode/factory_reset/config.rs`

- `ResetMode` gains `Mode2 = 2`, `Mode3 = 3`, `Mode4 = 4`.
- `TryFrom<u32>` accepts 1–4; everything else stays rejected (status
  `Invalid`). Mode stays number-only: the omnect-os CI branch
  (`feature_rust_init`) already sends numbers.

### 2.2 New: `src/mode/factory_reset/wipe.rs`

```rust
/// Path of the customer-provided mode-4 hook, installed into the initramfs
/// image by a Yocto bbappend.
const CUSTOM_WIPE_PATH: &str = "/opt/factory_reset/custom-wipe";
/// Chunk size for the mode-2 random overwrite.
const WIPE_CHUNK_SIZE: usize = 1024 * 1024;
```

- `wipe_random(device: &Path) -> Result<()>` — mode 2. Query the device size
  (`BLKGETSIZE64` ioctl), then stream `/dev/urandom` in `WIPE_CHUNK_SIZE`
  chunks over the whole device; log progress to kmsg every
  `WIPE_PROGRESS_LOG_INTERVAL` bytes (named const, 1 GiB); sync at the end.
- `wipe_discard(device: &Path) -> Result<()>` — mode 3. `BLKGETSIZE64` +
  `BLKDISCARD` ioctls, defined via `nix` ioctl macros (nix 0.29 is already a
  dependency; no new crates).
- `run_custom_wipe() -> Result<()>` — mode 4. `Command::new(CUSTOM_WIPE_PATH)`
  with no arguments. Missing binary, spawn error, or non-zero exit → error.
- Testability split: the mode-2 overwrite loop takes an open file + length so
  it is unit-testable against a temp file; `wipe_random` is the thin
  block-device wrapper (size query + call).
- New error variant in `FactoryResetError`, e.g.
  `WipeFailed { device: PathBuf, reason: String }`.

### 2.3 `src/mode/factory_reset/mod.rs`

Flow change in the reset sequence:

```
mount → preserve list → backup → unmount
  → wipe (mode 2/3/4; mode 1: no wipe step)   ← destructive phase starts here
  → reformat + mount with retry (existing)
  → restore → unmount
```

- The wipe is dispatched on `config.mode` and wipes the `etc` and `data`
  devices (same `layout.partitions` lookups as reformat). A failure on one
  device does not skip the other; failures are collected.
- The destructive boundary moves: for modes 2–4 any failure at or after the
  wipe reports `data_wiped: true` (a half-written random overwrite destroys
  data even if reformat never runs). Mode 1 keeps today's boundary
  (first reformat).
- Wipe failures never abort: they become a wipe note (e.g.
  `"wipe of data failed: <reason>"`), collected per device and joined with
  the existing `CONTEXT_SEPARATOR`. The note ends up in `error`, see 2.4.
- Injectable ops trait (same pattern as `ReformatRetryOps`) so the dispatch
  and continue-on-failure control flow is unit-testable without block
  devices.

### 2.4 Status mapping

Existing precedence (Error > Warning > Success) extended by the wipe note:

| Wipe | Reformat/restore | Status | Note placement |
| ---- | ---------------- | ------ | -------------- |
| ok / mode 1 | ok | Success (0) | — |
| failed | ok | Error (2) | wipe note in `error` |
| failed | retried reformat, ok | Error (2) | wipe note in `error`, retry note in `context` |
| ok / mode 1 | retried reformat, ok | Warning (4) | retry note in `context` |
| failed | mkfs failed twice / restore partial failure | Error (2) | wipe note joined into `error` ahead of the existing message; `context` unchanged |

A failed wipe reports Error, not Warning: the device is usable, but the
operation the caller asked for did not happen. This follows the existing rule
that a completed reset still reports Error when mkfs failed twice. `error` is
populated in every Error case, so the wipe note goes there — an Error with
`error: null` would be a shape ODS has never received.

Power loss during a wipe leaves a partly overwritten partition. The trigger is
cleared before the sequence starts, so the next boot is a Normal boot that
runs fsck, fails to mount, and propagates the error — on a release image that
ends in the fatal loop. Mode 2 makes this window minutes long instead of the
short window mode 1 has. Section 4 carries the proposed fix; it is not part of
this design. The existing reformat-retry and `FactoryResetLastError` machinery
is untouched.

## 3. Testing

- **Config:** modes 2/3/4 accepted; 0, 5, and string `"2"` rejected.
- **`wipe.rs`:**
  - overwrite loop against a temp file: full length overwritten, content is
    not the previous content, no short-write truncation.
  - custom wipe with temp scripts: exit 0 → Ok; exit 1 → Err; missing file →
    Err.
  - `BLKDISCARD`/`BLKGETSIZE64` wrappers stay thin and untested (need a real
    block device); their call sites are covered through the ops trait mock.
- **`mod.rs`:** mode 1 never calls wipe; wipe failure alone → Error status
  with the note in `error`; wipe failure + reformat retry → Error with the
  retry note still in `context`; wipe failure + restore partial failure →
  Error with both notes joined; etc-wipe failure still wipes data
  (continue-on-failure).
- On-device verification runs via the user's private Concourse team with the
  omnect-os `feature_rust_init` CI branch.

## 4. CI and documentation follow-ups (other repos)

- omnect-os CI covers only mode 1 today; nothing breaks. Optional follow-up
  on the `feature_rust_init` branch: add mode-2 and mode-3 test runs. Mode 4
  is testable too — CI installs a test hook at the mode-4 path directly, no
  customer bbappend needed, and the hook can cover exit 0, a non-zero exit,
  output on stdout/stderr, and a missing binary.
- **meta-omnect README (required, not optional):** the mode table still
  describes the legacy tools ("use dd to write random data", "recursive
  remove files with rm; notify disk with fstrim"). Both are replaced here, so
  the table becomes wrong. Rewrite it to describe behaviour ("2 = overwrite
  with random data (slow)", "3 = discard all blocks (fast, needs hardware
  discard support)") in the migration PR omnect/meta-omnect#636, which
  removes the legacy scripts but does not touch the README today.
- PR description in this repo documents all behaviour changes vs the legacy
  script (section 1.1).

### 4.1 Proposed follow-ups (out of scope here)

- **Power-loss resume:** set a "wipe in progress" marker in the boot env
  before the destructive phase and clear it once reformat succeeded. A boot
  that finds the marker set reformats `etc` and `data` instead of mounting
  them — the data is already gone, so there is nothing to lose, and the
  device comes up clean instead of stuck. It cannot loop, because the
  reformat clears the marker and a failing reformat lands in the existing
  double-mkfs path. Costs a new boot env key, so it needs a meta-omnect
  change as well.
- **Storage-type guidance in the meta-omnect README:** which mode suits which
  storage. Mode 2 suits rotating disks; on flash with wear leveling it adds a
  full write cycle and still cannot reach blocks the controller has remapped
  away. Mode 3 needs a disk that honours discard.
- **Discard flavours:** `BLKDISCARD` has siblings — `BLKZEROOUT` and
  `BLKSECDISCARD`. Different guarantees and very different runtimes, so a
  silent fallback on `EOPNOTSUPP` would hand the caller less privacy than
  asked for. Exposing them means new mode numbers in the trigger, the
  meta-omnect README and ODS, so it needs its own design.
