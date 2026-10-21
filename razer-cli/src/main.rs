mod laptop;

use laptop::{Backend, Laptop};
use librazer::command;
use librazer::feature;
use librazer::types::{
    BatteryCare, CpuBoost, FanMode, FanZone, GpuBoost, KeyboardEffect, LightsAlwaysOn, LogoMode,
    MaxFanSpeedMode, PerfMode,
};

use librazer::feature::Feature;
use librazer::keyboard::{self, KeyboardColor, Rgb};

use anyhow::{bail, Result};
use clap::{arg, Command};
use std::ffi::OsString;
use std::process::Command as procCommand;
use sysinfo::{ProcessExt, Signal, System, SystemExt};

trait Cli: feature::Feature {
    fn cmd(&self) -> Option<Command> {
        None
    }
    fn handle(&self, _device: &Laptop, _matches: &clap::ArgMatches) -> Result<()> {
        Ok(())
    }
}

macro_rules! impl_unary_cmd_cli {
    ($parser:block, $name:literal, $arg_name:literal, $desc:literal,$arg_desc:literal) => {
        clap::Command::new($name)
            .about($desc)
            .arg(arg!(<$arg_name> $arg_desc).value_parser($parser))
            .arg_required_else_help(true)

    }
}

macro_rules! impl_unary_handle_cli {
    (<$arg_type:ty>($matches:ident, $device:ident, $name:literal, $arg_name:literal, $setter:path)) => {
        match $matches.subcommand() {
            Some(($name, matches)) => {
                $setter($device, *matches.get_one::<$arg_type>($arg_name).unwrap())?
            }
            _ => (),
        }
    };
}

macro_rules! impl_unary_handle_with_arg_cli {
    (<$arg_type:ty>($matches:ident, $device:ident, $name:literal, $arg_name:literal, $arg2:literal, $setter:path)) => {
        match $matches.subcommand() {
            Some(($name, matches)) => $setter(
                $device,
                *matches.get_one::<$arg_type>($arg_name).unwrap(),
                $arg2,
            )?,
            _ => (),
        }
    };
}

macro_rules! impl_unary_cli {
    (<$feature_type:ty><$arg_type:ty>($desc:literal,$arg_desc:literal,$setter:path,$getter:path)) => {
        impl Cli for $feature_type {
            fn cmd(&self) -> Option<Command> {
                Some(
                    clap::Command::new(self.name())
                        .about($desc)
                        .arg(arg!(<ARG> $arg_desc).value_parser(clap::value_parser!($arg_type)))
                        .arg_required_else_help(true),
                )
            }
            fn handle(&self, device: &Laptop, matches: &clap::ArgMatches) -> Result<()> {
                match matches.subcommand() {
                    Some((ident, matches)) if ident == self.name() => {
                        let arg = matches.get_one::<$arg_type>("ARG").unwrap();
                        $setter(device, *arg)
                    }
                    Some(("info", _)) => {
                        $getter(device).map(|value| println!("{}: {:?}", self.name(), value))
                    }
                    _ => Ok(()),
                }
            }
        }

    }
}

impl_unary_cli! {<feature::KbdBacklight><u8>("Set keyboard backlight brightness", "Number in range [0, 255]", command::set_keyboard_brightness, command::get_keyboard_brightness)}
impl_unary_cli! {<feature::LidLogo><LogoMode>("Set lid logo mode", "", command::set_logo_mode, command::get_logo_mode)}
impl_unary_cli! {<feature::LightsAlwaysOn><LightsAlwaysOn>("Set lights always on", "", command::set_lights_always_on, command::get_lights_always_on)}

impl Cli for feature::BatteryCare {
    fn cmd(&self) -> Option<Command> {
        Some(
            clap::Command::new(self.name())
                .about("Control battery care (charge limiting)")
                .subcommand(
                    clap::Command::new("set")
                        .about("Set battery charge limit percentage")
                        .arg(
                            // Any whole percent 50-100: the EC accepts all of them
                            // (HW-verified), so no rounding and no preset list.
                            arg!(<PERCENT> "Charge limit percentage (any whole 50-100; 100 = no limit)")
                                .value_parser(
                                    clap::value_parser!(u8)
                                        .range(BatteryCare::MIN_PERCENT as i64..=BatteryCare::MAX_PERCENT as i64),
                                )
                        )
                )
                .subcommand(clap::Command::new("enable").about("Enable battery care (limit to 80%) [deprecated: use 'set 80']"))
                .subcommand(clap::Command::new("disable").about("Disable battery care (charge to 100%) [deprecated: use 'set 100']"))
                .subcommand(clap::Command::new("get").about("Get current battery care setting"))
                .arg_required_else_help(true),
        )
    }

    fn handle(&self, device: &Laptop, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, sub_matches)) if ident == self.name() => match sub_matches.subcommand() {
                Some(("set", set_matches)) => {
                    let percent = *set_matches.get_one::<u8>("PERCENT").unwrap();
                    let mode = BatteryCare::from_percent(percent)?;
                    command::set_battery_care(device, mode)?;
                    println!("Battery care set to {}", mode);
                    Ok(())
                }
                Some(("enable", _)) => {
                    command::set_battery_care(device, BatteryCare::from_percent(80).unwrap())?;
                    println!("Battery care enabled (charge limit set to 80%)");
                    Ok(())
                }
                Some(("disable", _)) => {
                    command::set_battery_care(device, BatteryCare::DISABLE)?;
                    println!("Battery care disabled (will charge to 100%)");
                    Ok(())
                }
                Some(("get", _)) => {
                    let current = command::get_battery_care(device)?;
                    println!("Current battery care: {}", current);
                    Ok(())
                }
                _ => Ok(()),
            },
            Some(("info", _)) => {
                let current = command::get_battery_care(device)?;
                println!("{}: {}", self.name(), current);
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl Cli for feature::KbdLighting {
    fn cmd(&self) -> Option<Command> {
        Some(
            clap::Command::new(self.name())
                .about("Control keyboard RGB backlight effect (the current effect is shown by `auto json`)")
                .subcommand(
                    clap::Command::new("effect")
                        .about("Set the backlight effect (off/spectrum/wave/breathing)")
                        .arg(
                            arg!(<EFFECT> "Effect")
                                .value_parser(clap::value_parser!(KeyboardEffect)),
                        )
                        .arg_required_else_help(true),
                )
                .subcommand(
                    clap::Command::new("color")
                        .about(
                            "Show a custom color: one #rrggbb for the whole keyboard, or six \
                             for the rows top to bottom. Not kept by the keyboard: sleep \
                             brings the effect back (the tray repaints its own color).",
                        )
                        .arg(
                            arg!([COLORS] ... "Colors")
                                .num_args(1..=6)
                                .value_parser(clap::value_parser!(Rgb)),
                        )
                        .arg(
                            arg!(--perf <MODE> "Instead of colors, the color of a perf mode (battery/silent/balanced/performance/hyperboost/custom)")
                                .required(false),
                        )
                        .arg(
                            arg!(--battery <PERCENT> "Preview the battery bar at this charge")
                                .required(false)
                                .value_parser(clap::value_parser!(u8).range(0..=100)),
                        )
                        .arg(arg!(--charging "Preview the battery bar as charging").requires("battery"))
                        .arg_required_else_help(true),
                )
                .arg_required_else_help(true),
        )
    }

    fn handle(&self, device: &Laptop, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, sub)) if ident == self.name() => match sub.subcommand() {
                Some(("color", m)) => {
                    if !keyboard::supports_custom_frame(device.info.pid) {
                        bail!(
                            "custom color is not mapped for this model ({:#06x})",
                            device.info.pid
                        );
                    }
                    use librazer::state::PerfMode as Mode;
                    let colors: Vec<Rgb> = m
                        .get_many::<Rgb>("COLORS")
                        .map(|v| v.copied().collect())
                        .unwrap_or_default();
                    let (color, perf) = match (m.get_one::<String>("perf"), colors.len()) {
                        (Some(name), 0) => {
                            let perf = match name.to_ascii_lowercase().as_str() {
                                "battery" => Mode::Battery,
                                "silent" => Mode::Silent,
                                "balanced" => Mode::Balanced,
                                "performance" => Mode::Performance,
                                "hyperboost" => Mode::Hyperboost,
                                "custom" => Mode::Custom(CpuBoost::Boost, GpuBoost::High),
                                other => bail!("unknown perf mode {other:?}"),
                            };
                            (KeyboardColor::FollowPerfMode, perf)
                        }
                        (Some(_), _) => bail!("give colors or --perf, not both"),
                        (None, 1) => (KeyboardColor::solid(colors[0]), Mode::Balanced),
                        (None, n) => match <[Rgb; keyboard::ROWS]>::try_from(colors.as_slice()) {
                            Ok(rows) => (KeyboardColor::Rows(rows), Mode::Balanced),
                            Err(_) => bail!("give one color or six, not {n}"),
                        },
                    };
                    let battery =
                        m.get_one::<u8>("battery")
                            .map(|&percent| keyboard::BatteryLevel {
                                percent,
                                charging: m.get_flag("charging"),
                            });
                    let frame = keyboard::render(color, perf, battery);
                    command::set_keyboard_frame(device, &frame)?;
                    println!("Keyboard color set");
                    Ok(())
                }
                Some(("effect", m)) => {
                    let effect = *m.get_one::<KeyboardEffect>("EFFECT").unwrap();
                    command::set_keyboard_effect(device, effect)?;
                    println!("Keyboard effect set to {:?}", effect);
                    Ok(())
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }
}

struct CustomCommand;

impl Feature for CustomCommand {
    fn name(&self) -> &'static str {
        "cmd"
    }
}

impl Cli for CustomCommand {
    fn cmd(&self) -> Option<Command> {
        Some(
            clap::Command::new(self.name())
                .about("Run custom command [WARNING: Use at your own risk]")
                .arg(
                    arg!(--tx <TX> "Transaction id override (hex, e.g. 0xff); default 0x1f")
                        .required(false)
                        .value_parser(clap_num::maybe_hex::<u8>),
                )
                .arg(
                    arg!(<COMMAND> "Command in hex format, e.g. 0x0d82")
                        .required(true)
                        .value_parser(clap_num::maybe_hex::<u16>),
                )
                .arg(
                    arg!(<ARGS>... "Arguments to the command, e.g. 0 1 3 5")
                        .required(false)
                        .trailing_var_arg(true)
                        .value_parser(clap_num::maybe_hex::<u8>),
                )
                .arg_required_else_help(true),
        )
    }
    fn handle(&self, device: &Laptop, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, matches)) if ident == self.name() => {
                let cmd = *matches.get_one::<u16>("COMMAND").unwrap();
                let args: Vec<u8> = matches
                    .get_many::<u8>("ARGS")
                    .map(|v| v.copied().collect())
                    .unwrap_or_default();
                let tx = matches.get_one::<u8>("tx").copied();
                match tx {
                    Some(t) => {
                        println!(
                            "Running custom command @ tx {:#04x}: {:x?} {:?}",
                            t, cmd, args
                        );
                        command::custom_command_tx(device, cmd, &args, t)
                    }
                    None => {
                        println!("Running custom command: {:x?} {:?}", cmd, args);
                        command::custom_command(device, cmd, &args)
                    }
                }
            }
            _ => Ok(()),
        }
    }
}

impl Cli for feature::Fan {
    fn cmd(&self) -> Option<Command> {
        Some(
            clap::Command::new(self.name())
                .about("Control fan")
                .subcommand(clap::Command::new("auto").about("Set fan mode to auto"))
                .subcommand(clap::Command::new("manual").about("Set fan mode to manual"))
                .subcommand(impl_unary_cmd_cli!{{clap::value_parser!(u16).range(librazer::state::FAN_RPM_MIN_ANY as i64..=librazer::state::FAN_RPM_MAX_ANY as i64)}, "rpm", "RPM", "Set fan rpm", "Fan RPM, a multiple of 100, within this laptop's fan range (`info` prints it as fan_rpm_range)"})
                .subcommand(impl_unary_cmd_cli!{{clap::value_parser!(MaxFanSpeedMode)}, "max", "MAX", "Control Max Fan Speed Mode", "Max Fan Speed Mode"})
                .arg_required_else_help(true),
        )
    }

    fn handle(&self, device: &Laptop, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, matches)) if ident == self.name() => {
                // The parser takes any value some Blade can run; this laptop's own range is
                // narrower. Outside it the EC holds its floor or ceiling and reports success.
                if let Some(("rpm", m)) = matches.subcommand() {
                    let rpm = *m.get_one::<u16>("RPM").unwrap();
                    let (min, max) = device.info.fan_rpm_range;
                    anyhow::ensure!(
                        (min..=max).contains(&rpm),
                        "{rpm} RPM is outside this laptop's fan range ({min}..={max} RPM); the \
                         EC would run at {} RPM and report success",
                        if rpm < min { min } else { max }
                    );
                }
                impl_unary_handle_with_arg_cli! {<u16>(matches, device, "rpm", "RPM", false, command::set_fan_rpm)}
                impl_unary_handle_cli! {<MaxFanSpeedMode>(matches, device, "max", "MAX", command::set_max_fan_speed_mode)}

                match matches.subcommand() {
                    Some(("auto", _)) => command::set_fan_mode(device, FanMode::Auto),
                    Some(("manual", _)) => command::set_fan_mode(device, FanMode::Manual),
                    _ => Ok(()),
                }
            }
            Some(("info", _)) => {
                match command::get_perf_mode(device) {
                    Ok((_, fan_mode @ FanMode::Auto)) => {
                        println!("Fan: {:?}", fan_mode)
                    }
                    Ok((_, fan_mode @ FanMode::Manual)) => {
                        // The set-point register reads back whatever was written, even a
                        // value the EC is ignoring, so it is labeled as a set point, never
                        // as the speed.
                        match command::get_fan_rpm(device, FanZone::Zone1) {
                            Ok(0) => println!("Fan: {:?} (no set point yet)", fan_mode),
                            Ok(rpm) => println!("Fan: {:?}, set point {} RPM", fan_mode, rpm),
                            Err(e) => println!("Fan: {:?}, set point unreadable: {}", fan_mode, e),
                        }
                    }
                    Err(e) => println!("{}", e),
                };
                // Both zones, through the same trust rule the tray uses. Some ECs have
                // no speed read at all (issue #2): fan control still works there, so
                // only the actual-speed line says so.
                let (fan, trusted) = match librazer::state::sample_fan_rpm(device, 3) {
                    Err(e) if librazer::error::is_not_supported(&e) => {
                        println!("Fan actual: not supported");
                        return Ok(());
                    }
                    r => r?,
                };
                println!(
                    "Fan actual: {}/{} RPM{}",
                    fan.fan1,
                    fan.fan2,
                    if trusted { "" } else { " (unconfirmed read)" }
                );
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl Cli for feature::Perf {
    fn cmd(&self) -> Option<Command> {
        Some(
            clap::Command::new(self.name())
                .about("Control performance modes")
                .subcommand(impl_unary_cmd_cli!{{clap::value_parser!(PerfMode)}, "mode", "MODE", "Set performance mode", "Performance mode"})
                .subcommand(impl_unary_cmd_cli!{{clap::value_parser!(CpuBoost)}, "cpu", "CPU", "Set CPU boost", "CPU boost"})
                .subcommand( impl_unary_cmd_cli!{{clap::value_parser!(GpuBoost)}, "gpu", "GPU", "Set GPU boost", "GPU boost"})
                .arg_required_else_help(true),
        )
    }

    fn handle(&self, device: &Laptop, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, matches)) if ident == self.name() => {
                impl_unary_handle_cli! {<PerfMode>(matches, device, "mode", "MODE", command::set_perf_mode)}
                impl_unary_handle_cli! {<CpuBoost>(matches, device, "cpu", "CPU", command::set_cpu_boost)}
                impl_unary_handle_cli! {<GpuBoost>(matches, device, "gpu", "GPU", command::set_gpu_boost)}
                Ok(())
            }
            Some(("info", _)) => {
                // A failure goes back to `print_info`, which names this feature.
                let perf_mode = command::get_perf_mode(device)?;
                println!("Performance: {:?}", perf_mode);
                if let (PerfMode::Custom, _) = perf_mode {
                    let cpu_boost = command::get_cpu_boost(device)?;
                    let gpu_boost = command::get_gpu_boost(device)?;
                    println!("CPU: {:?}", cpu_boost);
                    println!("GPU: {:?}", gpu_boost);

                    if let (CpuBoost::Boost | CpuBoost::Undervolt, GpuBoost::High) =
                        (cpu_boost, gpu_boost)
                    {
                        println!(
                            "Max Fan Speed: {:?}",
                            command::get_max_fan_speed_mode(device)?
                        )
                    }
                }
                /* Test command code
                let response = command::send_command(
                    device,
                    0x0d88,
                    &[0, 1, 0]);
                    println!("Rssponse: {:?}",response);
                */
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn enumerate(backend: &Backend, as_json: bool) -> Result<()> {
    let (pid_list, model_number_prefix) = backend.enumerate()?;
    let catalogued = librazer::descriptor::SUPPORTED
        .iter()
        .any(|supported| model_number_prefix == supported.model_number_prefix);

    if as_json {
        // This is the exact information the "unsupported model" issue template asks for,
        // so make it copy-pasteable and unambiguous rather than something a reporter has
        // to retype. PIDs are rendered as hex strings because that's how every table,
        // descriptor, and bug report writes them.
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "model": model_number_prefix,
                "catalogued": catalogued,
                "pids": pid_list.iter().map(|p| format!("0x{p:04x}")).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!("Model: {}", model_number_prefix);
    println!("Supported: {}", catalogued);
    println!("PID: {:#06x?}", pid_list);
    Ok(())
}

/// `LO-HI` for `probe --fan-range`, both multiples of 100 inside the known-chassis bounds.
fn parse_fan_range(s: &str) -> std::result::Result<(u16, u16), String> {
    let (lo, hi) = s
        .split_once('-')
        .ok_or_else(|| format!("{s:?}: expected LO-HI, e.g. 3500-5000"))?;
    let parse = |v: &str| v.trim().parse::<u16>().map_err(|e| format!("{v:?}: {e}"));
    let (lo, hi) = (parse(lo)?, parse(hi)?);
    let bounds = librazer::state::FAN_RPM_MIN_ANY..=librazer::state::FAN_RPM_MAX_ANY;
    if lo >= hi || !bounds.contains(&lo) || !bounds.contains(&hi) {
        return Err(format!(
            "{lo}-{hi}: needs LO < HI, both within {}-{}",
            bounds.start(),
            bounds.end()
        ));
    }
    if lo % 100 != 0 || hi % 100 != 0 {
        return Err(format!("{lo}-{hi}: both must be multiples of 100"));
    }
    Ok((lo, hi))
}

/// `probe`: a capture of every getter, and with `--writes` the write set (see
/// `librazer::probe`). The summary goes to stderr so stdout stays clean JSON. `device` must
/// be recording.
fn probe(
    backend: &Backend,
    device: &Laptop,
    out: Option<&String>,
    writes: bool,
    settle_secs: u64,
    fan_range: Option<(u16, u16)>,
    restore: Option<&String>,
) -> Result<()> {
    let (_, model_number) = backend.enumerate()?;
    // The tray re-asserts its own settings and would fight a write or a restore. A
    // simulated EC has no tray.
    #[cfg(feature = "sim")]
    let real = matches!(backend, Backend::Hardware);
    #[cfg(not(feature = "sim"))]
    let real = true;
    let tray_closed = || -> Result<()> {
        let mut sys = System::new_all();
        sys.refresh_processes();
        if real
            && sys
                .processes()
                .values()
                .any(|p| p.name().eq_ignore_ascii_case("razer-tray.exe"))
        {
            bail!("close razer-tray first (right-click the tray icon, Exit), then run this again");
        }
        Ok(())
    };

    if let Some(path) = restore {
        tray_closed()?;
        let rec: librazer::probe::Recovery = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        if rec.format != librazer::probe::RECOVERY_FORMAT {
            bail!(
                "{path} is not a recovery file this version reads ({:?})",
                rec.format
            );
        }
        if (rec.pid, rec.model_number.as_str()) != (device.info.pid, model_number.as_str()) {
            bail!(
                "{path} was saved on {} ({:#06x}), not this laptop ({}, {:#06x}); nothing written",
                rec.model_number,
                rec.pid,
                model_number,
                device.info.pid
            );
        }
        librazer::probe::restore(device, &rec.snapshot)?;
        std::fs::remove_file(path)?;
        eprintln!("restored and verified the settings saved in {path}");
        return Ok(());
    }

    let catalogued =
        librazer::matching::find_descriptor(&model_number, librazer::descriptor::SUPPORTED)
            .is_some();
    let machine = librazer::capture::Machine {
        pid: device.info.pid,
        model_number: model_number.clone(),
        profile: device.info.name.to_string(),
        catalogued,
    };
    let tool = concat!("razer-cli ", env!("CARGO_PKG_VERSION"));
    // A simulated capture says so, so it can't pass for a real machine's in an issue.
    #[cfg(feature = "sim")]
    let tool = &match backend {
        Backend::Sim(sim) => format!("{tool} (simulated EC: {})", sim.name()),
        Backend::Hardware => tool.to_string(),
    };
    let capture = if writes {
        tray_closed()?;
        // Sweep a catalogued range, or one the user gives. A generic profile's range is a
        // guess, so without --fan-range only its middle is tried.
        let sweep = match (fan_range, catalogued) {
            (Some(range), _) => librazer::probe::sweep_points(range),
            (None, true) => librazer::probe::sweep_points(device.info.fan_rpm_range),
            (None, false) => {
                let (lo, hi) = device.info.fan_rpm_range;
                vec![(lo / 2 + hi / 2) / 100 * 100]
            }
        };
        let snap = librazer::probe::snapshot(device)?;
        let recovery = match out {
            Some(path) => format!("{path}.recovery.json"),
            None => "razer-probe-recovery.json".to_string(),
        };
        let envelope = librazer::probe::Recovery {
            format: librazer::probe::RECOVERY_FORMAT.into(),
            pid: device.info.pid,
            model_number,
            snapshot: snap.clone(),
        };
        // Never over an unfinished run's file: that one holds the settings from before.
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&recovery)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "{recovery} is left from an unfinished run; put those settings back first: razer-cli probe --restore {recovery}"
            ),
            Err(e) => return Err(e.into()),
        };
        std::io::Write::write_all(
            &mut file,
            (serde_json::to_string_pretty(&envelope)? + "\n").as_bytes(),
        )?;
        drop(file);
        eprintln!(
            "saved the current settings to {recovery}; Ctrl-C stops the run and puts them back; if the process dies, run: razer-cli probe --restore {recovery}"
        );
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancel.clone();
        if let Err(e) =
            ctrlc::set_handler(move || flag.store(true, std::sync::atomic::Ordering::SeqCst))
        {
            eprintln!("warning: no Ctrl-C handler ({e}); a Ctrl-C will skip the restore");
        }
        let capture = librazer::probe::run_writes(
            device,
            machine,
            tool,
            &snap,
            &sweep,
            std::time::Duration::from_secs(settle_secs),
            &cancel,
        );
        if capture
            .restored
            .as_deref()
            .is_some_and(|r| r.starts_with("verified"))
        {
            std::fs::remove_file(&recovery)?;
        } else {
            eprintln!(
                "the restore didn't verify; the settings from before are still in {recovery}"
            );
        }
        capture
    } else {
        librazer::probe::run(device, machine, tool)
    };
    eprint!("{}", librazer::probe::summary(&capture));
    let json = serde_json::to_string_pretty(&capture)? + "\n";
    match out {
        Some(path) => {
            std::fs::write(path, json)?;
            eprintln!("wrote {path}");
        }
        None => print!("{json}"),
    }
    Ok(())
}

/// Machine-readable device state, one JSON object per line-free block. Kept as a flat,
/// stable shape (no nested enum tuples) so downstream consumers -- a Home Assistant
/// command-line sensor, a status line -- can parse it without knowing Rust's serde
/// enum encoding. Mirrors what `perf/fan/... info` prints, gathered in one read.
///
/// A value this laptop doesn't report (the EC answers "not supported", 0x05) is `null`,
/// and its field is named in `unsupported`, which is left out when empty. On a model that
/// answers every read the output is unchanged. `keyboard_effect` keeps its own rule: any
/// failure to read it is `null`, as before, and it is never listed.
#[derive(serde::Serialize)]
struct JsonStatus {
    name: &'static str,
    pid: String,
    perf_mode: Option<String>,
    cpu_boost: Option<String>,
    gpu_boost: Option<String>,
    fan_mode: Option<String>,
    fan_setpoint_rpm: Option<u16>,
    fan_actual_rpm: Option<[u16; 2]>,
    /// False when no read could be confirmed: a lone-zero zone never cleared, or a stop
    /// was seen only once. One raw read is wrong about 3 times in 10 on this hardware.
    fan_actual_trusted: Option<bool>,
    keyboard_brightness_percent: Option<u8>,
    logo_mode: Option<String>,
    /// Keyboard backlight effect, read back from the EC (0x0f82). `null` when the device
    /// reports an effect we don't model (e.g. a Synapse-set Static/Reactive).
    keyboard_effect: Option<String>,
    battery_care_percent: Option<u8>,
    max_fan: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unsupported: Vec<&'static str>,
}

/// One read for `json`: a command this laptop lacks becomes `None` and its field is listed
/// in `unsupported`; any other failure still fails the command.
fn json_read<T>(
    unsupported: &mut Vec<&'static str>,
    field: &'static str,
    read: Result<T>,
) -> Result<Option<T>> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(e) if librazer::error::is_not_supported(&e) => {
            unsupported.push(field);
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn print_json(device: &Laptop) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&json_status(device)?)?);
    Ok(())
}

fn json_status(device: &Laptop) -> Result<JsonStatus> {
    use librazer::state::brightness_to_percent;

    // Each read stands alone, in the order `DeviceState::read` uses, so one the EC lacks
    // doesn't hide the rest (issue #2: a Blade 15 2018 failed the whole command).
    let mut unsupported = Vec::new();
    let modes = json_read(
        &mut unsupported,
        "perf_mode",
        command::get_perf_mode(device),
    )?;
    if modes.is_none() {
        // The same command carries the fan mode.
        unsupported.push("fan_mode");
    }
    let custom = matches!(modes, Some((PerfMode::Custom, _)));
    let (cpu_boost, gpu_boost) = if custom {
        (
            json_read(
                &mut unsupported,
                "cpu_boost",
                command::get_cpu_boost(device),
            )?,
            json_read(
                &mut unsupported,
                "gpu_boost",
                command::get_gpu_boost(device),
            )?,
        )
    } else {
        (None, None)
    };
    let fan_setpoint_rpm = match modes {
        Some((_, FanMode::Manual)) => json_read(
            &mut unsupported,
            "fan_setpoint_rpm",
            command::get_fan_rpm(device, FanZone::Zone1),
        )?,
        _ => None,
    };
    let fan = json_read(
        &mut unsupported,
        "fan_actual_rpm",
        librazer::state::sample_fan_rpm(device, 3),
    )?;
    let brightness = json_read(
        &mut unsupported,
        "keyboard_brightness_percent",
        command::get_keyboard_brightness(device),
    )?;
    let logo = json_read(
        &mut unsupported,
        "logo_mode",
        command::get_logo_mode(device),
    )?;
    // Cosmetic, and older models may lack the getter: unknown is `null`, as before.
    let keyboard_effect = command::get_keyboard_effect(device).unwrap_or(None);
    let battery_care = json_read(
        &mut unsupported,
        "battery_care_percent",
        command::get_battery_care(device),
    )?;
    // Max fan is a Custom-only toggle, false outside Custom; unknown without a perf mode.
    let max_fan = match modes {
        Some((PerfMode::Custom, _)) => json_read(
            &mut unsupported,
            "max_fan",
            command::get_max_fan_speed_mode(device),
        )?
        .map(|mode| matches!(mode, MaxFanSpeedMode::Enable)),
        Some(_) => Some(false),
        None => None,
    };

    Ok(JsonStatus {
        name: device.info.name,
        pid: format!("{:#06x}", device.info.pid),
        perf_mode: modes.map(|(perf, _)| format!("{perf:?}")),
        cpu_boost: cpu_boost.map(|b| format!("{b:?}")),
        gpu_boost: gpu_boost.map(|b| format!("{b:?}")),
        fan_mode: modes.map(|(_, fan)| format!("{fan:?}")),
        fan_setpoint_rpm,
        fan_actual_rpm: fan.map(|(rpm, _)| [rpm.fan1, rpm.fan2]),
        fan_actual_trusted: fan.map(|(_, trusted)| trusted),
        keyboard_brightness_percent: brightness.map(brightness_to_percent),
        logo_mode: logo.map(|mode| format!("{mode:?}")),
        keyboard_effect: keyboard_effect.map(|e| format!("{e:?}")),
        battery_care_percent: battery_care.map(|care| care.to_percent()),
        max_fan,
        unsupported,
    })
}

fn taskkill() -> Result<()> {
    // Run nvidia-smi to get PIDs of GPU processes
    let output = match procCommand::new("nvidia-smi")
        .args(["--query-compute-apps=pid", "--format=csv,noheader"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nvidia-smi not available ({e}); nothing to terminate");
            return Ok(());
        }
    };

    if !output.status.success() {
        eprintln!("nvidia-smi command failed or no GPU processes found");
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pids: Vec<u32> = stdout
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect();

    if pids.is_empty() {
        println!("No GPU-using processes found.");
        return Ok(());
    }

    println!("GPU-using PIDs found: {:?}", pids);

    let mut sys = System::new_all();
    sys.refresh_processes();

    for pid in pids {
        if let Some(process) = sys.process(sysinfo::Pid::from(pid as usize)) {
            let name = process.name();
            // Never kill the compositor/shell: on this hardware nvidia-smi reports
            // dwm/explorer/etc. as GPU users, and killing them tears down the session.
            if librazer::process_guard::is_protected_process(name) {
                println!("Skipping protected process {} ({})", pid, name);
                continue;
            }
            println!("Killing process {} ({})", pid, name);
            // Send SIGKILL to the process
            if process.kill_with(Signal::Kill).unwrap_or(false) {
                println!("Successfully killed PID {}", pid);
            } else {
                eprintln!("Failed to kill PID {}", pid);
            }
        } else {
            eprintln!("Process with PID {} not found", pid);
        }
    }
    Ok(())
}

fn update_cmd(cmd: Command, features: &[Box<dyn Cli>]) -> Command {
    features
        .iter()
        .filter_map(|f| f.cmd())
        .fold(cmd, |cmd, f| cmd.subcommand(f))
}

fn handle(device: &Laptop, matches: &clap::ArgMatches, features: &Vec<Box<dyn Cli>>) -> Result<()> {
    if let Some(("info", _)) = matches.subcommand() {
        println!("Device: {:?}", device.info);
        return print_info(device, matches, features);
    }

    if let Some(("json", _)) = matches.subcommand() {
        return print_json(device);
    }

    for f in features {
        f.handle(device, matches)?;
    }
    Ok(())
}

/// `info` for every feature, one at a time: a feature this laptop lacks prints "not
/// supported" and the rest still print. Stopping at the first one hid every feature after
/// it (issue #2). Any other failure is printed too and fails the command at the end.
fn print_info(
    device: &Laptop,
    matches: &clap::ArgMatches,
    features: &Vec<Box<dyn Cli>>,
) -> Result<()> {
    let mut first_error = None;
    for f in features {
        if let Err(e) = f.handle(device, matches) {
            if librazer::error::is_not_supported(&e) {
                println!("{}: not supported", f.name());
            } else {
                println!("{}: error: {e}", f.name());
                first_error.get_or_insert(e);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn gen_cli_features(feature_list: &[&str]) -> Vec<Box<dyn Cli>> {
    use feature::*;
    librazer::iter_features!(|_, feature| -> Box<dyn Cli> { Box::new(feature) })
        .into_iter()
        .filter(|f| feature_list.contains(&f.name()))
        .collect()
}

/// Exit with a classified status instead of anyhow's blanket 1.
///
/// The point is scriptability: a caller needs to tell "no Razer laptop here" (3) from
/// "this model doesn't implement that command" (4, stop asking) from "the EC was busy"
/// (5, worth retrying). 2 belongs to clap's usage errors and is not ours to assign.
/// See `librazer::error::ExitCode` for the contract.
fn main() -> std::process::ExitCode {
    env_logger::init();
    match real_main(std::env::args_os().collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            let code = librazer::error::ExitCode::classify(&e);
            // `{:#}` renders the whole anyhow context chain on one line, which is what a
            // script's stderr wants; the Debug form spreads it over several.
            eprintln!("error: {e:#}");
            std::process::ExitCode::from(code as u8)
        }
    }
}

/// The whole argument tree. Separate from `real_main` so it can be checked without a
/// device: clap validates the tree only when it is used, and the CLI had no tests at all.
fn build_cli(cli_features: &[Box<dyn Cli>]) -> Command {
    let info_cmd = clap::Command::new("info").about("Get device info");
    let json_cmd = clap::Command::new("json").about("Print full device state as JSON");
    let auto_cmd = clap::Command::new("auto")
        .about("Automatically detect supported Razer device and enable device specific features")
        .subcommand(info_cmd.clone())
        .subcommand(json_cmd.clone())
        .subcommand_required(true);

    let manual_cmd =clap::Command::new("manual").about("Manually specify PID of the Razer device and enable all features (many might not work)")
            .arg(
                arg!(-p --pid <PID> "PID of the Razer device to use")
                .required(true)
                .value_parser(clap_num::maybe_hex::<u16>)
            )
            .arg_required_else_help(true)
            .subcommand(info_cmd)
            .subcommand(json_cmd)
            .subcommand_required(true);

    let cli = clap::command!()
        .color(clap::ColorChoice::Always)
        // Documented here as well as in the README, because a script author reaching for
        // `--help` shouldn't have to go find a web page to learn what a failure means.
        .after_help(
            "Exit codes:\n  \
             0  success\n  \
             1  unclassified error\n  \
             2  usage error (emitted by the argument parser)\n  \
             3  no usable Razer laptop found (retrying will not help)\n  \
             4  command not supported by this model (definitive; stop asking)\n  \
             5  device communication error: busy, rejected, or out of step (retryable)",
        )
        .subcommand_required(true)
        .subcommand(update_cmd(auto_cmd, cli_features))
        .subcommand(update_cmd(manual_cmd, cli_features))
        .subcommand(
            clap::Command::new("enumerate")
                .about("List discovered Razer devices")
                .arg(
                    arg!(--json "Print as JSON (this is what a device-support issue needs)")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(
            clap::Command::new("probe")
                .about("Read every setting the tools use and record each answer from the EC, for a device-support issue (read-only)")
                .arg(arg!(-o --out <FILE> "Write the JSON to FILE instead of stdout").required(false))
                .arg(
                    arg!(--writes "Also change perf mode, boosts, Max Fan and the fan for a few minutes, reading each back, then restore them (close the tray first)")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(
                    arg!(--settle <SECS> "With --writes: how long to watch the fans settle at each set point")
                        .value_parser(clap::value_parser!(u64))
                        .default_value("150"),
                )
                .arg(
                    arg!(--"fan-range" <RANGE> "With --writes: the manual fan range to sweep, LO-HI in RPM (default: a catalogued laptop's own range; otherwise only the middle of the generic one)")
                        .value_parser(parse_fan_range),
                )
                .arg(
                    arg!(--restore <FILE> "Put back the settings an interrupted --writes run saved")
                        .required(false)
                        .conflicts_with("writes"),
                ),
        )
        .subcommand(clap::Command::new("taskkill").about("Terminate all processes using dGPU"));
    sim_option(cli)
}

/// `--sim MODEL`, in `--features sim` builds only. Global, so it can follow a subcommand.
#[cfg(feature = "sim")]
fn sim_option(cli: Command) -> Command {
    cli.arg(
        arg!(--sim <MODEL> "Talk to a simulated EC built from MODEL, an EC model JSON file, instead of the hardware")
            .required(false)
            .global(true),
    )
}

#[cfg(not(feature = "sim"))]
fn sim_option(cli: Command) -> Command {
    cli
}

/// What `manual --pid` talks to: every feature offered, nothing known about the model.
fn manual_descriptor(pid: u16) -> librazer::descriptor::Descriptor {
    librazer::descriptor::Descriptor {
        model_number_prefix: "Unknown",
        name: "Unknown",
        pid,
        features: feature::ALL_FEATURES,
        init_cmds: &[],
        // Same bounds the generic auto profile would use for this PID: the
        // chassis's own range when the table knows it, else the flat default.
        fan_rpm_range: librazer::descriptor::fan_range_for_pid(pid)
            .unwrap_or(librazer::matching::FALLBACK_FAN_RPM_RANGE),
    }
}

fn real_main(args: Vec<OsString>) -> Result<()> {
    #[cfg(feature = "sim")]
    let backend = Backend::from_args(&args)?;
    #[cfg(not(feature = "sim"))]
    let backend = Backend::Hardware;

    // TODO: find a better way to detect auto mode in advance
    let is_auto_mode = laptop::subcommand(&args) == Some("auto".as_ref());
    let mut laptop = match is_auto_mode {
        true => Some(backend.detect()?),
        _ => None,
    };
    let feature_list = match laptop {
        Some(ref laptop) => laptop.info.features,
        _ => feature::ALL_FEATURES,
    };

    let mut cli_features: Vec<Box<dyn Cli>> = gen_cli_features(feature_list);
    cli_features.push(Box::new(CustomCommand));

    let cmd = build_cli(&cli_features);

    let matches = cmd.get_matches_from(args);

    let result = dispatch(&backend, &mut laptop, &matches, &cli_features);
    #[cfg(feature = "sim")]
    for line in gap_report(laptop.as_ref()) {
        eprintln!("{line}");
    }
    result
}

/// Run the parsed command. `laptop` holds the one `auto` detected, and receives the one
/// `probe` or `manual` opens, so the caller can still look at it afterwards.
fn dispatch(
    backend: &Backend,
    laptop: &mut Option<Laptop>,
    matches: &clap::ArgMatches,
    cli_features: &Vec<Box<dyn Cli>>,
) -> Result<()> {
    match matches.subcommand() {
        Some(("enumerate", submatches)) => enumerate(backend, submatches.get_flag("json")),
        Some(("taskkill", _)) => {
            #[cfg(feature = "sim")]
            if let Backend::Sim(_) = backend {
                bail!("taskkill ends this machine's real GPU processes; it has no simulated form");
            }
            taskkill()
        }
        Some(("probe", submatches)) => {
            let device = laptop.insert(backend.detect()?.recording());
            probe(
                backend,
                device,
                submatches.get_one::<String>("out"),
                submatches.get_flag("writes"),
                *submatches.get_one::<u64>("settle").unwrap(),
                submatches.get_one::<(u16, u16)>("fan-range").copied(),
                submatches.get_one::<String>("restore"),
            )
        }
        Some(("auto", submatches)) => {
            // Set above when the first argument is `auto`; the expect documents that
            // coupling rather than leaving a bare unwrap for a future reader to audit.
            let device = laptop
                .as_ref()
                .expect("auto mode detects the device before parsing arguments");
            handle(device, submatches, cli_features)
        }
        Some(("manual", submatches)) => {
            let pid = *submatches.get_one::<u16>("pid").unwrap();
            let device = laptop.insert(backend.open(manual_descriptor(pid))?);
            handle(device, submatches, cli_features)
        }
        // clap enforces `subcommand_required`, so neither of these should be reachable --
        // but they used to be `unimplemented!()`/`unreachable!()`, i.e. a panic with a
        // backtrace in a user's terminal if that assumption ever broke. An error costs
        // nothing and degrades politely.
        Some((cmd, _)) => anyhow::bail!("unhandled subcommand: {cmd}"),
        None => anyhow::bail!("no subcommand given (try --help)"),
    }
}

/// A simulated EC's coverage gaps, one line each, for the end of stderr. `json` tolerates a
/// failed keyboard effect read, and `info` prints a failed feature and goes on, so a gap
/// there would otherwise pass unnoticed.
#[cfg(feature = "sim")]
fn gap_report(laptop: Option<&Laptop>) -> Vec<String> {
    let mut seen = Vec::new();
    for gap in laptop.map(Laptop::gaps).unwrap_or_default() {
        if !seen.contains(&gap) {
            seen.push(gap);
        }
    }
    seen.iter()
        .map(|gap| format!("razer-cli: simulation gap: {gap}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> Command {
        let mut features = gen_cli_features(feature::ALL_FEATURES);
        features.push(Box::new(CustomCommand));
        build_cli(&features)
    }

    fn parses(args: &[&str]) -> bool {
        cli()
            .try_get_matches_from(std::iter::once("razer-cli").chain(args.iter().copied()))
            .is_ok()
    }

    #[test]
    fn the_argument_tree_is_valid() {
        cli().debug_assert();
    }

    #[test]
    fn json_nulls_a_read_the_laptop_lacks_and_fails_on_any_other_error() {
        use librazer::packet::ResponseError;
        let mut unsupported = Vec::new();
        let lacking: Result<u8> = Err(anyhow::Error::new(ResponseError::NotSupported));
        assert_eq!(
            json_read(&mut unsupported, "logo_mode", lacking).unwrap(),
            None
        );
        assert_eq!(json_read(&mut unsupported, "pid", Ok(7)).unwrap(), Some(7));
        assert_eq!(unsupported, vec!["logo_mode"]);

        let busy: Result<u8> = Err(anyhow::Error::new(ResponseError::Busy));
        assert!(json_read(&mut unsupported, "max_fan", busy).is_err());
        assert_eq!(unsupported, vec!["logo_mode"]);
    }

    #[test]
    fn fan_rpm_below_every_known_floor_is_a_usage_error() {
        assert!(!parses(&["auto", "fan", "rpm", "1800"]));
        assert!(parses(&["auto", "fan", "rpm", "3000"]));
        assert!(!parses(&["auto", "fan", "rpm", "9000"]));
    }

    #[test]
    fn every_documented_subcommand_parses() {
        for args in [
            &["auto", "json"][..],
            &["auto", "info"],
            &["auto", "fan", "auto"],
            &["auto", "perf", "mode", "balanced"],
            &["auto", "battery-care", "set", "80"],
            &["manual", "--pid", "0x029f", "json"],
            &["enumerate", "--json"],
            &["probe"],
            &["probe", "--out", "capture.json"],
            &["probe", "--writes", "--settle", "90"],
            &["probe", "--writes", "--fan-range", "3500-5000"],
            &["probe", "--restore", "razer-probe-recovery.json"],
            &["auto", "kbd-lighting", "color", "#ff0000"],
            &[
                "auto",
                "kbd-lighting",
                "color",
                "#ff0000",
                "#00ff00",
                "#0000ff",
                "ffffff",
                "#000000",
                "#123456",
            ],
        ] {
            assert!(parses(args), "{args:?} should parse");
        }
        assert!(parses(&[
            "auto",
            "kbd-lighting",
            "color",
            "--perf",
            "silent"
        ]));
        assert!(parses(&[
            "auto",
            "kbd-lighting",
            "color",
            "#ff0000",
            "--battery",
            "40",
            "--charging"
        ]));
        assert!(!parses(&[
            "auto",
            "kbd-lighting",
            "color",
            "#ff0000",
            "--charging"
        ]));
        assert!(!parses(&[
            "auto",
            "kbd-lighting",
            "color",
            "#ff0000",
            "--battery",
            "101"
        ]));
        // Not a color, or more colors than rows.
        assert!(!parses(&["auto", "kbd-lighting", "color", "red"]));
        assert!(!parses(&[
            "auto",
            "kbd-lighting",
            "color",
            "#000000",
            "#000000",
            "#000000",
            "#000000",
            "#000000",
            "#000000",
            "#000000"
        ]));
    }

    #[cfg(not(feature = "sim"))]
    #[test]
    fn there_is_no_sim_option_without_the_feature() {
        assert!(!parses(&["--sim", "model.json", "auto", "json"]));
    }

    // ---- the real handlers against a simulated EC ----

    use laptop::Simulation;
    use librazer::capture::Recording;
    use librazer::error::ExitCode;
    use librazer::sim::Model;
    use librazer::transport::HidTransport;
    use serde_json::{json, Value};

    /// Every command the CLI sends works; Balanced, fans on Auto at 2000/1900.
    fn model_json(pid: u16) -> Value {
        let rules: Vec<Value> = [
            0x0d02u16, 0x0d82, 0x0d01, 0x0d81, 0x0d88, 0x0d07, 0x0d87, 0x070f, 0x078f, 0x0712,
            0x0792, 0x0300, 0x0380, 0x0302, 0x0382, 0x0303, 0x0383, 0x0f02, 0x0f82, 0x0004,
            0x0084, 0x030b, 0x030a,
        ]
        .iter()
        .map(|c| json!({"command": format!("{c:#06x}"), "support": "works", "evidence": "inferred: CLI test model"}))
        .collect();
        json!({
            "format": librazer::sim::FORMAT,
            "name": "CLI test",
            "pid": format!("{pid:#06x}"),
            "sources": {},
            "state": {
                "perf_mode": [0, 0], "fan_mode": [0, 0], "setpoint_rpm": [0, 0],
                "actual_rpm": [2000, 1900], "cpu_boost": 1, "gpu_boost": 2, "max_fan": 0,
                "logo_power": 1, "logo_mode": 0, "kbd_brightness": 128, "kbd_effect": 3,
                "battery_care": 0x80 | 80, "lights_always_on": 0
            },
            "fan": {
                "auto_rpm": [2000, 1900], "manual": "honored", "floor_rpm": 2000,
                "ceiling_rpm": 5000, "max_fan_rpm": 4700, "slew_rpm_per_s": 30,
                "evidence": "inferred: CLI test model"
            },
            "rules": rules
        })
    }

    fn model(value: &Value) -> Model {
        Model::from_json(&value.to_string()).unwrap()
    }

    /// Put `rule` ahead of the rest (first match wins), or drop `command`'s rules.
    fn with_rule(mut value: Value, rule: Value) -> Value {
        value["rules"].as_array_mut().unwrap().insert(0, rule);
        value
    }

    fn without_rule(mut value: Value, command: &str) -> Value {
        value["rules"]
            .as_array_mut()
            .unwrap()
            .retain(|r| r["command"] != command);
        value
    }

    /// `razer-cli auto ARGS` through the real handlers, as `real_main` runs them.
    fn auto(laptop: &Laptop, args: &[&str]) -> Result<()> {
        let mut features = gen_cli_features(laptop.info.features);
        features.push(Box::new(CustomCommand));
        let matches = build_cli(&features)
            .try_get_matches_from(["razer-cli", "auto"].iter().chain(args))
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        handle(laptop, sub, &features)
    }

    #[test]
    fn auto_json_reports_the_simulated_state() {
        let laptop = Simulation::new(model(&model_json(0x029f))).detect();
        let status = serde_json::to_value(json_status(&laptop).unwrap()).unwrap();
        assert_eq!(
            status,
            json!({
                "name": "Razer Blade 16 (2023) Black",
                "pid": "0x029f",
                "perf_mode": "Balanced",
                "cpu_boost": null,
                "gpu_boost": null,
                "fan_mode": "Auto",
                "fan_setpoint_rpm": null,
                "fan_actual_rpm": [2000, 1900],
                "fan_actual_trusted": true,
                "keyboard_brightness_percent": librazer::state::brightness_to_percent(128),
                "logo_mode": "Static",
                "keyboard_effect": "Spectrum",
                "battery_care_percent": 80,
                "max_fan": false,
            })
        );
        assert!(laptop.gaps().is_empty());
    }

    #[test]
    fn auto_info_reads_every_catalogued_feature() {
        let laptop = Simulation::new(model(&model_json(0x029f))).detect();
        auto(&laptop, &["info"]).unwrap();
        assert!(laptop.gaps().is_empty(), "{:?}", laptop.gaps());
    }

    #[test]
    fn a_perf_mode_set_reads_back() {
        let laptop = Simulation::new(model(&model_json(0x029f))).detect();
        auto(&laptop, &["perf", "mode", "silent"]).unwrap();
        assert_eq!(
            json_status(&laptop).unwrap().perf_mode.as_deref(),
            Some("Silent")
        );

        auto(&laptop, &["perf", "mode", "custom"]).unwrap();
        auto(&laptop, &["perf", "cpu", "boost"]).unwrap();
        auto(&laptop, &["fan", "max", "enable"]).unwrap();
        let status = json_status(&laptop).unwrap();
        assert_eq!(status.cpu_boost.as_deref(), Some("Boost"));
        assert_eq!(status.max_fan, Some(true));
    }

    #[test]
    fn a_fan_set_point_outside_the_catalogued_range_never_reaches_the_ec() {
        let laptop = Simulation::new(model(&model_json(0x029f))).detect();
        auto(&laptop, &["fan", "manual"]).unwrap();
        laptop.drain_exchanges();
        // 2000-5000 on 0x029F; 5500 parses (some Blade runs it) but this one would clamp.
        assert!(auto(&laptop, &["fan", "rpm", "5500"]).is_err());
        assert!(laptop.drain_exchanges().is_empty());
        auto(&laptop, &["fan", "rpm", "3000"]).unwrap();
        assert_eq!(json_status(&laptop).unwrap().fan_setpoint_rpm, Some(3000));
    }

    #[test]
    fn max_fan_answering_not_supported_exits_4_on_catalogued_and_generic_models() {
        let lacking = json!({
            "command": "0x070f", "support": "not_supported",
            "evidence": "inferred: the Max Fan question in issue #10"
        });
        for pid in [0x029f, 0x0eee] {
            let laptop =
                Simulation::new(model(&with_rule(model_json(pid), lacking.clone()))).detect();
            auto(&laptop, &["perf", "mode", "custom"]).unwrap();
            laptop.drain_exchanges();
            let err = auto(&laptop, &["fan", "max", "enable"]).unwrap_err();
            assert_eq!(ExitCode::classify(&err) as u8, 4, "{pid:#06x}: {err:#}");
            let tries = laptop
                .drain_exchanges()
                .iter()
                .filter(|e| e.command == 0x070f)
                .count();
            assert_eq!(tries, 1, "{pid:#06x}: a 0x05 is final");
        }
    }

    #[test]
    fn an_unmodeled_command_is_an_error_that_names_the_gap() {
        let laptop = Simulation::new(model(&without_rule(model_json(0x029f), "0x0f02"))).detect();
        let err = auto(&laptop, &["kbd-lighting", "effect", "wave"]).unwrap_err();
        assert!(
            format!("{err:#}").contains("has no observation for 0x0f02"),
            "{err:#}"
        );
        assert!(librazer::sim::unmodeled(&err).is_some());
        assert_eq!(ExitCode::classify(&err), ExitCode::Generic);
        assert_eq!(laptop.gaps().len(), 1);
    }

    #[test]
    fn json_tolerates_a_missing_effect_read_but_the_gap_is_kept() {
        let laptop = Simulation::new(model(&without_rule(model_json(0x029f), "0x0f82"))).detect();
        assert_eq!(json_status(&laptop).unwrap().keyboard_effect, None);
        let gaps = laptop.gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].command, 0x0f82);
    }

    #[test]
    fn an_uncatalogued_pid_gets_the_generic_profile_and_learns() {
        let lacking = json!({
            "command": "0x0792", "support": "not_supported",
            "evidence": "inferred: an EC without a charge limit"
        });
        let laptop = Simulation::new(model(&with_rule(model_json(0x0eee), lacking))).detect();
        assert_eq!(laptop.info.name, "Unrecognized Razer laptop (untested)");
        assert_eq!(laptop.info.features, feature::ALL_FEATURES);
        let status = json_status(&laptop).unwrap();
        assert_eq!(status.battery_care_percent, None);
        assert_eq!(status.unsupported, vec!["battery_care_percent"]);
        librazer::state::DeviceState::read(&laptop).unwrap();
        assert!(librazer::state::unsupported_on(&laptop)
            .contains(librazer::unsupported::Optional::BatteryCare));
    }

    #[test]
    fn manual_opens_only_the_models_pid() {
        let sim = Simulation::new(model(&model_json(0x029f)));
        let laptop = sim.open(manual_descriptor(0x029f)).unwrap();
        assert_eq!(laptop.info.name, "Unknown");
        assert!(laptop.unsupported().is_none(), "manual doesn't learn");
        let err = sim.open(manual_descriptor(0x0123)).err().unwrap();
        assert_eq!(ExitCode::classify(&err), ExitCode::NoDevice);
        assert_eq!(sim.enumerate(), (vec![0x029f], "RZ09-0483T".to_string()));
    }

    // ---- `--sim MODEL` on the command line ----

    #[cfg(feature = "sim")]
    mod sim_option {
        use super::*;

        /// The model as a file, removed when dropped.
        struct ModelFile(std::path::PathBuf);

        impl ModelFile {
            fn new(name: &str, value: &Value) -> ModelFile {
                let path = std::env::temp_dir()
                    .join(format!("razer-cli-test-{}-{name}.json", std::process::id()));
                std::fs::write(&path, value.to_string()).unwrap();
                ModelFile(path)
            }

            fn path(&self) -> &str {
                self.0.to_str().unwrap()
            }
        }

        impl Drop for ModelFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        fn run(args: &[&str]) -> Result<()> {
            real_main(
                std::iter::once("razer-cli")
                    .chain(args.iter().copied())
                    .map(OsString::from)
                    .collect(),
            )
        }

        fn os(args: &[&str]) -> Vec<OsString> {
            args.iter().map(OsString::from).collect()
        }

        #[test]
        fn it_is_read_before_or_after_the_subcommand() {
            for args in [
                &["razer-cli", "--sim", "no-such.json", "auto", "json"][..],
                &["razer-cli", "--sim=no-such.json", "auto", "json"],
                &["razer-cli", "auto", "json", "--sim", "no-such.json"],
            ] {
                assert!(parses(&args[1..]), "{args:?} should parse");
                let args = os(args);
                assert_eq!(laptop::subcommand(&args), Some("auto".as_ref()));
                let err = laptop::Backend::from_args(&args).err().unwrap();
                assert!(format!("{err:#}").contains("no-such.json"), "{err:#}");
            }
            let args = os(&[
                "razer-cli",
                "auto",
                "cmd",
                "0x0d82",
                "--",
                "--sim",
                "m.json",
            ]);
            assert!(matches!(
                laptop::Backend::from_args(&args),
                Ok(laptop::Backend::Hardware)
            ));
        }

        #[test]
        fn a_sim_option_without_a_usable_value_never_falls_back_to_hardware() {
            for args in [
                &["razer-cli", "auto", "json", "--sim"][..],
                &["razer-cli", "--sim=", "auto", "json"],
            ] {
                let err = laptop::Backend::from_args(&os(args)).err().unwrap();
                assert!(
                    format!("{err:#}").contains("--sim needs a model file"),
                    "{err:#}"
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                let mut args = os(&["razer-cli", "auto", "json", "--sim"]);
                args.push(OsString::from_vec(vec![0x6d, 0xff, 0x2e]));
                let err = laptop::Backend::from_args(&args).err().unwrap();
                assert!(format!("{err:#}").contains("not UTF-8"), "{err:#}");
            }
        }

        #[test]
        fn gaps_are_reported_once_each() {
            let laptop =
                Simulation::new(model(&without_rule(model_json(0x029f), "0x0f82"))).detect();
            json_status(&laptop).unwrap();
            json_status(&laptop).unwrap();
            let report = gap_report(Some(&laptop));
            assert_eq!(report.len(), 1, "{report:?}");
            assert!(
                report[0].contains("has no observation for 0x0f82"),
                "{report:?}"
            );
            assert!(gap_report(None).is_empty());
        }

        #[test]
        fn every_kind_of_subcommand_runs_against_the_model() {
            let file = ModelFile::new("all", &model_json(0x029f));
            let m = file.path();
            for args in [
                &["--sim", m, "auto", "json"][..],
                &["--sim", m, "auto", "info"],
                &["--sim", m, "auto", "perf", "mode", "performance"],
                &["--sim", m, "auto", "fan", "rpm", "3000"],
                &["--sim", m, "auto", "battery-care", "set", "60"],
                &["--sim", m, "auto", "lid-logo", "breathing"],
                &["--sim", m, "auto", "kbd-backlight", "200"],
                &["--sim", m, "auto", "kbd-lighting", "color", "#ff0000"],
                &["--sim", m, "manual", "--pid", "0x029f", "json"],
                &["--sim", m, "enumerate", "--json"],
            ] {
                run(args).unwrap_or_else(|e| panic!("{args:?}: {e:#}"));
            }
            let err = run(&["--sim", m, "manual", "--pid", "0x0123", "json"]).unwrap_err();
            assert_eq!(ExitCode::classify(&err), ExitCode::NoDevice);
            assert!(run(&["--sim", m, "taskkill"]).is_err());
        }

        #[test]
        fn max_fan_not_supported_exits_4() {
            let mut value = with_rule(
                model_json(0x029f),
                json!({"command": "0x070f", "support": "not_supported", "evidence": "inferred: test"}),
            );
            value["state"]["perf_mode"] = json!([4, 4]);
            let file = ModelFile::new("maxfan", &value);
            let err = run(&["--sim", file.path(), "auto", "fan", "max", "enable"]).unwrap_err();
            assert_eq!(ExitCode::classify(&err) as u8, 4, "{err:#}");
        }

        #[test]
        fn a_simulated_probe_says_it_is_simulated() {
            let file = ModelFile::new("probe", &model_json(0x029f));
            let out = std::env::temp_dir().join(format!(
                "razer-cli-test-{}-capture.json",
                std::process::id()
            ));
            run(&[
                "--sim",
                file.path(),
                "probe",
                "--out",
                out.to_str().unwrap(),
            ])
            .unwrap();
            let capture: Value =
                serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
            let _ = std::fs::remove_file(&out);
            assert!(
                capture["tool"]
                    .as_str()
                    .unwrap()
                    .contains("(simulated EC: CLI test)"),
                "{}",
                capture["tool"]
            );
            assert_eq!(capture["machine"]["pid"], "0x029f");
            assert_eq!(capture["machine"]["catalogued"], true);
            let steps = capture["steps"].as_array().unwrap();
            assert_eq!(
                steps.len(),
                librazer::probe::READS.len() + 2 * librazer::probe::FAN_ACTUAL_ROUNDS
            );
        }

        #[test]
        fn a_missing_model_file_fails_before_anything_runs() {
            let err = run(&["--sim", "/nonexistent/razer-model.json", "auto", "json"]).unwrap_err();
            assert!(format!("{err:#}").contains("reading EC model"), "{err:#}");
        }
    }
}
