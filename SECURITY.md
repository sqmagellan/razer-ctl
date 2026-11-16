# Security

## Reporting

Please use [GitHub's private vulnerability reporting](../../security/advisories/new), not a public
issue. One person maintains this, so replies may take a while.

## What this software does and doesn't do

- **No kernel driver.** Everything goes over USB HID feature reports to the keyboard's control
  interface, from user space. The usual CPU temperature driver (WinRing0) was left out: it carries
  CVE-2020-14979, a local privilege escalation, and Defender has quarantined it since March 2025.
- **No network access.** The tray and the CLI open no connections: no update checks, no telemetry.
- **No elevation.** It runs as a normal user. The only system-level write is an `HKCU\...\Run`
  value, when you tick "Start with Windows".
- **What it writes:** HID commands to the Razer device, a TOML config under `%APPDATA%`, a log under
  `%LOCALAPPDATA%` (at most 4 MiB), and an empty lock file in the temp folder that keeps the tray
  and the CLI from talking to the EC at once. Only if you turn them on: the display refresh rate
  (`ChangeDisplaySettingsExW`) and the Windows power mode (`PowerSetActiveOverlayScheme`).
- **`nvidia-smi`** runs as a windowless subprocess for dGPU watts, and for the temperature where
  Windows doesn't report it. Without it those fields are left out.
- **"Close apps using the GPU"** ends processes and is the riskiest feature. It skips a safelist of
  session-critical processes, because an earlier version killed the desktop session.

## Binary integrity

Release binaries **aren't code-signed**, so SmartScreen warns on first run. Each release publishes
`SHA256SUMS.txt` to compare against `Get-FileHash`, and a build-provenance attestation (see the
README's Install section). Assume unsigned until a release says otherwise.
