# Contributing

Thanks for looking. Two things about this project shape everything below:

1. **It talks directly to your laptop's embedded controller.** A wrong command doesn't throw an
   exception, it writes to firmware. So the bar for "we know this works" is hardware evidence, not
   plausibility.
2. **The maintainer has one laptop** — a Blade 16 (2023), PID `0x029F`. Every other model is
   supported on the strength of someone else's report.

## The most useful contribution

A report from a model that isn't tested here. Use the
[unsupported model issue template](.github/ISSUE_TEMPLATE/unsupported_model.yml); it asks for
`razer-cli enumerate`, the SKU, and which features actually worked.

**Please leave fields blank rather than guessing.** A blank fan range costs nothing; a guessed one
goes into the device table and silently misconfigures that model for everyone who comes after.

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
whole-workspace form fails to compile `razer-cli` on macOS. On Windows also run
`cargo test -p razer-tray`, which is the only way to exercise the tray-only tests locally (CI runs
them on its Windows job).

### Where code goes

**`librazer` is the testable core** — HID protocol, device model, and every pure helper. It builds
and tests on any host.

**`razer-tray` can only be built for Windows.** That has a consequence people get wrong: a pure
function placed in the tray crate is a function the test suite can never reach on a normal
development machine. If it doesn't need Win32, put it in `librazer`. Several helpers were moved for
exactly this reason (the tooltip budgeter, the perf-mode cycle, the brightness step).

### Hardware claims

If you're adding or changing a device descriptor:

- Every entry declares a real `fan_rpm_range`. Measure it — set a manual RPM and read back
  `fan_actual_rpm` — don't infer it from a similar model.
- **Don't invent init sequences.** The generic profile for unknown models deliberately ships *no*
  init commands, because a plausible-but-wrong startup sequence is worse than none.
- Say in the PR what you verified on hardware and what you didn't. "Transcribed, not tested" is a
  perfectly acceptable and genuinely useful statement; a claim of testing that didn't happen is not.

### Protocol sources and licensing

This project is MIT. Reference implementations like OpenRazer and razer-laptop-control are GPL.

Facts about hardware — command IDs, checksum algorithms, which byte means what — aren't
copyrightable, and using them is fine. Their *code* is. So: read them for the facts, cite the fact,
and write the implementation yourself. Don't paste GPL code into this tree, even temporarily as a
scratch step. If you contribute code derived from a GPL source, say so in the PR so it can be
declined or rewritten rather than quietly relicensing someone else's work.

## Style

Comments explain *why*, especially where the code looks odd — most of the strange-looking parts here
exist because the hardware or Windows forced them, and that reasoning is the expensive part to
rediscover. Match the surrounding density.
