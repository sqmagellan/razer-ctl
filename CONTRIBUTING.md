# Contributing

Thanks for looking. Two things shape everything below:

1. **It talks directly to your laptop's embedded controller.** A wrong command doesn't throw an
   exception, it writes to firmware. So the bar for "we know this works" is hardware evidence, not
   plausibility.
2. **The maintainer has one laptop**, a Blade 16 (2023), PID `0x029F`. Every other model is
   supported on the strength of someone else's report.

## The most useful contribution

A report from a model that isn't tested here. Use the
[unsupported model issue template](.github/ISSUE_TEMPLATE/unsupported_model.yml); it asks for
`razer-cli enumerate`, the SKU, and which features actually worked.

**Leave fields blank rather than guessing.** A blank fan range costs nothing; a guessed one goes
into the device table and misconfigures that model for everyone after you.

## Code

```
cargo test -p librazer                                  # host-testable logic; runs anywhere
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo build --release --target x86_64-pc-windows-msvc
```

All four must pass; CI gates on all four. From macOS or Linux, `cargo xwin` substitutes for the last
one.

Use `-p librazer`, not a bare `cargo test`: `librazer::device` is gated to Windows and Linux, so the
whole-workspace form fails to compile `razer-tray` on macOS. `cargo test -p razer-cli` runs anywhere:
its tests drive the real handlers against a simulated EC (`librazer::sim`). With `--features sim`,
`razer-cli` also builds on macOS and takes `--sim MODEL.json` before or after any subcommand, so every
command talks to a simulated EC built from that model file instead of the hardware, e.g.
`cargo run -p razer-cli --features sim -- --sim MODEL.json auto json`. Never in a release. On Windows also run
`cargo test -p razer-tray`, which is the only way to exercise the tray-only tests locally (CI runs
them on its Windows job).

### Where code goes

**`librazer` is the testable core**: HID protocol, device model, and every pure helper. It builds
and tests on any host.

**`razer-tray` builds only for Windows**, so a pure function there can't be tested on most
machines. If it doesn't need Win32, put it in `librazer`.

### Hardware claims

If you're adding or changing a device descriptor:

- Every entry declares a real `fan_rpm_range`. Measure it: set a manual RPM and read back
  `fan_actual_rpm`. Don't infer it from a similar model.
- **Don't invent init sequences.** The generic profile for unknown models deliberately ships *no*
  init commands, because a plausible-but-wrong startup sequence is worse than none.
- Say in the PR what you verified on hardware and what you didn't. "Transcribed, not tested" is a
  useful statement; a claim of testing that didn't happen is not.

### Protocol sources and licensing

This project is MIT. Reference implementations like OpenRazer and razer-laptop-control are GPL.

Facts about hardware (command IDs, checksum algorithms, which byte means what) aren't
copyrightable, and using them is fine. Their *code* is. So: read them for the facts, cite the fact,
and write the implementation yourself. Don't paste GPL code into this tree, even temporarily as a
scratch step. If you contribute code derived from a GPL source, say so in the PR so it can be
declined or rewritten rather than quietly relicensing someone else's work.

## Style

Comments explain *why*, especially where the code looks odd. Most of the strange-looking parts here
exist because the hardware or Windows forced them, and that reasoning is the expensive part to
rediscover. Match the surrounding density.
