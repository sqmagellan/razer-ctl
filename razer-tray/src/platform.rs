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

#[cfg(target_os = "windows")]
pub fn get_power_state() -> Result<bool> {
    let mut ac_power: bool = true;
    // SAFETY: `status` is a fully-default-initialized SYSTEM_POWER_STATUS; we pass a
    // valid &mut to it and GetSystemPowerStatus only writes through that pointer.
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        match GetSystemPowerStatus(&mut status) {
            Ok(()) => match status.ACLineStatus {
                0 => ac_power = false,
                _ => ac_power = true,
            },
            Err(e) => {
                log::warn!("Failed to get power status: {:?}", e);
            }
        }
    }
    Ok(ac_power)
}

#[cfg(target_os = "linux")]
pub fn get_power_state() -> Result<bool> {
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

    // Default to AC power if we can't detect
    log::warn!("Could not detect power state, assuming AC power");
    Ok(true)
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
    std::thread::spawn(move || loop {
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

/// Set when Windows tells us the system has resumed from sleep. The event loop
/// consumes (and clears) it to trigger the profile re-assert.
///
/// This replaces a tick-gap heuristic: the loop ran once a second, and a gap over 30 s
/// was read as "we must have been suspended". That was wrong in both directions. The
/// tray runs at `IDLE_PRIORITY_CLASS` with EcoQoS throttling (see `efficiency_mode`),
/// so a busy machine can starve it far longer than the threshold while wide awake --
/// observed 2026-07-25 on the Blade: a **54.9 s** gap logged as "resume detected" with
/// no sleep involved, firing a spurious re-assert. In the other direction a short sleep
/// could go unnoticed. `PBT_APMRESUMESUSPEND` is the OS telling us directly, so it is
/// both precise and free.
#[cfg(target_os = "windows")]
pub static RESUMED: AtomicBool = AtomicBool::new(false);

/// True exactly once per resume event, clearing the flag.
#[cfg(target_os = "windows")]
pub fn take_resumed() -> bool {
    RESUMED.swap(false, Ordering::Relaxed)
}

/// No OS resume notification wired up off Windows; the caller keeps its previous
/// behavior (never fires) rather than guessing from wall-clock.
#[cfg(not(target_os = "windows"))]
pub fn take_resumed() -> bool {
    false
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
    use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_POWERSETTINGCHANGE,
        WM_POWERBROADCAST,
    };

    if msg == WM_POWERBROADCAST {
        match wparam.0 as u32 {
            PBT_POWERSETTINGCHANGE => {
                // SAFETY: for a PBT_POWERSETTINGCHANGE message Windows guarantees lparam points
                // to a POWERBROADCAST_SETTING valid for the duration of this call; we only read it.
                let setting = &*(lparam.0 as *const POWERBROADCAST_SETTING);
                if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
                    // Data[0]: 0 = off, 1 = on, 2 = dimmed. Treat dimmed as on.
                    let on = setting.Data[0] != 0;
                    DISPLAY_ON.store(on, Ordering::Relaxed);
                    log::info!("console display state: {}", if on { "on" } else { "off" });
                }
                return windows::Win32::Foundation::LRESULT(1);
            }
            // Both arrive on wake: RESUMEAUTOMATIC always, RESUMESUSPEND additionally
            // when the resume was user-initiated. Treating either as the signal (and
            // latching a bool rather than counting) means the pair collapses into one
            // re-assert.
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                RESUMED.store(true, Ordering::Relaxed);
                log::info!("system resume broadcast received");
                return windows::Win32::Foundation::LRESULT(1);
            }
            _ => {}
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
        use windows::Win32::System::Power::RegisterPowerSettingNotification;
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
        RegisterClassW(&wc);

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

        if let Err(e) = RegisterPowerSettingNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            log::warn!("display monitor: RegisterPowerSettingNotification failed: {e:?}");
            return;
        }
        log::info!("display-state monitor running");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, hwnd, 0, 0).as_bool() {
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
