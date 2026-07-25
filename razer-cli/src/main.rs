use librazer::command;
use librazer::device;
use librazer::feature;
use librazer::types::{
    BatteryCare, CpuBoost, FanMode, FanZone, GpuBoost, KeyboardEffect, LightsAlwaysOn, LogoMode,
    MaxFanSpeedMode, PerfMode,
};

use librazer::feature::Feature;

use anyhow::Result;
use clap::{arg, Command};
use std::process::Command as procCommand;
use sysinfo::{ProcessExt, Signal, System, SystemExt};

trait Cli: feature::Feature {
    fn cmd(&self) -> Option<Command> {
        None
    }
    fn handle(&self, _device: &device::Device, _matches: &clap::ArgMatches) -> Result<()> {
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
            fn handle(&self, device: &device::Device, matches: &clap::ArgMatches) -> Result<()> {
                match matches.subcommand() {
                    Some((ident, matches)) if ident == self.name() => {
                        let arg = matches.get_one::<$arg_type>("ARG").unwrap();
                        $setter(device, *arg)
                    }
                    Some(("info", _)) => Ok(println!("{}: {:?}", self.name(), $getter(device))),
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

    fn handle(&self, device: &device::Device, matches: &clap::ArgMatches) -> Result<()> {
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
                .about("Control keyboard RGB backlight effect (write-only; not read back)")
                .subcommand(
                    clap::Command::new("effect")
                        .about("Set the backlight effect (off/spectrum/wave/breathing)")
                        .arg(
                            arg!(<EFFECT> "Effect")
                                .value_parser(clap::value_parser!(KeyboardEffect)),
                        )
                        .arg_required_else_help(true),
                )
                .arg_required_else_help(true),
        )
    }

    fn handle(&self, device: &device::Device, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, sub)) if ident == self.name() => match sub.subcommand() {
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
    fn handle(&self, device: &device::Device, matches: &clap::ArgMatches) -> Result<()> {
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
                .subcommand(impl_unary_cmd_cli!{{clap::value_parser!(u16).range(librazer::state::FAN_RPM_MIN_ANY as i64..=librazer::state::FAN_RPM_MAX_ANY as i64)}, "rpm", "RPM", "Set fan rpm", "Fan RPM (chassis-dependent; the EC clamps values outside this model's real range)"})
                .subcommand(impl_unary_cmd_cli!{{clap::value_parser!(MaxFanSpeedMode)}, "max", "MAX", "Control Max Fan Speed Mode", "Max Fan Speed Mode"})
                .arg_required_else_help(true),
        )
    }

    fn handle(&self, device: &device::Device, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, matches)) if ident == self.name() => {
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
                        println!(
                            "Fan set to: {:?}@{:?} RPM",
                            fan_mode,
                            command::get_fan_rpm(device, FanZone::Zone1)
                        )
                    }
                    Err(e) => println!("{}", e),
                };
                println!(
                    "Fan actual: {:?} RPM",
                    command::get_fan_actual_rpm(device, FanZone::Zone1)
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

    fn handle(&self, device: &device::Device, matches: &clap::ArgMatches) -> Result<()> {
        match matches.subcommand() {
            Some((ident, matches)) if ident == self.name() => {
                impl_unary_handle_cli! {<PerfMode>(matches, device, "mode", "MODE", command::set_perf_mode)}
                impl_unary_handle_cli! {<CpuBoost>(matches, device, "cpu", "CPU", command::set_cpu_boost)}
                impl_unary_handle_cli! {<GpuBoost>(matches, device, "gpu", "GPU", command::set_gpu_boost)}
                Ok(())
            }
            Some(("info", _)) => {
                let perf_mode = command::get_perf_mode(device);
                println!("Performance: {:?}", perf_mode);
                if let Ok((PerfMode::Custom, _)) = perf_mode {
                    let cpu_boost = command::get_cpu_boost(device);
                    let gpu_boost = command::get_gpu_boost(device);
                    println!("CPU: {:?}", cpu_boost);
                    println!("GPU: {:?}", gpu_boost);

                    if let (Ok(CpuBoost::Boost) | Ok(CpuBoost::Undervolt), Ok(GpuBoost::High)) =
                        (cpu_boost, gpu_boost)
                    {
                        println!(
                            "Max Fan Speed: {:?}",
                            command::get_max_fan_speed_mode(device)
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

fn enumerate(as_json: bool) -> Result<()> {
    let (pid_list, model_number_prefix) = device::Device::enumerate()?;
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

/// Machine-readable device state, one JSON object per line-free block. Kept as a flat,
/// stable shape (no nested enum tuples) so downstream consumers -- a Home Assistant
/// command-line sensor, a status line -- can parse it without knowing Rust's serde
/// enum encoding. Mirrors what `perf/fan/... info` prints, gathered in one read.
#[derive(serde::Serialize)]
struct JsonStatus {
    name: &'static str,
    pid: String,
    perf_mode: &'static str,
    cpu_boost: Option<String>,
    gpu_boost: Option<String>,
    fan_mode: &'static str,
    fan_setpoint_rpm: Option<u16>,
    fan_actual_rpm: [u16; 2],
    keyboard_brightness_percent: u8,
    logo_mode: String,
    /// Keyboard backlight effect, read back from the EC (0x0f82). `null` when the device
    /// reports an effect we don't model (e.g. a Synapse-set Static/Reactive).
    keyboard_effect: Option<String>,
    battery_care_percent: u8,
    max_fan: bool,
}

fn print_json(device: &device::Device) -> Result<()> {
    use librazer::state::{brightness_to_percent, DeviceState, FanSpeed, PerfMode};

    let s = DeviceState::read(device)?;
    let fan = librazer::state::get_fan_rpm(device)?;

    let (perf_mode, cpu_boost, gpu_boost) = match s.perf_mode {
        PerfMode::Battery => ("Battery", None, None),
        PerfMode::Silent => ("Silent", None, None),
        PerfMode::Balanced => ("Balanced", None, None),
        PerfMode::Performance => ("Performance", None, None),
        PerfMode::Hyperboost => ("Hyperboost", None, None),
        PerfMode::Custom(cpu, gpu) => (
            "Custom",
            Some(format!("{:?}", cpu)),
            Some(format!("{:?}", gpu)),
        ),
    };

    let (fan_mode, fan_setpoint_rpm) = match s.fan_speed {
        FanSpeed::Auto => ("Auto", None),
        FanSpeed::Manual(rpm) => ("Manual", Some(rpm)),
    };

    let status = JsonStatus {
        name: device.info.name,
        pid: format!("{:#06x}", device.info.pid),
        perf_mode,
        cpu_boost,
        gpu_boost,
        fan_mode,
        fan_setpoint_rpm,
        fan_actual_rpm: [fan.fan1, fan.fan2],
        keyboard_brightness_percent: brightness_to_percent(s.lights_mode.keyboard_brightness),
        logo_mode: format!("{:?}", s.lights_mode.logo_mode),
        keyboard_effect: s.lights_mode.keyboard_effect.map(|e| format!("{e:?}")),
        battery_care_percent: s.battery_care.to_percent(),
        max_fan: s.max_fan,
    };

    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
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

fn handle(
    device: &device::Device,
    matches: &clap::ArgMatches,
    features: &Vec<Box<dyn Cli>>,
) -> Result<()> {
    if let Some(("info", _)) = matches.subcommand() {
        println!("Device: {:?}", device.info);
    }

    if let Some(("json", _)) = matches.subcommand() {
        return print_json(device);
    }

    for f in features {
        f.handle(device, matches)?;
    }
    Ok(())
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
    match real_main() {
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

fn real_main() -> Result<()> {
    env_logger::init();

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

    // TODO: find a better way to detect auto mode in advance
    let is_auto_mode = std::env::args_os().nth(1) == Some("auto".into());
    let device = match is_auto_mode {
        true => Some(device::Device::detect()?),
        _ => None,
    };
    let feature_list = match device {
        Some(ref device) => device.info.features,
        _ => feature::ALL_FEATURES,
    };

    let mut cli_features: Vec<Box<dyn Cli>> = gen_cli_features(feature_list);
    cli_features.push(Box::new(CustomCommand));

    let cmd = clap::command!()
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
        .subcommand(update_cmd(auto_cmd, &cli_features))
        .subcommand(update_cmd(manual_cmd, &cli_features))
        .subcommand(
            clap::Command::new("enumerate")
                .about("List discovered Razer devices")
                .arg(
                    arg!(--json "Print as JSON (this is what a device-support issue needs)")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(clap::Command::new("taskkill").about("Terminate all processes using dGPU"));

    let matches = cmd.get_matches();

    match matches.subcommand() {
        Some(("enumerate", submatches)) => {
            enumerate(submatches.get_flag("json"))?;
        }
        Some(("taskkill", _)) => {
            taskkill()?;
        }
        Some(("auto", submatches)) => {
            // Set above when the first argument is `auto`; the expect documents that
            // coupling rather than leaving a bare unwrap for a future reader to audit.
            let device = device
                .as_ref()
                .expect("auto mode detects the device before parsing arguments");
            handle(device, submatches, &cli_features)?;
        }
        Some(("manual", submatches)) => {
            let device = device::Device::new(librazer::descriptor::Descriptor {
                model_number_prefix: "Unknown",
                name: "Unknown",
                pid: *submatches.get_one::<u16>("pid").unwrap(),
                features: feature::ALL_FEATURES,
                init_cmds: &[],
                fan_rpm_range: (2200, 5000), // unknown chassis: safe modern-Blade default; EC clamps
            })?;
            handle(&device, submatches, &cli_features)?;
        }
        // clap enforces `subcommand_required`, so neither of these should be reachable --
        // but they used to be `unimplemented!()`/`unreachable!()`, i.e. a panic with a
        // backtrace in a user's terminal if that assumption ever broke. An error costs
        // nothing and degrades politely.
        Some((cmd, _)) => anyhow::bail!("unhandled subcommand: {cmd}"),
        None => anyhow::bail!("no subcommand given (try --help)"),
    };

    Ok(())
}
