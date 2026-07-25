# Security

## Reporting

Please use [GitHub's private vulnerability reporting](../../security/advisories/new) rather than a
public issue. This is a single-maintainer hobby project — expect a considered reply, not a fast one.

## What this software does and doesn't do

Worth knowing before you run it, since it's the kind of tool that reasonably invites suspicion:

- **No kernel driver.** Everything goes over USB HID feature reports to the keyboard's control
  interface, entirely from user space. Notably this project *rejected* the usual approach for reading
  CPU temperature — a WinRing0-based ring-0 driver — because that driver carries CVE-2020-14979, a
  local privilege-escalation flaw, and has been quarantined by Defender since March 2025. A fan curve
  wasn't worth a kernel attack surface on the user's machine.
- **No network access.** Nothing phones home, checks for updates, or sends telemetry.
- **No elevation.** It runs as a normal user. The only system-level thing it writes is an
  `HKCU\...\Run` value, when you tick "Start with Windows".
- **What it does write:** HID commands to the Razer device, a TOML config under `%APPDATA%`, and a
  log under `%TEMP%` (capped at 10 MiB).
- **`nvidia-smi`** is invoked as a subprocess for dGPU temperature, with no window. If it isn't
  present the fields are simply omitted.
- **"Close GPU apps"** terminates processes and is the most dangerous thing here. It is guarded by a
  hard safelist of session-critical processes, because an earlier version could and did kill the
  desktop session.

## Binary integrity

Release binaries are **not code-signed yet**, so SmartScreen will warn on first run. Each release
publishes `SHA256SUMS.txt`; compare it with `Get-FileHash` if that matters to you. Code signing via
the SignPath Foundation OSS programme is planned but not in place — assume unsigned until a release
says otherwise.
