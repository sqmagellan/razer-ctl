# razer-ctl: Razer Blade control without Synapse

[![razer-ctl-ci](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml/badge.svg)](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml)
[![latest release](https://img.shields.io/github/v/release/sqmagellan/razer-ctl)](https://github.com/sqmagellan/razer-ctl/releases/latest)

**[Download the latest release](https://github.com/sqmagellan/razer-ctl/releases/latest)**: two
standalone `.exe` files for Windows x86-64.

A tray app and a CLI that set a Razer Blade's performance mode, fans, lighting and charge limit over
HID. No Synapse, installer, service or account.

Every hardware claim was checked on one Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`,
Windows 11, unless a user report is cited. Other models: see [Device support](#device-support).

`razer-tray.exe` is the tray icon, menu and tooltip. `razer-cli.exe` does the same from a shell.
`librazer` holds the HID protocol and the testable logic. [`CHANGELOG.md`](CHANGELOG.md) has the
history and measurements; `README.upstream.md` covers the original project.

## What it controls

- **Performance mode**: Battery, Silent, Balanced, Performance, Hyperboost, and Custom (CPU and GPU
  boost, plus Max fan speed).
- **Fan**: Auto, or Manual at a set RPM within the model's range (2000–5000 on the 2023 Blade 16;
  3800 or more in Custom). Only Auto can stop the fans.
- **Keyboard lighting**: the EC's effects (Off, Spectrum, Wave, Breathing) or a custom color: config
  presets, one color or one per row, or "Follow performance mode" (matches the tray icon). Optional
  **Battery bar**: keys 1–0 show the charge in tens, cyan while charging. Brightness 0–100%. Fn keys
  keep working. Custom colors are mapped for `0x029F` only.
- **Keep keyboard lit**: stops the idle fade.
- **Logo lighting**: off, static or breathing.
- **Charge limit**: 50–100% (100 = off).
- **Plugged-in and battery profiles**: each keeps its own settings and switches on plug or unplug.
  The top of the menu names the one you're changing. On first run the tray reads the laptop into
  the current profile and writes nothing; the other profile is read the first time the laptop runs
  on that source.
- **Screen refresh rate** per profile. Default: "Leave as Windows has it".
- **Match Windows power to Razer performance mode** (off by default): Battery and Silent → Best power
  efficiency, Balanced → Balanced, the rest → Best performance. Turning it off restores each source's
  earlier mode.
- **Game rules ("Actions")**: settings that apply while a named app runs. A rule can match several
  executables, carry a `priority`, be disabled, or require AC (`require_ac`). It changes only the
  fields it sets, never the saved profile. Edits apply within a few seconds. The **Game rules** menu
  turns each rule on or off and marks the one running. **Rules window…** adds, edits and deletes
  rules in a standard Windows dialog; settings it doesn't show are kept. **Edit rules…** opens the
  config in Notepad beside `game-rules-help.txt`, a guide to every rule setting. On AC,
  `cpu_max_mhz` (800 or more) caps the CPU speed through the active Windows power plan. The old
  values come back when the rule ends or the tray quits, or at the next start after a crash; a
  setting changed meanwhile to another value is kept.
- **Perf-cycle hotkey** (off by default): `cycle_perf_hotkey = "Ctrl+Alt+P"` in the config acts like
  a left-click on the icon. Needs a modifier; read at startup.
- **Undo changes made by Synapse** (off by default): puts perf mode, fan, logo and charge limit back
  when something else changes them. Brightness is left alone. The menu says when Synapse is running;
  the tray never stops it.
- **Temperatures** in the tooltip. The CPU figure is the EC's sensor near the CPU (`0x0d85`), not the
  die. The GPU figure comes from Windows, as in Task Manager. `nvidia-smi` runs only for GPU watts,
  and for the temperature where Windows doesn't report it.
- **Close apps using the GPU**: ends dGPU processes, skipping system ones.
- **What's keeping the GPU awake?**: lists the apps with dedicated memory on the NVIDIA GPU,
  without waking it. `razer-tray --gpu-report <file>` writes the same report.
- **Start with Windows**: an `HKCU\…\Run` entry.
- **Status for scripts**: `razer-cli auto json` prints the state as flat JSON, actual fan RPM
  included (for a Home Assistant sensor or a status line). A value the laptop doesn't report is
  `null` and listed in `unsupported`. In Manual, `fan_floor_rpm` is the perf mode's measured
  minimum where known (3800 in Custom on the 2023 Blade 16). With Max Fan off the fans settle at the
  larger of it and `fan_setpoint_rpm`. `ec_cpu_temp_c` and `ec_gpu_temp_c` are the EC's
  temperatures. `charger`, `charger_watts` and `charger_code` are the power source;
  `charger_recommended_watts` is the charger the laptop asks for, and `auto info` says when they
  differ.
- **Model reports**: `razer-cli probe --out probe.json` reads every setting and saves the EC's
  answers, to attach to a device-support issue. It only reads. `probe --writes` (tray closed) also
  tests each write, then restores and checks the settings.

CLI exit codes:

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | unclassified error |
| 2 | usage error |
| 3 | no usable Razer laptop found |
| 4 | this model doesn't support the command |
| 5 | device busy, rejected or out of step; retry |

Config: `%APPDATA%\razer-tray\config\default-config.toml`. Log:
`%LOCALAPPDATA%\razer-tray\razer-tray.log` (1 MiB, three older files kept).

## Device support

**Tested:** Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`, Windows 11. One unit.

**Reported by users:**

| Model | USB PID | Works | Doesn't work yet |
|---|---|---|---|
| Razer Blade 18 (2023), `RZ09-0484U` ([#1](../../issues/1)) | `0x02A0` | Perf modes, Auto fan, Max Fan, keyboard brightness and effects, logo, charge limit, Fn keys | Manual fan RPM: the fans hold near 3200/3300 |
| Razer Blade 15 (2018) Base, `RZ09-0270` ([#2](../../issues/2)) | `0x023B` | Fan modes and RPM (3500–5700), keyboard brightness and effects | Perf modes change nothing noticeable. No charge limit, logo control or fan speed read. |
| Razer Blade 17 (2022), `RZ09-0423` ([#10](../../issues/10)) | `0x028B` | Perf modes, CPU/GPU boost, Auto fan, keyboard brightness and effects, logo, charge limit read | Manual fan RPM not followed exactly (max seen 4300). Max Fan answers not supported (0x05) |

**Catalogued:** a profile from the upstream project's tables. Not tested here.

| Model | USB PID | Manual fan range |
|---|---|---|
| Razer Blade 16 (2023), **tested** | `0x029F` | 2000–5000 RPM (measured) |
| Razer Blade 16 (2023) Black | `0x029F` | 2000–5000 RPM (measured) |
| Razer Blade 14 (2023) Mercury | `0x029D` | 2200–5000 RPM |
| Razer Blade 16 (2024) | `0x02B7` | 2200–5000 RPM |
| Razer Blade 16 (2025) RTX 5070 / 5080 / 5090 | `0x02C6` | 2200–5000 RPM |
| Razer Blade 15 (2022) | `0x028A` | 3500–5000 RPM |

**Other models** get a generic profile with every feature offered and a fan range from community
data for 44 more PIDs. There's no init sequence; a guessed one is worse than none. A logo, keyboard
brightness, charge limit or Max Fan the firmware lacks shows "(not supported)" and is left alone;
any other missing command fails with an error that names it. `razer-cli auto info` marks what's
missing. Expect perf modes and fans to work. If you try one, please
[open an issue](../../issues/new/choose).

### Left out on purpose

- **Temperature-driven fan curves.** A CPU die temperature on Windows needs a kernel driver. The
  usual one (WinRing0) has CVE-2020-14979, and Defender has quarantined it since March 2025. The ACPI
  thermal zones are stubs. The EC's sensor needs no driver, but it isn't the die, and its lag under
  load hasn't been measured.
- **Cloud profiles, macros and a per-key lighting editor.**

## Install

Download both `.exe` files from the
[latest release](https://github.com/sqmagellan/razer-ctl/releases/latest), put the tray anywhere, run
it and tick "Start with Windows". Neither needs admin rights or writes outside your profile.

**The binaries aren't code-signed**, so SmartScreen warns on first run (*More info* → *Run anyway*).
Each release lists a SHA-256 per file in `SHA256SUMS.txt`:

```powershell
Get-FileHash .\razer-tray.exe -Algorithm SHA256
```

With the [GitHub CLI](https://cli.github.com/) you can also check that this repo's release workflow
built the file from the tag:

```
gh attestation verify razer-tray.exe --repo sqmagellan/razer-ctl --source-ref refs/tags/vX.Y.Z --signer-workflow sqmagellan/razer-ctl/.github/workflows/release.yml
```

GitHub Actions builds every release binary from a tag. None is uploaded by hand.

## Quirks

[`CHANGELOG.md`](CHANGELOG.md) has the measurements.

- **A custom color is a frame the tray repaints.** The EC's static effect ignores its color and shows
  Razer green, so the tray writes the rows itself (`0x030b`, then `0x030a [5, 0]`). The EC doesn't
  keep the frame, so the tray repaints after a wake and after the idle fade. Effect speed is fixed.
- **Keep keyboard lit is a keep-alive.** The only firmware switch (driver mode, `0x0004`) kills the
  Fn media keys, so the tray stays in Normal mode and reads the brightness every ~3 s while the
  display is on. Reads write nothing.
- **Fan reads need filtering.** About 3 in 10 `0x0d88` samples carry one impossible zone. The tray
  drops them, and a stop needs two reads in a row. The tach reads in 100 RPM steps.
- **A set point of 0 doesn't hand the fan back to the firmware.** It stays Manual at 2000.
- **In Custom the 2023 Blade 16 runs a manual fan at 3800 RPM or more.** A lower set point reads
  back as written while the fans settle at 3800; Balanced follows 2000–5000. The menu marks those
  presets "(runs at 3800)", and `razer-cli` says so. Measured on an `RZ09-0483U`; the Black edition
  shares its PID and gets the same handling, untested.
- **Undo changes made by Synapse is last-writer-wins.** It beats occasional changes, not a tool that
  writes constantly.
- **Close apps using the GPU is conservative.** It skips session-critical processes and anything
  nvidia-smi can't name.
- **Device-loss recovery isn't tested on hardware.** Windows won't disable the internal keyboard
  that carries the control interface.
- **A config that doesn't parse is kept and not applied.** The tray saves a timestamped copy
  (`default-config.toml.invalid-<unix time>`) and says so in a dialog and at the top of the menu.
  Until the file parses or you pick **Reset settings to defaults**, the tray writes nothing to the
  laptop on its own and nothing over the file; menu picks still apply. A reset is a first run. Saves
  are atomic. Comments in the file are lost when the tray saves.
- **`battery-care get` can lag a `set` by 1–2 s on some models.** `battery-care set` reads back
  until the new limit shows.
- **The tooltip holds 63 characters, not 128** (`tray-icon` 0.19 leaves `cbSize` at 0). Less
  important fields drop out when a long `Custom (CPU …, GPU …)` label needs the room.
- **The tooltip refreshes on hover.** A background poll still drives "Undo changes made by Synapse"
  and picks up Fn brightness changes.

## Building

Stable Rust. On Windows: VS 2022 Build Tools and the MSVC toolchain. From macOS or Linux,
[`cargo xwin`](https://github.com/rust-cross/cargo-xwin) cross-builds.

```
cargo test -p librazer                                  # host-testable logic; runs anywhere
cargo clippy --all-targets -- -D warnings
cargo build --release --target x86_64-pc-windows-msvc   # or: cargo xwin build --release --target ...
```

`librazer::device` needs `hidapi` and builds only on Windows and Linux; on macOS use
`cargo test -p librazer`.

CI runs tests, clippy and `cargo fmt --check` and builds for Windows and Linux; `cargo audit` runs
weekly. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before sending a patch: `descriptor.rs` is
hand-maintained hardware data, and a wrong RPM range or init sequence is worse than none.

## Credits

Original by Tarek Dakhran ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)), multi-model
fork by blauzim ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)). Both MIT; so is this
fork. Protocol notes and fixes from Finesssee ([Finesssee/razer-ctl](https://github.com/Finesssee/razer-ctl),
MIT). Maintained by sqmagellan, developed with Claude Opus 5.5.

Protocol facts (command IDs, the checksum, `0x0004` being device mode) came from reading
[OpenRazer](https://github.com/openrazer/openrazer) and
[razer-laptop-control](https://github.com/Razer-Linux/razer-laptop-control-no-dkms), both GPL. No
code was copied from either.
