#![windows_subsystem = "windows"]

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
    TrayIconBuilder, TrayIconEvent,
};

use single_instance::SingleInstance;

#[cfg(target_os = "windows")]
use std::sync::atomic::Ordering;

use program::ProgramState;
use state::{get_fan_rpm, ConfigState, DeviceState};

pub const PKG_NAME: &str = env!("CARGO_PKG_NAME");

pub fn get_logging_file_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{}.log", PKG_NAME))
}

fn init_logging_to_file() -> Result<()> {
    use log4rs::append::rolling_file::policy::compound::{
        roll::delete::DeleteRoller, trigger::size::SizeTrigger, CompoundPolicy,
    };
    let policy = CompoundPolicy::new(
        Box::new(SizeTrigger::new(50 << 20)),
        Box::new(DeleteRoller::new()),
    );

    let logfile = log4rs::append::rolling_file::RollingFileAppender::builder()
        .encoder(Box::new(log4rs::encode::pattern::PatternEncoder::new(
            "{h({d(%Y-%m-%d %H:%M:%S)(local)} - {l}: {m}{n})}",
        )))
        .build(get_logging_file_path(), Box::new(policy))?;

    let config = log4rs::config::Config::builder()
        .appender(log4rs::config::Appender::builder().build("logfile", Box::new(logfile)))
        .build(
            log4rs::config::Root::builder()
                .appender("logfile")
                .build(log::LevelFilter::Trace),
        )?;

    log4rs::init_config(config)?;
    Ok(())
}

fn init(tray_icon: &mut tray_icon::TrayIcon, device: &device::Device) -> Result<ProgramState> {
    log::info!(
        "loading config file {}",
        confy::get_configuration_file_path(PKG_NAME, None)?.display()
    );
    let config: ConfigState = confy::load(PKG_NAME, None).unwrap_or_default();
    let fan_actual = get_fan_rpm(device)?;
    let mut state = ProgramState::new(config.ac_state, fan_actual, config.enforce)?;
    state.ac_power = platform::get_power_state()?;
    state.ac_state = config.ac_state;
    state.battery_state = config.battery_state;
    if !state.ac_power {
        state.device_state = state.battery_state
    }
    state.update(tray_icon, state.device_state, device)?;
    Ok(state)
}

fn main() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // Initialize GTK for tray icon on Linux
        gtk::init().map_err(|_| anyhow::anyhow!("Failed to initialize GTK"))?;
    }

    #[cfg(target_os = "windows")]
    platform::efficiency_mode();

    // Start the display-state monitor (event-driven; gates always-on backlight).
    #[cfg(target_os = "windows")]
    platform::spawn_display_state_monitor();

    // Create a named mutex (unique string for your app)
    let instance = SingleInstance::new("razer-tray").unwrap();
    if !instance.is_single() {
        log::info!("Another instance is already running. Exiting.");
        return Ok(());
    }

    init_logging_to_file()?;
    log::info!("{0} starting {1} {0}", "==".repeat(20), PKG_NAME);

    let device = match device::Device::detect() {
        Ok(d) => {
            log::info!("detected device: {} (0x{:04X})", d.info().name, d.info().pid);
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

    let mut tray_icon = TrayIconBuilder::new().build()?;

    let mut state: ProgramState = init(&mut tray_icon, &device)?;

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let event_loop = EventLoopBuilder::new().build();

    let mut last_device_state_check_timestamp = std::time::Instant::now();
    // The last-input tick recorded at the previous Mirror poll. We only re-poll when
    // this changes (new input), so polling follows your activity and stops when you
    // stop touching the machine. None = not yet polled / non-Windows (always refresh).
    let mut last_polled_input_tick: Option<u32> = None;
    // Tracks the last-seen console display power state, to drive the always-on
    // backlight gate only on transitions.
    #[cfg(target_os = "windows")]
    let mut last_display_on = true;
    // Tracks wall-clock between event-loop ticks. The loop ticks ~every second;
    // a gap far larger than that means the process was suspended (system sleep),
    // which we use as a cheap, API-free "resumed from sleep" signal.
    let mut last_tick_timestamp = std::time::Instant::now();

    // loop through the default start up sequence to initialise the device.
    for element in device.info().init_cmds {
        command::send_command(&device, *element, &[0, 0, 0, 0])?;
    }

    // Install a low-level keyboard hook used to refresh the tooltip on keypress.
    // If it fails we log and carry on -- it's a nice-to-have, not load-bearing.
    // We hold the handle for the life of the process: tao's event loop never
    // returns, so there's no reachable place to call UnhookWindowsHookEx, and
    // Windows reclaims low-level hooks automatically when the process exits.
    #[cfg(target_os = "windows")]
    let _keyboard_hook = unsafe {
        // SAFETY: SetWindowsHookExW with a valid extern "system" proc and a thread id of
        // 0 (all threads) for a WH_KEYBOARD_LL hook. We keep the returned HHOOK for the
        // process lifetime; Windows reclaims low-level hooks automatically on exit.
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, WH_KEYBOARD_LL};
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(platform::keyboard_hook_proc), None, 0) {
            Ok(hook) => Some(hook),
            Err(e) => {
                log::warn!("Failed to install keyboard hook ({e:?}); keypress tooltip refresh disabled");
                None
            }
        }
    };

    event_loop.run(move |_, _, control_flow| {
        let now = std::time::Instant::now();
        let since_last_tick = now.duration_since(last_tick_timestamp);
        last_tick_timestamp = now;
        *control_flow = ControlFlow::WaitUntil(now + std::time::Duration::from_millis(1000));

        if let Err(e) = (|| -> Result<()> {
            if let Ok(event) = menu_channel.try_recv() {
                log::info!("Menu Event {:?}", event.id);
                if event.id == MenuId("dgpu_terminate_proc".to_string()) {
                    log::info!("match event id");
                    platform::gpu_taskkill()?;
                } else if event.id == MenuId("toggle_enforce".to_string()) {
                    state.enforce = !state.enforce;
                    if let Err(e) = state.persist() {
                        log::warn!("Failed to persist enforce flag: {:?}", e);
                    }
                    // Rebuild the menu so the checkmark reflects the new state.
                    let (m, h) = menu::build(&state.device_state, state.enforce)?;
                    state.menu = m;
                    state.event_handlers = h;
                    tray_icon.set_menu(Some(Box::new(state.menu.clone())));
                    log::info!("enforce toggled to {}", state.enforce);
                } else if event.id == MenuId("toggle_autostart".to_string()) {
                    #[cfg(target_os = "windows")]
                    {
                        if let Err(e) = platform::set_autostart(!platform::autostart_enabled()) {
                            log::warn!("Failed to toggle autostart: {:?}", e);
                        }
                        // Rebuild the menu so the checkmark reflects the new state.
                        let (m, h) = menu::build(&state.device_state, state.enforce)?;
                        state.menu = m;
                        state.event_handlers = h;
                        tray_icon.set_menu(Some(Box::new(state.menu.clone())));
                    }
                } else {
                    let new_device_state = state.handle_event(event.id.as_ref())?;
                    log::info!("new_device_state 1 {:?}", new_device_state);
                    state.update(&mut tray_icon, new_device_state, &device)?;
                }
            }

            if matches!(tray_channel.try_recv(), Ok(event) if event.click_type == tray_icon::ClickType::Left) {
                let new_device_state = state.get_next_perf_mode();
                log::info!("new_device_state 2 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device)?;
            }

            state.ac_power = platform::get_power_state()?;
            if let Some(new_device_state) = state::profile_for_power(
                state.ac_power,
                &state.device_state,
                &state.ac_state,
                &state.battery_state,
            ) {
                log::info!("new_device_state 3 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device)?;
            }

            // Resume-from-sleep reassert. The event loop is frozen while the
            // machine sleeps, so a tick gap far larger than our ~1s cadence means
            // we just woke. Synapse (and sometimes firmware) re-assert their own
            // state on resume, so when Enforce is on we immediately re-assert ours
            // rather than waiting for the next 10s enforce poll. Brightness is left
            // alone (enforce_to omits it) so a pre-sleep Fn setting isn't clobbered.
            if state.enforce && since_last_tick > std::time::Duration::from_secs(30) {
                log::info!("resume detected (tick gap {:?}); re-asserting enforced state", since_last_tick);
                if let Err(e) = state.device_state.enforce_to(&device) {
                    log::warn!("enforce: resume re-assert failed: {:?}", e);
                }
            }

            // Always-on display gate. This unit's firmware keeps the keyboard lit
            // literally always, so we drop the firmware always-on flag when the
            // console display powers off (screen-off timeout / display sleep) and
            // restore it when the display comes back. Driven by the display-state
            // monitor (GUID_CONSOLE_DISPLAY_STATE) -> acts only on transitions, no
            // polling. Only touches the flag when the user has always-on enabled;
            // device_state (the menu's intent) is left unchanged.
            #[cfg(target_os = "windows")]
            {
                let display_on = platform::DISPLAY_ON.load(Ordering::Relaxed);
                if display_on != last_display_on {
                    last_display_on = display_on;
                    if state.device_state.lights_mode.always_on == LightsAlwaysOn::Enable {
                        let effective = if display_on {
                            LightsAlwaysOn::Enable
                        } else {
                            LightsAlwaysOn::Disable
                        };
                        match command::set_lights_always_on(&device, effective) {
                            Ok(()) => log::info!(
                                "display {} -> always-on {:?}",
                                if display_on { "on" } else { "off" },
                                effective
                            ),
                            Err(e) => log::warn!("display-gate: set_lights_always_on failed: {:?}", e),
                        }
                    }
                }
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
            if new_input
                && now > last_device_state_check_timestamp + std::time::Duration::from_secs(2)
            {
                last_device_state_check_timestamp = now;
                last_polled_input_tick = input_tick;
                if let Ok(observed) = DeviceState::read(&device) {
                    state.observed = observed;

                    // Adopt an externally-made keyboard-brightness change (e.g. the
                    // hardware Fn brightness keys) into the app's own state. We also
                    // write it into the active AC/battery profile and persist it, so
                    // (a) the menu checkmark reflects it, (b) it survives an AC/battery
                    // switch, and (c) the reconciliation step above sees device_state
                    // == the active profile and does NOT re-apply -- no tug-of-war.
                    let observed_brightness = state.observed.lights_mode.keyboard_brightness;
                    if observed_brightness != state.device_state.lights_mode.keyboard_brightness {
                        state.device_state.lights_mode.keyboard_brightness = observed_brightness;
                        if state.ac_power {
                            state.ac_state.lights_mode.keyboard_brightness = observed_brightness;
                        } else {
                            state.battery_state.lights_mode.keyboard_brightness = observed_brightness;
                        }
                        if let Err(e) = state.persist() {
                            log::warn!("failed to persist adopted brightness: {:?}", e);
                        }
                        if let Ok((menu, handlers)) = menu::build(&state.device_state, state.enforce) {
                            state.menu = menu;
                            state.event_handlers = handlers;
                            tray_icon.set_menu(Some(Box::new(state.menu.clone())));
                        }
                    }

                    // Enforce (opt-in): if the real device drifted from our intended
                    // state on a field we own -- perf mode, fan, logo, battery care --
                    // re-assert it. This is how razer-tray wins a tug-of-war with
                    // Synapse. Brightness is deliberately excluded (it stays on the
                    // adopt path above so the Fn keys keep working). It rides the same
                    // input-gated read above, so it adds no idle cost and reasserts
                    // whenever you're active (incl. right after you return to the
                    // machine); the resume-from-sleep reassert covers the wake case.
                    if state.enforce {
                        let drifted = state.observed.enforced_fields_differ(&state.device_state);
                        if drifted {
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
                }
                if let Ok(fan) = get_fan_rpm(&device) {
                    state.fan_actual = fan;
                }
                let _ = tray_icon.set_icon(Some(state.icon()));
                if let Ok(tooltip) = state.tooltip() {
                    let _ = tray_icon.set_tooltip(Some(tooltip));
                }
            }

            // Always-on backlight is handled by the firmware flag set in apply()
            // (command::set_lights_always_on), so there is no software heartbeat:
            // the keyboard stays lit until the display/system powers it down, with
            // zero polling. (A previous iteration polled every 5s gated on idle,
            // which incorrectly let the backlight time out while merely idle.)

            // Update fan RPM and tooltip whenever a key is pressed, since keypresses
            // already turn on the backlight; piggybacking here adds no idle-time cost.
            #[cfg(target_os = "windows")]
            if platform::KEY_PRESSED.swap(false, Ordering::Relaxed) {
                if let Ok(fan) = get_fan_rpm(&device) {
                    state.fan_actual = fan;
                    if let Ok(tooltip) = state.tooltip() {
                        let _ = tray_icon.set_tooltip(Some(tooltip));
                    }
                }
            }

            Ok(())
        })() {
            loop {
                log::error!("trying to recover from: {:?}", e);
                match init(&mut tray_icon, &device) {
                    Ok(new_state) => {
                        state = new_state;
                        break;
                    }
                    Err(e) => {
                        log::error!("failed to recover: {:?}", e);
                        // Sleep between attempts. We're inside this inner `loop`, so we
                        // never return to the event loop until init() succeeds -- which
                        // means `control_flow` has no effect here. Without a sleep a
                        // persistent failure (e.g. the device unplugged) would busy-spin
                        // this thread, pegging a core and spamming HID reads + the log.
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                    }
                }
            }
        }
    })
}
