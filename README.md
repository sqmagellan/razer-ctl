# razer-ctl: Razer Blade control without Synapse

[![razer-ctl-ci](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml/badge.svg)](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml)
[![latest release](https://img.shields.io/github/v/release/sqmagellan/razer-ctl)](https://github.com/sqmagellan/razer-ctl/releases/latest)

**[Download the latest release](https://github.com/sqmagellan/razer-ctl/releases/latest)**: two
standalone `.exe` files for Windows x86-64.

A tray app and a CLI that set a Razer Blade's performance mode, fans, lighting and charge limit over
HID, without Synapse. No installer, service or account. The tray binary is 1.8 MB.

This fork is maintained by one person with one laptop. Every hardware claim here was checked on a
Razer Blade 16 (2023), `RZ09-0483`, USB PID `0x029F`, Windows 11, and nowhere else. An unknown model
starts on a generic profile, and a command its firmware lacks fails with one clear error. See
[Device support](#device-support).

`razer-tray.exe` is a tray icon with a menu and a live tooltip. `razer-cli.exe` does the same from a
shell. `librazer` holds the HID protocol, the device model and the host-testable logic.
`README.upstream.md` covers the multi-model project this forked from. [`CHANGELOG.md`](CHANGELOG.md)
has the release history and the measurements behind it.

## What it controls

- **Performance mode**: Battery, Silent, Balanced, Performance, Hyperboost, and Custom (CPU and GPU
  boost set separately, plus Max fan speed).
- **Fan**: Auto, or Manual at a set RPM. The menu offers only speeds the EC accepts, 2000–5000 RPM
  on the 2023 Blade 16. 2000 is a hardware floor, so lower writes are rejected. Only Auto can stop
  the fans.
- **Keyboard lighting**: the EC's built-in effects (Off, Spectrum, Wave, Breathing), or a custom
  color. Custom colors are presets from the config, one color or one per row, or "Follow performance
  mode", which matches the tray icon's color. Optional **Battery bar**: keys 1–0 show the charge in
  tens, the rest dimmed, cyan while charging. Brightness 0–100% in 10% steps. Everything runs in
  Normal mode, so the Fn keys keep working. Custom colors are mapped for `0x029F` only.
- **Keep keyboard lit**: stops the idle fade with a Normal-mode keep-alive.
- **Logo lighting**: off, static or breathing.
- **Charge limit**: 50–100% (100 = off).
- **Plugged-in and battery profiles**: each keeps its own settings and switches when you plug or
  unplug. The top of the menu says which one your changes go to.
- **Screen refresh rate** per profile, from the rates the display offers at its current resolution.
  "Leave as Windows has it" is the default.
- **Match Windows power to Razer performance mode** (off by default, in Performance mode): sets the
  Windows "Power mode" with each change. Battery and Silent → Best power efficiency, Balanced →
  Balanced, the rest → Best performance. A line under it shows what Windows reports. Turning it off
  puts back the mode each power source had before.
- **App profiles ("Actions")**: settings that apply while a named app runs. A rule can match several
  executables, carry a `priority`, be disabled, and require AC (`require_ac`). It changes only the
  fields it sets and never writes the saved profile. A pick you make while it runs holds until the
  app exits. Edits to the rules take effect within a few seconds.
- **Perf-cycle hotkey** (off by default): `cycle_perf_hotkey = "Ctrl+Alt+P"` in the config acts like
  a left-click on the icon. Needs a modifier. Read at startup.
- **Undo changes made by Synapse** (off by default): re-applies perf mode, fan, logo and charge limit
  when something else changes them. Brightness is left alone. If Synapse is running, the menu says so
  at the top. The tray never stops another program.
- **Close apps using the GPU**: ends dGPU processes, skipping a safelist of system processes.
- **Start with Windows**: an `HKCU\…\Run` entry.
- **Status for scripts**: `razer-cli auto json` prints the device state, including actual fan RPM, as
  flat JSON (for a Home Assistant sensor or a status line). `razer-cli enumerate --json` prints the
  model block a device-support report needs.

CLI exit codes:

| Code | Meaning |
|---|---|
| 0 | success; the change happened |
| 1 | unclassified error |
| 2 | usage error (from the argument parser) |
| 3 | no usable Razer laptop found; retrying won't help |
| 4 | this model doesn't support the command |
| 5 | device communication error (busy, rejected, out of step); retryable |

All checked on hardware except 2, which the argument parser raises before our code runs.

The config is `%APPDATA%\razer-tray\config\default-config.toml`. The log is
`%LOCALAPPDATA%\razer-tray\razer-tray.log` (1 MiB, three older files kept).

## Device support

**Tested:** Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`, Windows 11. One unit.

**Catalogued:** these have a specific profile from the upstream project's tables. Not tested here.

| Model | USB PID | Manual fan range |
|---|---|---|
| Razer Blade 16 (2023), **tested** | `0x029F` | 2000–5000 RPM (measured) |
| Razer Blade 16 (2023) Black | `0x029F` | 2000–5000 RPM (measured) |
| Razer Blade 14 (2023) Mercury | `0x029D` | 2200–5000 RPM |
| Razer Blade 16 (2024) | `0x02B7` | 2200–5000 RPM |
| Razer Blade 16 (2025) RTX 5070 / 5080 / 5090 | `0x02C6` | 2200–5000 RPM |
| Razer Blade 15 (2022) | `0x028A` | 3500–5000 RPM |

**Other models** start on a generic profile with every feature offered. The fan range comes from a
table of 44 more PIDs transcribed from community data. There is no init sequence, because a guessed
one is worse than none. Commands the firmware lacks answer `NotSupported` and fail at once. Expect
perf modes and fans to work and some lighting or battery features not to; the error says which. If
you try one, please [open an issue](../../issues/new/choose).

### Left out on purpose

- **Temperature-driven fan curves.** A real CPU temperature on Windows needs a kernel driver, and
  the usual one (WinRing0) has CVE-2020-14979 and has been quarantined by Defender since March 2025.
  The ACPI thermal zones are stubs: here they held 45.1 °C and 27.9 °C through a 25-second
  four-core load. The dGPU temperature from NVIDIA is real and is in the tooltip.
- **Cloud profiles, macros and a per-key lighting editor.**

## Install

Download both `.exe` files from the
[latest release](https://github.com/sqmagellan/razer-ctl/releases/latest), put the tray binary
anywhere, run it, and tick "Start with Windows". The CLI is standalone. Neither needs administrator
rights or writes outside your user profile.

**The binaries aren't code-signed**, so SmartScreen warns on first run ("Windows protected your PC"
→ *More info* → *Run anyway*). Signing costs money every year. Each release lists a SHA-256 for every
file in `SHA256SUMS.txt`:

```powershell
Get-FileHash .\razer-tray.exe -Algorithm SHA256
```

With the [GitHub CLI](https://cli.github.com/) you can also check the build-provenance attestation,
which ties the file to this repository, its release workflow and the tagged commit:

```
gh attestation verify razer-tray.exe --repo sqmagellan/razer-ctl --source-ref refs/tags/vX.Y.Z --signer-workflow sqmagellan/razer-ctl/.github/workflows/release.yml
```

A hash alone proves nothing if someone could change both the file and the page that lists the hash.
Release binaries are built by GitHub Actions from a tag, never uploaded from a personal machine.

## Quirks

[`CHANGELOG.md`](CHANGELOG.md) has the measurements behind each.

- **A custom color is a frame the tray repaints.** The EC's static effect ignores its color argument
  and shows Razer green. Six `0x030b` row writes followed by `0x030a [5, 0]` show any colors in
  Normal mode with the Fn keys intact. The EC doesn't store the frame: standby and the idle fade
  bring back the stored effect, so the tray repaints after a wake and on the first input after the
  fade. Effect speed is fixed.
- **Keep keyboard lit is a keep-alive.** Razer's `0x0004` is device mode, and its Enable value is
  driver mode, which hands the whole Fn layer to a host driver and kills the Fn media keys. There's
  no firmware timeout setting and no HID LampArray, so the tray stays in Normal mode and reads the
  brightness every ~3 s while the display is on. A read writes nothing, so it never fights the Fn
  keys.
- **Fan reads need filtering.** About 3 in 10 sparse `0x0d88` samples carry one impossible zone,
  which the measured 23–53 RPM/s slew rules out. `FanRpmFilter` drops them, and a stop needs two
  reads in a row. The tach reads in 100 RPM steps.
- **A set point of 0 doesn't return the fan to the firmware curve.** The mode stays Manual at 2000,
  so razer-control-revived's "0 clears the manual flag" must not be ported.
- **Undo changes made by Synapse is last-writer-wins.** It beats occasional changes and loses to a
  tool that writes every fraction of a second.
- **Close apps using the GPU is conservative.** It skips session-critical processes and anything
  nvidia-smi can't name, so a few dGPU users may survive.
- **Device-loss recovery isn't runtime-tested.** The control interface is part of the internal
  keyboard's USB composite device, which Windows won't disable, so the backoff is only code-reviewed.
- **A config that doesn't parse is kept.** At startup the tray logs the error and saves a timestamped
  copy (`default-config.toml.invalid-<unix time>`). A bad hand edit made while it runs is left alone
  and nothing is saved over it until it's fixed. Saves are atomic (temp file, then rename). Comments
  in the file are lost when the tray saves.
- **`battery-care get` can be stale right after a `set`** (~1–2 s firmware lag). Read it again.
- **The tooltip holds 63 characters, not 128.** `tray-icon` 0.19 leaves `cbSize` at 0, and the shell
  falls back to the original 64-unit layout. Each tooltip field has a priority, and the least
  important drop out when a long `Custom (CPU …, GPU …)` label needs the room.
- **The tooltip refreshes on hover**, which let the global `WH_KEYBOARD_LL` hook go. The input-gated
  poll stays: it drives the drift checks for "Undo changes made by Synapse" and picks up Fn brightness
  changes.

## Building

Stable Rust. On Windows: the VS 2022 Build Tools and the MSVC toolchain. From macOS or Linux,
[`cargo xwin`](https://github.com/rust-cross/cargo-xwin) cross-builds the Windows target.

```
cargo test -p librazer                                  # host-testable logic; runs anywhere
cargo clippy --all-targets -- -D warnings
cargo build --release --target x86_64-pc-windows-msvc   # or: cargo xwin build --release --target ...
```

`librazer::device` needs `hidapi` and is built only on Windows and Linux, so a bare `cargo test`
fails on macOS; use `-p librazer`.

CI runs the tests, clippy and `cargo fmt --check`, and builds for Windows and Linux. A weekly
workflow runs `cargo audit`. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before sending a patch. In
short: `librazer` is the testable core, `descriptor.rs` is hand-maintained hardware data that needs a
measured `fan_rpm_range`, and a wrong RPM ceiling or init sequence is worse than none.

## Credits

Original by Tarek Dakhran ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)), multi-model
fork by blauzim ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)). Both MIT; this fork is
MIT too. Maintained by sqmagellan, developed with Claude Opus 5.5.

Protocol facts (command IDs, the checksum, `0x0004` being device mode) came from reading
[OpenRazer](https://github.com/openrazer/openrazer) and
[razer-laptop-control](https://github.com/Razer-Linux/razer-laptop-control-no-dkms), both GPL. No
code was copied from either.
