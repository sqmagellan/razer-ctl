# razer-ctl — Razer Blade control without Synapse

[![razer-ctl-ci](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml/badge.svg)](https://github.com/sqmagellan/razer-ctl/actions/workflows/ci.yml)
[![latest release](https://img.shields.io/github/v/release/sqmagellan/razer-ctl)](https://github.com/sqmagellan/razer-ctl/releases/latest)

**[⬇ Download the latest release](https://github.com/sqmagellan/razer-ctl/releases/latest)** — two
standalone `.exe` files, Windows x86-64. No installer.

**TL;DR:** a tray app + CLI that drive a Razer Blade's performance modes, fans, lighting, and battery
care straight over HID, so you don't need Synapse running. No installer, no service, no background
account. The tray binary is 1.8 MB.

**Honest scope, up front:** this is a fork maintained by one person with **one** laptop. Everything
here is hardware-verified on a Razer Blade 16 (2023) — `RZ09-0483`, USB PID `0x029F`, Windows 11 — and
nothing else. It is built to *degrade gracefully* on other Blades rather than to claim support it
hasn't earned: an unrecognised model gets a generic profile instead of refusing to start, and any
command the firmware doesn't implement fails with one clean error. If you have a different Blade,
the [device support](#device-support) section says exactly what to expect and how to report what you
find.

`README.upstream.md` covers the wider multi-model project this forked from.

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
- **Charge limit** — any whole percent from 50 to 100 (100 = off). The tray offers presets; the CLI takes
  any value in range.
- **AC vs battery profiles** — separate profiles, switched automatically when you plug/unplug.
- **App profiles ("Actions")** — auto-apply settings while a named app runs, then fall back to the
  power-source profile when it quits. Rules can match several executables, carry a `priority` so
  overlapping rules resolve by intent instead of by file order, be `enabled = false` without being
  deleted, and be restricted to AC or battery with `require_ac` — which is what stops "force
  Hyperboost while the game runs" from doing that on a train. A rule overlays only the fields it
  sets (perf mode, fan, logo, keyboard effect, charge limit, max fan); the rest fall through to your
  saved profile. Opt-in, and a *transient* override, so it never overwrites the saved profiles.
- **Keyboard always-on** — keeps the backlight lit while the display's on. It's a Normal-mode keep-alive,
  not Razer's driver-mode flag, so your Fn media keys keep working (see Quirks — this one took a day to get right).
- **Enforce mode** (opt-in, off by default) — re-asserts perf/fan/logo/charge-limit if Synapse changes
  them; it leaves brightness alone.
- **Close GPU apps** — terminates dGPU-using processes, with a hard safelist so it never takes down the desktop.
- **Start with Windows** — an `HKCU\…\Run` entry.
- **Machine-readable status** — `razer-cli auto json` prints the whole device state, including the
  *actual* fan RPM, as flat JSON — ready for a Home Assistant command-line sensor or a shell status
  line. `razer-cli enumerate --json` gives the model/PID block a device-support report needs.
- **Exit codes that mean something**, so a script can branch without parsing stderr:

  | Code | Meaning |
  |---|---|
  | 0 | success — the change actually happened |
  | 1 | unclassified error |
  | 2 | usage error (emitted by the argument parser) |
  | 3 | no usable Razer laptop found — retrying won't help |
  | 4 | command not supported by this model — definitive, stop asking |
  | 5 | device communication error (busy / rejected / out of step) — retryable |

  All of these are verified on hardware, 4 by issuing a command this EC genuinely refuses. 2 is
  skipped deliberately: the argument parser exits 2 from inside its own code, before ours runs, so
  it can never be reliably ours to assign.

Config lives at `%APPDATA%\razer-tray\config\default-config.toml`; the log at `%TEMP%\razer-tray.log`
(Info level, capped at 10 MiB, wiped on rollover).

## Device support

**Tested:** Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`, Windows 11. One physical unit. Every
hardware claim in this README was verified there and nowhere else.

**Catalogued** — these ship a specific profile (feature set, init sequence, fan envelope) inherited
from the upstream project's tables. They are *not* tested here:

| Model | USB PID | Manual fan range |
|---|---|---|
| Razer Blade 16 (2023) — **tested** | `0x029F` | 2200–5000 RPM |
| Razer Blade 16 (2023) Black | `0x029F` | 2200–5000 RPM |
| Razer Blade 14 (2023) Mercury | `0x029D` | 2200–5000 RPM |
| Razer Blade 16 (2024) | `0x02B7` | 2200–5000 RPM |
| Razer Blade 16 (2025) RTX 5070 / 5080 / 5090 | `0x02C6` | 2200–5000 RPM |
| Razer Blade 15 (2022) | `0x028A` | 3500–5000 RPM |

**Everything else:** an unrecognised Razer laptop starts anyway on a generic profile — all features
offered, a fan envelope looked up from a table of 44 further PIDs (transcribed from community data,
*not* tested), and deliberately **no init sequence**, because inventing a plausible one is worse than
having none. A command the firmware doesn't implement answers `NotSupported` and fails immediately
with one clear error, so a missing control costs you an error message rather than a hang or a wrong
write.

So on an uncatalogued Blade, expect perf modes and fans to work, expect some lighting or battery
features possibly not to, and expect to be told which. If you try it, please
[open an issue](../../issues/new/choose) — the "unsupported model" template asks for the handful of
values (`razer-cli enumerate` output, SKU, what worked) needed to turn a guess into a real
descriptor.

### What is NOT here, on purpose

- **No arbitrary keyboard colour.** It requires Razer "driver mode", which disables the Fn media
  keys. Effects (Spectrum/Wave/Breathing/Off) work in Normal mode; see Quirks.
- **No CPU/GPU temperature-driven fan curve.** An honest CPU temperature on Windows needs a kernel
  driver; the usual one (WinRing0) carries CVE-2020-14979, a local privilege-escalation flaw, and
  has been quarantined by Defender since March 2025. Not worth it for a fan curve. Windows' own ACPI
  thermal zones were tested here and are frozen stubs — they held *exactly* 45.1 °C and 27.9 °C
  through a 25-second four-core load — so they are not a substitute. dGPU temperature via NVIDIA is
  real and is in the tooltip.
- **No Synapse-style cloud profiles, macros, or per-key lighting.**

## Install

Grab `razer-tray.exe` and `razer-cli.exe` from the
[latest release](https://github.com/sqmagellan/razer-ctl/releases/latest). Drop the tray binary
somewhere (e.g. `C:\Program Files\RazerTray\`), run it, and use the tray's "Start with Windows" to
autostart. The CLI is standalone — `razer-cli.exe --help`. No installer, no service, no Synapse.

Nothing needs administrator rights, and nothing is written outside your own user profile except
where you choose to put the binaries.

**The binaries are not code-signed**, so Windows SmartScreen will warn you the first time you run
one ("Windows protected your PC" → *More info* → *Run anyway*). Signing a hobby project costs real
money per year, which is the whole reason this is unsigned rather than an oversight. What you get
instead is a published SHA-256 for every file, so you can check that what you downloaded is what CI
built:

```powershell
Get-FileHash .\razer-tray.exe -Algorithm SHA256
```

Compare it against `SHA256SUMS.txt` in the same release. Every release binary is built by GitHub
Actions from a tagged commit — never uploaded from a personal machine — so the hash traces back to
public source.

Stronger than the hash, if you have the [GitHub CLI](https://cli.github.com/): each binary carries a
Sigstore-backed build-provenance attestation, so you can verify *where it came from* rather than
merely that it downloaded intact.

```
gh attestation verify razer-tray.exe --repo sqmagellan/razer-ctl
```

That confirms the exact repository, workflow, and commit that produced the file. A hash published
next to a download can't tell you that — and proves nothing at all if whoever tampered with the
binary could also edit the page hosting the hash.

## Changelog

### 2026-07 — charge limit, readable lighting, leaner binary
HW-verified on `0x029F` (2026-07-25).

- **Charge limit is any whole percent 50–100**, not 8 fixed presets. Probing the EC showed it accepts
  every integer in that range and refuses 49 and below, so the old
  50/55/…/80 enum was *our* restriction — it hid 43 usable values, including the entire 81–99 band.
  `battery-care set 88` now does what it says instead of silently rounding to 80.
  Existing configs (which stored `"Percent80"`) still load; that shim is load-bearing, because the
  tray loads config with `unwrap_or_default()` and a parse failure would silently discard your saved
  profiles.
- **Keyboard effect is read back from the device** (`0x0f82`), so the menu and `auto json` show what
  the firmware is actually running. This corrects a claim in this README: the getter does exist.
- **Tray clicks are no longer queued behind hover events.** The event loop took *one* event
  per ~1s tick, but `Move` fires at the mouse report rate while the cursor is over the icon and
  the channel is unbounded — so a click was processed only after every Move that preceded it.
  Approaching the icon enqueues a hundred-plus events, which pushed the mode switch out by
  minutes, and because the backlog outgrew the drain rate the lag *accumulated* across a
  session. Both channels are now drained each pass, with hover events coalesced into at most
  one refresh and a click superseding them. Throttling the hover work never helped: a skipped
  event still consumed its whole tick.
- **Left-click no longer opens the menu *and* changes the perf mode.** `tray-icon`'s
  `menu_on_left_click` defaults to true and must be disabled explicitly. Under 0.11.3 the
  Windows backend showed the menu only on `WM_RBUTTONUP` and ignored the flag; 0.19 honours it,
  so a single left-click did both — the menu opened on button-*down* while the perf-mode cycle
  ran on button-*up*, changing the mode behind the popup where you couldn't see it. Worse, each
  invisible cycle *persisted*, so the saved AC/battery profile quietly drifted. Left-click is
  ours (cycle the mode), right-click is the menu.
- **dGPU temperature and power in the tooltip**, sampled on a background thread so the UI
  never blocks on a subprocess, and failing open: no NVIDIA tools or no dGPU simply omits the
  fields rather than showing "0°C". The monitor tolerates ~a minute of failures before giving
  up, because the tray starts at login — exactly when the driver is least likely to answer — and
  quitting on the first sample turned a transient startup condition into an empty tooltip for
  the whole session.
- **Tray binary is 1.84 MB, down from 3.40 MB.** The six icons are decoded and downscaled to 64×64
  raw RGBA at build time, so the `image` crate — a full PNG/JPEG/GIF/WebP decoder, present for six
  fixed icons — is no longer linked into a binary we're about to sign. (`embed-resource` was also a
  build-dependency with no `build.rs` at all; removed.)
- **One fan-RPM ceiling instead of three.** `command.rs` bounded RPM at 5500, `FAN_RPM_MAX_ANY` said
  5300, and the device table reaches 5600 — so a CLI-supplied RPM was checked against a limit no real
  machine had. Both ends are now derived from the tables (2000–5600) and a test keeps them honest.
- **The tray's pure logic moved into `librazer`** (perf-mode cycle, nearest-brightness step) where the
  host test suite can actually reach it — the tray crate can't be built on a non-Windows host, so
  tests placed there would never have run.

### 2026-07 — portability & correctness pass (pre-publication)
Groundwork for making this usable on Blades other than the one it was written on. Everything below
was verified on `0x029F` where hardware could verify it; the parts that inherently cannot be
(other chassis) are marked.

- **Report checksum is now computed.** Every outgoing packet carries the real XOR-of-bytes-2..88
  checksum that OpenRazer and razer-laptop-control both send, instead of the hard-coded `0x00` we
  used to ship. HW-verified 2026-07-25: this Blade's EC accepts `crc = 0` *and* a correct CRC
  equally, so this fixes no bug you can see here — it removes a whole class of silent failure on a
  model whose firmware does validate the field.
- **An unrecognized Blade no longer refuses to start.** A `RZ09-` SKU missing from `SUPPORTED` gets
  a generic profile (all features offered, no init sequence guessed) and a loud warning naming the
  SKU, rather than a hard "not supported" exit whose only workaround was `manual --pid`. Unsupported
  commands answer `NotSupported` and fail fast, so a missing control degrades to one clean error.
- **Per-PID fan envelopes for 44 uncatalogued chassis.** Transcribed from the community device
  tables so the generic profile offers that machine's real fan range. *Transcribed, not tested* —
  this project owns one Blade. Catalogued models in `SUPPORTED` always win.
- **Resume detection now uses the OS power broadcast.** `PBT_APMRESUMESUSPEND` replaces the
  "a tick gap over 30 s means we slept" heuristic, which misfired on this machine: running at
  `IDLE_PRIORITY_CLASS` under EcoQoS, the loop was starved for **54.9 s while wide awake** and
  logged a resume that never happened, firing a spurious re-assert.
- **`DeviceState::read()` no longer duplicates its perf-mode query.** It was calling `get_perf_mode()`
  twice — once for the perf mode, once for the fan mode — and each call reads both fan zones, so 13
  round-trips became 11. (It is 12 again now that the keyboard-effect getter is read; see below.) A
  test pins the budget so it can't quietly regress.
- **`librazer` is usable as a library.** `HidTransport::send` takes a `Packet`, but `packet` was a
  private module, so no outside crate could implement the trait — the seam was unusable by exactly
  the consumers it exists for. `tests/public_api.rs` compiles as a separate crate and pins this.
- **CI actually gates.** It now runs the test suite (it never did) and builds `windows-msvc` — the
  configuration we ship; it was building `windows-gnu`. `cargo fmt --check` and clippy are both
  gated, and the tree is now rustfmt-clean. `Cargo.lock` is committed, as it should be for a repo
  that ships binaries.


### 2026-07 — keyboard lighting (effects-only)
All HW-verified on a **single physical unit — a Razer Blade 16 (2023), `RZ09-0483U`, PID `0x029F`**
(no other model was tested for lighting).
- **Keyboard RGB effects** — Off / Spectrum / Wave / Breathing, in the tray ("Keyboard lighting"
  submenu) and CLI (`razer-cli auto kbd-lighting effect <off|spectrum|wave|breathing>`). These are the
  EC's built-in animated effects (extended-matrix command `0x0f02`, VARSTORE, our native `0x1F`
  transaction), so they run in Normal mode and the Fn media keys keep working. Readable back via
  `0x0f82`, so the menu and `auto json` reflect what the firmware is actually running.
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
- **Keyboard effect *can* be read back** — this README previously said it couldn't. `0x0f82` is a working
  get-mirror of the `0x0f02` effect write on `0x029F`: it returns the effect id last applied (Spectrum→3,
  Wave→4 plus direction, Breathing→2, Off→0), verified across separate processes so it's a real EC read
  rather than a cache. The effect is therefore in `read()` and `auto json`. It is still deliberately
  *not* enforced: it's cosmetic, and on a model whose firmware lacks the getter an unknown read would
  look like permanent drift and re-assert forever. An effect id we don't model (a Synapse-set Static or
  Reactive) reads as unknown rather than as an error.
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
- **A config the app can't parse is preserved, not eaten.** It used to be loaded with
  `unwrap_or_default()`, so an unreadable file became silent defaults — and the tray then persisted
  those defaults over it, losing every saved profile without a word. Found the hard way: a
  hand-edited file with a duplicate `app_profiles` key reset the AC profile and wrote it back within
  seconds. Now it logs the parse error and keeps a copy at `default-config.toml.invalid` (the first
  bad file, so a restart can't overwrite the evidence).
- **`battery-care get` can read stale right after a `set`** (~1–2 s firmware lag) — re-read to confirm.
- **The tray tooltip really holds 63 characters, not 128.** `NOTIFYICONDATAW::szTip` is
  declared `[u16; 128]` and the modern shell honours all 128 — but only when `cbSize` names a
  struct version that has the long field. `tray-icon` 0.19 builds the struct with
  `..std::mem::zeroed()`, leaving `cbSize` at **0**, which matches no declared version; the shell
  accepts the call *without error* and then behaves like the original layout, where `szTip` was
  `[u16; 64]`. Measured on `0x029F`: an 83-unit tooltip was accepted, yet the display cut
  mid-way through the 🔋 surrogate pair, and the cut point moved with the length of the perf-mode
  name. So the tooltip is *budgeted*, not appended-to: fields carry a priority and the least
  important ones (logo mode first, then always-on, then GPU watts) drop out when a long
  `Custom (CPU …, GPU …)` label crowds them. Appending and then truncating is what hid the GPU
  fields entirely — truncation always eats the newest field.
- **Tooltip refresh is hover-driven (tray-icon 0.19).** The tooltip/icon freshen when you actually hover
  the tray — which means you're at the machine, so the backlight's already awake and the read can't cause
  a visible pulse (Move is throttled to 500 ms). That let me delete the entire global `WH_KEYBOARD_LL`
  low-level keyboard hook — the most invasive Windows code in the crate, there only to freshen the tooltip
  on keypress. The input-gated Mirror poll *stays*: it's the load-bearing anti-pulsing gate that also
  drives enforce-drift and Fn-brightness adopt.

## Building

Stable Rust, no nightly features. Natively on Windows you need the VS 2022 Build Tools and the
MSVC toolchain; from macOS or Linux, [`cargo xwin`](https://github.com/rust-cross/cargo-xwin)
cross-builds the shipped target offline.

```
cargo test -p librazer                                 # host-testable logic; runs anywhere
cargo clippy --all-targets -- -D warnings
cargo build --release --target x86_64-pc-windows-msvc   # or: cargo xwin build --release --target ...
```

`-p librazer` is not laziness: `librazer::device` is gated to Windows and Linux (it needs `hidapi`),
so a bare `cargo test` fails to compile `razer-cli` on macOS. On Windows or Linux the whole workspace
tests, and `cargo test -p razer-tray` additionally covers the tray-only logic.

CI runs the tests, clippy, `cargo fmt --check`, and builds both `x86_64-pc-windows-msvc` (what we
ship) and Linux.

Conventions worth knowing before you send a patch:
- **`librazer` is the testable core.** It holds the HID protocol, the device model, and every pure
  helper, and it builds and tests on any host. The tray crate can only be built for Windows, so
  logic placed there is logic the test suite can never reach — if a function is pure, it belongs in
  `librazer`.
- **`librazer/src/descriptor.rs` is hand-maintained hardware data.** Every entry must declare its
  real `fan_rpm_range`, so adding a model means supplying that model's measured fan envelope rather
  than a guess.
- **The tree is `rustfmt`-clean and CI gates on it.**
- **Don't guess at hardware.** A wrong RPM ceiling or init sequence is worse than an absent one: the
  generic profile deliberately ships *no* init commands rather than inventing a plausible sequence.

## Credits

Original by Tarek Dakhran ([tdakhran/razer-ctl](https://github.com/tdakhran/razer-ctl)); multi-model
fork by blauzim ([blauzim/razer-ctl](https://github.com/blauzim/razer-ctl)). Both MIT, and this fork
stays MIT. This fork is maintained by sqmagellan, developed with Claude Opus 5.

Protocol knowledge came from reading [OpenRazer](https://github.com/openrazer/openrazer) and
[razer-laptop-control](https://github.com/Razer-Linux/razer-laptop-control-no-dkms) — both GPL — for
the *facts* they document (command IDs, the checksum algorithm, that `0x0004` is device mode and not
a backlight flag). No code was copied from either; the implementations here are written from those
facts. Facts about hardware aren't copyrightable; expression is, and none was taken.
