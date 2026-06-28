//! OS-specific glue, gathered behind one door: power-state detection, dGPU process
//! termination, autostart registration, the keyboard hook + idle signal, the
//! display-state monitor, and process efficiency hints. Everything Windows- or
//! Linux-only lives here so the rest of the crate reads as portable logic.

use anyhow::Result;
use std::process::Command as procCommand;
use sysinfo::{ProcessExt, Signal, System, SystemExt};

#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the low-level keyboard hook; consumed by the event loop to refresh the
/// tooltip on keypress (keypresses already wake the backlight, so it's free).
#[cfg(target_os = "windows")]
pub static KEY_PRESSED: AtomicBool = AtomicBool::new(false);

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
            Ok(()) => {
                match status.ACLineStatus {
                    0 => ac_power = false,
                    _ => ac_power = true,
                }
            }
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
        // read ("Insufficient Permissions") -- killing by an unknown name is unsafe.
        if name == "Insufficient Permissions" || librazer::process_guard::is_protected_process(name)
        {
            log::info!("Skipping protected process: {} ({})", pid, name);
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

#[cfg(target_os = "windows")]
pub unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallNextHookEx, HHOOK};
    // SAFETY: a WH_KEYBOARD_LL hook proc invoked by the OS. The body only touches an
    // atomic (cannot unwind in practice), and we always forward to the next hook via
    // CallNextHookEx as the API requires; not doing so would break the global hook chain.
    if code >= 0 && (wparam.0 == 0x0100 || wparam.0 == 0x0104) {
        KEY_PRESSED.store(true, Ordering::Relaxed);
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
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

/// Window procedure for the hidden message-only window that receives power
/// notifications. Updates DISPLAY_ON from GUID_CONSOLE_DISPLAY_STATE events.
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
        DefWindowProcW, PBT_POWERSETTINGCHANGE, WM_POWERBROADCAST,
    };

    if msg == WM_POWERBROADCAST && wparam.0 as u32 == PBT_POWERSETTINGCHANGE {
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
            DEVICE_NOTIFY_WINDOW_HANDLE, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
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
