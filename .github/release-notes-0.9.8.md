## What's new in 0.9.8

- **Game rules menu.** Each app rule has a check mark to turn it on or off. The rule in force shows "(running)".
- **Rules window.** Add, edit and delete rules in a standard Windows dialog. Pick the app from running programs or browse for it. "Edit rules…" opens the config beside a plain-English guide.
- **App rules end cleanly.** A game closing during Modern Standby could leave the laptop in Performance. The tray now checks the laptop 3 s and 15 s after a rule starts or ends and fixes any drift.
- **CLI and tray take turns.** A lock shared by every razer-ctl process keeps `razer-cli` from reading a reply meant for the tray.
- **Charger details.** The full charger table, 40 W USB-C to 400 W. `auto info` says when the charger is smaller than the laptop recommends.
- **Protocol fixes.** Max Fan changes only its own bit. `razer-cli` reads a charge limit back until it shows. CPU boost level 4 is now Overclock; "Undervolt" still works. Five changes come from [Finesssee's fork](https://github.com/Finesssee/razer-ctl) (MIT). Thanks to Finesssee for documenting and measuring them.

Full details in [CHANGELOG.md](https://github.com/sqmagellan/razer-ctl/blob/main/CHANGELOG.md).

## Downloads

Unsigned Windows x86-64 binaries, so SmartScreen warns on first run. Hashes are in `SHA256SUMS.txt`.

Each binary carries a build-provenance attestation proving this repo's release workflow built it from this tag:

```
gh attestation verify razer-tray.exe --repo sqmagellan/razer-ctl --source-ref refs/tags/v0.9.8 --signer-workflow sqmagellan/razer-ctl/.github/workflows/release.yml
```

**Tested on one model.** See Device support in the README for other Blades, and please open an "unsupported model" issue if you try one.
