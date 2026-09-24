//! OS-specific glue, gathered behind one door: power-state detection, dGPU process
//! termination, autostart registration, the keyboard hook + idle signal, the
//! display-state monitor, and process efficiency hints. Everything Windows- or
//! Linux-only lives here so the rest of the crate reads as portable logic.

use anyhow::Result;
use std::process::Command as procCommand;
use sysinfo::{ProcessExt, Signal, System, SystemExt};

#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};

/// Tracks whether the console display is powered on. Updated by the power-setting
/// notification handler; read by the event loop to gate the firmware always-on
/// flag. Starts true (fail-open: keyboard stays lit if we never hear otherwise).
#[cfg(target_os = "windows")]
pub static DISPLAY_ON: AtomicBool = AtomicBool::new(true);

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    IDLE_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
};

/// Whether the machine is on AC. `previous` is returned when Windows cannot say.
///
/// `ACLineStatus` is 0 (battery), 1 (AC) or 255 (unknown). Unknown, and a failed call, used
/// to count as AC, which could put an AC performance profile on a machine running on
/// battery. Keeping the last known answer is the only choice that can't flip the profile
/// on a non-answer.
#[cfg(target_os = "windows")]
pub fn get_power_state(previous: bool) -> bool {
    // SAFETY: `status` is a fully-default-initialized SYSTEM_POWER_STATUS; we pass a
    // valid &mut to it and GetSystemPowerStatus only writes through that pointer.
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        match GetSystemPowerStatus(&mut status) {
            Ok(()) => match status.ACLineStatus {
                0 => false,
                1 => true,
                other => {
                    log::debug!("ACLineStatus {other} (unknown); keeping {previous}");
                    previous
                }
            },
            Err(e) => {
                log::warn!("Failed to get power status: {:?}", e);
                previous
            }
        }
    }
}

/// Battery charge and whether it is charging, for the keyboard's battery bar. `None` when
/// Windows cannot say (no battery, or an unknown percent).
#[cfg(target_os = "windows")]
pub fn battery_level() -> Option<librazer::keyboard::BatteryLevel> {
    // SAFETY: as in `get_power_state`.
    let status = unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        GetSystemPowerStatus(&mut status).ok()?;
        status
    };
    // BatteryFlag 128 = no system battery, 255 = unknown; bit 8 = charging.
    if status.BatteryLifePercent > 100 || status.BatteryFlag == 128 || status.BatteryFlag == 255 {
        return None;
    }
    Some(librazer::keyboard::BatteryLevel {
        percent: status.BatteryLifePercent,
        charging: status.BatteryFlag & 8 != 0,
    })
}

#[cfg(not(target_os = "windows"))]
pub fn battery_level() -> Option<librazer::keyboard::BatteryLevel> {
    None
}

#[cfg(target_os = "linux")]
pub fn get_power_state(previous: bool) -> bool {
    linux_power_state().unwrap_or(previous)
}

#[cfg(target_os = "linux")]
fn linux_power_state() -> Result<bool> {
    // Try AC adapter first
    if let Ok(online) = std::fs::read_to_string("/sys/class/power_supply/AC/online")
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/AC0/online"))
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/ACAD/online"))
    {
        return Ok(online.trim() == "1");
    }

    // Fallback: check battery status
    if let Ok(status) = std::fs::read_to_string("/sys/class/power_supply/BAT0/status")
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/BAT1/status"))
    {
        let status = status.trim();
        return Ok(status == "Charging" || status == "Full" || status == "Not charging");
    }

    anyhow::bail!("could not detect the power state")
}

#[cfg(target_os = "windows")]
const AUTOSTART_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
const AUTOSTART_VALUE_NAME: &str = "razer-tray";

/// Whether razer-tray is registered to launch at login (HKCU Run key).
#[cfg(target_os = "windows")]
pub fn autostart_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(AUTOSTART_RUN_KEY)
        .and_then(|run| run.get_value::<String, _>(AUTOSTART_VALUE_NAME))
        .is_ok()
}

/// Register/unregister razer-tray for launch at login via the per-user Run key.
#[cfg(target_os = "windows")]
pub fn set_autostart(enable: bool) -> Result<()> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(AUTOSTART_RUN_KEY)?;
    if enable {
        let exe = std::env::current_exe()?;
        // Quote the path so a space in it doesn't break the command.
        run.set_value(AUTOSTART_VALUE_NAME, &format!("\"{}\"", exe.display()))?;
        log::info!("Autostart enabled: {}", exe.display());
    } else {
        let _ = run.delete_value(AUTOSTART_VALUE_NAME); // ignore "not present"
        log::info!("Autostart disabled");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn gpu_taskkill() -> Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = match procCommand::new("nvidia-smi")
        .args(["--query-compute-apps=name,pid", "--format=csv,noheader"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            // No NVIDIA tools on PATH (or it failed to launch). Nothing to terminate;
            // don't panic -- this runs from the tray event loop and a panic here would
            // crash/recover-churn the app the moment the user clicks the menu item.
            log::info!("nvidia-smi not available ({e}); skipping dGPU terminate");
            return Ok(());
        }
    };

    if !output.status.success() {
        log::info!("nvidia-smi command failed or no GPU processes found");
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines();

    let mut pids_to_kill = Vec::new();

    for line in lines {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            continue;
        }

        let name = parts[0];
        let pid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Skip the compositor/shell (on this hardware nvidia-smi lists dwm/explorer/
        // shell hosts as GPU users) and any process whose name nvidia-smi couldn't
        // read -- it emits a bracketed placeholder like "[Insufficient Permissions]"
        // for protected/elevated processes, and killing a PID whose name we can't
        // even read is unsafe.
        let unreadable =
            name.starts_with('[') || name.eq_ignore_ascii_case("Insufficient Permissions");
        if unreadable || librazer::process_guard::is_protected_process(name) {
            log::info!("Skipping protected/unreadable process: {} ({})", pid, name);
        } else {
            pids_to_kill.push((pid, name.to_string()));
        }
    }

    if pids_to_kill.is_empty() {
        log::info!("No GPU-using processes to kill.");
        return Ok(());
    }

    let mut sys = System::new_all();
    sys.refresh_processes();

    for (pid, name) in pids_to_kill {
        if let Some(process) = sys.process(sysinfo::Pid::from(pid as usize)) {
            // Defense in depth: trust the OS-resolved name, not nvidia-smi's. The
            // OS can read names (e.g. "dwm.exe") that nvidia-smi reports only as
            // "[Insufficient Permissions]", so this catches protected processes the
            // parse-loop guard above couldn't identify by name.
            let real_name = process.name();
            if librazer::process_guard::is_protected_process(real_name) {
                log::info!("Skipping protected process: {} ({})", pid, real_name);
                continue;
            }
            log::info!("Attempting to kill process {} ({})", pid, name);
            if process.kill_with(Signal::Kill).unwrap_or(false) {
                log::info!("Successfully killed PID {}", pid);
            } else {
                log::info!("Failed to kill PID {}", pid);
            }
        } else {
            log::info!("Process with PID {} not found", pid);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
pub fn gpu_taskkill() -> Result<()> {
    // dGPU process termination for Linux
    let output = procCommand::new("nvidia-smi")
        .args(["--query-compute-apps=name,pid", "--format=csv,noheader"])
        .output();

    if output.is_err() {
        log::info!("nvidia-smi not found or no GPU processes");
        return Ok(());
    }

    let output = output?;
    if !output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut system = System::new_all();
    system.refresh_all();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            continue;
        }

        let pid: usize = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if let Some(process) = system.process(sysinfo::Pid::from(pid)) {
            log::info!("Terminating GPU process: {} (PID: {})", parts[0], pid);
            process.kill_with(Signal::Term);
        }
    }

    Ok(())
}

/// Returns the system-wide "last input" tick (keyboard + mouse) from
/// GetLastInputInfo, or None if it can't be read / on non-Windows. The Mirror
/// refresh polls only when this value *changes* (new input since the last poll),
/// so reads happen while you're actively using the machine -- including moving the
/// trackpad to reach the tray -- but stop the instant you stop touching it. That
/// matters because the keyboard firmware re-brightens the backlight on ANY HID
/// activity (including our reads); gating on real input keeps us from re-poking an
/// idle, dimming keyboard, so it dims off normally. (The trackpad movement that
/// brings you to the tray already woke the backlight, so the hover read is free.)
#[cfg(target_os = "windows")]
pub fn last_input_tick() -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    // SAFETY: `info` is initialized with the cbSize the API contract requires; we pass
    // a valid &mut and only read dwTime back after a successful (TRUE) return.
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info).as_bool() {
            Some(info.dwTime)
        } else {
            None // can't determine -> caller treats as "always refresh"
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn last_input_tick() -> Option<u32> {
    // No cheap, portable idle signal here; returning None makes the caller fall
    // back to a plain timed refresh (the original always-refresh behavior).
    None
}

/// Latest dGPU telemetry, published by [`spawn_gpu_telemetry_monitor`] and read by the
/// tooltip. Packed into atomics so the UI thread never blocks on a subprocess.
///
/// [`GPU_UNAVAILABLE`] means "no reading": no NVIDIA tools, no dGPU, or the query failed.
/// The tooltip omits the fields entirely in that case rather than showing a zero, because
/// "0 °C" reads as a measurement and would be a lie.
#[cfg(target_os = "windows")]
pub static GPU_TEMP_C: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(GPU_UNAVAILABLE);
/// dGPU board power in centiwatts (so one decimal survives an integer atomic).
#[cfg(target_os = "windows")]
pub static GPU_POWER_CW: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(GPU_UNAVAILABLE);

/// Sentinel for "we have no valid reading".
///
/// Windows-gated like every use of it: the telemetry atomics, `gpu_telemetry`, and the
/// monitor thread are all Windows-only, so an ungated constant here is dead code on
/// Linux -- which `-D warnings` in CI correctly rejects.
#[cfg(target_os = "windows")]
pub const GPU_UNAVAILABLE: u32 = u32::MAX;

/// How often to sample the dGPU. Slow on purpose: this is a tooltip garnish, and each
/// sample costs an `nvidia-smi` process (~60-80 ms measured on the Blade 16 2023).
#[cfg(target_os = "windows")]
const GPU_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Consecutive failed samples tolerated before the monitor gives up for the session.
///
/// At [`GPU_POLL_INTERVAL`] this is about a minute of grace, which covers a driver that
/// isn't ready yet at login. The cost on a machine that genuinely has no NVIDIA tools is
/// this many short-lived failed spawns, once per session.
#[cfg(target_os = "windows")]
const GPU_MAX_CONSECUTIVE_FAILURES: u32 = 12;

/// Current dGPU temperature (°C) and board power (watts), if a reading is available.
///
/// `watts` is 0.0 when the temperature is known but power isn't (some GPUs don't report
/// `power.draw`); callers should treat that as "no power figure" rather than "0 W".
#[cfg(target_os = "windows")]
pub fn gpu_telemetry() -> Option<(u32, f32)> {
    use std::sync::atomic::Ordering;
    let temp = GPU_TEMP_C.load(Ordering::Relaxed);
    if temp == GPU_UNAVAILABLE {
        return None;
    }
    let watts = match GPU_POWER_CW.load(Ordering::Relaxed) {
        GPU_UNAVAILABLE => 0.0,
        cw => cw as f32 / 100.0,
    };
    Some((temp, watts))
}

/// No dGPU telemetry source wired up off Windows, so the tooltip omits those fields.
#[cfg(not(target_os = "windows"))]
pub fn gpu_telemetry() -> Option<(u32, f32)> {
    None
}

/// Whether the NVIDIA dGPU is powered down (D3), from the power state Windows keeps for
/// the device. Reading it asks the PnP manager, not the GPU, so it can't wake the GPU;
/// `nvidia-smi` can. None if there is no NVIDIA display adapter or the state can't be read.
#[cfg(target_os = "windows")]
fn nvidia_gpu_asleep() -> Option<bool> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW,
        CM_Locate_DevNodeW, CM_GETIDLIST_FILTER_CLASS, CM_GETIDLIST_FILTER_PRESENT,
        CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
    };
    use windows::Win32::Devices::Properties::{DEVPKEY_Device_PowerData, DEVPROPTYPE};

    // The Display adapter setup class.
    let class = w!("{4d36e968-e325-11ce-bfc1-08002be10318}");
    let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
    // SAFETY: plain Configuration Manager calls with buffers sized as the API reports;
    // the returned list is NUL-separated wide strings ending in an empty one.
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut len, class, flags) != CR_SUCCESS || len == 0 {
            return None;
        }
        let mut list = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(class, &mut list, flags) != CR_SUCCESS {
            return None;
        }
        for id in list.split(|&c| c == 0).filter(|id| !id.is_empty()) {
            if !String::from_utf16_lossy(id)
                .to_ascii_uppercase()
                .starts_with("PCI\\VEN_10DE")
            {
                continue;
            }
            let mut id_z = id.to_vec();
            id_z.push(0);
            let mut devinst = 0u32;
            if CM_Locate_DevNodeW(
                &mut devinst,
                PCWSTR(id_z.as_ptr()),
                CM_LOCATE_DEVNODE_NORMAL,
            ) != CR_SUCCESS
            {
                continue;
            }
            // CM_POWER_DATA: u32 size, then the most recent DEVICE_POWER_STATE
            // (1 = D0 ... 4 = D3).
            let mut data = [0u8; 64];
            let mut size = data.len() as u32;
            let mut kind = DEVPROPTYPE::default();
            if CM_Get_DevNode_PropertyW(
                devinst,
                &DEVPKEY_Device_PowerData,
                &mut kind,
                Some(data.as_mut_ptr()),
                &mut size,
                0,
            ) != CR_SUCCESS
                || size < 8
            {
                continue;
            }
            let state = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
            return Some(state >= 4);
        }
        None
    }
}

/// Poll dGPU temperature and power on a background thread.
///
/// Deliberately a *subprocess* (`nvidia-smi`) rather than FFI into `nvml.dll`, even though
/// the DLL is present in System32 on this machine. Getting an NVML signature subtly wrong
/// is a crash in the user's tray, and this is decoration -- a slow, safe, obviously-correct
/// query on its own thread costs nothing on the UI path. FFI is the natural optimization if
/// the sample rate ever needs to be high.
///
/// Fails open and silent: a machine with no NVIDIA tools, or no dGPU, simply never gets a
/// reading and the tooltip omits those fields.
///
/// A powered-down dGPU is left alone: `nvidia-smi` would wake it, which on battery would
/// cost more than the reading is worth. The fields drop out of the tooltip until the GPU
/// is awake for some other reason. (Untested on a sleeping GPU: the Blade here runs in
/// dGPU-only mode, where the GPU never sleeps.)
///
/// It tolerates [`GPU_MAX_CONSECUTIVE_FAILURES`] failures before giving up, rather than
/// quitting on the first. The tray starts at login, and that is exactly when the NVIDIA
/// driver is least likely to be ready -- a cold-booted or resumed machine can answer
/// "couldn't communicate with the NVIDIA driver" for several seconds. Quitting on the
/// first sample turned a transient startup condition into a permanently empty tooltip
/// for the whole session.
#[cfg(target_os = "windows")]
pub fn spawn_gpu_telemetry_monitor() {
    use std::os::windows::process::CommandExt;
    use std::sync::atomic::Ordering;

    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut failures = 0u32;
    let mut was_asleep = false;
    std::thread::spawn(move || loop {
        let state = nvidia_gpu_asleep();
        static FIRST_STATE: std::sync::Once = std::sync::Once::new();
        FIRST_STATE.call_once(|| log::info!("dGPU power state: asleep = {state:?}"));
        let asleep = state == Some(true);
        if asleep != was_asleep {
            was_asleep = asleep;
            log::info!(
                "dGPU {}",
                if asleep {
                    "powered down; pausing telemetry"
                } else {
                    "awake; resuming telemetry"
                }
            );
        }
        if asleep {
            GPU_TEMP_C.store(GPU_UNAVAILABLE, Ordering::Relaxed);
            GPU_POWER_CW.store(GPU_UNAVAILABLE, Ordering::Relaxed);
            std::thread::sleep(GPU_POLL_INTERVAL);
            continue;
        }
        let output = procCommand::new("nvidia-smi")
            .args([
                "--query-gpu=temperature.gpu,power.draw",
                "--format=csv,noheader,nounits",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        let parsed = match &output {
            Ok(o) if o.status.success() => {
                // "54, 28.33" -- nounits keeps it to bare numbers.
                let text = String::from_utf8_lossy(&o.stdout);
                let first = text.lines().next().unwrap_or_default().to_string();
                let mut fields = first.split(',').map(str::trim);
                let temp = fields.next().and_then(|t| t.parse::<u32>().ok());
                let watts = fields.next().and_then(|w| w.parse::<f32>().ok());
                temp.map(|t| (t, watts))
            }
            _ => None,
        };

        match parsed {
            Some((temp, watts)) => {
                failures = 0;
                // Log the first good sample. Without this, "the tooltip shows no GPU
                // fields" is undiagnosable from a log: success was previously silent, so
                // an absent reading and a working one looked identical.
                static FIRST_SAMPLE: std::sync::Once = std::sync::Once::new();
                FIRST_SAMPLE.call_once(|| {
                    log::info!("dGPU telemetry: first sample {temp} C, {watts:?} W");
                });
                GPU_TEMP_C.store(temp, Ordering::Relaxed);
                GPU_POWER_CW.store(
                    watts
                        .map(|w| (w * 100.0).round() as u32)
                        .unwrap_or(GPU_UNAVAILABLE),
                    Ordering::Relaxed,
                );
            }
            None => {
                failures += 1;
                // Report why, once, with the tool's own words -- "no dGPU" and "driver
                // not ready yet" are very different for someone filing a bug.
                if failures == 1 {
                    let detail = match &output {
                        Ok(o) => format!(
                            "exit {:?}, stdout {:?}, stderr {:?}",
                            o.status.code(),
                            String::from_utf8_lossy(&o.stdout).trim(),
                            String::from_utf8_lossy(&o.stderr).trim()
                        ),
                        Err(e) => format!("could not run nvidia-smi: {e}"),
                    };
                    log::info!("dGPU telemetry attempt failed ({detail}); will retry");
                }
                if failures >= GPU_MAX_CONSECUTIVE_FAILURES {
                    // Give up rather than respawning a doomed process forever. Values go
                    // to the unavailable sentinel, so the tooltip just omits the fields.
                    log::info!(
                        "dGPU telemetry unavailable after {failures} attempts; not polling further"
                    );
                    GPU_TEMP_C.store(GPU_UNAVAILABLE, Ordering::Relaxed);
                    GPU_POWER_CW.store(GPU_UNAVAILABLE, Ordering::Relaxed);
                    return;
                }
                // Leave any previous reading in place: a transient failure shouldn't blank
                // a field that was working a few seconds ago.
            }
        }

        std::thread::sleep(GPU_POLL_INTERVAL);
    });
}

/// What woke the machine, as far as the tray can tell. See [`take_wake`].
// Only the Windows power thread constructs these.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WakeSource {
    /// A `PBT_APMRESUME*` message. On this Blade it arrives for some resumes and not
    /// others (four hibernate-path resumes in September logged none).
    Resume,
    /// The console display came back on after being off for at least
    /// [`WAKE_DISPLAY_OFF_MIN`]. This is the signal Modern Standby reliably produces: the
    /// Blade sleeps in S0 Low Power Idle, and its standby exits never sent a resume message.
    /// It also fires for a plain screen timeout, which is why a wake only triggers a read,
    /// and writes only on measured drift.
    DisplayOn,
}

impl std::fmt::Display for WakeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WakeSource::Resume => "resume broadcast",
            WakeSource::DisplayOn => "display on after standby/off",
        })
    }
}

/// How long the display must have been off before "display on" counts as a wake. Short
/// enough to catch a lid-close standby, long enough to ignore a dim/undim.
#[cfg(target_os = "windows")]
pub const WAKE_DISPLAY_OFF_MIN_MS: u64 = 60_000;

/// 0 = none, 1 = [`WakeSource::Resume`], 2 = [`WakeSource::DisplayOn`]. A resume wins
/// over a display-on if both land before the loop looks.
#[cfg(target_os = "windows")]
static WAKE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// `GetTickCount64` at the last display-off, or 0 while the display is on.
#[cfg(target_os = "windows")]
static DISPLAY_OFF_AT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The pending wake signal, if any, clearing it.
#[cfg(target_os = "windows")]
pub fn take_wake() -> Option<WakeSource> {
    match WAKE.swap(0, Ordering::Relaxed) {
        1 => Some(WakeSource::Resume),
        2 => Some(WakeSource::DisplayOn),
        _ => None,
    }
}

/// No OS wake notification wired up off Windows.
#[cfg(not(target_os = "windows"))]
pub fn take_wake() -> Option<WakeSource> {
    None
}

/// Set when Windows tells us the system has resumed from sleep.
///
/// This replaces a tick-gap heuristic: the loop ran once a second, and a gap over 30 s
/// was read as "we must have been suspended". That was wrong in both directions. The
/// tray runs at `IDLE_PRIORITY_CLASS` with EcoQoS throttling (see `efficiency_mode`),
/// so a busy machine can starve it far longer than the threshold while wide awake --
/// observed 2026-07-25 on the Blade: a **54.9 s** gap logged as "resume detected" with
/// no sleep involved, firing a spurious re-assert. In the other direction a short sleep
/// could go unnoticed. `PBT_APMRESUMESUSPEND` is the OS telling us directly, so it is
/// both precise and free.
/// Now it feeds [`WAKE`] together with the display-state signal.
#[cfg(target_os = "windows")]
fn signal_resume() {
    WAKE.store(1, Ordering::Relaxed);
}

/// Window procedure for the hidden message-only window that receives power
/// notifications. Updates DISPLAY_ON from GUID_CONSOLE_DISPLAY_STATE events and
/// RESUMED from system suspend/resume broadcasts.
#[cfg(target_os = "windows")]
unsafe extern "system" fn power_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::System::Power::POWERBROADCAST_SETTING;
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
        PBT_APMSUSPEND, PBT_POWERSETTINGCHANGE, WM_POWERBROADCAST,
    };

    if msg == windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY {
        HOTKEY_PRESSED.store(true, Ordering::Relaxed);
        return windows::Win32::Foundation::LRESULT(0);
    }
    if msg == WM_POWERBROADCAST {
        match wparam.0 as u32 {
            PBT_POWERSETTINGCHANGE => {
                // SAFETY: for a PBT_POWERSETTINGCHANGE message Windows guarantees lparam points
                // to a POWERBROADCAST_SETTING valid for the duration of this call; we only read it.
                let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
                if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
                    // Data[0]: 0 = off, 1 = on, 2 = dimmed. Treat dimmed as on.
                    let on = setting.Data[0] != 0;
                    let was_on = DISPLAY_ON.swap(on, Ordering::Relaxed);
                    let now = GetTickCount64();
                    if on {
                        let off_at = DISPLAY_OFF_AT_MS.swap(0, Ordering::Relaxed);
                        let off_ms = if off_at == 0 {
                            0
                        } else {
                            now.saturating_sub(off_at)
                        };
                        log::info!("console display state: on (off {} s)", off_ms / 1000);
                        if !was_on && off_ms >= WAKE_DISPLAY_OFF_MIN_MS {
                            // Don't downgrade a pending Resume.
                            let _ =
                                WAKE.compare_exchange(0, 2, Ordering::Relaxed, Ordering::Relaxed);
                        }
                    } else {
                        if was_on {
                            DISPLAY_OFF_AT_MS.store(now.max(1), Ordering::Relaxed);
                        }
                        log::info!("console display state: off");
                    }
                }
                return windows::Win32::Foundation::LRESULT(1);
            }
            // Both arrive on wake: RESUMEAUTOMATIC always, RESUMESUSPEND additionally
            // when the resume was user-initiated. Treating either as the signal (and
            // latching a bool rather than counting) means the pair collapses into one
            // re-assert.
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                signal_resume();
                log::info!(
                    "system resume broadcast received ({})",
                    if wparam.0 as u32 == PBT_APMRESUMEAUTOMATIC {
                        "automatic"
                    } else {
                        "suspend"
                    }
                );
                return windows::Win32::Foundation::LRESULT(1);
            }
            // Logged, not acted on: which messages this machine sends around Modern Standby
            // is exactly what has not been measured, and the log is the instrument.
            PBT_APMSUSPEND => {
                log::info!("system suspend broadcast received");
                return windows::Win32::Foundation::LRESULT(1);
            }
            PBT_APMPOWERSTATUSCHANGE => {}
            other => log::info!("power broadcast {other:#x}"),
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Spawn a background thread that owns a hidden message-only window, registers
/// for console-display-state power notifications, and pumps messages. This is the
/// event-driven (zero-poll) source of truth for DISPLAY_ON. Errors are logged and
/// the thread exits, leaving DISPLAY_ON at its fail-open default of true.
#[cfg(target_os = "windows")]
pub fn spawn_display_state_monitor() {
    // SAFETY: a self-contained Win32 message-only-window setup on its own thread. The
    // window class name is 'static; the class/window outlive the message loop below;
    // power_wnd_proc is a valid extern "system" proc. The thread blocks in GetMessageW.
    std::thread::spawn(|| unsafe {
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Power::{
            RegisterPowerSettingNotification, RegisterSuspendResumeNotification,
        };
        use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassW, TranslateMessage,
            DEVICE_NOTIFY_WINDOW_HANDLE, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
            WNDCLASSW,
        };

        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(e) => {
                log::warn!("display monitor: GetModuleHandleW failed: {e:?}");
                return;
            }
        };
        let class_name = w!("razer_tray_power_window");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(power_wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            log::warn!("display monitor: RegisterClassW failed");
            return;
        }

        let hwnd: HWND = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("razer-tray power"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            hinstance,
            None,
        );
        if hwnd.0 == 0 {
            log::warn!("display monitor: CreateWindowExW returned null");
            return;
        }

        // Two SEPARATE registrations, because they are two different mechanisms and only one
        // of them was ever here.
        //
        // PBT_APMRESUMEAUTOMATIC / PBT_APMRESUMESUSPEND arrive as a WM_POWERBROADCAST
        // *broadcast*, and a message-only window (HWND_MESSAGE, above) does not receive
        // broadcasts -- that is the documented point of one. Registering for a power SETTING
        // establishes delivery of that setting and nothing else, so the resume arm of
        // power_wnd_proc was unreachable from the day it was written.
        //
        // This was not theory: the tray log's last `resume detected` line is 2026-07-25,
        // the day the tick-gap heuristic was replaced by the broadcast, and Windows recorded
        // nine Kernel-Power 107 resumes after that with no handler firing.
        //
        // RegisterSuspendResumeNotification is the fix: it is a TARGETED registration, so the
        // messages are delivered to this window specifically rather than broadcast to
        // top-level windows, which is what makes it work on a message-only window.
        let mut have_display = false;
        let mut have_resume = false;

        if let Err(e) = RegisterPowerSettingNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            log::warn!("display monitor: RegisterPowerSettingNotification failed: {e:?}");
        } else {
            have_display = true;
        }

        if let Err(e) = RegisterSuspendResumeNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            log::warn!("display monitor: RegisterSuspendResumeNotification failed: {e:?}");
        } else {
            have_resume = true;
        }

        // The perf-cycle hotkey lives on this window too: RegisterHotKey delivers WM_HOTKEY
        // to the registering window, and this is the tray's only window with a message loop
        // of its own. Registered here, before the early return below, and independent of it.
        let mut have_hotkey = false;
        let spec = HOTKEY_SPEC
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(spec) = spec {
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                RegisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
            };
            match parse_hotkey(&spec) {
                Some((mods, vk)) => {
                    match RegisterHotKey(hwnd, 1, HOT_KEY_MODIFIERS(mods) | MOD_NOREPEAT, vk) {
                        Ok(()) => {
                            have_hotkey = true;
                            log::info!("perf-cycle hotkey registered: {spec}");
                        }
                        // Most often: another program already owns that combination.
                        Err(e) => log::warn!("could not register hotkey {spec}: {e:?}"),
                    }
                }
                None => log::warn!(
                    "cycle_perf_hotkey {spec:?} not understood (e.g. \"Ctrl+Alt+P\"; a \
                     modifier is required)"
                ),
            }
        }

        // Independent capabilities: losing display-state should not cost us resume, and vice
        // versa. Only an empty window is worth abandoning.
        if !have_display && !have_resume && !have_hotkey {
            log::warn!("display monitor: no power notifications registered, thread exiting");
            return;
        }
        log::info!(
            "display-state monitor running (display-state: {}, suspend/resume: {})",
            if have_display { "yes" } else { "NO" },
            if have_resume { "yes" } else { "NO" }
        );

        let mut msg = MSG::default();
        // GetMessageW returns -1 on error, which `as_bool()` reads as true: a busy-spin.
        while GetMessageW(&mut msg, hwnd, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

#[cfg(target_os = "windows")]
pub fn efficiency_mode() {
    // SAFETY: GetCurrentProcess returns a pseudo-handle valid for this call; the
    // throttling struct lives on the stack across the call and we pass its exact size.
    unsafe {
        let handle: HANDLE = GetCurrentProcess();

        let _ = SetPriorityClass(handle, IDLE_PRIORITY_CLASS);

        let power_throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            &power_throttling as *const _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

/// Razer processes whose presence means Synapse (or its service layer) is running and may
/// write the same EC settings. Matched case-insensitively against process names.
const SYNAPSE_PROCESSES: &[&str] = &[
    "Razer Synapse Service.exe",
    "Razer Synapse Service Process.exe",
    "Razer Synapse 3.exe",
    "RazerAppEngine.exe",
    "RazerCentralService.exe",
    "Razer Central.exe",
    "RzSynapse.exe",
];

/// A one-line warning if Synapse appears to be running, else `None`.
///
/// Warn only. The tray never stops another program: the peers that ship the same kind
/// of tool (G-Helper, Legion Toolkit) settled on the same line, because killing a vendor
/// service a user deliberately installed is worse than a tug-of-war they were told about.
pub fn detect_synapse() -> Option<String> {
    let mut sys = System::new();
    sys.refresh_processes();
    let found: Vec<String> = sys
        .processes()
        .values()
        .map(|p| p.name().to_string())
        .filter(|n| SYNAPSE_PROCESSES.iter().any(|s| s.eq_ignore_ascii_case(n)))
        .collect();
    (!found.is_empty()).then(|| {
        log::warn!(
            "Razer Synapse is running ({}); it may overwrite these settings. Enable \
             'Keep settings enforced' or close Synapse.",
            found.join(", ")
        );
        "Razer Synapse is running (it may fight these settings)".to_string()
    })
}

// The three positions of the Settings "Power mode" slider, as overlay-scheme GUIDs.
#[cfg(target_os = "windows")]
const POWER_MODES: [(windows::core::GUID, &str); 3] = [
    (
        windows::core::GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a),
        "Best power efficiency",
    ),
    (windows::core::GUID::zeroed(), "Balanced"),
    (
        windows::core::GUID::from_u128(0xded574b5_45a0_4f42_8737_46345c09c238),
        "Best performance",
    ),
];

/// Which Windows power mode a perf mode maps to (an index into `POWER_MODES`).
#[cfg(target_os = "windows")]
fn power_mode_for(perf: crate::state::PerfMode) -> usize {
    use crate::state::PerfMode;
    match perf {
        PerfMode::Battery | PerfMode::Silent => 0,
        PerfMode::Balanced => 1,
        PerfMode::Performance | PerfMode::Hyperboost | PerfMode::Custom(..) => 2,
    }
}

/// powrprof.dll's getter and setter for the power mode, resolved once; either is None if
/// this Windows lacks it. The DLL is loaded once and never freed, which keeps the
/// pointers valid (it used to be loaded again on every call, and every menu rebuild
/// calls the getter).
#[cfg(target_os = "windows")]
fn power_overlay_procs() -> (
    windows::Win32::Foundation::FARPROC,
    windows::Win32::Foundation::FARPROC,
) {
    use windows::core::{s, w};
    use windows::Win32::Foundation::FARPROC;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    static PROCS: std::sync::OnceLock<(FARPROC, FARPROC)> = std::sync::OnceLock::new();
    // SAFETY: powrprof.dll is a system DLL; looking up a missing symbol returns None.
    *PROCS.get_or_init(|| unsafe {
        match LoadLibraryW(w!("powrprof.dll")) {
            Ok(module) => (
                GetProcAddress(module, s!("PowerGetEffectiveOverlayScheme")),
                GetProcAddress(module, s!("PowerSetActiveOverlayScheme")),
            ),
            Err(_) => (None, None),
        }
    })
}

/// The power-mode GUID Windows reports as in effect, or None if it can't be read.
#[cfg(target_os = "windows")]
fn effective_power_mode_guid() -> Option<windows::core::GUID> {
    use windows::core::GUID;
    type GetEffective = unsafe extern "system" fn(*mut GUID) -> u32;
    let proc = power_overlay_procs().0?;
    // SAFETY: the symbol has the signature above, as used by the Settings app.
    unsafe {
        let get: GetEffective = std::mem::transmute(proc);
        let mut current = GUID::zeroed();
        (get(&mut current) == 0).then_some(current)
    }
}

/// The Windows power mode now in effect, as the Settings app names it.
#[cfg(target_os = "windows")]
pub fn windows_power_mode() -> Option<&'static str> {
    let current = effective_power_mode_guid()?;
    POWER_MODES
        .iter()
        .find(|(guid, _)| *guid == current)
        .map(|(_, name)| *name)
}

#[cfg(not(target_os = "windows"))]
pub fn windows_power_mode() -> Option<&'static str> {
    None
}

/// Set the Windows power mode (Settings > Power, "Power mode") to match a perf mode.
///
/// Uses `PowerSetActiveOverlayScheme` from powrprof.dll: the call the Settings slider
/// makes, and the one G-Helper and Legion Toolkit use. It is undocumented, so it is
/// resolved at runtime; a Windows without it reports an error instead of failing to load.
#[cfg(target_os = "windows")]
pub fn set_windows_power_mode(perf: crate::state::PerfMode) -> Result<()> {
    let (guid, label) = POWER_MODES[power_mode_for(perf)];
    set_power_overlay(guid, label)
}

/// Set the Windows power mode by its Settings name, as `windows_power_mode` reports it.
#[cfg(target_os = "windows")]
pub fn set_windows_power_mode_named(name: &str) -> Result<()> {
    let (guid, label) = *POWER_MODES
        .iter()
        .find(|(_, label)| *label == name)
        .ok_or_else(|| anyhow::anyhow!("unknown Windows power mode {name:?}"))?;
    set_power_overlay(guid, label)
}

#[cfg(target_os = "windows")]
fn set_power_overlay(guid: windows::core::GUID, label: &str) -> Result<()> {
    use windows::core::GUID;

    // Compared against what Windows says is in effect, not against what we last set:
    // Windows keeps the slider per power source, and the user can move it themselves.
    if effective_power_mode_guid() == Some(guid) {
        return Ok(());
    }
    // `PowerSetActiveOverlayScheme(GUID)` takes the GUID BY VALUE. Declaring it that way
    // lets the compiler apply the platform ABI (on x64 a 16-byte struct is passed via a
    // hidden pointer, which is why passing `&guid` also happened to work).
    type SetOverlay = unsafe extern "system" fn(GUID) -> u32;
    let proc = power_overlay_procs()
        .1
        .ok_or_else(|| anyhow::anyhow!("PowerSetActiveOverlayScheme not available"))?;
    // SAFETY: the symbol has the signature above, as used by the Settings app.
    let status = unsafe {
        let set: SetOverlay = std::mem::transmute(proc);
        set(guid)
    };
    anyhow::ensure!(status == 0, "PowerSetActiveOverlayScheme returned {status}");
    log::info!("Windows power mode -> {label}");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_windows_power_mode(_perf: crate::state::PerfMode) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_windows_power_mode_named(_name: &str) -> Result<()> {
    Ok(())
}

/// The primary display's current mode.
#[cfg(target_os = "windows")]
fn current_display_mode() -> Option<windows::Win32::Graphics::Gdi::DEVMODEW> {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `mode` is a sized DEVMODEW that outlives the call; None = primary display.
    unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode).as_bool() }
        .then_some(mode)
}

/// Refresh rates the primary display offers at its current resolution, ascending.
#[cfg(target_os = "windows")]
pub fn refresh_rates() -> Vec<u32> {
    use windows::Win32::Graphics::Gdi::{
        EnumDisplaySettingsW, DEVMODEW, ENUM_DISPLAY_SETTINGS_MODE,
    };
    let Some(current) = current_display_mode() else {
        return Vec::new();
    };
    let mut rates = std::collections::BTreeSet::new();
    for i in 0.. {
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: as above; mode index `i` enumerates until the call returns FALSE.
        if !unsafe { EnumDisplaySettingsW(None, ENUM_DISPLAY_SETTINGS_MODE(i), &mut mode) }
            .as_bool()
        {
            break;
        }
        if mode.dmPelsWidth == current.dmPelsWidth
            && mode.dmPelsHeight == current.dmPelsHeight
            && mode.dmBitsPerPel == current.dmBitsPerPel
            && mode.dmDisplayFrequency > 1
        {
            rates.insert(mode.dmDisplayFrequency);
        }
    }
    rates.into_iter().collect()
}

#[cfg(not(target_os = "windows"))]
pub fn refresh_rates() -> Vec<u32> {
    Vec::new()
}

/// Switch the primary display to `hz` at its current resolution. A no-op when it is
/// already there, so re-applying a profile doesn't flicker the screen.
#[cfg(target_os = "windows")]
pub fn set_refresh_rate(hz: u32) -> Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, CDS_UPDATEREGISTRY, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY,
    };
    let mut mode = current_display_mode().ok_or_else(|| anyhow::anyhow!("no display mode"))?;
    if mode.dmDisplayFrequency == hz {
        return Ok(());
    }
    anyhow::ensure!(
        refresh_rates().contains(&hz),
        "{hz} Hz is not offered at the current resolution"
    );
    mode.dmDisplayFrequency = hz;
    mode.dmFields = DM_DISPLAYFREQUENCY;
    // SAFETY: `mode` is a DEVMODEW from EnumDisplaySettingsW with only the frequency
    // changed; None = primary display.
    let result =
        unsafe { ChangeDisplaySettingsExW(None, Some(&mode), HWND(0), CDS_UPDATEREGISTRY, None) };
    anyhow::ensure!(
        result == DISP_CHANGE_SUCCESSFUL,
        "ChangeDisplaySettingsExW: {result:?}"
    );
    log::info!("display refresh rate -> {hz} Hz");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_refresh_rate(_hz: u32) -> Result<()> {
    Ok(())
}

/// Parse a hotkey like `"Ctrl+Alt+P"` into (modifier flags, virtual-key code).
/// Modifiers: Ctrl/Control, Alt, Shift, Win; key: A-Z, 0-9, F1-F24. Case-insensitive.
/// Built everywhere so its tests run on any host; only Windows registers the hotkey.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn parse_hotkey(spec: &str) -> Option<(u32, u32)> {
    const MOD_ALT: u32 = 0x1;
    const MOD_CONTROL: u32 = 0x2;
    const MOD_SHIFT: u32 = 0x4;
    const MOD_WIN: u32 = 0x8;
    let mut mods = 0;
    let mut key = None;
    for part in spec.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" => mods |= MOD_WIN,
            k if key.is_none() => {
                let upper = k.to_ascii_uppercase();
                key = match upper.as_bytes() {
                    [c] if c.is_ascii_alphanumeric() => Some(*c as u32),
                    [b'F', rest @ ..] => std::str::from_utf8(rest)
                        .ok()
                        .and_then(|n| n.parse::<u32>().ok())
                        .filter(|n| (1..=24).contains(n))
                        .map(|n| 0x70 + n - 1),
                    _ => None,
                };
                key?;
            }
            _ => return None,
        }
    }
    // A bare key would steal it from every application.
    (mods != 0).then_some(())?;
    key.map(|k| (mods, k))
}

/// Set when the perf-cycle hotkey is pressed; consumed by the event loop.
static HOTKEY_PRESSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the perf-cycle hotkey was pressed since the last call.
pub fn take_hotkey() -> bool {
    HOTKEY_PRESSED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// The hotkey spec handed to the power-notification thread, which owns the window that
/// receives WM_HOTKEY. Set before `spawn_display_state_monitor`.
#[cfg(target_os = "windows")]
static HOTKEY_SPEC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(target_os = "windows")]
pub fn set_hotkey_spec(spec: Option<String>) {
    *HOTKEY_SPEC.lock().unwrap_or_else(|p| p.into_inner()) = spec;
}

#[cfg(test)]
mod tests {
    use super::parse_hotkey;

    #[test]
    fn hotkeys_parse_and_bare_keys_are_refused() {
        assert_eq!(parse_hotkey("Ctrl+Alt+P"), Some((0x3, b'P' as u32)));
        assert_eq!(parse_hotkey("win + shift + f12"), Some((0xC, 0x7B)));
        assert_eq!(parse_hotkey("Ctrl+1"), Some((0x2, b'1' as u32)));
        assert_eq!(parse_hotkey("P"), None, "no modifier");
        assert_eq!(parse_hotkey("Ctrl+F25"), None);
        assert_eq!(parse_hotkey("Ctrl+Alt+P+Q"), None, "two keys");
        assert_eq!(parse_hotkey("Hyper+P"), None);
    }
}
