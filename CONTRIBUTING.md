# Contributing

Two facts shape everything below:

1. **It talks directly to the laptop's embedded controller.** A wrong command doesn't throw an
   exception; it writes to firmware. The bar is hardware evidence, not plausibility.
2. **The maintainer has one laptop**, a Blade 16 (2023), PID `0x029F`. Every other model rests on
   someone else's report.

## The most useful contribution

A report from a model not tested here. Use the
[unsupported model issue template](.github/ISSUE_TEMPLATE/unsupported_model.yml); it asks for a
`razer-cli probe` file, the SKU, and which features worked.

**Leave fields blank rather than guessing.** A blank fan range costs nothing. A guessed one goes
into the device table and misconfigures that model for everyone after you.

## Code

```
cargo test -p librazer                                  # host-testable logic; runs anywhere
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo build --release --target x86_64-pc-windows-msvc
```

CI gates on all four. From macOS or Linux, `cargo xwin` replaces the last one.

Use `-p librazer`, not a bare `cargo test`: `librazer::device` is gated to Windows and Linux, so the
whole workspace fails to build `razer-tray` on macOS. `cargo test -p razer-cli` runs anywhere; its
tests drive the real handlers against a simulated EC (`librazer::sim`). With `--features sim`,
`razer-cli` also builds on macOS and takes `--sim MODEL.json` before or after any subcommand, e.g.
`cargo run -p razer-cli --features sim -- --sim MODEL.json auto json`. Never in a release. On
Windows also run `cargo test -p razer-tray`; it is the only local run of the tray tests (CI runs
them on its Windows job).

### Where code goes

**`librazer` is the testable core**: HID protocol, device model and every pure helper. It builds and
tests on any host.

**`razer-tray` builds only for Windows**, so a pure function there can't be tested on most machines.
If it doesn't need Win32, put it in `librazer`.

### Hardware claims

When adding or changing a device descriptor:

- Every entry declares a real `fan_rpm_range`. Measure it: set a manual RPM and read back
  `fan_actual_rpm`. Don't infer it from a similar model.
- **Don't invent init sequences.** The generic profile ships *no* init commands on purpose; a
  plausible but wrong startup sequence is worse than none.
- Say in the PR what you verified on hardware and what you didn't. "Transcribed, not tested" is
  useful. A claim of testing that didn't happen is not.

### Protocol sources and licensing

This project is MIT. OpenRazer and razer-laptop-control are GPL.

Hardware facts (command IDs, checksums, which byte means what) aren't copyrightable. Code is. Read
those projects for the facts, cite the fact, and write the code yourself. Don't paste GPL code into
this tree, even as a scratch step. If a contribution derives from GPL code, say so in the PR so it
can be declined or rewritten.

## Style

Comments explain *why*, especially where the code looks odd. Most odd-looking parts exist because
the hardware or Windows forced them, and that reasoning is expensive to rediscover. Match the
surrounding density.
