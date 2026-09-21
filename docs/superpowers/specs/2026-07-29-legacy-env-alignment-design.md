# Design: Legacy Alignment of Boot-Env and fsck Handling

> **Correction (2026-09-21).** The three factory-reset gaps this document
> records as out of scope are closed: an unusable trigger is cleared and
> reported, `preserve` is mandatory, and a problem in a preserve configuration
> file reports status 3 as the shell implementation did. See
> `2026-08-11-factory-reset-modes-2-3-4-design.md` §1.2.

**Date:** 2026-07-29
**Status:** Implemented — omnect/omnect-os-init#22
**Scope:** omnect-os-init — `runtime::omnect_device_service`, `filesystem::boot_sequence`,
`bootloader::{grub, BootEnvKey}`, `error`, `mode::normal`.
**Extends:** `2026-05-27-fsck-and-resize-design.md` §2.1 with the missing
consumer of the boot-env channel — see §4.
**Corrects:** `2026-05-27-boot-failure-recovery-policy.md` Task 3 — see §8.

This document is written **after** implementation. Its purpose is to let a
reviewer judge the change without re-deriving the legacy behaviour from
meta-omnect, and to record which deviations were deliberate.

---

## 1. Problem

The Rust initramfs replaced the shell initramfs, and several behaviours were
ported from the *code* rather than from the *contract*. Three of them were
live defects:

- **The update-rollback path never fired.** ODS creates
  `/run/omnect-device-service/omnect_validate_update_failed` to detect a
  rollback. The initramfs never created it, so a failed A/B validation was
  invisible to the twin.
- **Trigger markers reappeared on every boot.** `omnect_bootloader_updated` was
  read but never cleared, so `system_info::bootloader_updated()` stayed true for
  the rest of the device's life.
- **The boot partition's fsck result never reached the ODS JSON.** It was
  written to the boot env and then dropped from `OdsStatus`, with nothing
  reading it back.

A fourth showed up as a smoke-test failure in omnect-os
(`factory_reset_test.sh`: `factory reset status parse error: parsed "null:null"`),
which is the ODS JSON key mismatch tracked in
omnect/omnect-device-service#205.

---

## 2. Evidence base

Every decision below is checked against these sources. A reviewer can verify
each claim in one file.

**Legacy shell initramfs** — meta-omnect branch `main`,
`recipes-omnect/initrdscripts/omnect-os-initramfs/`:

| File | Installed as | Relevant part |
|---|---|---|
| `omnect-device-service-setup` | `/init.d/95-…` | `run_omnect_device_service_setup`, `fsck_handling` |
| `factory-reset` | `/init.d/86-…` | `factory_reset_enabled`, `handle_status` |
| `grub-sh` / `uboot-sh` | `/init.d/09-bootloader_sh` | `get_/set_bootloader_env_var`, `save_fsck_status` |
| `common-sh` | `/init.d/05-…` | `run_cmd`, `msg_fatal`, `on_exit` |

**Bootloader** — meta-omnect: `recipes-bsp/grub/grub-efi/omnect.inc.in`,
`recipes-bsp/u-boot/u-boot/omnect_env.env`, `…/omnect_env.h`.

**Consumer** — omnect-device-service: `twin::firmware_update::update_validation`,
`twin::firmware_update` (trigger writer), `twin::system_info`,
`twin::factory_reset`.

Note that the legacy scripts are gone on meta-omnect's `feature_init_rust`
branch. Use `git show main:<path>` to read them.

---

## 3. Update-flag handling

### 3.1 `omnect_validate_update_failed` is its own env variable

The bootloader sets it when it rolls back, and clears `omnect_validate_update`
in the same step — grub's `omnect.inc.in` and u-boot's `omnect_env.env` both do
this. Nothing anywhere writes the string `"failed"` into
`omnect_validate_update`.

The port read the failed state as *the value* `"failed"` of
`omnect_validate_update`, so it could never trigger. Added
`BootEnvKey::ValidateUpdateFailed`; removed the value-based parsing
(`ValidateUpdateState`).

### 3.2 Flags carry meaning in presence, not in value

Legacy tests every flag with `[ -n "${flag}" ]`. Any non-empty value counts as
set. The port compared against `"1"`.

The rule now has one definition for these flags, `BootEnv::is_flag_set`, because
the codebase had two that disagreed. `update_pending_from_env` used `is_some()`,
so an entry with an empty value counted as "update in flight".

`omnect_first_boot_done` deliberately does **not** follow that rule. It is not a
handshake the bootloader writes but a marker this crate writes, so it is matched
against its exact value (`FIRST_BOOT_DONE`): missing, empty or unexpected all mean
a fresh first boot. That is the safe direction, because the work it gates —
resize-data, the extra-bootargs sync — costs a no-op when repeated and can leave a
device unresized when skipped. The exact-`1` expectation predates the rename: omnect-os
`main` checks `bootloader_env.sh get resized-data != "1"` and `feature_rust_init`
checks the same against `omnect_first_boot_done` (`base_test.sh:222`). So the reader
had been looser than both its writer and the test.

That is not hypothetical on GRUB. Its rollback path assigns
`omnect_validate_update=` and saves it, which leaves the entry present with an
empty value — verified with `grub-editenv set key=` followed by `list`. U-Boot
does not show it, because its `get_env` maps an empty value to `None`. Legacy
avoided it by clearing with an *unset*, which removes the entry.

Left uncorrected, a rolled-back GRUB device would report `update_pending` on
every later boot: a fatal error would then reboot instead of halting, and the
extra-bootargs sync would stay gated off. Nothing clears the entry — ODS unsets
`omnect_validate_update` only on the successful-validation path.

One step remains unverified: that GRUB's own `save_env` at boot writes the empty
value the same way `grub-editenv` does. Same block format and same `name=value`
lines, but not the same code path. `grub-editenv /boot/EFI/BOOT/grubenv list`
after a rollback settles it on a device.

### 3.3 Both flags set at once is fatal

Legacy: `msg_fatal` + `return 1` → `on_exit` → halt loop on a release image,
interactive shell on a debug image. Ported as
`InitramfsError::ConflictingUpdateFlags`, classified `Fatal`, which produces the
same two outcomes.

This cannot loop. `omnect_validate_update` is set in the conflict case, so
`update_pending` is true, and `Fatal` + `update_pending` resolves to *reboot*.
On that reboot the bootloader clears `omnect_validate_update` and sets the
failed flag, leaving exactly one flag set. The next boot proceeds normally.

Neither trigger file is written in the conflict case: acting on an inconsistent
env would send ODS down the wrong update path.

### 3.4 Consumed flags are cleared; `omnect_validate_update` is not

`omnect_validate_update_failed` and `omnect_bootloader_updated` are cleared once
their trigger file exists, as legacy does. The initramfs is the only place that
clears them — ODS never does.

`omnect_validate_update` is deliberately left alone. ODS unsets it itself after
a healthy boot (`update_validation`), and clearing it here would drop an
in-flight update.

### 3.5 Clearing is best-effort

A failed clear is logged and the boot continues. Legacy behaves the same way: a
failing `set_bootloader_env_var` breaks the `&&` chain, but
`run_omnect_device_service_setup` still returns 0. The cost of a failed clear is
one duplicated trigger file on the next boot, which is not worth aborting a boot
that has otherwise succeeded.

---

## 4. fsck records: the boot env is a same-boot carrier

`2026-05-27-fsck-and-resize-design.md` §2.1 names the boot env as the primary
persistence channel and states that `persist_fsck_results` and its call ordering
are unchanged — both still hold. What it does not name is the *consumer*, and
that is what was missing.

Legacy is two-phase within a single boot:

1. fsck writes `omnect_fsck_<partition>` into the env (`save_fsck_status`).
2. `fsck_handling` in step 95 reads it into the JSON and **clears** it.

The env is therefore a transport, not a store. A record survives into the next
boot only when a boot aborts between the two phases.

With one exception, which is the case that matters most: on GRUB the boot
partition's own record is **not** written when fsck demands a reboot.
`grub.rs::save_fsck_status` skips the grubenv write for `is_reboot_required()`,
because writing to a filesystem that fsck just declared inconsistent is not
sound. Legacy does the same — `grub-sh::save_fsck_status` guards the boot branch
with `elif [ ! "${fsck_res}" = "2" ]` and otherwise logs "can not save fsck
state" — so this is not a regression, and the two differ only in reach: legacy
compares the literal `2`, while `is_reboot_required()` tests the bit, so it also
covers 3, 6, 7. Non-boot partitions do carry across, via the file on the boot
partition.

The port implemented phase 1 only. Consequences: `omnect_fsck_*` accumulated in
the fixed-size env block forever, and the boot partition's record never reached
the JSON, because `apply_boot_env_decision` drops it from `OdsStatus` once it is
in the env.

Added `drain_fsck_env(ods_status, bootloader)`, called from `mode::normal::run`
after `persist_fsck_results` and before the ODS JSON is written. It moves every
record from the env into `OdsStatus` and clears it. It is a no-op in degraded
boot, where `OdsStatus` still holds this boot's records directly — which
preserves the degraded-mode rule in `2026-05-27-degraded-boot-design.md` §3.3.

The records stay in `OdsStatus` after `persist_fsck_results`. Clearing them there
was safe only while nothing read the env back; the drain reads into the same map
keys, so keeping them cannot duplicate anything, and a persist whose write failed
— those errors are only logged — still reaches the JSON. The clear came from
`2026-05-19-degraded-boot-mode-design.md` §3.4 as a bare line of code with no
stated reason; the "avoid double-serialization" wording was added later as a
comment in response to review finding M2 on PR #9, not as a design decision. The
intent that *is* documented — "fsck results remain in the JSON so ODS and
operators can still read them" — argues for keeping them.

**Consequence for operators:** `fw_printenv omnect_fsck_data` after boot no
longer shows the last result — the value is consumed. The record is in
`/run/omnect-device-service/omnect-os-initramfs.json` and, when the data
partition was mounted, in `/data/var/log/fsck/<partition>.log`.

### 4.1 Clean results are not recorded

`2026-05-27-fsck-and-resize-design.md` §2 states `clean (0) | mount; nothing
recorded`, and "records every non-clean result in `OdsStatus.fsck`". The port
recorded clean results too, which deviated from that spec. `record_fsck_result` now
drops them, so the JSON names only partitions that needed attention.

---

## 5. ODS JSON contract

Kept as it is: the typed `{"code":N,"output":"…"}` shape per partition, and
fields omitted when they carry no value (`fsck`, `factory_reset`,
`degraded_boot`, `resize_data`, `extra_bootargs`). Only `first_boot` and
`data_wiped` are always present, because absence of a bool would be ambiguous.

This repo owns the contract; consumers adapt. omnect/omnect-device-service#205
names `runtime::omnect_device_service` as the contract source and resolves the
`factory_reset` key spelling on the ODS side.

**This is a deliberate break from the contract meta-omnect `main` ships today**,
not a disagreement with two stale commits. `omnect-device-service-setup`'s
`fsck_handling` starts from `echo "{}"` and then sets `.fsck=$fsck`
unconditionally, so `.fsck` is always present and is `{}` when there is nothing to
report; each value is `$(get_fsck_status ${i})`, a bare decoded string with no
`{code, output}` wrapper. `factory_reset_status_handling` likewise writes
`."factory-reset"`. Two commits on the older `factory_reset` branch reproduce both
of those in this crate.

Rejected anyway, on two grounds. Omit-when-empty is the established direction:
meta-omnect `73eb46a` ("return `null` from initramfs in case no factory reset
happened") already moved factory-reset from `{}` to `null` for the same reason. And
a bare string would be the only `OdsStatus` field without structure, which costs
the exit code — the one part a consumer can act on without parsing free text.

The cost is real and lands on consumers: §7 names the smoke tests that had to
change, and omnect/omnect-device-service#205 is the ODS side.

`tests/fsck_status.rs` pins the fsck part of the contract, because the omnect-os
smoke tests read it directly.

---

## 6. GRUB env durability

Two gaps against `grub-sh`:

- **No `sync`.** `set_bootloader_env_var` ends with `sync`; the port did not.
  `grub-editenv` leaves the write in the page cache, and the paths that write
  the env then reboot or halt — neither `reboot(2)` nor the halt loop syncs.
  Added `sync_filesystems()` after every grubenv write and after the fsck files on the
  boot partition. U-Boot keeps no sync, matching `uboot-sh`: `fw_setenv` writes
  the env region itself.
- **No fallback when the record does not fit.** grubenv is a fixed-size block
  shared by all variables, and the boot partition is vfat, where a damaged
  filesystem makes `fsck.vfat` verbose. Legacy stored a short
  `"fsck output to big"` placeholder on failure. Added the same, but keeping the
  exit code, so the JSON still shows that fsck reported something and with which
  code. Two attempts only — if the retry also fails, grubenv is unusable rather
  than full, and the error surfaces.

---

### 6.1 The fsck log is flushed on its own

`persist_fsck_results` writes `/data/var/log/fsck/<partition>.log` after the env
write. That file had no flush of its own: on GRUB it was carried incidentally by the
*next* partition's `sync_filesystems()`, the last partition's log by nothing, and on
u-boot by nothing at all — while the path that needs it most, `FsckRequiresReboot`,
continues into `reboot(2)`, which does not sync.

`write_fsck_log` now flushes the file with `sync_all()` and fsyncs the directory, so
the name is durable too. The guarantee is local and identical on both bootloaders
instead of depending on what happens to sync next.

---

### 6.2 Structural cleanups from a follow-up review pass

A second review pass on the finished change (no new behaviour claim, just
duplication this branch had grown):

- **`FsckChannel`.** `save_fsck_status`, `get_fsck_status` and
  `clear_fsck_status` each matched on `PartitionName` to pick grubenv vs. a
  file — the same rule three times, free to diverge. Replaced with one
  `FsckChannel::for_partition` and a match on the channel in each method.
- **`io_err`.** The three file-backed helpers (`save_fsck_to_file`,
  `get_fsck_from_file`, `clear_fsck_file`) each hand-built
  `BootEnvError::CommandFailed { command: format!("<verb> {path}"), reason:
  e.to_string() }`. One helper now takes the verb and the path.
- **`MockBootEnv` fsck injection.** Four ad-hoc `BootEnv` test doubles
  (`RecordingBootEnv` in `lib.rs`; `TrackingBootEnv`, `FailingBootEnv`,
  `FsckReadFailsBootEnv` in `boot_sequence.rs`) existed only because
  `MockBootEnv` had `with_get_env_error`/`with_set_env_error` but no
  fsck-specific equivalent. Added `with_save_fsck_error`,
  `with_get_fsck_error`, and `saved_fsck_calls()` — a shared handle usable
  even after the mock is moved into a `Box<dyn BootEnv>` and dropped, which is
  what `RecordingBootEnv` existed to work around.

`sync_filesystems` (§6) predates this pass by one commit and is the same
category of fix: one definition instead of a duplicate with its own copy of
the rationale.

---

## 7. Cross-repo changes

The initramfs change alone does not make the smoke test pass.

| Repo | Branch | Commit | Change |
|---|---|---|---|
| meta-omnect | `feature_init_rust` | `8bdde53` | u-boot writeable env flags list was missing `omnect_first_boot_done`, `omnect_factory_reset_last_error`, `omnect_fsck_<partition>` |
| omnect-os | `feature_rust_init` | `856c50ff` | smoke tests read the legacy JSON contract |

The meta-omnect gap matters beyond this change: the old resize marker
`resized-data` is in the list, so renaming it to `omnect_first_boot_done` lost
that property. On u-boot the observed effect is that `first_boot` reports true
again after an A/B update, and the first-boot work repeats.

---

## 8. Known gaps, deliberately left

1. **Encoding failure is indistinguishable from "no fsck ran".** Accepted.
   `encode_fsck_output` returns an empty string when gzip or base64 fail, and
   `get_fsck_status` then reports `Ok(None)` — the same as an absent record. Since
   the records now stay in `OdsStatus` (§4), the only loss is the record's trip
   across a reboot, so the failure costs a diagnostic on the boot that follows and
   nothing on this one. Making it explicit would need `encode_fsck_output` to
   return a `Result` and a second env format for the degraded payload; both buy a
   clearer log line for a record that is gone either way.

   The doc comment on `encode_fsck_output` was corrected twice while deciding
   this: it claimed ODS decodes the value (ODS reads no fsck data at all — the only
   reader is `decode_fsck_output` in this crate), and it called the
   `/data/var/log/fsck/<partition>.log` copy the primary artifact, which inverts
   the channel roles in `2026-05-27-fsck-and-resize-design.md` §2.1 and does not
   hold on the `FsckRequiresReboot` path, where the data partition is not mounted.
2. **Factory-reset path.** Three verified gaps, out of scope here: an invalid
   trigger is never cleared and never reported (`BootMode::detect` falls back to
   `Normal`, and only `factory_reset::run` clears the key); `preserve` is
   optional and unknown fields are ignored, so a typo means "preserve nothing"
   and the reset wipes everything with status 0; config-file errors are reported
   as status 1 where legacy used 3.
3. **Correction to an earlier plan.** Done, as a marked note rather than an edit.
   `2026-05-27-boot-failure-recovery-policy.md` Task 3 states that
   `omnect_validate_update` may hold `"1"` or `"failed"` and that the two need not
   be distinguished. The value `"failed"` does not exist — see §3.1 — and the
   `is_some()` check it justifies has been replaced, see §3.2. The original text
   stays, with a dated correction beside it: the assumption recorded there is what
   this port was built on, so removing it would hide why the defect existed.

---

## 9. Testing

Covered by unit tests: the failed-flag key and its clearing; the
bootloader-updated marker and its clearing; both-flags-set is fatal and writes
no file; any non-empty value triggers; an entry with an empty value counts as
unset, both at `is_flag_set` and at `update_pending_from_env`; a clear failure
does not abort; `omnect_validate_update` stays set; a boot-env read error is typed as
`Bootloader`; the documented file modes; `drain_fsck_env` for move-and-clear,
stale-plus-current, no-bootloader, and read failure; the grubenv fallback for
fits / does-not-fit / env-unusable.

Covered by integration tests: the fsck part of the ODS JSON contract
(`tests/fsck_status.rs`) — key absent when clean, `{code, output}` per failing
partition, only failing partitions present.

Not covered, and why:

- The call ordering `persist → drain → JSON` in `mode::normal::run`. Not
  reachable without a mounted rootfs.
- The wiring of `save_fsck_status` to the grubenv fallback. `GrubBootEnv` drives
  `grub-editenv` as a subprocess; the retry logic itself is tested through a
  writer closure.

CI runs the valid combinations of {grub,uboot} × {gpt,dos} × {resize-data} ×
{release-image}; this change was verified against eight of them plus
`factory-reset`.
