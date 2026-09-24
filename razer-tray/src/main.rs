#![windows_subsystem = "windows"]

mod config;
mod menu;
mod platform;
mod program;
mod state;

use anyhow::Result;

use librazer::types::LightsAlwaysOn;
use librazer::{command, device};

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{MenuEvent, MenuId},
    MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};

use single_instance::SingleInstance;

use sysinfo::{ProcessExt, SystemExt};

#[cfg(target_os = "windows")]
use std::sync::atomic::Ordering;

use program::{Pick, ProgramState};
use state::{ActionSession, DeviceState};

pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");

/// `%LOCALAPPDATA%\razer-tray\razer-tray.log`. It used to live in `%TEMP%`, where Storage
/// Sense and disk-cleanup tools delete it, which is the worst place for the one file that
/// explains what the tray did.
pub fn get_logging_file_path() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(PKG_NAME).join(format!("{}.log", PKG_NAME))
}

fn init_logging_to_file() -> Result<()> {
    use log4rs::append::rolling_file::policy::compound::{
        roll::fixed_window::FixedWindowRoller, trigger::size::SizeTrigger, CompoundPolicy,
    };
    let path = get_logging_file_path();
    // 1 MiB x 4 files. The old policy deleted the whole 10 MiB log on rollover, so the
    // history that explains a problem vanished exactly when it got long.
    let pattern = path.with_extension("{}.log");
    let policy = CompoundPolicy::new(
        Box::new(SizeTrigger::new(1 << 20)),
        Box::new(FixedWindowRoller::builder().build(&pattern.to_string_lossy(), 3)?),
    );

    let logfile = log4rs::append::rolling_file::RollingFileAppender::builder()
        .encoder(Box::new(log4rs::encode::pattern::PatternEncoder::new(
            "{h({d(%Y-%m-%d %H:%M:%S)(local)} - {l}: {m}{n})}",
        )))
        .build(path, Box::new(policy))?;

    let config = log4rs::config::Config::builder()
        .appender(log4rs::config::Appender::builder().build("logfile", Box::new(logfile)))
        .build(
            log4rs::config::Root::builder()
                .appender("logfile")
                // Info covers every meaningful event (startup, device detect, menu actions,
                // enforce, profile switches, display state); Trace adds HID-level noise and
                // is only useful while debugging. Bounded to 4 MiB across the rotation.
                .build(log::LevelFilter::Info),
        )?;

    log4rs::init_config(config)?;
    Ok(())
}

/// Open the device, retrying for a while: the tray autostarts at login, which is exactly
/// when the HID stack is least likely to be ready.
fn detect_with_retry() -> Result<device::Device> {
    let mut delay = std::time::Duration::from_secs(1);
    let mut last = None;
    for attempt in 1..=6 {
        match device::Device::detect() {
            Ok(d) => return Ok(d),
            Err(e) => {
                log::warn!("device detect attempt {attempt} failed: {e:?}");
                last = Some(e);
            }
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_secs(8));
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("device detection failed")))
}

/// An input gap at least this long may have let the keyboard backlight fade, which can
/// drop a custom color. The EC fades after about 4 s idle; 3 s leaves margin for the
/// 1 s loop tick.
const KEYBOARD_FADE_MS: u32 = 3000;

/// Backoff for resync attempts after a failed exchange: 2 s doubling to 60 s. The old
/// recovery slept 1 s at a time INSIDE the event-loop callback until it succeeded, so a
/// lasting failure froze the menu, hover and Quit.
fn resync_backoff(failures: u32) -> std::time::Duration {
    std::time::Duration::from_secs((2u64 << failures.min(5)).min(60))
}

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // Initialize GTK for tray icon on Linux
        gtk::init().map_err(|_| anyhow::anyhow!("Failed to initialize GTK"))?;
    }

    // Logging first: every line before this point used to be lost, including the
    // "another instance is running" exit and any startup failure.
    if let Err(e) = init_logging_to_file() {
        eprintln!("razer-tray: could not open the log: {e:?}");
    }

    // Single instance before any thread or subprocess is started.
    let instance = match SingleInstance::new("razer-tray") {
        Ok(i) => Some(i),
        Err(e) => {
            log::warn!("single-instance check unavailable ({e}); continuing");
            None
        }
    };
    if instance.as_ref().is_some_and(|i| !i.is_single()) {
        log::info!("Another instance is already running. Exiting.");
        return Ok(());
    }

    log::info!(
        "{0} starting {1} {2} {0}",
        "==".repeat(20),
        PKG_NAME,
        env!("CARGO_PKG_VERSION")
    );

    #[cfg(target_os = "windows")]
    platform::efficiency_mode();

    // Sample dGPU temp/power in the background for the tooltip. Fails open: no NVIDIA
    // tools or no dGPU means the tooltip simply omits those fields.
    #[cfg(target_os = "windows")]
    platform::spawn_gpu_telemetry_monitor();

    let mut device = match detect_with_retry() {
        Ok(d) => {
            log::info!(
                "detected device: {} (0x{:04X})",
                d.info().name,
                d.info().pid
            );
            d
        }
        Err(e) => {
            log::error!("{:?}", e);
            native_dialog::MessageDialog::new()
                .set_type(native_dialog::MessageType::Error)
                .set_text(format!("{:?}", e).as_str())
                .show_alert()?;
            return Err(e);
        }
    };

    // Device preparation BEFORE the profile is applied, and never fatal. It used to run
    // after init(), so an init failure skipped it, and on models with init sequences the
    // first apply and reconcile ran against an uninitialized device.
    for element in device.info().init_cmds {
        if let Err(e) = command::send_command(&device, *element, &[0, 0, 0, 0]) {
            log::warn!("init command {element:#06x} failed: {e:?}");
        }
    }
    // Ensure the keyboard is in Normal (hardware) device mode, never Razer "driver mode".
    // The 0x0004 command's Enable value (0x03) is driver mode, which hands key handling to
    // a host driver and disables the EC's native Fn media keys (brightness/volume/kbd
    // backlight). A previous build used it for "always-on"; we never enter it -- always-on
    // is a Normal-mode keep-alive (see the keep-alive block in the event loop below).
    if let Err(e) = command::set_lights_always_on(&device, LightsAlwaysOn::Disable) {
        log::warn!("could not force Normal device mode: {:?}", e);
    }

    // Left-click is OURS: it cycles the perf mode (see the tray-event handler below).
    // The menu belongs to right-click.
    //
    // `menu_on_left_click` defaults to TRUE, and it must be turned off explicitly. Under
    // tray-icon 0.11.3 the Windows backend showed the menu only on WM_RBUTTONUP and ignored
    // this flag, so left-click did nothing but cycle. 0.19 honors it, which made a single
    // left-click do BOTH: the menu opens on WM_LBUTTONDOWN while our cycle runs on
    // WM_LBUTTONUP, so the mode changed *behind* the popup and you couldn't see it until you
    // moved the mouse away and the menu dismissed.
    let mut tray_icon = TrayIconBuilder::new()
        .with_menu_on_left_click(false)
        .build()?;

    let mut config_file = config::ConfigFile::open()?;
    log::info!("loading config file {}", config_file.path().display());
    let first_run = !config_file.exists();
    let mut config = config_file.load();
    // A file that exists but can't be read yet (an editor or antivirus holding it at
    // login) must not be replaced by defaults on the device either: wait a little for it.
    for _ in 0..10 {
        if !config_file.is_blocked() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        config = config_file.load();
    }

    // Start the display-state monitor (event-driven; gates always-on backlight, signals
    // wakes, and owns the perf-cycle hotkey, which is why it waits for the config).
    #[cfg(target_os = "windows")]
    {
        platform::set_hotkey_spec(config.cycle_perf_hotkey.clone());
        platform::spawn_display_state_monitor();
    }

    let mut state = ProgramState::new(
        config,
        config_file,
        platform::get_power_state(true),
        device.info().fan_rpm_range, // per-chassis fan bounds, from the descriptor
    )?;
    state.warning = platform::detect_synapse();
    state.refresh_rates = platform::refresh_rates();
    state.custom_colors = librazer::keyboard::supports_custom_frame(device.info().pid);
    state.rebuild_menu(Some(&tray_icon));
    if first_run {
        // confy used to create the file on first run; people look for it to add Actions.
        if let Err(e) = state.persist() {
            log::warn!("could not create the config file: {e:?}");
        }
    }

    // First contact. A failure here no longer ends the process: the tray comes up showing
    // the saved profile and the event loop keeps retrying with backoff. A single HID error
    // at login used to exit the tray with nothing in the log.
    let mut sync_failures: u32 = 0;
    let mut next_sync_at = std::time::Instant::now();
    match state.sync(&mut tray_icon, &device) {
        Ok(()) => {}
        Err(e) => {
            log::error!("startup sync failed, will retry: {e:?}");
            state.needs_sync = true;
            state.sync_apply = true;
            sync_failures = 1;
            next_sync_at = std::time::Instant::now() + resync_backoff(0);
            state.refresh_ui(&tray_icon);
        }
    }

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let event_loop = EventLoopBuilder::new().build();

    let mut last_device_state_check_timestamp = std::time::Instant::now();
    // The last-input tick recorded at the previous Mirror poll. We only re-poll when
    // this changes (new input), so polling follows your activity and stops when you
    // stop touching the machine. None = not yet polled / non-Windows (always refresh).
    let mut last_polled_input_tick: Option<u32> = None;
    // Throttles the always-on keep-alive: while "keyboard always-on" is enabled and the
    // display is on, we touch the device every few seconds to re-brighten the backlight
    // (the EC fades it after ~4s idle). This records the last keep-alive tick.
    #[cfg(target_os = "windows")]
    let mut last_keepalive_timestamp = std::time::Instant::now();
    // The input tick seen on the previous loop pass, to spot input after an idle spell.
    let mut last_seen_input_tick: Option<u32> = platform::last_input_tick();
    // Throttles the on-hover Mirror refresh (tray-icon Enter/Move events fire rapidly).
    let mut last_hover_refresh = std::time::Instant::now();

    // "Actions" (app-triggered profiles). We scan the process list on a slow cadence
    // (only when rules exist) and act on *transitions* -- apply on launch, revert on
    // exit. The running session lives in `state.action`. A persistent System avoids
    // re-enumerating everything each scan.
    let mut last_app_scan_timestamp = std::time::Instant::now();
    let mut app_scan_sys = sysinfo::System::new();
    let mut last_config_check = std::time::Instant::now();
    let mut last_synapse_check = std::time::Instant::now();

    // Wake reconciles pending: (when, why). A wake schedules a read at +3 s and a second at
    // +15 s -- the second catches an EC that settles late (the G-Helper #5682 lesson: one
    // re-apply at the instant of resume can lose to the firmware's own post-wake writes).
    let mut wake_checks: Vec<(std::time::Instant, String, platform::WakeSource)> = Vec::new();

    event_loop.run(move |_, _, control_flow| {
        let now = std::time::Instant::now();
        *control_flow = ControlFlow::WaitUntil(now + std::time::Duration::from_millis(1000));

        // Retry a failed exchange without blocking the loop. After a few failures the
        // handle itself may be stale (the device re-enumerated across a resume), so reopen.
        if state.needs_sync && now >= next_sync_at {
            if sync_failures >= 3 {
                match device::Device::detect() {
                    Ok(d) => {
                        // A re-enumerated device may have reset: re-apply intent, not just
                        // probe, and replay its init sequence first.
                        log::info!("reopened the device");
                        device = d;
                        for element in device.info().init_cmds {
                            let _ = command::send_command(&device, *element, &[0, 0, 0, 0]);
                        }
                        state.sync_apply = true;
                    }
                    Err(e) => log::warn!("device reopen failed: {e:?}"),
                }
            }
            match state.sync(&mut tray_icon, &device) {
                Ok(()) => {
                    log::info!("resync succeeded after {sync_failures} failure(s)");
                    sync_failures = 0;
                }
                Err(e) => {
                    log::warn!("resync failed: {e:?}");
                    next_sync_at = now + resync_backoff(sync_failures);
                    sync_failures += 1;
                }
            }
        }

        if let Err(e) = (|| -> Result<()> {
            // Drain, for the same reason as the tray channel below: one event per ~1s tick
            // makes a queued selection wait behind everything ahead of it. Menu events are
            // distinct user choices, so each is handled rather than coalesced.
            while let Ok(event) = menu_channel.try_recv() {
                log::info!("Menu Event {:?}", event.id);
                // A hand edit first, so a toggle below lands on top of it.
                state.reload_config_if_changed(&tray_icon);
                if event.id == MenuId("dgpu_terminate_proc".to_string()) {
                    log::info!("match event id");
                    platform::gpu_taskkill()?;
                } else if event.id == MenuId("toggle_enforce".to_string()) {
                    state.enforce = !state.enforce;
                    if let Err(e) = state.persist() {
                        log::warn!("Failed to persist enforce flag: {:?}", e);
                    }
                    // Rebuild the menu so the checkmark reflects the new state.
                    state.rebuild_menu(Some(&tray_icon));
                    log::info!("enforce toggled to {}", state.enforce);
                } else if let Some(follow) = match event.id.as_ref() {
                    "power_mode:follow" => Some(true),
                    "power_mode:off" => Some(false),
                    _ => None,
                } {
                    state.match_power_mode = follow;
                    if let Err(e) = state.persist() {
                        log::warn!("Failed to persist power-mode flag: {:?}", e);
                    }
                    if follow {
                        if let Err(e) =
                            platform::set_windows_power_mode(state.device_state.perf_mode)
                        {
                            log::warn!("Windows power mode: {e:?}");
                        }
                    }
                    state.rebuild_menu(Some(&tray_icon));
                    log::info!("Windows power mode follows perf mode: {follow}");
                } else if event.id == MenuId("toggle_battery_bar".to_string()) {
                    state.battery_bar = !state.battery_bar;
                    if let Err(e) = state.persist() {
                        log::warn!("Failed to persist battery-bar flag: {:?}", e);
                    }
                    state.rebuild_menu(Some(&tray_icon));
                    if !state.needs_sync {
                        state.paint_keyboard(&device, false);
                    }
                    log::info!("keyboard battery bar toggled to {}", state.battery_bar);
                } else if event.id == MenuId("toggle_autostart".to_string()) {
                    #[cfg(target_os = "windows")]
                    {
                        if let Err(e) = platform::set_autostart(!platform::autostart_enabled()) {
                            log::warn!("Failed to toggle autostart: {:?}", e);
                        }
                        // Rebuild the menu so the checkmark reflects the new state.
                        state.rebuild_menu(Some(&tray_icon));
                    }
                } else if let Some(new_device_state) = state.handle_event(event.id.as_ref()) {
                    log::info!("new_device_state 1 {:?}", new_device_state);
                    state.update(&mut tray_icon, new_device_state, Pick::Menu, &device)?;
                } else {
                    // Predefined items (About, Quit) and stale ids from a menu that was
                    // rebuilt while open. Not an error: it used to trigger a full re-init.
                    log::debug!("no handler for menu event {:?}", event.id);
                }
            }

            // Tray-icon events. Left-click cycles the perf mode; hover (Enter/Move)
            // refreshes the displayed state on demand -- this is what replaced the old
            // global keyboard hook: we read exactly when you look at the tray. A hover
            // means you're actively on the machine, so the backlight is already awake and
            // the read can't cause a visible pulse.
            //
            // DRAIN the channel, don't take one event per pass. `Move` fires rapidly while
            // the cursor sits over the icon and the channel is unbounded, so a single event
            // per ~1s tick meant a click was processed only after every Move that preceded
            // it: moving onto the icon queues a hundred-plus Moves, which delayed the mode
            // switch by minutes and read as a broken click. Throttling the hover work did
            // not help -- a skipped event still consumed its entire tick.
            //
            // Draining also coalesces: however many hover events arrived, they collapse into
            // at most one refresh, and a click supersedes them (its update() re-renders the
            // icon and tooltip anyway).
            // The perf-cycle hotkey acts exactly like a left-click.
            let mut clicked = platform::take_hotkey();
            let mut hovered = false;
            while let Ok(event) = tray_channel.try_recv() {
                match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => clicked = true,
                    TrayIconEvent::Enter { .. } | TrayIconEvent::Move { .. } => hovered = true,
                    _ => {}
                }
            }

            // While a resync is pending, the resync at the top of the loop is the only thing
            // that talks to the device. With a stale handle every exchange costs ~2 s of
            // retries, and polling through that froze the menu and Quit for a minute a tick.
            let io_ok = !state.needs_sync;

            if clicked {
                let new_device_state = state.get_next_perf_mode();
                log::info!("left-click: cycling perf mode to {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, Pick::Cycle, &device)?;
            } else if io_ok
                && hovered
                && now > last_hover_refresh + std::time::Duration::from_millis(500)
            {
                last_hover_refresh = now;
                if let Ok(observed) = DeviceState::read(&device) {
                    state.observed = observed;
                }
                state.refresh_fan(&device);
                state.refresh_ui(&tray_icon);
            }

            // Power source. An Action session moves its picks to the new source's profile;
            // an app scan is forced so a rule restricted to the other source retires now
            // rather than up to 5 s later.
            let ac_now = platform::get_power_state(state.ac_power);
            if ac_now != state.ac_power {
                log::info!("power source: {}", if ac_now { "AC" } else { "battery" });
                let (old_base, new_base) = if ac_now {
                    (state.battery_state, state.ac_state)
                } else {
                    (state.ac_state, state.battery_state)
                };
                if let Some(session) = &mut state.action {
                    if let Some(rule) = state.app_profiles.get(session.rule) {
                        session.power_changed(rule, &old_base, &new_base, state.fan_rpm_range);
                    }
                }
                state.ac_power = ac_now;
                // The header names the profile picks go to.
                state.rebuild_menu(Some(&tray_icon));
                last_app_scan_timestamp = now - std::time::Duration::from_secs(10);
                // Windows keeps the power-mode slider per power source, so the new source's
                // slider needs setting even when the perf mode doesn't change.
                if state.match_power_mode {
                    if let Err(e) = platform::set_windows_power_mode(state.target().perf_mode) {
                        log::warn!("Windows power mode: {e:?}");
                    }
                }
            }

            // Hand edits to the config take effect without a restart (Action rules can
            // only be written by hand).
            // Synapse started or closed since startup. A full process scan, so only every
            // few minutes.
            if now > last_synapse_check + std::time::Duration::from_secs(300) {
                last_synapse_check = now;
                let warning = platform::detect_synapse();
                if warning != state.warning {
                    state.warning = warning;
                    state.rebuild_menu(Some(&tray_icon));
                }
            }

            if now > last_config_check + std::time::Duration::from_secs(5) {
                last_config_check = now;
                if state.reload_config_if_changed(&tray_icon) {
                    state.refresh_rates = platform::refresh_rates();
                    state.rebuild_menu(Some(&tray_icon));
                }
            }

            // "Actions": app-triggered profile switches. Only runs when rules are
            // configured (empty by default). Scans the process list on a slow cadence and
            // acts only on *transitions*: a rule's session starts when its process appears
            // and ends when the last match exits. Picks made in between hold for the
            // session (see ActionSession).
            if !state.app_profiles.is_empty()
                && now > last_app_scan_timestamp + std::time::Duration::from_secs(5)
            {
                last_app_scan_timestamp = now;
                app_scan_sys.refresh_processes();
                let running: Vec<String> = app_scan_sys
                    .processes()
                    .values()
                    .map(|p| p.name().to_string())
                    .collect();
                // Power source is part of selection: a rule may be restricted to AC or
                // battery, so an AC/battery transition can change which rule applies even
                // when the running process set hasn't changed at all.
                let matched =
                    state::matching_app_profile(&state.app_profiles, &running, state.ac_power);
                if matched != state.action.map(|a| a.rule) {
                    match matched {
                        Some(i) => {
                            let rule = &state.app_profiles[i];
                            // Overlay the rule onto the user's saved power-source profile
                            // (not any prior transient), so unset fields fall back to what
                            // they configured for AC/battery.
                            let session = ActionSession::start(
                                i,
                                rule,
                                &state.saved_profile(),
                                state.fan_rpm_range,
                            );
                            log::info!(
                                "action: '{}' running (priority {}) -> {:?}",
                                rule.label(),
                                rule.priority,
                                session.effective
                            );
                            state.action = Some(session);
                        }
                        None => {
                            log::info!(
                                "action: no rule app running -> reverting to {:?}",
                                state.saved_profile().perf_mode
                            );
                            state.action = None;
                        }
                    }
                }
            } else if state.app_profiles.is_empty() {
                state.action = None;
            }

            // Converge the device on what it should be now: the Action session, else the
            // saved profile for the power source. Transient, because neither needs saving.
            let target = state.target();
            if io_ok && target != state.device_state {
                log::info!("new_device_state 3 {:?}", target);
                state.update_transient(&mut tray_icon, target, &device)?;
            }

            // Wake handling, rebuilt around what Modern Standby actually delivers. The Blade
            // sleeps in S0 Low Power Idle: 69 standby cycles in 17 days against 5 real
            // resumes, and the resume message arrived for only some of those. A wake is
            // therefore a resume message OR the display coming back after a long off, and
            // it schedules READS; a write happens only when a read shows drift.
            if let Some(source) = platform::take_wake() {
                log::info!("wake detected ({source}); reconciling at +3 s and +15 s");
                wake_checks.retain(|(_, _, s)| *s != source);
                wake_checks.push((
                    now + std::time::Duration::from_secs(3),
                    format!("wake ({source})"),
                    source,
                ));
                wake_checks.push((
                    now + std::time::Duration::from_secs(15),
                    format!("wake +15s ({source})"),
                    source,
                ));
                // An external display may have come or gone with the wake, and Synapse may
                // have been started or closed.
                state.refresh_rates = platform::refresh_rates();
                state.warning = platform::detect_synapse();
                last_synapse_check = now;
                state.rebuild_menu(Some(&tray_icon));
            }
            if io_ok {
                if let Some(pos) = wake_checks.iter().position(|(at, _, _)| now >= *at) {
                    let (_, reason, source) = wake_checks.remove(pos);
                    // A resume message re-asserts under `reassert_on_resume` as before. A
                    // display-on wake also fires for a plain screen timeout, so it only
                    // measures (logs drift) unless Enforce is on: re-asserting there would
                    // revert every CLI or Synapse change at each screen timeout.
                    let write = match source {
                        platform::WakeSource::Resume => state.reassert_on_resume || state.enforce,
                        platform::WakeSource::DisplayOn => state.enforce,
                    };
                    state.reconcile(&mut tray_icon, &device, &reason, write);
                    // A custom color does not survive standby (the keyboard falls back to
                    // its stored effect), so repaint it whatever the reconcile decided.
                    state.paint_keyboard(&device, true);
                }
            }

            // A custom color can be gone after the backlight's idle fade: with "Keep
            // keyboard lit" off, the keyboard wakes back up on its stored effect, not the
            // frame (seen 2026-09-23). The first input after a pause longer than the fade
            // repaints it. Harmless when the frame survived: the same colors are re-sent.
            let tick_now = platform::last_input_tick();
            if let (Some(cur), Some(prev)) = (tick_now, last_seen_input_tick) {
                if cur != prev
                    && cur.wrapping_sub(prev) >= KEYBOARD_FADE_MS
                    && io_ok
                    && state.has_custom_color()
                {
                    state.paint_keyboard(&device, true);
                }
            }
            last_seen_input_tick = tick_now;

            // Keyboard always-on (opt-in) keep-alive. The keyboard's EC fades the
            // backlight after ~4s of no input. The ONLY way to keep it lit without Razer
            // "driver mode" (which disables the Fn media keys) is to touch the device
            // faster than that fade. Any HID access re-brightens, so we issue a
            // lightweight brightness *read* (writes nothing -> never fights the Fn
            // brightness keys) every few seconds while always-on is enabled and the
            // display is on. It naturally stops while the display sleeps and while the
            // system is suspended (the loop is frozen), so the backlight goes dark then.
            #[cfg(target_os = "windows")]
            if io_ok
                && state.device_state.lights_mode.always_on == LightsAlwaysOn::Enable
                && platform::DISPLAY_ON.load(Ordering::Relaxed)
                && now > last_keepalive_timestamp + std::time::Duration::from_secs(3)
            {
                last_keepalive_timestamp = now;
                let _ = command::get_keyboard_brightness(&device);
            }

            // Mirror: refresh the displayed device state (tooltip/icon) so it's fresh
            // when you look at the tray. Display-only -- it never re-applies state, so
            // it can't fight external changes or touch the saved AC/battery profiles.
            // A failed read is swallowed: keep the last good values and try again rather
            // than tearing down and re-initing the device.
            //
            // We poll at most every 2s AND only when there's been new input since the
            // last poll (`last_input_tick` changed). So reads track your activity --
            // including the trackpad movement that brings you to the tray, which both
            // makes the tooltip fresh on hover and means the read can't visibly disturb
            // the backlight (your input already woke it). The moment you stop touching
            // the machine, polling stops and the keyboard dims/off normally; we never
            // re-poke an idle keyboard. (None from last_input_tick -> always refresh,
            // the non-Windows fallback.)
            let input_tick = platform::last_input_tick();
            let new_input = match (input_tick, last_polled_input_tick) {
                (Some(cur), Some(prev)) => cur != prev,
                _ => true,
            };
            if io_ok
                && new_input
                && now > last_device_state_check_timestamp + std::time::Duration::from_secs(2)
            {
                last_device_state_check_timestamp = now;
                last_polled_input_tick = input_tick;
                // The battery bar follows the charge. Only here, on input, because every
                // frame write lights the backlight back up.
                state.paint_keyboard(&device, false);
                match DeviceState::read(&device) {
                    Ok(observed) => {
                        state.observed = observed;

                        // Adopt an externally-made keyboard-brightness change (e.g. the
                        // hardware Fn brightness keys) into the app's own state. We also
                        // write it into the active AC/battery profile and persist it, so
                        // (a) the menu checkmark reflects it, (b) it survives an AC/battery
                        // switch, and (c) the convergence step above sees device_state
                        // == the target and does NOT re-apply -- no tug-of-war.
                        // Adopt a pending hand edit first, so the save below neither
                        // writes over it nor gets discarded by it.
                        state.reload_config_if_changed(&tray_icon);
                        let observed_brightness = state.observed.lights_mode.keyboard_brightness;
                        if observed_brightness != state.device_state.lights_mode.keyboard_brightness
                        {
                            state.device_state.lights_mode.keyboard_brightness =
                                observed_brightness;
                            if state.ac_power {
                                state.ac_state.lights_mode.keyboard_brightness =
                                    observed_brightness;
                            } else {
                                state.battery_state.lights_mode.keyboard_brightness =
                                    observed_brightness;
                            }
                            if let Some(session) = &mut state.action {
                                session.effective.lights_mode.keyboard_brightness =
                                    observed_brightness;
                            }
                            if let Err(e) = state.persist() {
                                log::warn!("failed to persist adopted brightness: {:?}", e);
                            }
                            state.rebuild_menu(Some(&tray_icon));
                        }

                        // Enforce (opt-in): if the real device drifted from our intended
                        // state on a field we own -- perf mode, fan, logo, battery care --
                        // re-assert it. This is how razer-tray wins a tug-of-war with
                        // Synapse. Brightness is deliberately excluded (it stays on the
                        // adopt path above so the Fn keys keep working). It rides the same
                        // input-gated read above, so it adds no idle cost and reasserts
                        // whenever you're active (incl. right after you return to the
                        // machine); the wake reconcile covers the wake case.
                        if state.enforce
                            && state.observed.enforced_fields_differ(&state.device_state)
                        {
                            log::info!("enforce: device drifted; re-asserting intended state");
                            if let Err(e) = state.device_state.enforce_to(&device) {
                                log::warn!("enforce: re-assert failed: {:?}", e);
                            } else {
                                // Device now matches intent on the enforced fields;
                                // reflect that in `observed` (preserving the real
                                // brightness) so the tooltip/icon don't show the
                                // stale drift until the next read.
                                let brightness = state.observed.lights_mode.keyboard_brightness;
                                state.observed = state.device_state;
                                state.observed.lights_mode.keyboard_brightness = brightness;
                            }
                        }
                    }
                    // An interrupted two-zone write leaves the zones disagreeing, and every
                    // read fails until something rewrites both. Repair it from intent,
                    // regardless of Enforce: this is a broken state, not a user choice.
                    Err(e)
                        if e.downcast_ref::<librazer::error::PerfZonesDiverged>()
                            .is_some() =>
                    {
                        state.reconcile(&mut tray_icon, &device, "mirror (zones diverged)", true);
                    }
                    // The bus itself is failing: stop polling and let the resync (with its
                    // device reopen) take over.
                    Err(e) if librazer::error::is_retryable(&e) => {
                        log::warn!("mirror read failed, will probe: {e:?}");
                        state.needs_sync = true;
                        state.sync_apply = false;
                        next_sync_at = now + resync_backoff(0);
                        sync_failures = sync_failures.max(1);
                    }
                    Err(_) => {}
                }
                if !state.needs_sync {
                    state.refresh_fan(&device);
                }
                state.refresh_ui(&tray_icon);
            }

            // Always-on backlight is the Normal-mode keep-alive above (a periodic read
            // that re-brightens the EC's idle-fade). We deliberately do NOT use the
            // firmware "device mode" flag for it -- driver mode disables the Fn media
            // keys. When always-on is off there's no polling: the keyboard fades/off
            // naturally and the Fn keys behave normally.

            Ok(())
        })() {
            // One failed exchange. The intent in `state` is kept as-is (including a running
            // Action) and the retry at the top of the loop re-applies it. The old handler
            // re-read the config and re-applied the SAVED profile, which reverted a change
            // whose only failure was the read that followed it, and it blocked inside this
            // callback until the device answered.
            // Only device trouble resyncs; a failed "Close GPU apps" or a menu hiccup is not
            // a reason to rewrite the device.
            if !librazer::error::is_retryable(&e) {
                log::error!("tick failed: {:?}", e);
                return;
            }
            log::error!("tick failed, will resync: {:?}", e);
            state.sync_apply = true;
            if !state.needs_sync {
                state.needs_sync = true;
                next_sync_at = now + resync_backoff(0);
                sync_failures = sync_failures.max(1);
            }
        }
    })
}
