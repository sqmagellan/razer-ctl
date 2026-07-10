# razer-ctl (local fork) — Blade control without Synapse

**TL;DR:** a tray app + CLI that drive a Razer Blade's performance, fans, lighting, and battery care
straight over HID, so you don't need Synapse running. Local-only fork — never pushed. Validated on a
Razer Blade 16 (2023), `RZ09-0483`, USB PID `0x029F`, Windows 11.

> This fork lives on a local-only `local` branch that's never pushed; `main` tracks upstream. This
> README documents the local build; `README.upstream.md` covers the wider multi-model project.
> Upstream lists other Blade models, but only the 2023 16" has had hands-on testing here.

## What it is

`razer-tray.exe` is the daily driver — a tray icon with a menu and a live tooltip. `razer-cli.exe` does
the same things from the command line. `librazer` holds the HID protocol, the device model, and the
host-testable logic. Given the goal is "replace Synapse for the things I actually use," it stays small
and only writes to the device when you ask it to.

## What it controls

- **Performance modes** — Balanced, Silent, Battery, Performance, Hyperboost, and Custom (per-axis CPU/GPU boost).
- **Fan** — Auto, or Manual at a real RPM. The range is per-chassis (declared on each descriptor), so the
  menu only offers speeds the EC actually honors — 2200–5000 on the 2023 Blade 16, the ends labelled
  (min)/(max) — instead of dead 0/500 steps and an out-of-range 5500.
- **Keyboard brightness** — 0–100% in 10% steps; the exact value shows in the tooltip.
- **Logo lighting** — off / static / breathing (its own light zone).
- **Keyboard lighting** — Off / Spectrum / Wave / Breathing: the EC's built-in animated effects, over
  the same HID path in Normal mode, so the Fn keys keep working. No color picker by design — arbitrary
  color needs Razer driver mode (what Synapse does), which kills the Fn keys (see Quirks).
- **Charge limit** — 50–80% in 5% steps, or off (100%).
- **AC vs battery profiles** — separate profiles, switched automatically when you plug/unplug.
- **App profiles** — auto-apply a perf mode while a named app is running, then fall back to the
  power-source profile when it quits (first running match wins, case-insensitive). Opt-in, and a
  *transient* override, so it never overwrites your saved AC/battery profiles.
- **Keyboard always-on** — keeps the backlight lit while the display's on. It's a Normal-mode keep-alive,
  not Razer's driver-mode flag, so your Fn media keys keep working (see Quirks — this one took a day to get right).
- **Enforce mode** (opt-in, off by default) — re-asserts perf/fan/logo/charge-limit if Synapse changes
  them; it leaves brightness alone.
- **Close GPU apps** — terminates dGPU-using processes, with a hard safelist so it never takes down the desktop.
- **Start with Windows** — an `HKCU\…\Run` entry.
- **Machine-readable status** — `razer-cli auto json` prints the whole device state, including the
  *actual* fan RPM, as flat JSON — ready for a Home Assistant command-line sensor or a shell status line.

Config lives at `%APPDATA%\razer-tray\config\default-config.toml`; the log at `%TEMP%\razer-tray.log`
(Info level, capped at 10 MiB, wiped on rollover).

## Install

Drop `razer-tray.exe` somewhere (e.g. `C:\Program Files\RazerTray\`), run it, and use the tray's
"Start with Windows" to autostart. The CLI is standalone — `razer-cli.exe --help`. No installer, no
service, no Synapse.

## Changelog

### 2026-07 — keyboard lighting (effects-only)
All HW-verified on a **single physical unit — a Razer Blade 16 (2023), `RZ09-0483U`, PID `0x029F`**
(no other model was tested for lighting).
- **Keyboard RGB effects** — Off / Spectrum / Wave / Breathing, in the tray ("Keyboard lighting"
  submenu) and CLI (`razer-cli auto kbd-lighting effect <off|spectrum|wave|breathing>`). These are the
  EC's built-in animated effects (extended-matrix command `0x0f02`, VARSTORE, our native `0x1F`
  transaction), so they run in Normal mode and the Fn media keys keep working. Write-only *intent*
  (no getter on this device) — persisted and re-applied like always-on, never read back or reconciled.
- **No keyboard color, by design.** Arbitrary static/per-key color needs Razer *driver mode*
  (host-streamed frames — what Synapse does), which disables the Fn media keys. In Normal mode the EC
  ignores color payloads (falls back to Razer green) and effect-speed parameters (Wave runs at a fixed
  rate — no slow/fast). Verified across *both* the extended (`0x0f02`) and standard (`0x030a`) matrix
  families at transactions `0x1F` and `0xFF`, plus the per-key custom-frame path. See Quirks.
- **`razer-cli … cmd --tx <hex>`** — a transaction-id override on the raw `cmd` path, for protocol
  debugging (the reference drivers issue some commands at `0xFF` rather than our default `0x1F`).

### 2026-07 — control, status & self-healing pass
Landed on top of `0.9.0` (tray stayed `0.9.0`, CLI `0.8.6`); all HW-verified on `0x029F`.

**New control & status**
- **`razer-cli auto json`** — full device state as one flat JSON object (perf mode, CPU/GPU boost,
  fan mode + setpoint + *actual* RPM, keyboard %, logo, charge limit), read in a single pass. Flat on
  purpose — no Rust enum-tuple encoding — so Home Assistant or a status line can parse it blind.
- **App profiles** — apply a perf mode while a named app runs, reverting to the AC/battery profile on
  exit. A transient override that never clobbers a saved profile; empty by default.
- **Resume re-assert without full enforce** — the intended mode is re-applied on wake by default
  (`reassert_on_resume`), not only when enforce is on. Wake used to keep whatever the EC reset itself to.
- **Labelled Custom boosts** — the Custom submenu now labels its two groups ("CPU boost" / "GPU boost")
  instead of two unlabelled Low/Med/High stacks.

**Self-healing / correctness**
- **Startup reconcile** — on launch the tray reads the device back after its startup apply and
  re-asserts once if reality doesn't match intent. A just-booted (especially crash-rebooted) EC will ACK
  a perf-mode write without actually switching and keeps its last mode across a reboot — so the tray
  could show "Balanced" while the EC sat in the battery profile. This runs regardless of the enforce
  flag (startup correctness isn't optional) and retired the external `RazerPerfSilent` scheduled-task hack.
- **Honest, per-chassis fan range** — the manual menu used to list 0/500/1000/1500/2000 (all at or below
  the EC's floor, so no effect) and 5500 (above the ceiling). It now offers only speeds the hardware
  honors, and the envelope is *per model* (a required `fan_rpm_range` on every descriptor), so the app
  stays universal. Probed on hardware: setpoint 2000 still idles ~2400, and it won't exceed 5000.

**Housekeeping**
- **tray-icon 0.11.3 → 0.19** — enabled hover-driven tooltip refresh and let me delete the global
  keyboard hook (see Quirks).
- **Security** — dropped the unmaintained `failure` crate (RUSTSEC) via single-instance 0.3, and picked
  up patched `crossbeam-epoch` + `anyhow`. Audit-clean apart from Linux-only GTK transitives that aren't
  in the Windows binary.

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

- **No arbitrary keyboard color — it needs driver mode, which we refuse.** You can pick a keyboard
  *effect* (Spectrum/Wave/Breathing/Off) but not a chosen color. On this hardware a static/per-key color
  is host-driven: the EC only self-animates its built-in effects in Normal mode and *ignores* any color
  bytes we send (it falls back to Razer brand green — its unparsed-command default). Getting real color
  means Razer "driver mode" (the host streams frames every tick, which is what Synapse does), and driver
  mode disables the Fn media keys — the same trap as always-on. HW-verified the color is dropped across
  both matrix command families (`0x0f02` extended, `0x030a` standard) at both transaction ids (`0x1F`,
  `0xFF`) and via the custom-frame path, so this is a hardware/design limit, not a missing feature — as
  observed on the one unit tested (`RZ09-0483U`, `0x029F`). For the same reason effect *speed* isn't
  adjustable (Wave runs at a fixed EC rate).
- **Keyboard effect can't be read back.** Chroma is write-only on this device — no getter — so the tray
  menu reflects the effect *you last set through the app* (persisted intent), not whatever the firmware
  is actually running. Set an effect outside the app (Synapse, the raw CLI) and the menu won't show it
  checked; pick it in-app once and it persists + re-applies on launch.
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
- **Tooltip refresh is hover-driven (tray-icon 0.19).** The tooltip/icon freshen when you actually hover
  the tray — which means you're at the machine, so the backlight's already awake and the read can't cause
  a visible pulse (Move is throttled to 500 ms). That let me delete the entire global `WH_KEYBOARD_LL`
  low-level keyboard hook — the most invasive Windows code in the crate, there only to freshen the tooltip
  on keypress. The input-gated Mirror poll *stays*: it's the load-bearing anti-pulsing gate that also
  drives enforce-drift and Fn-brightness adopt.

## Building

Two ways to build now: `cargo xwin` cross-build from the iMac (offline), or natively on the Blade itself
(VS 2022 Build Tools + the stable-MSVC toolchain). Either way, the gate is the same:

```
.local-notes/check.sh    # clippy -D warnings + Windows cross-build + host tests; must end "ALL GREEN"
```

Rules for this fork:
- Don't push, and don't add a remote. The `local` branch moves between machines via `git bundle`.
- `librazer/src/descriptor.rs` holds hand-maintained hardware descriptors — edit deliberately. Every
  entry now has to declare its `fan_rpm_range`, so adding a model means giving its real fan envelope.
- Don't run `cargo fmt` — it rewrites the hand-formatting and touches `descriptor.rs`.

## Credits

Original by Tarek Dakhran ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)); multi-model fork
by blauzim ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)). This local fork is maintained by
sqmagellan, developed with McClaude (Claude Opus 4.8).
