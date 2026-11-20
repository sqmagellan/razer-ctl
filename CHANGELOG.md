# Changelog

Release history for this fork: the README has current behavior, this file how it got there.
Hardware claims were checked on a Razer Blade 16 (2023), `RZ09-0483`, PID `0x029F`, Windows 11,
unless a user report is cited.

## Unreleased

Tested on `0x029F` (2026-11-19 and 20): panel mode writes and reads back both ways. Two Apple USB-C
chargers (140 W and 96 W) both read as 65 W, and the hold and its notice work. A 5300 RPM write
reads back, but the fans stop at 5000, so the limit stays 5000. The fan guard has not engaged on
hardware yet.

**New**
- **Mini-LED panel mode.** On a Blade 16 (2023) with the dual-mode panel, pick 3840x2400 at 120 Hz
  or 1920x1200 at 240 Hz from the menu or with `razer-cli auto panel set uhd|fhd`. It takes effect
  after a restart, which the tray offers (default No). Never set at startup, by a profile or by a
  reset. Protocol from Paul Rodriguez's [razer-helper](https://github.com/Paulrod20/razer-helper)
  (MIT).
- **Weak-charger guard.** On a USB-C charger, Performance, Hyperboost and Custom run as Balanced, as
  in Synapse. Saved profiles keep their mode. `weak_charger_guard = false` turns it off. A Windows
  notification says so when the charger starts holding the picked mode (plug, start or pick), and
  the menu marks held modes "(Balanced on USB-C)".
- **Fan guard.** With a manual fan speed set, the fans go to full when the CPU sensor reaches 72 °C
  or the dGPU 87 °C. They return to the set point once that sensor stays under 58 or 67 °C for 30 s.
  `fan_guard = false` turns it off. Both guards are ideas from
  [nezinomas/blade-lite](https://github.com/nezinomas/blade-lite) (MIT).

**Fixes**
- **Manual fan read from fan 2.** On `0x029F` the EC runs fan 1 faster than a low set point, and
  once (raw writes) put its own number in fan 1's set point. The set point now comes from fan 2,
  and fan 1 counts as drift only when it is below it.

- Versions: `librazer` adds `guard`, `panel` and `command::get_fan_setpoint`; `ConfigState`
  gains `weak_charger_guard`, `fan_guard`, `fan_guard_cpu_c` and `fan_guard_gpu_c`; breaking:
  `Zone2DriftGuard` is now `ZoneDriftGuard`, `fan_setpoint_zones_differ` is now
  `fan_zone1_below`.

## 0.9.8: game rules menu and window, CPU speed cap, app rules that end cleanly (2026-11-15)
Built and tested on `0x029F` (2026-11-11 to 2026-11-15). The CPU cap ran in a test tray from
2026-11-06 and the dGPU check from 2026-10-25. Five changes come from
[Finesssee/razer-ctl](https://github.com/Finesssee/razer-ctl) (MIT), a fork for the same laptop.
Thanks to Finesssee for documenting and measuring them.

**New**
- **Game rules menu.** Each app rule has a check mark to turn it on or off. The rule in force
  shows "(running)". A click saves `enabled` and starts or ends the rule at once.
- **Rules window.** "Rules window…" opens a standard Windows dialog to add, edit and delete rules.
  The app list shows running programs, those using the NVIDIA GPU first. Settings the dialog
  doesn't show are kept. It uses stock controls and adds 116 KB to the exe.
- **Edit rules…** opens the config in Notepad beside `game-rules-help.txt`, a plain-English guide
  to every rule setting. A test fails if a setting is missing from the guide.
- **CPU speed cap in game rules.** `cpu_max_mhz` (800 or more) caps the CPU on AC while the rule
  runs, through `PROCFREQMAX` and `PROCFREQMAX1` on the active Windows power plan; a plan switch
  gets the cap too. Before each write the old values and the cap go to a file. The old values come
  back when the rule ends, on unplug and on Quit, or at the next start after a crash. A setting
  changed meanwhile to a value other than the cap is left alone. A failed write is retried after
  5 s, doubling up to 5 minutes. The rules window has a "CPU speed limit" row. A Helldivers 2 rule
  held 3600 MHz for 28 minutes and put the settings back when the game closed.
- **"What's keeping the GPU awake?"** lists the apps with dedicated memory on the NVIDIA GPU, from
  the GPU Process Memory performance counters, matched to the dGPU by its adapter LUID. Neither
  read wakes the GPU. With no other display adapter (dGPU-only mode) it says the GPU drives the
  display and can't sleep. `razer-tray --gpu-report <file>` writes the same report.
- **Full charger table.** `0x078c` covers 40 W USB-C and 120 W to 400 W barrel chargers. Its
  second byte is the charger the laptop recommends: `auto json` adds `charger_recommended_watts`,
  and `auto info` names it when a smaller charger is plugged in. From Finesssee/razer-ctl.

**Reliability**
- **App rules end cleanly.** After a game closed in Modern Standby, the EC was back in Performance
  a minute after the tray wrote Balanced. The fan write re-sent a perf mode read too early; it now
  re-sends the mode it wrote. The tray also reads the laptop 3 s and 15 s after a rule starts or
  ends, and writes again only on drift. Forced drift was corrected in 3.3 s.
- **CLI and tray take turns on the EC.** A file lock shared by every razer-ctl process covers each
  request and its reply, so `razer-cli` can't read the tray's reply. A request waits up to 1 s,
  then fails unsent; the tray retries on its next pass. Two loops of 80 `auto json` reads during a
  rule change: no failures, longest read 0.6 s. From Finesssee/razer-ctl.
- **Max Fan changes only its own bit.** `0x070f` also holds other settings (`0x04` is a one-time
  full charge, per [openblade-core](https://github.com/OpenBladeProject/openblade-core)). Max Fan
  now changes only `0x02`. It read `0x02` on and `0x00` off.
- **Charge limit read-back.** openblade-core records a Blade 16 reporting the old limit for 1-2 s.
  `razer-cli` now reads a new limit up to 9 times, 250 ms apart, and fails if it never shows. The
  tray still writes once; Enforce repairs a late read. `0x029F` showed no lag.
- **Max Fan reads from any `0x078f` value.** The byte is a bitfield (`0x01` CPU boost, `0x02`
  Max Fan, `0x04` charge to full once, `0x08` local dimming). Other bits no longer break the read.
  From Finesssee/razer-ctl.
- **CPU boost level 4 is Overclock.** It was labeled Undervolt; the capture in `data/README.md`
  says "cpu overclock". "Undervolt" still works in configs and scripts. A config saved by 0.9.8
  says "Overclock", which 0.9.7 can't read. From Finesssee/razer-ctl.
- **Fan mode 2 and status `0x04`.** Fan mode 2 (Synapse's ForceAuto) reads as Auto; it is never
  sent. Status `0x04`, the EC's timeout, is retried after 20 ms instead of 500 ms. From
  Finesssee/razer-ctl.
- **A pasted rule loads.** A saved config without rules held `app_profiles = []`, so a rule pasted
  below it failed to parse.

**Not taken**
- Finesssee/razer-ctl writes perf and fan mode at once, then waits 200 ms. On `0x029F` the
  two-step write reached 5000 RPM in about 80 s, the same as that sequence, so 0.9.8 keeps it.

- Versions: `razer-tray` 0.9.8, `librazer` 0.12.0 (breaking: `CpuBoost::Undervolt` is
  `Overclock`; `ResponseError` and `capture::Verdict` gain `Timeout`; `PowerSource` gains
  `recommended`; `AppProfile` gains `cpu_max_mhz`), `razer-cli` 0.9.2.

## 0.9.7: temperatures, `razer-cli probe`, Blade 17 (2022) profile (2026-10-25)
Tested on `0x029F` (2026-10-19 to 2026-10-23): `probe --writes` read back every write and restored
every setting; the tray from this code has run on it since 2026-10-23. #10's reporter measured the
Blade 17 (2022) (`0x028B`).

**New**
- **Razer Blade 17 (2022), `RZ09-0423`, PID `0x028B`** gets its own profile, from a user report and
  its reporter's pull request ([#10](https://github.com/sqmagellan/razer-ctl/issues/10),
  [#11](https://github.com/sqmagellan/razer-ctl/pull/11)). Perf modes and CPU/GPU boost read back.
  Manual fan RPM is not followed exactly; the highest seen was 4300. Max Fan answers "not
  supported" (0x05), so the tray grays it out and never sends it. Not tested here.
- **A laptop can lack Max Fan.** On an unlisted model, a "not supported" answer to Max Fan is
  handled like a missing logo: the item is grayed out and "(max)" leaves the tooltip. It used to
  stay offered, and a Custom apply with it on always failed.
- **`razer-cli probe`** saves every setting the EC reports on both fan zones, with the raw status
  byte, retries and timing, as one JSON file for a model report. It writes nothing.
  `probe --writes` (tray closed) also changes perf mode, boosts, Max Fan and the fan for a few
  minutes, reads each back, then restores and checks everything. It saves a recovery file first;
  Ctrl-C restores it, and `probe --restore FILE` replays one.
- **`auto fan rpm` switches the fan to Manual** on a laptop that can read its fan mode. It used to
  write only the set point, so a fan on Auto stayed on the firmware curve and a fan sweep measured
  that curve ([#10](https://github.com/sqmagellan/razer-ctl/issues/10)). The value must be a
  multiple of 100 within the laptop's fan range; stderr says when the mode changed.
- **Blade 16 (2023) Custom floor.** In Custom with a manual fan, this EC runs the fans at the set
  point or 3800 RPM, whichever is higher. A lower set point reads back but isn't used. `auto fan
  rpm` and `auto info` say so, `auto json` adds `fan_floor_rpm`, and the tray's presets read "2000
  RPM (runs at 3800)". Auto has no such limit: under CPU and GPU load it reached 4100 RPM in
  Balanced and 4600 in Performance and Custom.
- **EC temperatures.** The EC reports a CPU-side and a GPU temperature (`0x0d85`, found by
  [blade-control](https://github.com/jdsgnrinfo/blade-control)). The tooltip adds "CPU 62°C",
  `auto info` prints both and `auto json` has `ec_cpu_temp_c` and `ec_gpu_temp_c`. On `0x029F` the
  GPU figure runs 1–2 °C under `nvidia-smi`. The CPU figure is a sensor near the CPU, not the die:
  53–67 °C at idle, up 7 °C in 40 s at half load, then down about 1 °C per 30 s. `probe` reads it,
  so a report shows whether another model has it.
- **The EC's power source.** `auto json` adds `charger` ("battery", "usb-c" or "main"),
  `charger_watts` and `charger_code`; `auto info` prints it under the charge limit (`0x078c`, codes
  from blade-control's table). `0x029F` on its barrel charger reads 0x12, the 330 W charger; no
  other state was seen here. `probe` reads it too.

**Reliability**
- **GPU temperature without a process.** The tray reads the dGPU temperature from Windows (D3DKMT,
  Task Manager's figure) in 0.2 ms instead of 40–80 ms; the raw value matched `nvidia-smi` within
  0.1 °C. `nvidia-smi` still gives the watts, every 30 s or on a 3 °C move, and the temperature
  where Windows has none. Neither runs while the GPU sleeps, and `nvidia-smi` waits for two awake
  polls in a row, so the tray doesn't re-wake a GPU something else woke briefly. On `0x029F` in
  Optimus mode, with every other waker off, the GPU slept through 90 s of the tray and 30 s of EC
  temperature reads at 4 per second. `nvidia-smi` wakes it on every call.
- **Other firmware's perf modes read as ours.** 1 (Gaming on 2018–2020 Blades) reads as Performance
  and 3 (Battery Saver on 2025 firmware) as Battery. `0x029F` stores and reads back both; a read of
  either used to fail. The tray never sends them.
- **Blade 15 Base 2020 (`0x0255`) shows no charge limit.** Its EC accepts the setting and ignores
  it, per a user report in blade-control's model table.
- **Blade 16 (2026), PID `0x02E0`,** is tried before other Razer devices, like the other listed
  Blades.
- **A perf mode write the EC acknowledges and ignores is caught.** It happened once, on 2026-10-19.
  The tray reads perf and fan mode back after applying and retries up to 3 times; `auto perf mode`
  and `auto fan auto|manual` fail instead of reporting success.
- **GPU back in the tooltip with decimal temperatures.** A driver's `nvidia-smi` printed "57.54"
  where a whole number was expected, so every poll failed and the tooltip dropped the GPU.
- **Tests run against simulated ECs** built from four laptops' reports and hardware runs. A command
  the reports don't cover answers "unmodeled" instead of agreeing with the code.
- Versions: `razer-tray` 0.9.7, `librazer` 0.11.0 (breaking: `Optional` gains `MaxFan`),
  `razer-cli` 0.9.1.

## 0.9.6: laptops that lack a feature, a first run that reads the laptop, Blade 18 profile (2026-10-15)
Tested on `0x029F` (2026-10-15): `auto json` matches 0.9.5 byte for byte; fan set points above 5000
run at 5000; the tray ran 11 minutes on the existing config, a missing config and a broken one.
#2's reporter ran a test build on a Blade 15 2018 Base (`0x023b`).

**New**
- **Razer Blade 18 (2023), `RZ09-0484U`, PID `0x02A0`** gets its own profile, from a user report
  ([#1](https://github.com/sqmagellan/razer-ctl/issues/1)). All worked for them but manual fan RPM,
  which holds the fans near 3200/3300 (still open). Not tested here.
- **Unlisted laptops work without a feature they lack.** A logo, keyboard brightness or charge
  limit the EC answers "not supported" (0x05) is skipped by reads, writes and drift checks, and its
  menu shows "(not supported)". One such answer used to fail every read, so the tray could change
  only the refresh rate ([#2](https://github.com/sqmagellan/razer-ctl/issues/2)). Supported models
  are unchanged.
- **Blade 15 2018 Base (`0x023b`):** fan range up to 5700 (its owner heard 5700 over 5000; Synapse
  offers 5700). The logo menu is grayed out from the start: that EC reads the logo back but ignores
  writes. Per-PID list in `librazer/src/unsupported.rs`.
- **`razer-cli auto info` and `auto json` report what they can.** Both stopped at the first rejected
  read. `info` marks each missing feature "not supported", including "Fan actual" on an EC with no
  speed read. `json` sets those fields to `null` and lists them in a new `unsupported` field, left
  out when empty, so a laptop that answers everything prints the same JSON as before.

**Reliability**
- **First run reads the laptop instead of resetting it.** A new config used to be hardcoded
  defaults applied at startup: Performance mode, logo and backlight off, 80% charge limit. Now each
  profile starts unset (`ac_state_unset` / `battery_state_unset`) and adopts the first reading on
  its power source without writing it back. Hand-editing an unset profile sets it. The `0x029F` EC
  keeps brightness per power source and can take seconds to switch, so an adopted profile can hold
  the other source's brightness.
- **A config that doesn't parse is no longer applied or replaced.** It used to become defaults on
  the laptop, then on disk. Now the tray only reads the laptop and saves nothing until the file
  parses or you pick **Reset settings to defaults**, offered only then. A dialog and the top of the
  menu say so. An unreadable file is handled the same way.
- **A Razer mouse is no longer picked as the laptop.** On an unlisted model the app tried each Razer
  USB device in random order; a mouse answers too, so the fan range and results changed between
  runs (#2). Known laptop PIDs now go first, and the log names the PID in use.
- **Fan zone 2 is checked.** A manual fan with zone 2 off target counts as drift and is repaired
  like other drift. After 3 failed repairs in a row, the check stops until restart. From upstream
  blauzim/razer-ctl `435c870`. Zone 2 reads back as written on `0x029F`; the repair path isn't
  tested on hardware.
- **A resync that adopts a profile doesn't write it back.** With "Undo changes made by Synapse" on,
  it used to write the adopted profile in the same pass. Unit tests cover it.
- **`razer-cli auto fan rpm` checks the laptop's own range.** It accepted 2000–5600 on any model.
  Out of range, the EC runs at its floor or ceiling and still reports success. On `0x029F`, 5300,
  5500 and 5700 all ran at 5000.
- **A failed fan speed read pauses fan reads for 60 s.** Each retried for about 4 s on the UI
  thread, so hovering froze the tray on #2's laptop. "Response does not match" errors now show the
  sent and received header.
- **An "off" charge limit is read twice before it's believed.** On `0x029F`, `auto json` falsely
  read a 60% limit as off three times in an hour; the battery held at 74% on AC for 19 hours. The
  second read wins when they differ, and both raw bytes are logged. It costs one read and 50 ms when
  the limit really is off.
- `razer-cli fan info` in the 0.9.x notes below doesn't exist; it's `razer-cli auto info`.
- Configs from 0.9.5 load and save byte for byte as before.
- Versions: `razer-tray` 0.9.6, `librazer` 0.10.0 (breaking: `ConfigState` gains `ac_state_unset`
  and `battery_state_unset`), `razer-cli` 0.9.0.

## 0.9.5: keyboard colors, a clearer menu, and a tray that can't die at login or lose its config
Built and tested on `0x029F` (2026-09-23). Probes on the same machine set the scope: `0x070f` is Max
Fan (fans 2100 → 4700 RPM, no charging above the limit); tray and CLI together cause no bad fan
reads (0/60 either way); the ACPI "High Precision Temperature" counter is frozen at 45.05 °C under
load like the others; the machine sleeps in Modern Standby (69 standby cycles in 17 days, 5
resumes); the EC kept its state across two measured standbys.

**New**
- **Custom keyboard colors.** Presets from the config (one color, or one per row), "Follow
  performance mode" (the tray icon's color), and a battery bar on keys 1–0 (lit keys = charge in
  tens, the rest at 20%, cyan while charging). The 0.9.0-era note that color needs driver mode was
  wrong: six `0x030b` row writes and `0x030a [5, 0]` show a frame in Normal mode with the Fn keys
  intact. The EC doesn't keep the frame, so the tray repaints it after a wake, after every apply and
  on the first input after the idle fade. Mapped for `0x029F` only.
  `razer-cli auto kbd-lighting color` previews it, with `--perf`, `--battery` and `--charging`.
- **Screen refresh rate per profile**, **Match Windows power to Razer performance mode** (opt-in;
  turning it off restores each power source's earlier Windows mode), an opt-in **perf-cycle
  hotkey**, and a menu warning when Synapse is running.
- **Menu.** A top line says whether changes go to the plugged-in or the battery profile. Keyboard
  lighting holds effects, colors, brightness, Battery bar and Keep keyboard lit. Enforce is now
  "Undo changes made by Synapse" and "Close GPU apps" is "Close apps using the GPU".
- Fn-lock (`0x0206`) was tried and dropped: it round-trips on `0x029F` but changes nothing.

**Reliability**
- **Startup and recovery.** One HID error at startup used to exit the tray with nothing logged. The
  tray now starts from the config and resyncs with backoff (2 s doubling to 60 s), reopening the
  device after three failures, without blocking the event loop or reloading the config. A resync
  aims at the current target, so it honors a plug or unplug while it waits. Only bus trouble
  triggers a resync. A write the EC rejects is logged, shown as what the device did, and no longer
  stops the independent writes after it.
- **The config file.** Saved atomically (temp file, then rename). Missing, unreadable and
  unparseable files are handled separately; an unparseable one gets a timestamped copy. Hand edits
  are adopted within 5 s and before any save, and one that doesn't parse is never overwritten. If
  the file can't be read at startup, the tray waits for it and only reads the device. First run
  creates the file again.
- **Modern Standby.** A wake is the resume message, or the display returning after at least 60 s
  off. It schedules reads at +3 s and +15 s and writes only on drift. The +15 s check follows
  G-Helper #5682, where one re-apply at resume lost to the firmware. A display-on wake writes only
  with "Undo changes made by Synapse" on, so a screen timeout never reverts a CLI change.
- **Actions.** A pick made while an Action runs holds until its process exits; it was reverted a
  second later. An AC/battery switch carries picks of the Action's fields across, re-checks the
  rules at once, and no longer applies an AC-only rule on battery. A left-click cycle during an
  Action isn't saved into the profile.
- **Fans.** Max fan exists only in Custom, and a manual RPM snaps to the chassis range and the
  100-RPM step; both used to look like permanent drift. A rejected manual RPM no longer switches the
  fan to Manual, and fan write failures are returned. The tooltip never shows an ignored set point
  as the speed, and shows "…" until a read is trusted. `razer-cli fan info` (Wrong; see 0.9.6.)
  uses the same trust rule, and `auto json` adds `fan_actual_trusted`.
- **Power source.** An "unknown" `ACLineStatus` keeps the previous answer instead of meaning AC.
- **dGPU telemetry doesn't wake a sleeping GPU.** Before each `nvidia-smi` sample the tray reads the
  GPU's power state from Windows, which doesn't wake it, and skips the sample in D3. Only matters
  with Optimus. Untested on a sleeping GPU: the Blade here runs dGPU-only.
- **Transport.** HID I/O errors are retried with backoff and exit 5, as documented; they aborted on
  the first try with exit 1. Raw `cmd` probes are sent once. Perf zones left disagreeing by an
  interrupted write are a typed error, and the tray repairs them.
- **Log** moved to `%LOCALAPPDATA%\razer-tray\`, 1 MiB, rotated through three older files. Logging
  starts first.
- **Supply chain.** bincode is gone: the 90-byte packet is encoded by hand, checked byte-identical
  first. Every Action is pinned to a commit SHA, tokens are read-only except the release job's,
  cargo runs `--locked`, and the documented `gh attestation verify` pins the tag and workflow. The
  CLI has tests.
- American spelling throughout.
- Versions: `razer-tray` 0.9.5, `librazer` 0.9.0 (breaking: `Packet` is no longer serde),
  `razer-cli` 0.8.9.

## 0.9.3: resume detection that fires, and Actions kept out of saved profiles
HW-verified on `0x029F` (2026-09-05).

- **The tray never noticed a resume, and its log hid it for six weeks.** `PBT_APMRESUMESUSPEND`
  replaced the tick-gap heuristic on 2026-07-25, but every `resume detected` line since came from
  the heuristic: nine Kernel-Power 107 resumes from 07-30 to 09-04 logged none. The window was the
  cause. `spawn_display_state_monitor` creates a message-only window and registers only
  `RegisterPowerSettingNotification` for `GUID_CONSOLE_DISPLAY_STATE`. `PBT_APMRESUME*` is a
  broadcast, which a message-only window never gets, and that registration delivers only its own
  setting. The tray now also calls `RegisterSuspendResumeNotification`, which targets the window.
  The two registrations are independent, and startup logs which took. On a hibernate and resume at
  19:18:17 both broadcasts arrived, the latch merged them into one re-assert, and the log showed its
  first `resume detected (OS power broadcast)`.
- **An Action's settings could leak into the saved AC or battery profile.** `menu::build` bakes the
  effective `DeviceState` into every handler, and during an Action that is the overlay. Picking any
  item handed `update()` the Action's perf mode and fan, and `update()` saved all of it to
  `ac_state` or `battery_state`. After the game closed, the profile held the Action's settings. The
  README's claim that an Action "never overwrites a saved profile" was the intent, not the
  behavior. `DeviceState::carry_changes` now saves only the fields that differ between the state
  the menu was built from and the one picked. With no Action active `base == before`, so the common
  path is bit-identical to before; one of three new tests checks that. Only the saved profile
  changes. Whether a manual edit should override a running Action on the device, or wait until it
  ends, is a separate question, left untouched.
- **`librazer` 0.8.7 → 0.8.8** for `DeviceState::carry_changes`. It only adds, so `razer-cli` stays
  at 0.8.7. Verified on the Blade: `fmt --check`, clippy `--target x86_64-pc-windows-msvc
  -Dwarnings`, `cargo build --release`, and 122 tests.

## 0.9.2: measured fan floor, filtered fan reads, an audit gate
HW-verified on `0x029F` (2026-08-15).

- **The manual fan floor is 2000 RPM, not the 2200 the descriptor claimed.** Raw `0x0d01` set
  points past the CLI's clamp (1800, 1500, 1200, 1000, 800, 400, 100, 0) all settle at exactly 2000
  on both zones, held 40 s each at 0–17% CPU load. 2000 and 3000 are honored exactly. It is a
  floor, not EC thermal demand: Auto idle is asymmetric (2000/1900), the manual floor always
  symmetric (2000/2000). Only the two `RZ09-0483` rows were lowered. Every other row in both tables
  comes from a third-party `laptops.json`; guessing one chassis from another's measurement is how
  the wrong 2200 got in. Those rows are now marked suspect.
- **Sub-floor fan writes are rejected.** `set_fan_rpm` had no lower bound: `set_fan_rpm(dev, 400)`
  returned `Ok(())` while the fan stayed at 2000. The EC stores a sub-floor value in its read-back
  register and ignores it, so every caller reported a speed the fan wasn't running. Both ends are
  bounded now, and the error says why.
- **Single `0x0d88` fan reads are filtered.** About 3 in 10 sparse samples had one impossible zone
  (0/1800, then 2000/0, then 0/1800 at 30 s spacing), which the measured 23–53 RPM/s slew rules
  out. It goes both ways: one sample showed a lone 2000 with the fans stopped. `FanRpmFilter` drops
  lone-zone reads and needs two reads in a row to report a stop; running values publish at once,
  since a ramp moves every poll. A large nonzero gap isn't flagged: 100–200 RPM of Auto asymmetry
  is normal here, and no higher threshold has been measured. Tach output is quantized to 100 RPM.
- **Zero RPM is real, but Auto-only.** 14 of 15 reads 1.5 s apart returned 0/0 over 22 s. Manual
  can't stop the fans, and a set point of 0 doesn't return to the firmware curve on this chassis:
  the mode stays Manual at 2000. So razer-control-revived's "0 clears the manual flag" behavior
  must not be ported here.
- **`cargo audit` runs weekly in its own workflow.** CI catches a change that broke something;
  audit catches a world that changed under untouched code, which only a timer finds, and a separate
  workflow keeps four build/test jobs from rerunning weekly. The bumps that paid off here were
  advisory- or capability-driven, never calendar-driven: single-instance 0.1.2 → 0.3 dropped the
  unmaintained `failure` dep (RUSTSEC-2019-0036 / -2020-0036, CVSS 9.8), and tray-icon 0.11.3 →
  0.19 removed a keyboard-hook hack. Version lag alone isn't a defect. Not `--deny warnings`: the
  first run found 13 warnings and zero vulnerabilities in 371 dependencies, ten of the warnings in
  the Linux-only GTK stack that lets `cargo build` run on a Linux runner. `cargo-audit` fails on
  vulnerabilities by default.
- **clap 4.6.1 → 4.6.6** (lockfile only), the one bump outstanding; three of the four crates
  checked were current. `Cargo.toml` lists a caret floor, not what gets built, so reading manifests
  instead of the lockfile overstated the work by three crates. The resolver also merged duplicate
  `windows-*` crates (net -5 crates); nothing `razer-tray` declares changed. Verified on the Blade:
  librazer tests, clippy `--target x86_64-pc-windows-msvc -Dwarnings`, `fmt --check`,
  `cargo test -p razer-tray`, `cargo build --release` and `cargo audit`, all clean.


## 2026-07: charge limit, readable lighting, leaner binary
HW-verified on `0x029F` (2026-07-25).

- **Charge limit is any whole percent 50–100**, not 8 fixed presets. The EC accepts every integer
  in that range and refuses 49 and below, so the old 50/55/…/80 enum was our own limit. It hid 43
  usable values, all of 81–99 among them. `battery-care set 88` now sets 88 instead of rounding to
  80. Configs that stored `"Percent80"` still load. That shim matters: the tray loads config with
  `unwrap_or_default()`, so a parse failure would discard saved profiles.
- **Keyboard effect is read back** (`0x0f82`), so the menu and `auto json` show what the firmware
  runs. The README said this getter didn't exist; it does.
- **Tray clicks no longer wait behind hover events.** The event loop took one event per ~1 s tick,
  but `Move` fires at the mouse report rate over the icon and the channel is unbounded, so a click
  waited behind every earlier Move. Approaching the icon queued a hundred-plus events and delayed
  the mode switch by minutes; the backlog outgrew the drain rate, so the lag grew all session.
  Both channels now drain each pass, hover events coalesce into at most one refresh, and a click
  supersedes them. Throttling the hover work never helped: a skipped event still used its tick.
- **Left-click no longer opens the menu and changes the perf mode.** `tray-icon`'s
  `menu_on_left_click` defaults to true and must be turned off. Under 0.11.3 the Windows backend
  showed the menu only on `WM_RBUTTONUP` and ignored the flag. 0.19 honors it, so one left-click
  opened the menu on button-down and cycled the perf mode, hidden behind the popup, on button-up.
  Each hidden cycle was saved, so the AC/battery profile drifted. Left-click cycles the mode;
  right-click opens the menu.
- **dGPU temperature and power in the tooltip**, sampled on a background thread so the UI never
  waits on a subprocess. With no NVIDIA tools or no dGPU the fields are left out, not shown as
  "0°C". The monitor allows about a minute of failures: the tray starts at login, when the driver
  is least likely to answer, and quitting on the first sample left the tooltip empty all session.
- **Tray binary is 1.84 MB, down from 3.40 MB.** The six icons are decoded and scaled to 64×64 raw
  RGBA at build time, so the `image` crate (a full PNG/JPEG/GIF/WebP decoder) is no longer linked.
  `embed-resource`, a build-dependency with no `build.rs`, is removed.
- **One fan-RPM ceiling instead of three.** `command.rs` capped RPM at 5500, `FAN_RPM_MAX_ANY` at
  5300, and the device table reaches 5600, so CLI input was checked against a limit no real machine
  had. Both ends now come from the tables (2000–5600), and a test keeps them in step.
- **The tray's pure logic moved into `librazer`** (perf-mode cycle, nearest-brightness step), where
  host tests reach it. The tray crate doesn't build on a non-Windows host, so tests there never ran.

## 2026-07: portability and correctness pass (pre-publication)
Groundwork for Blades other than the one it was written on. Verified on `0x029F` where hardware
could verify it; what can't be (other chassis) is marked.

- **Report checksum is computed.** Each packet carries the XOR of bytes 2..88 that OpenRazer and
  razer-laptop-control send, instead of a hard-coded `0x00`. HW-verified 2026-07-25: this Blade's
  EC accepts `crc = 0` and a correct CRC alike, so nothing visible changes here. It prevents silent
  failures on a model whose firmware checks the field.
- **An unrecognized Blade no longer refuses to start.** A `RZ09-` SKU missing from `SUPPORTED` gets
  a generic profile (all features offered, no guessed init sequence) and a warning naming the SKU,
  instead of a "not supported" exit whose only workaround was `manual --pid`. Unsupported commands
  answer `NotSupported` and fail fast, so a missing control gives one clean error.
- **Per-PID fan ranges for 44 uncatalogued chassis,** transcribed from community device tables so
  the generic profile offers each machine's real range. Transcribed, not tested: this project owns
  one Blade. Catalogued models in `SUPPORTED` always win.
- **Resume detection uses the OS power broadcast.** `PBT_APMRESUMESUSPEND` replaces the "a tick gap
  over 30 s means we slept" heuristic. That misfired here: at `IDLE_PRIORITY_CLASS` under EcoQoS
  the loop was starved for 54.9 s while awake, logged a resume that never happened and fired a
  spurious re-assert.
- **`DeviceState::read()` no longer queries perf mode twice.** It called `get_perf_mode()` for the
  perf mode and again for the fan mode, and each call reads both fan zones. 13 round-trips are now
  11 (12 again since the keyboard-effect getter was added). A test pins the count.
- **`librazer` works as a library.** `HidTransport::send` takes a `Packet`, but `packet` was a
  private module, so no outside crate could implement the trait. `tests/public_api.rs` compiles as
  a separate crate and pins this.
- **CI gates.** It now runs the tests (it never did) and builds `windows-msvc`, the shipped target,
  instead of `windows-gnu`. `cargo fmt --check` and clippy are gated, and the tree is
  rustfmt-clean. `Cargo.lock` is committed, as a repo that ships binaries should.


## 2026-07: keyboard lighting (effects-only)
All HW-verified on one unit, a Razer Blade 16 (2023), `RZ09-0483U`, PID `0x029F`; no other model
was tested for lighting.
- **Keyboard RGB effects**: Off / Spectrum / Wave / Breathing, in the tray ("Keyboard lighting"
  submenu) and CLI (`razer-cli auto kbd-lighting effect <off|spectrum|wave|breathing>`). These are
  the EC's built-in effects (extended-matrix command `0x0f02`, VARSTORE, our native `0x1F`
  transaction), so they run in Normal mode and the Fn media keys keep working. Read back via
  `0x0f82`, so the menu and `auto json` show what the firmware runs.
- **No keyboard color, by design.** (Wrong; see 0.9.5.) Static or per-key color needs Razer driver
  mode (host-streamed frames, as Synapse does), which disables the Fn media keys. In Normal mode the
  EC ignores color payloads (falls back to Razer green) and effect speed (Wave has one fixed rate).
  Checked on both the extended (`0x0f02`) and standard (`0x030a`) matrix families at transactions
  `0x1F` and `0xFF`, plus the per-key custom-frame path. See Quirks.
- **`razer-cli … cmd --tx <hex>`**: a transaction-id override on the raw `cmd` path for protocol
  debugging; the reference drivers send some commands at `0xFF`, not our default `0x1F`.

## 2026-07: control, status and self-healing pass
Landed on `0.9.0` (tray stayed `0.9.0`, CLI `0.8.6`); all HW-verified on `0x029F`.

**New control & status**
- **`razer-cli auto json`**: full device state as one flat JSON object (perf mode, CPU/GPU boost,
  fan mode, set point and actual RPM, keyboard %, logo, charge limit), read in one pass. No Rust
  enum-tuple encoding, so Home Assistant or a status line can parse it blind.
- **App profiles**: apply a perf mode while a named app runs, then return to the AC/battery profile
  on exit. A temporary override that never overwrites a saved profile (Wrong; see 0.9.3.); empty by
  default.
- **Resume re-assert without full enforce**: on wake the intended mode is re-applied by default
  (`reassert_on_resume`), not only with enforce on. Wake used to keep whatever the EC reset to.
- **Labeled Custom boosts**: the Custom submenu labels its two groups ("CPU boost" / "GPU boost")
  instead of two unlabeled Low/Med/High stacks.

**Self-healing / correctness**
- **Startup reconcile**: after its startup apply the tray reads the device back and re-asserts once
  on a mismatch. A just-booted EC, especially after a crash reboot, ACKs a perf-mode write without
  switching and keeps its last mode across a reboot, so the tray could show "Balanced" while the EC
  sat in the battery profile. It runs whatever the enforce flag says, and it retired the external
  `RazerPerfSilent` scheduled-task hack.
- **Per-chassis fan range**: the manual menu listed 0/500/1000/1500/2000 (all at or below the EC's
  floor, so no effect) and 5500 (above the ceiling). It now offers only speeds the hardware honors,
  per model (a required `fan_rpm_range` on every descriptor), so the app stays universal. On
  hardware, set point 2000 still idles at ~2400 and the fan won't pass 5000.

**Housekeeping**
- **tray-icon 0.11.3 → 0.19**: hover-driven tooltip refresh, which let the global keyboard hook go
  (see Quirks).
- **Security**: dropped the unmaintained `failure` crate (RUSTSEC) via single-instance 0.3 and took
  patched `crossbeam-epoch` and `anyhow`. Audit-clean apart from Linux-only GTK dependencies that
  aren't in the Windows binary.

## 0.9.0: first versioned local release
The first numbered release of the fork, which had stayed at upstream's `0.8.6`. Highlights:

**Architecture**
- The device sits behind a `HidTransport` trait so the logic is host-testable with a mock, and the
  state model moved into `librazer`. 47 host tests.
- The ~1,500-line tray `main.rs` is split into `main` / `menu` / `state` / `program` / `platform`.
- Sentence-case menu labels; reworked tooltip.

**Behavior**
- Mirror reads are input-gated: they follow real input and stop when idle, which ended the old
  ~10 s backlight pulsing.
- Fn-key brightness changes are read back and adopted into the active profile.
- Enforce mode (opt-in), with a re-assert on resume from sleep.
- Keyboard always-on is a Normal-mode keep-alive (see Quirks); it never enters driver mode, so the
  Fn keys keep working.
- Logging dropped from Trace to Info, capped at 10 MiB.

**Fixes**
- `battery-care` output went to a suppressed log level; now it prints, and `auto info` shows it.
- "Close GPU apps" / `taskkill` could kill the desktop: nvidia-smi lists dwm, explorer and shell
  hosts as GPU users, the old code had no real filter, and it SIGKILLed the session. A shared,
  host-tested `process_guard` safelist now serves the CLI and tray, and nvidia-smi's
  `[Insufficient Permissions]` placeholder is handled.
- The recovery loop no longer busy-spins on device loss; "Close GPU apps" no longer panics when
  nvidia-smi isn't on `PATH`; Linux build fix.
