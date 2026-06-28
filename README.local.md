# razer-ctl (local fork) — Razer Blade tray control without Synapse

A local hardening fork of [blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)
(itself a fork of Tarek Dakhran's original [tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)).
It lets you control a Razer Blade's performance, fans, lighting, and battery-care
**directly over the keyboard's HID control interface — no Razer Synapse required**.

> **Scope of this fork.** All work lives on a local-only git branch (`local`) that is
> **never pushed**; `main` continues to track upstream. This README documents the local
> build; the upstream `README.md` covers the broader multi-model project.
>
> **Validated hardware:** Razer Blade 16 (2023), model `RZ09-0483`, USB HID PID `0x029F`,
> Windows 11. Upstream lists other Blade models, but this fork's runtime testing was done
> only on the 2023 16".

## Components

| Binary | What it is |
|---|---|
| `razer-tray.exe` | The daily-driver Windows tray app (menu + tooltip + background reconciliation). |
| `razer-cli.exe`  | A scriptable CLI for the same device features (`enumerate`, `auto info`, `auto perf/fan/kbd-backlight/battery-care/lid-logo/lights-always-on`). |
| `librazer`       | The shared library: HID protocol, device model, and the testable logic (perf/fan/battery encodings, profile selection, enforce-drift). |

## Features

- **Performance modes** — Balanced, Silent, Battery, Performance, Hyperboost, and Custom
  (per-axis CPU/GPU boost).
- **Fan control** — Auto, or Manual with an explicit RPM (2000–5000), plus max-fan mode.
- **Keyboard brightness** — 0–100% in 10% menu steps (mapped to the device's 0–255 scale);
  exact value shown in the tooltip.
- **Logo lighting** — off / static / breathing (a separate light zone from the keyboard).
- **Charge limit (battery care)** — 50–80% in 5% steps, or off (100%).
- **AC ↔ battery profiles** — separate saved profiles; the tray switches automatically when
  you plug/unplug.
- **Keyboard always-on** — holds the backlight on via a firmware flag, gated by a display-state
  monitor so it drops on screen-off and on sleep, and restores on wake. *(See quirks — this is
  mutually exclusive with the Fn brightness keys.)*
- **Enforce mode (opt-in, default off)** — re-asserts your perf/fan/logo/charge-limit settings
  if something else (e.g. Synapse) changes them; deliberately leaves keyboard brightness alone.
- **Close GPU apps** — terminates dGPU-using processes to drop to the iGPU, with a hard
  safelist so it never kills the compositor/shell.
- **Start with Windows** — toggles an `HKCU\…\Run` autostart entry.
- **Tray tooltip** — compact live status: mode · fan · logo · 🔆 brightness% · 🔋 charge-limit.

The tray writes a log to `%TEMP%\razer-tray.log` (size-capped, single rolling file) and stores
config at `%APPDATA%\razer-tray\config\default-config.toml`.

## Install / run

Drop `razer-tray.exe` somewhere (e.g. `C:\Program Files\RazerTray\`) and run it; use the tray
menu's **Start with Windows** to autostart. The CLI is standalone — run `razer-cli.exe --help`.
No installer, service, or Synapse needed.

## Changelog

### 0.9.0 — first versioned local release
First numbered release of the local fork (previously rode at upstream's `0.8.6`). Highlights
since the upstream baseline:

**Architecture / quality**
- Introduced a `HidTransport` trait so the device logic is testable on any host with a mock
  transport; moved the state model into `librazer`. Host test suite grew to **47 tests**.
- Split the ~1,500-line `razer-tray` `main.rs` into `main`/`menu`/`state`/`program`/`platform`.
- Sentence-case menu labels + tooltip rework (🔆 percent instead of raw 0–255; 🔋 charge limit).

**Behavior**
- **Mirror is input-gated** — device reads track real input and stop when idle, fixing the
  ~10 s backlight *pulsing* the old timed poll caused.
- **Fn-key brightness adoption** — an external brightness change is read back and folded into the
  active profile (no tug-of-war with the reconciliation loop).
- **Enforce mode** (opt-in) with resume-from-sleep re-assert.
- **Always-on backlight now survives sleep correctly** — the flag is dropped on `PBT_APMSUSPEND`
  (before the event loop freezes) and restored on resume, in addition to the existing
  screen-off display gate.

**Bug fixes**
- `battery-care` output went to a suppressed log level → now prints (and `auto info` shows it).
- **`Close GPU apps` / `taskkill` could kill the desktop** — `nvidia-smi` reports the compositor
  and shell (dwm/explorer/shell hosts) as GPU users. Added a shared, host-tested
  `process_guard` safelist used by both the tray and CLI, plus handling for `nvidia-smi`'s
  unreadable `[Insufficient Permissions]` placeholder (verified against the real `dwm`).
- Recovery loop no longer busy-spins on device loss (1 s backoff).
- `Close GPU apps` no longer panics when `nvidia-smi` isn't on `PATH`.
- Linux build fix for `read_device_model`.

**About screen**
- Version `0.9.0`; authors “sqmagellan, forked from blauzim and Tarek Dakhran”; website points at
  the upstream repo (this fork isn't published); keeps the log path line.

## Quirks & limitations (found during hardware testing)

- **⚠️ “Keyboard always-on” disables the Fn keyboard-brightness keys.** This is a *firmware-level
  mutual exclusion* on the RZ09-0483, not a software bug: with the always-on flag set, the
  embedded controller holds the lighting and **ignores the hardware Fn brightness keys**. The
  tray's brightness submenu and the CLI still work (they write the register directly). If you
  rely on the Fn brightness keys, leave **Keyboard always-on off** — the backlight then follows
  the firmware's native behavior (on while active, off when idle/asleep). The Fn keys work this
  way **without Synapse**.
- **Enforce mode is last-writer-wins.** It re-asserts on an input-gated poll and on resume, so it
  beats occasional external changes but will lose a tug-of-war with a tool that re-asserts every
  sub-second.
- **`Close GPU apps` is intentionally conservative.** It skips a safelist of session-critical
  processes and any process `nvidia-smi` can't name, so a few stubborn dGPU users may survive
  rather than risk the desktop.
- **Device-loss recovery couldn't be runtime-tested on this unit.** The control interface rides
  the internal keyboard's USB composite, which Windows marks a *critical system device* and
  refuses to disable; the recovery backoff is verified by code review only.
- **`battery-care get` can read stale right after a `set`.** The firmware register lags the write
  by ~1–2 s — re-read to confirm.
- **No Windows Dynamic Lighting.** The keyboard exposes no HID LampArray (usage page `0x59`), so
  the OS can't manage the backlight; the firmware-flag approach is the only option.
- **`tray-icon` is pinned at 0.11.3** (newer hover-event API unavailable in this build env), so
  the tooltip refresh uses a keyboard hook + last-input polling rather than hover events.

## Building / development

```bash
# full local gate: clippy (-D warnings) + Windows cross-build + host tests
.local-notes/check.sh        # must end "ALL GREEN"
```

House rules for this fork:
- **Never push** and never add a remote; the `local` branch moves between machines via
  `git bundle` only.
- **Don't edit `librazer/src/descriptor.rs`** (hand-maintained hardware descriptors).
- **Don't run `cargo fmt`** (it rewrites hand-formatting and touches `descriptor.rs`).

## Credits

Original project by **Tarek Dakhran** ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl));
multi-model fork by **blauzim** ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)).
This local fork is maintained by **sqmagellan**, developed with help from **McClaude (Claude Opus 4.8)**.
