# Design: Load Kernel Modules Early in the Initramfs

**Date:** 2026-09-25
**Status:** Draft, for review
**Scope:** omnect-os-init — a new early step after the rootfs mount that loads
the modules listed in a config file in the rootfs, one Cargo feature.
meta-omnect sets the feature and ships the first config file (`imx_sdma` on
i.MX8MM).

---

## 1. Problem

Some drivers are built as modules because their firmware lives in the rootfs
and cannot be built into the kernel. When such a module is loaded only by udev
after systemd has started, every device that depends on it appears late.

The case that needs this today is the SDMA driver on i.MX8MM
(`CONFIG_IMX_SDMA=m`, firmware `imx/sdma/sdma-imx7d.bin`). `spi-imx` defers its
probe until the SDMA controller exists, and the i.MX UART driver requests its
DMA channel when a port is opened. Measured on a phyGATE Tauri-L
(`phygate-tauri-l-imx8mm-2`):

| Time | Event |
|---|---|
| 4.5 s | systemd starts |
| 10.27 s | udev loads `imx_sdma`, the firmware loads |
| 10.32 s | TPM on `spi0.1` appears |
| 10.95 s | CAN controller `can0` on `spi0.0` appears |

A unit ordered on its device (`aziot-tpmd` requires `dev-tpmrm0.device`) is
not affected. Anything that is not ordered on its device is: an application
that opens `can0` early, or a UART opened before SDMA exists, which then runs
without DMA until it is opened again.

## 2. Legacy behaviour

meta-omnect installed `init.d/90-imx_sdma` for `mx8mm-nxp-bsp`, with the
reason "load imx_sdma in initramfs to prevent race conditions with drivers
using sdma". After the rootfs mount it bind-mounted the rootfs `/lib/modules`
and `/lib/firmware` into the initramfs and ran `modprobe imx_sdma`. It did not
unmount them ("Device or resource busy"), and it ignored every error. It was
the only legacy script that loaded a module.

## 3. Goals and non-goals

Goals:

- The modules listed in a config file, with their dependencies and firmware,
  load before `switch_root`, so the devices behind them exist when systemd
  starts.
- A machine or a customer layer adds a module by adding a config file, without
  a code change.
- A missing module or firmware never stops the boot and never adds a
  firmware-loader timeout.
- Images without the feature do not change.

Non-goals:

- Module aliases and `modules.softdep`. The config names modules by name.
- Changing the kernel configuration or the firmware packaging, except the one
  kernel option in section 10.

## 4. Config file

Files: `<rootfs>/etc/omnect/early-modules.d/*.conf`, read in sorted file name
order. Each line names one module, followed by optional parameters:

```
# /etc/omnect/early-modules.d/imx-sdma.conf
imx_sdma

# a module with parameters
example_mod debug=1 mode=fast
```

- `#` starts a comment; empty lines are ignored.
- The first word is the module name; `-` and `_` are equal, as in `modprobe`.
- The rest of the line is passed unchanged as the parameter string of
  `finit_module(2)`. It is the only source of parameters: a module loaded in
  the initramfs stays loaded after `switch_root`, so udev and `modprobe` in the
  rootfs skip it, and its `options` lines in `/etc/modprobe.d` never take
  effect. A module that needs options gets them on its config line.
- A module that appears twice is loaded once, with the parameters of its first
  line; the second line is logged as a warning.
- The files are read from the rootfs image, before the etc overlay is mounted.
  A file changed or added on the device has no effect; the list belongs to the
  image, like the modules it names. A customer layer changes the list at build
  time: it adds a file, or replaces or removes one in a bbappend.

## 5. Load sequence

A new step, `early_modules::load(rootfs)`, runs in `run_init` right after
`mount_core_partitions` has mounted the rootfs, and before anything else.

1. Read and parse the config files. No module listed: stop here and leave
   `firmware_class.path` alone.
2. Read `<release>` from `/proc/sys/kernel/osrelease`. Read
   `<rootfs>/lib/modules/<release>/modules.dep` and `modules.builtin`.
3. Point the kernel firmware search at the rootfs: write
   `<rootfs>/lib/firmware` to `/sys/module/firmware_class/parameters/path`.
4. For each listed module, in config order:
   1. skip it when it is in `modules.builtin`, or when
      `/sys/module/<name>/initstate` exists (already loaded);
   2. find its path in `modules.dep` (module name = file name without `.ko`
      and without a compression suffix, `-` replaced by `_`);
   3. load its dependencies first, in the reverse order in which
      `modules.dep` lists them, with the same skip check. A dependency gets
      the parameters of its own config line if it has one, and none
      otherwise;
   4. open `<rootfs>/lib/modules/<release>/<path>` and load it with
      `finit_module`. A compressed file (`.ko.xz`, `.ko.zst`, `.ko.gz`) gets
      the flag `MODULE_INIT_COMPRESSED_FILE`; a `.ko` file gets no flag.

The path is not restored. After `switch_root` it names a directory that does
not exist, so the loader skips it and uses its default paths. The only effect
is the stale value in sysfs. A value set on the kernel command line is
replaced.

Firmware needs no list of its own. While the path points at the rootfs, every
firmware file a listed module requests is found there.

`nix` 0.29 has no constant for `MODULE_INIT_COMPRESSED_FILE`, so the code
defines it as a named constant with the value from the kernel header
`include/uapi/linux/module.h` and builds the flags with
`ModuleInitFlags::from_bits_retain`. The kernel understands the flag only with
`CONFIG_MODULE_DECOMPRESS=y` (since Linux 5.17). Because it is passed only for
compressed files, an uncompressed module still loads on an older kernel.

## 6. Why no bind mounts and no `modprobe`

- `finit_module` takes a file descriptor, so the module is read directly from
  the rootfs. `modules.dep` gives the dependencies, and the kernel decompresses
  compressed modules itself, so `modprobe` adds nothing for a list of module
  names. The initramfs needs no `kmod` package; `nix` needs its `kmod`
  feature.
- The kernel reads firmware relative to the root of PID 1
  (`kernel_read_file_from_path_initns`). Before `switch_root` that is the
  initramfs, which has no `/lib/firmware`. `firmware_class.path` is the first
  entry the loader tries, and it can be changed at runtime (mode 0644), so
  writing the rootfs path to it replaces the legacy bind mount of
  `/lib/firmware`. Nothing is left mounted in the initramfs.
- `/lib` can be a link to `usr/lib` in the rootfs. The link must be relative
  (as it is on rpi4), so that `<rootfs>/lib/...` stays inside the rootfs.

## 7. The asynchronous firmware load

A driver can request its firmware with `request_firmware_nowait`, as
`sdma_probe` does. Then `finit_module` returns before the firmware is read.
The read runs on a kernel workqueue and needs the rootfs path to be valid at
that time.

- The rootfs stays mounted at the same path until `switch_root`. On the
  measured device that is at least several hundred milliseconds later, because
  the other partitions are checked and mounted in between. A direct read of
  the 3 KB SDMA firmware file finishes well before that.
- If the read ever runs after `switch_root`, the rootfs path no longer
  exists, and the loader finds the firmware in the default path
  `/lib/firmware` of the new root.

The step does not wait for firmware. The kernel offers no general signal for
"firmware loaded", and a wait would add boot time on every boot to cover a
case that the timing already excludes.

## 8. Error handling

The step is best effort, as in legacy. Every outcome is logged, none is
returned as an error, and none changes the ODS status.

| Outcome | Handling |
|---|---|
| no config file, or no module listed | nothing to do, `firmware_class.path` unchanged |
| a config file cannot be read | `warn`, skip that file |
| module is built in, or already loaded | `info`, skip |
| `modules.dep` or `modules.builtin` cannot be read | `warn`, skip all loads, `firmware_class.path` unchanged |
| module name not in `modules.dep` | `warn`, skip the module |
| a dependency fails to load | `warn`, skip the module that needs it; dependencies loaded before stay loaded |
| module file cannot be opened | `warn`, skip the module |
| `firmware_class.path` cannot be written | `warn`, skip all loads (without the path a firmware request finds nothing in the initramfs and waits for the sysfs fallback, 60 s by default) |
| `finit_module` fails with `EEXIST` | `info` (loaded in the meantime) |
| compressed module, kernel without `CONFIG_MODULE_DECOMPRESS` | `finit_module` fails; `warn`, skip the module |
| `finit_module` fails otherwise | `warn`, skip the module |
| rootfs mount failed | the step does not run |

When a listed module did not load, the step ends with one `warn` line that
names all of them.

A release image must never stop in the fatal-error loop because of this step,
so no path returns an error.

## 9. Feature and config file

The step is compiled only with the Cargo feature `early-modules`. The feature
and the config file are set in two places, so both cases of a mismatch are
defined:

- feature off, config file present: the file is ignored;
- feature on, no config file: nothing is loaded and `firmware_class.path` is
  not touched.

## 10. Build and recipe

- omnect-os-init: Cargo feature `early-modules = ["core"]`, off by default;
  `nix` gets the `kmod` feature.
- meta-omnect, Rust initramfs recipe:
  `CARGO_FEATURES:append:mx8mm-nxp-bsp = ",early-modules"`. This is the
  override the legacy recipe used, and it is active on the phytec i.MX8MM
  machines.
- meta-omnect, rootfs: install `/etc/omnect/early-modules.d/imx-sdma.conf`
  (one line, `imx_sdma`) for `mx8mm-nxp-bsp`.
- meta-omnect, rpi kernels: set `CONFIG_MODULE_DECOMPRESS=y`. The rpi4 kernel
  builds compressed modules (`CONFIG_MODULE_COMPRESS_XZ=y`) without it, so
  `finit_module` could not load them. rpi gets no feature and no config file
  for now; the kernel option only makes the loader usable there later.
- The README table of runtime dependencies gets a row for the files read from
  the rootfs: `/etc/omnect/early-modules.d/*.conf`, `modules.dep`,
  `modules.builtin`, the listed modules and their firmware. It also says that
  `modprobe.d` options do not apply to a listed module.

## 11. Testing

Unit tests, with the sysfs, procfs and rootfs paths injectable:

- config parsing: comments, empty lines, parameters, `-`/`_` in names,
  duplicate modules, sorted file order;
- `modules.dep` parsing: name from path with and without compression suffix,
  dependency order;
- a built-in or already loaded module is skipped, also as a dependency;
- a failed dependency skips the module that needs it;
- a dependency that is also listed gets the parameters of its config line;
- `MODULE_INIT_COMPRESSED_FILE` is set for `.ko.*` files only;
- no listed module leaves `firmware_class.path` alone;
- `EEXIST` is treated as loaded.

`finit_module` sits behind a small trait, so the tests never load a module.

On hardware (phyGATE Tauri-L):

- `dmesg` shows `imx-sdma … loaded firmware` and the TPM on `spi0.1` before
  systemd starts;
- with `imx-sdma.ko` removed from the rootfs, the device boots normally and
  logs the warning;
- an image for another machine contains no loader code (`cargo build` without
  the feature).

## 12. Comparison to legacy

| | Legacy `90-imx_sdma` | This design |
|---|---|---|
| Gate | `mx8mm-nxp-bsp` override | Cargo feature, set for `mx8mm-nxp-bsp` |
| Module list | fixed in the script | config files in the rootfs |
| Module load | `modprobe` from a bind-mounted `/lib/modules` | `finit_module` on the rootfs file, dependencies from `modules.dep` |
| Firmware | bind-mounted `/lib/firmware`, left mounted | `firmware_class.path` set to the rootfs |
| Errors | ignored | logged, never fatal |
| Position | after the rootfs mount | after the rootfs mount |

## 13. Alternatives considered

1. **Load only `imx_sdma`** — the same step with the module name fixed in the
   code. Less code, but every new module needs a code change and a new
   feature.
2. **Keep the legacy approach** — bind-mount the rootfs `/lib/modules` and
   `/lib/firmware` into the initramfs and run `modprobe`. It works, but it
   needs `kmod` in the initramfs and leaves two mounts behind in the
   initramfs that cannot be unmounted while a firmware load may still run.
3. **Load the modules early in the rootfs** — `modules-load.d` entries, so
   `systemd-modules-load.service` loads them before `sysinit.target` instead
   of udev loading them later. No init code is needed, and most services start
   after `sysinit.target`, so the gap gets much smaller. It does not close:
   units that run before `sysinit.target` still race, and a deferred probe
   such as `spi-imx` finishes asynchronously after the module load.
4. **Build the drivers into the kernel and put the firmware into the
   initramfs** — for SDMA, `CONFIG_IMX_SDMA=y`. The firmware loader waits for
   the initramfs to be unpacked, so a built-in driver finds the firmware there
   at probe time. This removes the race completely, but it changes the kernel
   for every image of the machine, puts a second copy of the firmware into the
   initramfs that has to match the rootfs package, and needs a new
   decision about the firmware license, which is the reason the driver is a
   module.
5. **Do nothing** — every consumer orders itself on its device unit, as
   `aziot-tpmd` does. This puts the burden on each application and on each
   UART user, and it gives up a guarantee the legacy initramfs provided.
