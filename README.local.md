# razer-ctl (local fork) — Blade control without Synapse

**TL;DR:** a tray app + CLI that drive a Razer Blade's performance, fans, lighting, and battery care
straight over HID, so you don't need Synapse running. Local-only fork — never pushed. Validated on a
Razer Blade 16 (2023), `RZ09-0483`, USB PID `0x029F`, Windows 11.

> This fork lives on a local-only `local` branch that's never pushed; `main` tracks upstream. This
> README documents the local build; the upstream `README.md` covers the wider multi-model project.
> Upstream lists other Blade models, but only the 2023 16" has had hands-on testing here.

## What it is

`razer-tray.exe` is the daily driver — a tray icon with a menu and a live tooltip. `razer-cli.exe` does
the same things from the command line. `librazer` holds the HID protocol, the device model, and the
host-testable logic. Given the goal is "replace Synapse for the things I actually use," it stays small
and only writes to the device when you ask it to.

## What it controls

- **Performance modes** — Balanced, Silent, Battery, Performance, Hyperboost, and Custom (per-axis CPU/GPU boost).
- **Fan** — Auto, or Manual at an explicit RPM (2000–5000).
- **Keyboard brightness** — 0–100% in 10% steps; the exact value shows in the tooltip.
- **Logo lighting** — off / static / breathing (its own light zone).
- **Charge limit** — 50–80% in 5% steps, or off (100%).
- **AC vs battery profiles** — separate profiles, switched automatically when you plug/unplug.
- **Keyboard always-on** — keeps the backlight lit while the display's on. It's a Normal-mode keep-alive,
  not Razer's driver-mode flag, so your Fn media keys keep working (see Quirks — this one took a day to get right).
- **Enforce mode** (opt-in, off by default) — re-asserts perf/fan/logo/charge-limit if Synapse changes
  them; it leaves brightness alone.
- **Close GPU apps** — terminates dGPU-using processes, with a hard safelist so it never takes down the desktop.
- **Start with Windows** — an `HKCU\…\Run` entry.

Config lives at `%APPDATA%\razer-tray\config\default-config.toml`; the log at `%TEMP%\razer-tray.log`
(Info level, capped at 10 MiB, wiped on rollover).

## Install

Drop `razer-tray.exe` somewhere (e.g. `C:\Program Files\RazerTray\`), run it, and use the tray's
"Start with Windows" to autostart. The CLI is standalone — `razer-cli.exe --help`. No installer, no
service, no Synapse.

## Changelog

### 0.9.0 — first versioned local release
The first numbered cut of the fork; it rode at upstream's `0.8.6` until now. Given how much changed,
the highlights:

**Architecture**
- Put the device behind a `HidTransport` trait so the logic is host-testable with a mock, and moved the
  state model into `librazer`. 47 host tests.
- Split the ~1,500-line tray `main.rs` into `main` / `menu` / `state` / `program` / `platform`.
- Sentence-case menu labels; reworked tooltip.

**Behavior**
- Mirror reads are input-gated — they track real input and stop when idle, which killed the old ~10 s
  backlight pulsing.
- Fn-key brightness changes are read back and adopted into the active profile.
- Enforce mode (opt-in), with a resume-from-sleep re-assert.
- Keyboard always-on is now a Normal-mode keep-alive (see Quirks); it never enters driver mode, so the
  Fn keys keep working.
- Logging dropped from Trace to Info, bounded at 10 MiB.

**Fixes**
- `battery-care` output went to a suppressed log level — now it prints (and `auto info` shows it).
- "Close GPU apps" / `taskkill` could kill the desktop. Given nvidia-smi reports dwm/explorer/shell hosts
  as GPU users and the old code had no real filter, it SIGKILLed the session. Added a shared, host-tested
  `process_guard` safelist used by both the CLI and tray, plus handling for nvidia-smi's
  `[Insufficient Permissions]` placeholder.
- Recovery loop no longer busy-spins on device loss; "Close GPU apps" no longer panics when nvidia-smi
  isn't on `PATH`; Linux build fix.

## Quirks & limits (the hard-won ones)

- **Always-on used to kill every Fn media key — that's why it's a keep-alive now.** The old "always-on"
  was Razer's device-mode command (`0x0004`); Enable is `0x03` = driver mode, which hands key/light
  handling to a host driver and makes the EC ignore the whole Fn layer — screen brightness, volume, and
  keyboard brightness all go dead. OpenRazer confirms `0x0004` is device mode, and the upstream forks ship
  the same mislabel. There's no firmware "backlight timeout" knob and no HID LampArray, so the fix is a
  keep-alive: stay in Normal mode and re-brighten with a brightness *read* every ~3 s while the display's
  on (the EC fades at ~4 s). A read writes nothing, so it never fights the Fn keys. Validated on hardware —
  always-on lit steady, all Fn keys working.
- **Enforce mode is last-writer-wins.** It re-asserts on an input-gated poll and on resume, so it beats
  occasional changes but will lose to a tool that re-asserts every sub-second.
- **"Close GPU apps" is conservative on purpose** — it skips a safelist of session-critical processes and
  anything nvidia-smi can't name, so a few stubborn dGPU users may survive rather than risk the desktop.
- **Device-loss recovery isn't runtime-tested on this unit.** The control interface rides the internal
  keyboard's USB composite, which Windows won't let you disable, so the recovery backoff is code-reviewed only.
- **`battery-care get` can read stale right after a `set`** (~1–2 s firmware lag) — re-read to confirm.
- **`tray-icon` is pinned at 0.11.3** — the newer hover-event API isn't available in this build env, so
  the tooltip refresh uses a keyboard hook plus last-input polling.

## Building

```
.local-notes/check.sh    # clippy -D warnings + Windows cross-build + host tests; must end "ALL GREEN"
```

Rules for this fork:
- Don't push, and don't add a remote. The `local` branch moves between machines via `git bundle`.
- Don't edit `librazer/src/descriptor.rs` — those are hand-maintained hardware descriptors.
- Don't run `cargo fmt` — it rewrites the hand-formatting and touches `descriptor.rs`.

## Credits

Original by Tarek Dakhran ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)); multi-model fork
by blauzim ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)). This local fork is maintained by
sqmagellan, developed with McClaude (Claude Opus 4.8).
