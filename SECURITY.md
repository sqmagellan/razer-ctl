# Security

## Reporting

Please use [GitHub's private vulnerability reporting](../../security/advisories/new) rather than a
public issue. This is a single-maintainer hobby project, so replies may take a while.

## What this software does and doesn't do

A tool that talks to your laptop's embedded controller should say what it touches:

- **No kernel driver.** Everything goes over USB HID feature reports to the keyboard's control
  interface, from user space. The usual way to read CPU temperature (the WinRing0 ring-0 driver) was
  left out: it carries CVE-2020-14979, a local privilege escalation, and Defender has quarantined it
  since March 2025. A fan curve isn't worth a kernel attack surface.
- **No network access.** Nothing in the tray or the CLI opens a connection: no update checks, no
  telemetry.
- **No elevation.** It runs as a normal user. The only system-level thing it writes is an
  `HKCU\...\Run` value, when you tick "Start with Windows".
- **What it does write:** HID commands to the Razer device, a TOML config under `%APPDATA%`, and a
  log under `%LOCALAPPDATA%` (at most 4 MiB). Only if you turn them on: the display refresh rate
  (`ChangeDisplaySettingsExW`) and the Windows power mode (`PowerSetActiveOverlayScheme`).
- **`nvidia-smi`** is invoked as a subprocess for dGPU temperature, with no window. If it isn't
  present the fields are left out.
- **"Close apps using the GPU"** ends processes and is the riskiest thing here. It skips a safelist
  of session-critical processes, because an earlier version did kill the desktop session.

## Binary integrity

Release binaries are **not code-signed yet**, so SmartScreen warns on first run. Each release
publishes `SHA256SUMS.txt` to compare against `Get-FileHash`. Code signing via
the SignPath Foundation OSS program is planned but not in place; assume unsigned until a release
says otherwise.
