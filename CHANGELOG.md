# Changelog

Release history for this fork. The README carries the current behavior; this file carries how it
got there. Every hardware claim was verified on a Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`,
Windows 11, and nowhere else.

## 0.9.5: keyboard colors, a clearer menu, and a tray that can't die at login or lose its config
Built and tested on `0x029F` (2026-09-23). Probes that set the scope, same machine: `0x070f` is Max
Fan (fans 2100 → 4700 RPM, no charging above the limit); tray and CLI running together don't cause
bad fan reads (0/60 either way); the ACPI "High Precision Temperature" counter is frozen at 45.05 °C
under load like the others; the machine sleeps in Modern Standby (69 standby cycles in 17 days
against 5 resumes); the EC kept its state across two measured standbys.

**New**
- **Custom keyboard colors.** Presets from the config (one color, or one per row), "Follow
  performance mode" (the tray icon's color), and a battery bar on keys 1–0 (lit keys = charge in
  tens, the rest at 20%, cyan while charging). The 0.9.0-era note that color needs driver mode was
  wrong: six `0x030b` row writes and `0x030a [5, 0]` show a frame in Normal mode with the Fn keys
  intact. The EC doesn't keep the frame, so the tray repaints it after a wake, after every apply,
  and on the first input after the idle fade. Mapped for `0x029F` only. `razer-cli auto
  kbd-lighting color` previews it, with `--perf`, `--battery` and `--charging`.
- **Screen refresh rate per profile**, **Match Windows power to Razer performance mode** (opt-in,
  and turning it off restores the earlier Windows mode for each power source), an opt-in
  **perf-cycle hotkey**, and a warning at the top of the menu when Synapse is running.
- **Menu.** A line at the top says whether changes go to the plugged-in or the battery profile.
  Keyboard lighting holds effects, colors, brightness, Battery bar and Keep keyboard lit. Enforce is
  now "Undo changes made by Synapse" and "Close GPU apps" is "Close apps using the GPU".
- Fn-lock (`0x0206`) was tried and dropped: it round-trips on `0x029F` but changes nothing.

**Reliability**
- **Startup and recovery.** One HID error during startup used to exit the tray with nothing in the
  log. The tray now comes up from the config and resyncs with backoff (2 s doubling to 60 s),
  reopening the device after three failures, without blocking the event loop or reloading the
  config. A resync aims at the current target, so a plug or unplug while it waits is honored. Only
  bus trouble triggers a resync; a write the EC rejects is logged, shown as what the device did, and
  no longer stops the independent writes after it.
- **The config file.** Saved atomically (temp file, then rename). Missing, unreadable and
  unparseable files are handled separately; an unparseable one gets a timestamped copy. Hand edits
  are adopted within 5 s and before any save, and one that doesn't parse is never written over. A
  file that can't be read at startup is waited for, and meanwhile the tray only reads the device.
  The file is created on first run again.
- **Modern Standby.** A wake is the resume message, or the display coming back after at least 60 s
  off. It schedules reads at +3 s and +15 s and writes only if a read shows drift. The +15 s check
  follows G-Helper #5682, where one re-apply at the moment of resume lost to the firmware. A
  display-on wake writes only with "Undo changes made by Synapse" on, so a screen timeout never reverts a CLI change.
- **Actions.** A pick made while an Action runs holds until its process exits (it was reverted a
  second later). An AC/battery switch carries picks of the Action's fields across, re-checks the
  rules at once, and no longer applies an AC-only rule on battery. The left-click cycle during an
  Action isn't saved into the profile.
- **Fans.** Max fan exists only in Custom, and a manual RPM snaps to the chassis range and the
  100-RPM step; both used to look like permanent drift. A rejected manual RPM no longer switches the
  fan to Manual, and fan write failures are returned. The tooltip never shows an ignored set point
  as the speed, and shows "…" until a read is trusted. `razer-cli fan info` uses the same trust rule,
  and `auto json` adds `fan_actual_trusted`.
- **Power source.** An "unknown" `ACLineStatus` keeps the previous answer instead of meaning AC.
- **dGPU telemetry doesn't wake a sleeping GPU.** Before each `nvidia-smi` sample the tray reads
  the GPU's power state from Windows, which doesn't wake it, and skips the sample while the GPU is
  in D3. Only matters with Optimus; untested on a sleeping GPU because the Blade here runs in
  dGPU-only mode.
- **Transport.** HID I/O errors are retried with backoff and exit 5, as documented (they aborted on
  the first attempt with exit 1). Raw `cmd` probes are sent once, never re-sent. Perf zones left
  disagreeing by an interrupted write are a typed error, and the tray repairs them.
- **Log** moved to `%LOCALAPPDATA%\razer-tray\`, 1 MiB, rotated through three older files. Logging
  starts before anything else.
- **Supply chain.** bincode is gone: the 90-byte packet is encoded by hand, checked byte-identical
  first. Every Action is pinned to a commit SHA, tokens are read-only except for the release job,
  cargo runs `--locked`, and the documented `gh attestation verify` pins the tag and workflow. The
  CLI has tests.
- American spelling throughout.
- Versions: `razer-tray` 0.9.5, `librazer` 0.9.0 (breaking: `Packet` is no longer serde),
  `razer-cli` 0.8.9.

## 0.9.3: resume detection that fires, and Actions kept out of saved profiles
HW-verified on `0x029F` (2026-09-05).

- **The tray never noticed a resume, and its own log said so for six weeks.** `PBT_APMRESUMESUSPEND`
  replaced the old tick-gap heuristic on 2026-07-25, and every `resume detected` line written since
  is still the heuristic's: nine Kernel-Power 107 resumes between 07-30 and 09-04 produced none.
  The cause is the window. `spawn_display_state_monitor` creates a message-only window and registers
  only `RegisterPowerSettingNotification` for `GUID_CONSOLE_DISPLAY_STATE`; `PBT_APMRESUME*` arrives
  as a broadcast, a message-only window receives no broadcasts, and a power-setting registration
  delivers that setting and nothing else. It now also calls `RegisterSuspendResumeNotification`,
  which is targeted and so reaches the window. The two registrations are independent (losing one
  does not cost the other), and startup logs which of them took. Verified on a hibernate/resume at
  19:18:17: both broadcasts arrived, the latch collapsed them into one re-assert, and that is the
  first `resume detected (OS power broadcast)` line in the log's history.
- **An Action's settings could end up in the saved AC or battery profile.** `menu::build` bakes a
  whole `DeviceState` into every handler, built from whatever is currently effective, which during an
  Action is the overlay. Picking one unrelated item handed `update()` a state carrying the
  Action's perf mode and fan, and `update()` assigned the lot to `ac_state` or `battery_state`.
  Close the game and the profile you returned to had quietly acquired the Action's settings. The
  README's claim that an Action "never overwrites a saved profile" was the intent, not the
  behavior. `DeviceState::carry_changes` now writes only the fields that differ between the state
  the menu was built from and the state the user picked. With no Action active `base == before`, so
  the common path is bit-identical to the old wholesale assignment, and that equivalence is one of
  the three new tests. Only the saved profile is narrowed: whether a manual edit should override a
  running Action *on the device*, or stay masked until it ends, is a separate question and is
  untouched.
- **`librazer` 0.8.7 → 0.8.8** for the new `DeviceState::carry_changes`. The addition is additive and
  changes no existing call, so `razer-cli` stays at 0.8.7. Verified on the
  Blade: `fmt --check`, clippy `--target x86_64-pc-windows-msvc -Dwarnings`, `cargo build --release`,
  and 122 tests.

## 0.9.2: measured fan floor, filtered fan reads, an audit gate
HW-verified on `0x029F` (2026-08-15).

- **The manual fan floor is 2000 RPM, not the 2200 the descriptor claimed.** Writing raw `0x0d01`
  past the CLI's own clamp, set points of 1800 / 1500 / 1200 / 1000 / 800 / 400 / 100 / 0 all settle
  at exactly 2000 on both zones, held 40 s each across 0–17% CPU load. 2000 and 3000 are honored
  exactly. It's a floor, not the EC substituting its own thermal demand: Auto idle is asymmetric
  (2000/1900) while the manual floor is always symmetric (2000/2000). Only the two `RZ09-0483` rows
  were lowered. Every other row in both tables is transcribed from a third-party `laptops.json`, and
  guessing a second chassis from a measurement of the first is how the wrong 2200 arrived in the
  first place. Those rows are now marked suspect rather than silently trusted.
- **Sub-floor fan writes are rejected instead of silently accepted.** `set_fan_rpm` had no lower
  bound, so `set_fan_rpm(dev, 400)` returned `Ok(())` while the fan kept spinning at 2000. The EC
  banks a sub-floor value in its read-back register and ignores it, so every caller above then
  reported a speed the hardware wasn't honoring. It's bounded at both ends now, with an error that
  explains why the loud case beats the silent one.
- **Individual `0x0d88` fan reads aren't trustworthy, so they're filtered.** Roughly 3 in 10 sparse
  samples carried one impossible zone (0/1800, then 2000/0, then 0/1800 at 30 s spacing), which
  can't happen at the measured 23–53 RPM/s slew. It goes both directions, and one sample carried a
  lone 2000 while the fans were genuinely stopped. `FanRpmFilter` drops lone-zone reads and makes a
  stop earn two consecutive confirmations, while running values publish immediately, because during
  a ramp the number legitimately moves every poll. Suspicion is kept narrow on purpose: a
  large-but-nonzero gap isn't flagged, since 100–200 RPM of Auto asymmetry is normal here and no
  threshold above that has been measured. Tach output is quantized to 100 RPM.
- **Zero RPM is real, but Auto-only.** 14 of 15 consecutive reads at 1.5 s spacing returned 0/0 over
  22 s. Manual can't stop the fans, and a set point of 0 doesn't hand back to the firmware curve on
  this chassis: the mode stays Manual and the fan holds 2000. So razer-control-revived's "0 clears
  the manual flag" behavior must not be ported here.
- **`cargo audit` runs weekly as its own workflow.** It's separate from `ci.yml` on purpose. Every
  other gate answers "did this change break something"; audit answers "did the world change under
  code nobody touched", which only surfaces on a timer, and a `schedule:` belongs to the workflow
  rather than re-running four build/test jobs weekly for nothing. Both dependency bumps that ever
  paid for themselves here were advisory-driven or capability-driven, never calendar-driven:
  single-instance 0.1.2 → 0.3 dropped the unmaintained `failure` dep (RUSTSEC-2019-0036 /
  -2020-0036, CVSS 9.8), and tray-icon 0.11.3 → 0.19 deleted a keyboard-hook hack outright. Version
  lag by itself isn't a defect. Deliberately not `--deny warnings`: the first run reported 13
  warnings and zero vulnerabilities across 371 dependencies, and ten of the thirteen are the
  Linux-only GTK stack that exists solely so `cargo build` works on a Linux runner. A gate that
  cries wolf gets switched off, and `cargo-audit` already fails on vulnerabilities by default.
- **clap 4.6.1 → 4.6.6** (lockfile only). The one bump that was actually outstanding; three of the
  four crates checked were already at latest. `Cargo.toml` lists a caret floor, not what gets built,
  so reading manifests instead of the lockfile overstated the work by three crates. The resolver
  also unified duplicate `windows-*` crates it was already carrying, a net -5 crates, and nothing
  `razer-tray` declares changed. Verified on the Blade: librazer tests, clippy
  `--target x86_64-pc-windows-msvc -Dwarnings`, `fmt --check`, `cargo test -p razer-tray`,
  `cargo build --release`, and `cargo audit`, all clean.


## 2026-07: charge limit, readable lighting, leaner binary
HW-verified on `0x029F` (2026-07-25).

- **Charge limit is any whole percent 50–100**, not 8 fixed presets. Probing the EC showed it accepts
  every integer in that range and refuses 49 and below, so the old
  50/55/…/80 enum was *our* restriction. It hid 43 usable values, including the entire 81–99 band.
  `battery-care set 88` now does what it says instead of silently rounding to 80.
  Existing configs (which stored `"Percent80"`) still load; that shim is load-bearing, because the
  tray loads config with `unwrap_or_default()` and a parse failure would silently discard your saved
  profiles.
- **Keyboard effect is read back from the device** (`0x0f82`), so the menu and `auto json` show what
  the firmware is actually running. This corrects a claim in this README: the getter does exist.
- **Tray clicks are no longer queued behind hover events.** The event loop took *one* event
  per ~1s tick, but `Move` fires at the mouse report rate while the cursor is over the icon and
  the channel is unbounded, so a click was processed only after every Move that preceded it.
  Approaching the icon enqueues a hundred-plus events, which pushed the mode switch out by
  minutes, and because the backlog outgrew the drain rate the lag *accumulated* across a
  session. Both channels are now drained each pass, with hover events coalesced into at most
  one refresh and a click superseding them. Throttling the hover work never helped: a skipped
  event still consumed its whole tick.
- **Left-click no longer opens the menu *and* changes the perf mode.** `tray-icon`'s
  `menu_on_left_click` defaults to true and must be disabled explicitly. Under 0.11.3 the
  Windows backend showed the menu only on `WM_RBUTTONUP` and ignored the flag; 0.19 honors it,
  so a single left-click did both: the menu opened on button-*down* while the perf-mode cycle
  ran on button-*up*, changing the mode behind the popup where you couldn't see it. Worse, each
  invisible cycle *persisted*, so the saved AC/battery profile quietly drifted. Left-click is
  ours (cycle the mode), right-click is the menu.
- **dGPU temperature and power in the tooltip**, sampled on a background thread so the UI
  never blocks on a subprocess, and failing open: no NVIDIA tools or no dGPU simply omits the
  fields rather than showing "0°C". The monitor tolerates ~a minute of failures before giving
  up, because the tray starts at login, exactly when the driver is least likely to answer, and
  quitting on the first sample turned a transient startup condition into an empty tooltip for
  the whole session.
- **Tray binary is 1.84 MB, down from 3.40 MB.** The six icons are decoded and downscaled to 64×64
  raw RGBA at build time, so the `image` crate (a full PNG/JPEG/GIF/WebP decoder, present for six
  fixed icons) is no longer linked into a binary we're about to sign. (`embed-resource` was also a
  build-dependency with no `build.rs` at all; removed.)
- **One fan-RPM ceiling instead of three.** `command.rs` bounded RPM at 5500, `FAN_RPM_MAX_ANY` said
  5300, and the device table reaches 5600, so a CLI-supplied RPM was checked against a limit no real
  machine had. Both ends are now derived from the tables (2000–5600) and a test keeps them honest.
- **The tray's pure logic moved into `librazer`** (perf-mode cycle, nearest-brightness step) where the
  host test suite can actually reach it; the tray crate can't be built on a non-Windows host, so
  tests placed there would never have run.

## 2026-07: portability and correctness pass (pre-publication)
Groundwork for making this usable on Blades other than the one it was written on. Everything below
was verified on `0x029F` where hardware could verify it; the parts that inherently cannot be
(other chassis) are marked.

- **Report checksum is now computed.** Every outgoing packet carries the real XOR-of-bytes-2..88
  checksum that OpenRazer and razer-laptop-control both send, instead of the hard-coded `0x00` we
  used to ship. HW-verified 2026-07-25: this Blade's EC accepts `crc = 0` *and* a correct CRC
  equally, so this fixes no bug you can see here. It removes a whole class of silent failure on a
  model whose firmware does validate the field.
- **An unrecognized Blade no longer refuses to start.** A `RZ09-` SKU missing from `SUPPORTED` gets
  a generic profile (all features offered, no init sequence guessed) and a loud warning naming the
  SKU, rather than a hard "not supported" exit whose only workaround was `manual --pid`. Unsupported
  commands answer `NotSupported` and fail fast, so a missing control degrades to one clean error.
- **Per-PID fan envelopes for 44 uncatalogued chassis.** Transcribed from the community device
  tables so the generic profile offers that machine's real fan range. *Transcribed, not tested*:
  this project owns one Blade. Catalogued models in `SUPPORTED` always win.
- **Resume detection now uses the OS power broadcast.** `PBT_APMRESUMESUSPEND` replaces the
  "a tick gap over 30 s means we slept" heuristic, which misfired on this machine: running at
  `IDLE_PRIORITY_CLASS` under EcoQoS, the loop was starved for **54.9 s while wide awake** and
  logged a resume that never happened, firing a spurious re-assert.
- **`DeviceState::read()` no longer duplicates its perf-mode query.** It was calling `get_perf_mode()`
  twice (once for the perf mode, once for the fan mode), and each call reads both fan zones, so 13
  round-trips became 11. (It is 12 again now that the keyboard-effect getter is read; see below.) A
  test pins the budget so it can't quietly regress.
- **`librazer` is usable as a library.** `HidTransport::send` takes a `Packet`, but `packet` was a
  private module, so no outside crate could implement the trait. The seam was unusable by exactly
  the consumers it exists for. `tests/public_api.rs` compiles as a separate crate and pins this.
- **CI actually gates.** It now runs the test suite (it never did) and builds `windows-msvc`, the
  configuration we ship; it was building `windows-gnu`. `cargo fmt --check` and clippy are both
  gated, and the tree is now rustfmt-clean. `Cargo.lock` is committed, as it should be for a repo
  that ships binaries.


## 2026-07: keyboard lighting (effects-only)
All HW-verified on a **single physical unit, a Razer Blade 16 (2023), `RZ09-0483U`, PID `0x029F`**
(no other model was tested for lighting).
- **Keyboard RGB effects**: Off / Spectrum / Wave / Breathing, in the tray ("Keyboard lighting"
  submenu) and CLI (`razer-cli auto kbd-lighting effect <off|spectrum|wave|breathing>`). These are the
  EC's built-in animated effects (extended-matrix command `0x0f02`, VARSTORE, our native `0x1F`
  transaction), so they run in Normal mode and the Fn media keys keep working. Readable back via
  `0x0f82`, so the menu and `auto json` reflect what the firmware is actually running.
- **No keyboard color, by design.** (Wrong; see 0.9.5.) Arbitrary static/per-key color needs Razer *driver mode*
  (host-streamed frames, what Synapse does), which disables the Fn media keys. In Normal mode the EC
  ignores color payloads (falls back to Razer green) and effect-speed parameters (Wave runs at a fixed
  rate, no slow/fast). Verified across *both* the extended (`0x0f02`) and standard (`0x030a`) matrix
  families at transactions `0x1F` and `0xFF`, plus the per-key custom-frame path. See Quirks.
- **`razer-cli … cmd --tx <hex>`**: a transaction-id override on the raw `cmd` path, for protocol
  debugging (the reference drivers issue some commands at `0xFF` rather than our default `0x1F`).

## 2026-07: control, status and self-healing pass
Landed on top of `0.9.0` (tray stayed `0.9.0`, CLI `0.8.6`); all HW-verified on `0x029F`.

**New control & status**
- **`razer-cli auto json`**: full device state as one flat JSON object (perf mode, CPU/GPU boost,
  fan mode + setpoint + *actual* RPM, keyboard %, logo, charge limit), read in a single pass. Flat on
  purpose (no Rust enum-tuple encoding), so Home Assistant or a status line can parse it blind.
- **App profiles**: apply a perf mode while a named app runs, reverting to the AC/battery profile on
  exit. A transient override that never clobbers a saved profile; empty by default.
- **Resume re-assert without full enforce**: the intended mode is re-applied on wake by default
  (`reassert_on_resume`), not only when enforce is on. Wake used to keep whatever the EC reset itself to.
- **Labeled Custom boosts**: the Custom submenu now labels its two groups ("CPU boost" / "GPU boost")
  instead of two unlabeled Low/Med/High stacks.

**Self-healing / correctness**
- **Startup reconcile**: on launch the tray reads the device back after its startup apply and
  re-asserts once if reality doesn't match intent. A just-booted (especially crash-rebooted) EC will ACK
  a perf-mode write without actually switching and keeps its last mode across a reboot, so the tray
  could show "Balanced" while the EC sat in the battery profile. This runs regardless of the enforce
  flag (startup correctness isn't optional) and retired the external `RazerPerfSilent` scheduled-task hack.
- **Honest, per-chassis fan range**: the manual menu used to list 0/500/1000/1500/2000 (all at or below
  the EC's floor, so no effect) and 5500 (above the ceiling). It now offers only speeds the hardware
  honors, and the envelope is *per model* (a required `fan_rpm_range` on every descriptor), so the app
  stays universal. Probed on hardware: setpoint 2000 still idles ~2400, and it won't exceed 5000.

**Housekeeping**
- **tray-icon 0.11.3 → 0.19**: enabled hover-driven tooltip refresh, which let the global
  keyboard hook go (see Quirks).
- **Security**: dropped the unmaintained `failure` crate (RUSTSEC) via single-instance 0.3, and picked
  up patched `crossbeam-epoch` + `anyhow`. Audit-clean apart from Linux-only GTK transitives that aren't
  in the Windows binary.

## 0.9.0: first versioned local release
The first numbered cut of the fork; it rode at upstream's `0.8.6` until now. Given how much changed,
the highlights:

**Architecture**
- Put the device behind a `HidTransport` trait so the logic is host-testable with a mock, and moved the
  state model into `librazer`. 47 host tests.
- Split the ~1,500-line tray `main.rs` into `main` / `menu` / `state` / `program` / `platform`.
- Sentence-case menu labels; reworked tooltip.

**Behavior**
- Mirror reads are input-gated: they track real input and stop when idle, which killed the old ~10 s
  backlight pulsing.
- Fn-key brightness changes are read back and adopted into the active profile.
- Enforce mode (opt-in), with a resume-from-sleep re-assert.
- Keyboard always-on is now a Normal-mode keep-alive (see Quirks); it never enters driver mode, so the
  Fn keys keep working.
- Logging dropped from Trace to Info, bounded at 10 MiB.

**Fixes**
- `battery-care` output went to a suppressed log level; now it prints (and `auto info` shows it).
- "Close GPU apps" / `taskkill` could kill the desktop. Given nvidia-smi reports dwm/explorer/shell hosts
  as GPU users and the old code had no real filter, it SIGKILLed the session. Added a shared, host-tested
  `process_guard` safelist used by both the CLI and tray, plus handling for nvidia-smi's
  `[Insufficient Permissions]` placeholder.
- Recovery loop no longer busy-spins on device loss; "Close GPU apps" no longer panics when nvidia-smi
  isn't on `PATH`; Linux build fix.

