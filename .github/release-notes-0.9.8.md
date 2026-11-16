## What's new in 0.9.8

- **Game rules in the tray menu.** Each app rule has a check mark to turn it on or off, and the one in force says "(running)". "Rules window…" adds, edits and deletes rules in a plain Windows dialog: the app (running programs, ones using the graphics card first), power mode, CPU and GPU boost for Custom, fans, plugged in only and priority. "Edit rules…" opens the config in Notepad beside a plain-English guide with two worked examples.
- **App rules end cleanly.** After a rule ends, the tray reads the laptop again at 3 s and 15 s and puts back anything that drifted, and the fan write no longer restores a perf mode it read a moment too early. Before, a game closing during Modern Standby could leave the laptop in Performance.
- **The CLI and the tray take turns.** Each request to the laptop's controller holds a lock shared by every razer-ctl process, so `razer-cli` can't read a reply meant for the tray. The wait is capped at 1 s.
- **Charger details.** The full charger table (40 W USB-C to 400 W barrel), and the charger the laptop recommends: `auto info` says when a smaller one is plugged in; `auto json` adds `charger_recommended_watts`.
- **CPU boost "Undervolt" is now "Overclock",** which is what level 4 does. Old configs and scripts that say Undervolt still work.
- **Protocol fixes.** Max Fan changes and reads only its own bit of a shared byte; a charge limit is read back until it shows; fan mode 2 and the controller's timeout status are understood. Five of these changes come from Finesssee's fork (MIT); thanks to Finesssee for writing them down and measuring them.

Full details in [CHANGELOG.md](https://github.com/sqmagellan/razer-ctl/blob/main/CHANGELOG.md).

## Downloads

Unsigned Windows x86-64 binaries, so SmartScreen warns on first run. Hashes are in `SHA256SUMS.txt`.

Each binary carries a build-provenance attestation proving this repo's release workflow built it from this tag:

```
gh attestation verify razer-tray.exe --repo sqmagellan/razer-ctl --source-ref refs/tags/v0.9.8 --signer-workflow sqmagellan/razer-ctl/.github/workflows/release.yml
```

**Tested on one model.** See Device support in the README for other Blades, and please open an "unsupported model" issue if you try one.
