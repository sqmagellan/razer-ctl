# razer-ctl — Razer Blade control without Synapse

[![razer-ctl-ci](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml/badge.svg)](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml)
[![latest release](https://img.shields.io/github/v/release/sqmagellan/razer-ctl)](https://github.com/sqmagellan/razer-ctl/releases/latest)

**[Download the latest release](https://github.com/sqmagellan/razer-ctl/releases/latest)** — two
standalone `.exe` files, Windows x86-64. No installer.

A tray app and a CLI that drive a Razer Blade's performance modes, fans, lighting, and battery care
straight over HID, so you don't need Synapse running. No installer, no service, no account. The tray
binary is 1.8 MB.

**Scope, up front:** this is a fork maintained by one person with one laptop. Every hardware claim
here was verified on a Razer Blade 16 (2023), `RZ09-0483`, USB PID `0x029F`, Windows 11, and nowhere
else. An unrecognized model gets a generic profile instead of refusing to start, and any command the
firmware doesn't implement fails with one clean error. [Device support](#device-support) says what
to expect.

`razer-tray.exe` is the daily driver, a tray icon with a menu and a live tooltip. `razer-cli.exe`
does the same things from the command line. `librazer` holds the HID protocol, the device model, and
the host-testable logic. `README.upstream.md` covers the wider multi-model project this forked from,
and [`CHANGELOG.md`](CHANGELOG.md) carries the release history and the measurements behind it.

## What it controls

- **Performance modes** — Balanced, Silent, Battery, Performance, Hyperboost, and Custom (per-axis
  CPU/GPU boost).
- **Fan** — Auto, or Manual at a real RPM. The range is per-chassis, so the menu only offers speeds
  the EC honors: 2000–5000 on the 2023 Blade 16. 2000 is a hardware floor, not a preference, so
  sub-floor writes are rejected rather than silently accepted. Only *Auto* can stop the fans.
- **Keyboard brightness** — 0–100% in 10% steps.
- **Logo lighting** — off, static, or breathing, on its own zone.
- **Keyboard lighting** — Off, Spectrum, Wave, or Breathing. These are the EC's built-in effects,
  run in Normal mode, so the Fn keys keep working. No color picker by design; see
  [Quirks](#quirks).
- **Charge limit** — any whole percent from 50 to 100 (100 = off).
- **AC vs battery profiles** — separate profiles, switched when you plug or unplug.
- **App profiles ("Actions")** — apply settings while a named app runs, then fall back. A rule can
  match several executables, carry a `priority`, be disabled without being deleted, and be gated to
  AC with `require_ac`. It overlays only the fields it sets, and it's transient, so it never
  overwrites a saved profile. A setting you pick while it runs holds until the app exits. Edits to
  the rules in the config file take effect within a few seconds, without a restart.
- **Refresh rate per power source** — a menu of the rates the display offers at its current
  resolution. The pick is stored in the AC or battery profile, so it switches when you plug or
  unplug. "Don't change" (the default) leaves the display alone.
- **Match Windows power to Razer performance mode** (off by default, in the Performance mode
  menu) — moves the Settings "Power mode" slider with the perf mode: Battery/Silent → Best power
  efficiency, Balanced → Balanced, the rest → Best performance. While it's on, a line under it
  shows the mode Windows reports. Turning it off puts back the mode Windows had before, for each
  power source.
- **Perf-cycle hotkey** (off by default) — set `cycle_perf_hotkey = "Ctrl+Alt+P"` in the config; it
  acts like a left-click on the tray icon. A modifier is required. Read at startup.
- **Synapse warning** — if Razer Synapse is running, the menu says so at the top. The tray never
  stops another program's services.
- **Keyboard always-on** — a Normal-mode keep-alive, not Razer's driver-mode flag, so the Fn media
  keys keep working.
- **Enforce mode** (off by default) — re-asserts perf, fan, logo, and charge limit if Synapse
  changes them. It leaves brightness alone.
- **Close GPU apps** — terminates dGPU processes behind a hard safelist, so it can't take down the
  desktop.
- **Start with Windows** — an `HKCU\…\Run` entry.
- **Machine-readable status** — `razer-cli auto json` prints the whole device state, including the
  actual fan RPM, as flat JSON, ready for a Home Assistant sensor or a shell status line.
  `razer-cli enumerate --json` gives the model/PID block a device-support report needs.

Exit codes, so a script can branch without parsing stderr:

| Code | Meaning |
|---|---|
| 0 | success — the change actually happened |
| 1 | unclassified error |
| 2 | usage error, from the argument parser |
| 3 | no usable Razer laptop found — retrying won't help |
| 4 | command not supported by this model — definitive, stop asking |
| 5 | device communication error (busy, rejected, out of step) — retryable |

All verified on hardware, 4 by issuing a command this EC genuinely refuses. 2 is skipped
deliberately, because the argument parser exits 2 from inside its own code, before ours runs.

Config lives at `%APPDATA%\razer-tray\config\default-config.toml`, the log at
`%LOCALAPPDATA%\razer-tray\razer-tray.log` (Info level, 1 MiB, rotated through three older files).

## Device support

**Tested:** Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`, Windows 11. One physical unit.

**Catalogued:** these ship a specific profile, inherited from the upstream project's tables. They
aren't tested here.

| Model | USB PID | Manual fan range |
|---|---|---|
| Razer Blade 16 (2023) — **tested** | `0x029F` | 2000–5000 RPM (measured) |
| Razer Blade 16 (2023) Black | `0x029F` | 2000–5000 RPM (measured) |
| Razer Blade 14 (2023) Mercury | `0x029D` | 2200–5000 RPM |
| Razer Blade 16 (2024) | `0x02B7` | 2200–5000 RPM |
| Razer Blade 16 (2025) RTX 5070 / 5080 / 5090 | `0x02C6` | 2200–5000 RPM |
| Razer Blade 15 (2022) | `0x028A` | 3500–5000 RPM |

**Everything else** starts anyway on a generic profile. All features are offered, the fan envelope
comes from a table of 44 further PIDs transcribed from community data, and there's deliberately no
init sequence, because inventing a plausible one is worse than having none. Unsupported commands
answer `NotSupported` and fail immediately. So expect perf modes and fans to work, expect some
lighting or battery features possibly not to, and expect to be told which. If you try it, please
[open an issue](../../issues/new/choose).

### What is NOT here, on purpose

- **No arbitrary keyboard color.** It needs Razer driver mode, which disables the Fn media keys.
- **No temperature-driven fan curve.** An honest CPU temperature on Windows needs a kernel driver,
  and the usual one (WinRing0) carries CVE-2020-14979 and has been quarantined by Defender since
  March 2025. Windows' own ACPI thermal zones aren't a substitute: tested here, they're frozen stubs
  that held exactly 45.1 °C and 27.9 °C through a 25-second four-core load. dGPU temperature via
  NVIDIA is real and is in the tooltip.
- **No Synapse-style cloud profiles, macros, or per-key lighting.**

## Install

Grab both `.exe` files from the
[latest release](https://github.com/sqmagellan/razer-ctl/releases/latest). Drop the tray binary
somewhere, run it, and use "Start with Windows" to autostart. The CLI is standalone. Nothing needs
administrator rights, and nothing is written outside your own user profile.

**The binaries aren't code-signed**, so SmartScreen will warn you the first time you run one
("Windows protected your PC" → *More info* → *Run anyway*). Signing a hobby project costs real money
per year, which is the whole reason, not an oversight. What you get instead is a published SHA-256
for every file, to compare against `SHA256SUMS.txt` in the same release:

```powershell
Get-FileHash .\razer-tray.exe -Algorithm SHA256
```

Stronger than the hash, if you have the [GitHub CLI](https://cli.github.com/): each binary carries a
Sigstore-backed build-provenance attestation.

```
gh attestation verify razer-tray.exe --repo sqmagellan/razer-ctl
```

That confirms the exact repository, workflow, and commit that produced the file. A hash can't tell
you that, and proves nothing if whoever tampered with the binary could also edit the page hosting
the hash. Every release binary is built by GitHub Actions from a tagged commit, never uploaded from
a personal machine.

## Quirks

The hard-won ones. [`CHANGELOG.md`](CHANGELOG.md) carries the measurements behind each.

- **Custom keyboard color is a frame the tray keeps repainting.** The EC's own static effect
  ignores the color and shows Razer green, and this README used to say color needed driver mode.
  It doesn't: six `0x030b` row writes followed by `0x030a [5, 0]` show any colors in Normal mode,
  with the Fn keys intact (verified 2026-09-23). The EC doesn't keep the frame, and standby brings
  the stored effect back, so the tray repaints it after every wake. Effect speed is still fixed.
- **Always-on is a keep-alive, because the firmware flag killed every Fn media key.** Razer's
  `0x0004` is device mode, and Enable is `0x03` = driver mode, which hands the whole Fn layer to a
  host driver. There's no firmware backlight-timeout knob and no HID LampArray, so we stay in Normal
  mode and re-brighten with a brightness *read* every ~3 s while the display's on. A read writes
  nothing, so it never fights the Fn keys.
- **Fan reads need filtering.** Roughly 3 in 10 sparse `0x0d88` samples carry one impossible zone,
  which can't happen at the measured 23–53 RPM/s slew. `FanRpmFilter` drops lone-zone reads and
  makes a stop earn two consecutive confirmations. Tach output is quantised to 100 RPM.
- **A set point of 0 doesn't hand back to the firmware curve.** The mode stays Manual and the fan
  holds 2000, so razer-control-revived's "0 clears the manual flag" behavior must not be ported.
- **Enforce mode is last-writer-wins.** It beats occasional changes but loses to a tool that
  re-asserts every sub-second.
- **"Close GPU apps" is conservative on purpose.** It skips session-critical processes and anything
  nvidia-smi can't name, so a few stubborn dGPU users may survive rather than risk the desktop.
- **Device-loss recovery isn't runtime-tested.** The control interface rides the internal keyboard's
  USB composite, which Windows won't let you disable, so the backoff is code-reviewed only.
- **A config the app can't parse is preserved, not eaten.** At startup it logs the error and keeps a
  timestamped copy (`default-config.toml.invalid-<unix time>`). A hand edit that doesn't parse
  while the tray runs is left alone, and nothing is saved over it until it's fixed. Writes are
  atomic (temp file, then rename). Comments in the file are not preserved when the tray saves.
- **`battery-care get` can read stale right after a `set`** (~1–2 s firmware lag). Re-read to
  confirm.
- **The tray tooltip really holds 63 characters, not 128.** `tray-icon` 0.19 leaves `cbSize` at 0,
  which matches no declared struct version, so the shell silently behaves like the original
  `[u16; 64]` layout. The tooltip is therefore *budgeted*, not appended-to: fields carry a priority
  and the least important ones drop out when a long `Custom (CPU …, GPU …)` label crowds them.
- **Tooltip refresh is hover-driven**, which is what let me delete the global `WH_KEYBOARD_LL`
  keyboard hook, the most invasive Windows code in the crate. The input-gated Mirror poll stays,
  because it's the anti-pulsing gate that also drives enforce-drift and Fn-brightness adopt.

## Building

Stable Rust, no nightly features. On Windows you need the VS 2022 Build Tools and the MSVC
toolchain. From macOS or Linux, [`cargo xwin`](https://github.com/rust-cross/cargo-xwin)
cross-builds the shipped target offline.

```
cargo test -p librazer                                  # host-testable logic; runs anywhere
cargo clippy --all-targets -- -D warnings
cargo build --release --target x86_64-pc-windows-msvc   # or: cargo xwin build --release --target ...
```

`-p librazer` isn't laziness. `librazer::device` is gated to Windows and Linux because it needs
`hidapi`, so a bare `cargo test` fails to compile `razer-cli` on macOS.

CI runs the tests, clippy, `cargo fmt --check`, and builds both `x86_64-pc-windows-msvc` and Linux.
A separate weekly workflow runs `cargo audit`. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before you
send a patch. The short version: `librazer` is the testable core, `descriptor.rs` is hand-maintained
hardware data that must carry a real measured `fan_rpm_range`, and a wrong RPM ceiling or init
sequence is worse than an absent one.

## Credits

Original by Tarek Dakhran ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)), multi-model
fork by blauzim ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)). Both MIT, and this fork
stays MIT. Maintained by sqmagellan, developed with Claude Opus 5.

Protocol knowledge came from reading [OpenRazer](https://github.com/openrazer/openrazer) and
[razer-laptop-control](https://github.com/Razer-Linux/razer-laptop-control-no-dkms), both GPL, for
the *facts* they document: command IDs, the checksum algorithm, and that `0x0004` is device mode and
not a backlight flag. No code was copied from either. Facts about hardware aren't copyrightable,
expression is, and none was taken.
