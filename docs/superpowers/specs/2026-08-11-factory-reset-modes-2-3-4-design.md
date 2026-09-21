# Factory Reset Modes 2 and 3 — Design

**Date:** 2026-08-11
**Status:** In review (PR #23)

## 1. Overview

Factory reset mode 1 (backup / reformat / restore) is implemented. This design
adds the remaining wipe modes from the legacy bash script:

| Mode | Meaning                            | Implementation (this design)          |
| ---- | ---------------------------------- | ------------------------------------- |
| 2    | overwrite `etc` and `data` with random data (slow, better privacy) | native Rust write loop over the whole device |
| 3    | discard all blocks of `etc` and `data` (fast, needs hardware discard support) | `BLKDISCARD` ioctl |

Legacy mode 4 (custom wipe hook) is dropped, see 1.1.

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
- **Mode 4 is dropped.** The legacy script ran a customer-supplied hook at
  `/opt/factory_reset/custom-wipe`. The mode is removed instead of ported: it
  hands the wipe — the one step the caller asked for — to code we do not
  build or test, and with modes 2 and 3 covering rotating disks and flash
  there is no case left that needs it. A mode-4 trigger is rejected as
  `Invalid` (status 1). Removal has to land in the same release in every
  place that knows the mode, see section 4.
- **No dependency changes in the initramfs image:** no `fstrim`/`blkdiscard`
  binary needed; `dd` no longer used for the wipe.

### 1.2 Userland (ODS) contract

The result schema is unchanged (status codes 0–4, optional `error`/`context`,
`paths`, `data_wiped` — ODS PR #207 parses it), and the wipe-failure note
travels in the existing free-text `error` field.

The accepted trigger modes narrow from 1–4 to 1–3. Both observable deltas are
intended: a mode-2/3 trigger now performs a reset (status 0, 2 or 4) instead
of failing with status 1, and a mode-4 trigger is rejected as `Invalid`
(status 1). ODS and omnect-ui drop `Mode4` from their mode type, so the value
can no longer be sent from the cloud or the local UI — see section 4.

**Correction (2026-09-21).** When this section was written the init did not
report a rejected trigger at all: a value it could not parse — a mode outside
the accepted range included — only produced a kmsg warning, the boot
continued as Normal, no `factory_reset` object was written, and the trigger
kept its value, so the warning repeated on every following boot. A mode-4
trigger therefore did not come back as `Invalid`; it came back as silence,
which is worse for the caller and which the on-device tests catch. The init
now reports every trigger it cannot use and clears it, which is what this
section describes. The reported status follows the rule the shell
implementation used: anything about `mode` is `Invalid` (status 1), an
unusable `preserve` is `ConfigError` (status 3).

## 2. Component Changes

### 2.1 `src/mode/factory_reset/config.rs`

- `ResetMode` gains `Mode2 = 2` and `Mode3 = 3`.
- `TryFrom<u32>` accepts 1–3; everything else is rejected and reported as
  status `Invalid`, see the correction in 1.2. Mode stays number-only: the
  trigger already carries the mode as a number in the on-device tests.

### 2.2 New: `src/mode/factory_reset/wipe.rs`

```rust
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
- Testability split: the mode-2 overwrite loop takes an open file + length so
  it is unit-testable against a temp file; `wipe_random` is the thin
  block-device wrapper (size query + call).
- New error variant in `FactoryResetError`, e.g.
  `WipeFailed { device: PathBuf, reason: String }`.

### 2.3 `src/mode/factory_reset/mod.rs`

Flow change in the reset sequence:

```
mount → preserve list → backup → unmount
  → wipe (mode 2/3; mode 1: no wipe step)     ← destructive phase starts here
  → reformat + mount with retry (existing)
  → restore → unmount
```

- The wipe is dispatched on `config.mode` and wipes the `etc` and `data`
  devices (same `layout.partitions` lookups as reformat). A failure on one
  device does not skip the other; failures are collected.
- The destructive boundary moves: for modes 2 and 3 any failure at or after
  the wipe reports `data_wiped: true` (a half-written random overwrite destroys
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
| failed | mkfs failed twice / restore partial failure | Error (2) | wipe note joined into `error` ahead of the existing message; retry and restore notes in `context` as today |
| ok / mode 1 | mkfs failed twice / restore partial failure | Error (2) | existing `error` message; retry and restore notes in `context` (unchanged) |

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

- **Config:** modes 2 and 3 accepted; 0, 4, 5, and string `"2"` rejected.
- **`wipe.rs`:**
  - overwrite loop against a temp file: full length overwritten, content is
    not the previous content, no short-write truncation.
  - `BLKDISCARD`/`BLKGETSIZE64` wrappers stay thin and untested (need a real
    block device); their call sites are covered through the ops trait mock.
- **`mod.rs`:** mode 1 never calls wipe; wipe failure alone → Error status
  with the note in `error`; wipe failure + reformat retry → Error with the
  retry note still in `context`; wipe failure + restore partial failure →
  Error with both notes joined; etc-wipe failure still wipes data
  (continue-on-failure).
- On-device verification runs in a private Concourse team, on the OS build
  branch that carries the Rust init.

## 4. CI and documentation follow-ups (other repos)

- On-device tests trigger only mode 1 today; nothing breaks. Optional
  follow-up: add mode-2 and mode-3 test runs.
- **meta-omnect README (required, not optional):** the mode table still
  describes the legacy tools ("use dd to write random data", "recursive
  remove files with rm; notify disk with fstrim"). Both are replaced here, so
  the table becomes wrong. Rewrite it to describe behaviour ("2 = overwrite
  with random data (slow)", "3 = discard all blocks (fast, needs hardware
  discard support)"), and drop the mode-4 row together with the custom-wipe
  paragraph and its bbappend instructions, in the migration PR
  omnect/meta-omnect#636, which removes the legacy scripts but does not touch
  the README today.
- **Mode-4 removal (required, same release):** `Mode4` has to go from the
  mode type in omnect/omnect-device-service and omnect/omnect-ui as well.
  Removing it only here would leave both able to send a 4 that comes back as
  `Invalid`, which reads as a broken reset rather than a removed mode.
- PR description in this repo documents all behaviour changes vs the legacy
  script (section 1.1).

### 4.1 Proposed follow-ups (out of scope here)

- **Power-loss resume:** set a "wipe in progress" marker in the boot env
  before the destructive phase and clear it once the reformat succeeded. A
  boot that finds the marker set reformats `etc` and `data` instead of
  mounting them, and does not repeat the wipe — the blocks it reached are
  already overwritten, and repeating a wipe that runs for minutes delays the
  boot without buying privacy. The backup lives in initramfs RAM, so in this
  case the `preserve` paths are lost either way. A failing reformat ends
  where it ends today: mkfs retried once, Error status, failure signal in the
  boot env, then the fatal path of the following Normal boot. The init never
  reboots itself there, so the marker cannot produce a boot cycle; it turns a
  guaranteed mount failure into one reformat attempt per boot.

  A marker alone is not enough for the reported outcome. The preserve keys
  come from the trigger, which is cleared as the first step, and they resolve
  against `etc/omnect/factory-reset.json` and `etc/omnect/factory-reset.d`,
  which sit on the partition being wiped — after a power loss the recovery
  boot cannot reconstruct what was to be restored. So the marker also has to
  record whether the backup held anything, and the recovery boot reports
  Success only when it did not, Error otherwise: a reset that comes up clean
  but silently dropped the preserved paths must not be reported as success.
  `backup_all` returns only the paths that existed, so that flag is about
  what was really backed up, not about a preserve list whose paths were all
  absent.

  The status reaches the cloud only if the device gets there — after `etc` is
  reformatted the network configuration is gone, which is the case the boot
  env record covers.

  Costs new boot env keys, so it needs a meta-omnect change as well.
- **Report an outcome that no status file carried:** when the destructive
  phase leaves a partition unmountable, the init records
  `omnect_factory_reset_last_error` in the boot env, but nothing reads or
  clears it. ODS takes the factory-reset result from the status file in
  `/run`, which the failing boot never reached, and a later boot that comes
  up carries no result at all — so the failure never reaches the cloud. Fix:
  on a boot where no reset ran, the init turns a set key into the
  factory-reset result and clears it. That keeps one input for ODS, needs no
  new boot env privileges for it, and it is the mechanism the outcome rule
  above needs, because the recovery boot decides the outcome after the reset
  is over.
- **Post-reset hook:** a hook called after a successful reset, so a customer
  application can pick up a signal on the following boot — for example a file
  written into the fresh `data`. Raised in review of the mode-4 removal, but
  a different feature: the removed hook took over the wipe itself, this one
  only runs once the reset succeeded. Open: when exactly it runs, what it may
  touch on a freshly reformatted `etc` and `data`, and whether a non-zero
  exit changes the reported status. No customer has asked for it, so it stays
  an idea.
- **Storage-type guidance in the meta-omnect README:** which mode suits which
  storage. Mode 2 suits rotating disks; on flash with wear leveling it adds a
  full write cycle and still cannot reach blocks the controller has remapped
  away. Mode 3 needs a disk that honours discard.
- **Discard flavours:** `BLKDISCARD` has siblings — `BLKZEROOUT` and
  `BLKSECDISCARD`. Different guarantees and very different runtimes, so a
  silent fallback on `EOPNOTSUPP` would hand the caller less privacy than
  asked for. Exposing them means new mode numbers in the trigger, the
  meta-omnect README and ODS, so it needs its own design.
