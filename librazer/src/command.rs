//! Razer HID command layer.
//!
//! Each command is a 16-bit id, `(command_class << 8) | command_id`, sent as a feature
//! report over [`HidTransport`]. Setting the `0x80` bit on the command_id is the "get"
//! mirror of the corresponding "set" (e.g. `0x0303` set keyboard brightness / `0x0383` get;
//! `0x0004` set device mode / `0x0084` get). Command classes used here: `0x00` standard/
//! device, `0x03` lighting/LED, `0x07` battery, `0x0d` performance/fan.
//!
//! ⚠️ DEVICE MODE (`0x0004`) IS NOT A LIGHTING COMMAND -- it's the Razer "set device mode"
//! command: arg `0x00` = Normal (hardware) mode, `0x03` = Driver mode. In Driver mode the
//! keyboard hands key/light handling to a host driver (Synapse) and the EC stops emitting its
//! native Fn media keys -- screen brightness, volume, AND keyboard brightness all go dead.
//! It was historically mislabeled "lights always on" because Driver mode also skips the
//! firmware's idle dimming (so the backlight stays lit). DO NOT use Driver mode to keep the
//! backlight on: keep the device in Normal mode and re-brighten with a periodic read instead
//! (razer-tray's always-on keep-alive). Confirmed against OpenRazer's
//! `razer_chroma_standard_set_device_mode` (report `0x00/0x04`, modes `0x00`/`0x03`).

use crate::packet::Packet;
use crate::transport::HidTransport;
use crate::types::{
    BatteryCare, Cluster, CpuBoost, EcTemperatures, FanMode, FanZone, GpuBoost, KeyboardEffect,
    LightsAlwaysOn, LogoMode, MaxFanSpeedMode, PerfMode, PowerSource,
};

use anyhow::{bail, ensure, Context, Result};

fn _send_command(device: &impl HidTransport, command: u16, args: &[u8]) -> Result<Packet> {
    let response = device.send(Packet::new(command, args))?;
    ensure!(response.get_args().starts_with(args));
    Ok(response)
}

fn _set_perf_mode(
    device: &impl HidTransport,
    perf_mode: PerfMode,
    fan_mode: FanMode,
) -> Result<()> {
    [1, 2].into_iter().try_for_each(|zone| {
        _send_command(
            device,
            0x0d02,
            &[0x01, zone, perf_mode as u8, fan_mode as u8],
        )
        .map(|_| ())
    })
}

fn _set_boost(device: &impl HidTransport, cluster: Cluster, boost: u8) -> Result<()> {
    let args = &[0x01, cluster as u8, boost];
    ensure!(
        get_perf_mode(device)?.0 == PerfMode::Custom,
        "Performance mode must be {:?}",
        PerfMode::Custom
    );
    ensure!(device
        .send(Packet::new(0x0d07, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

fn _get_boost(device: &impl HidTransport, cluster: Cluster) -> Result<u8> {
    let response = device.send(Packet::new(0x0d87, &[0, cluster as u8, 0]))?;
    ensure!(response.get_args()[1] == cluster as u8);
    Ok(response.get_args()[2])
}

pub fn set_perf_mode(device: &impl HidTransport, perf_mode: PerfMode) -> Result<()> {
    _set_perf_mode(device, perf_mode, FanMode::Auto)
}

/// Read the perf/fan mode, which the EC mirrors across its two zones.
///
/// The two zones are separate HID round-trips, so a mode change landing between them
/// (a tray Actions transition, an AC/battery switch, Synapse) makes the reads disagree
/// through no fault of the device. That used to be a hard error -- and it is the source
/// of the cosmetic "Modes do not match" line seen in `auto info`. A disagreement is now
/// re-read once before failing, which resolves the race in the common case.
pub fn get_perf_mode(device: &impl HidTransport) -> Result<(PerfMode, FanMode)> {
    fn read_zones(
        device: &impl HidTransport,
    ) -> Result<((PerfMode, FanMode), (PerfMode, FanMode))> {
        let [r1, r2]: [Result<(PerfMode, FanMode)>; 2] = [1, 2].map(|zone| {
            let response = device.send(Packet::new(0x0d82, &[0, zone, 0, 0]))?;
            Ok((
                PerfMode::try_from(response.get_args()[2])?,
                FanMode::try_from(response.get_args()[3])?,
            ))
        });

        // Keep the cause's type (busy, transport, "not supported"): it decides whether a
        // caller retries and which exit code the CLI gives. A formatted ensure! dropped it.
        let r1 = r1.context("Failed to get performance mode and fan mode (zone 1)")?;
        let r2 = r2.context("Failed to get performance mode and fan mode (zone 2)")?;
        Ok((r1, r2))
    }

    let (r1, r2) = read_zones(device)?;
    if r1 == r2 {
        return Ok(r1);
    }

    // Disagreement: either a mode changed mid-read (transient -- a re-read agrees) or the
    // zones genuinely diverged (persists).
    let (r1, r2) = read_zones(device)?;
    if r1 != r2 {
        // Typed, so the tray can repair it (re-assert intent to both zones) instead of
        // failing every read until the user happens to pick a mode by hand.
        return Err(anyhow::Error::new(crate::error::PerfZonesDiverged(
            format!("zone 1 = {r1:?}, zone 2 = {r2:?}"),
        )));
    }

    Ok(r1)
}

pub fn set_cpu_boost(device: &impl HidTransport, boost: CpuBoost) -> Result<()> {
    _set_boost(device, Cluster::Cpu, boost as u8)
}

pub fn set_gpu_boost(device: &impl HidTransport, boost: GpuBoost) -> Result<()> {
    _set_boost(device, Cluster::Gpu, boost as u8)
}

pub fn get_cpu_boost(device: &impl HidTransport) -> Result<CpuBoost> {
    CpuBoost::try_from(_get_boost(device, Cluster::Cpu)?)
}

pub fn get_gpu_boost(device: &impl HidTransport) -> Result<GpuBoost> {
    GpuBoost::try_from(_get_boost(device, Cluster::Gpu)?)
}

pub fn set_fan_rpm(device: &impl HidTransport, rpm: u16, check_mode: bool) -> Result<()> {
    // Bound against the widest range any known chassis supports, rather than a magic
    // 5500 that matched no real machine. The per-device envelope is narrower still
    // (`Descriptor::fan_rpm_range`, which is what the UI offers) -- this check only
    // rejects values that could not be meaningful on any Blade.
    //
    // BOTH ends are checked, and the lower one is the load-bearing half. Above the range
    // the EC clamps and the fan visibly runs flat out, so an unchecked high value is
    // merely imprecise. Below it, the EC accepts the write, keeps the value in the
    // read-back register, and silently spins at its floor anyway -- measured 2026-08-15
    // on 0x029F, where set points of 1800 down to 0 all produced exactly 2000 RPM. So an
    // unchecked low value returns Ok(()) while doing nothing, and every caller above
    // here goes on to report a speed the hardware is ignoring. Reject it instead, and
    // note that the floor being global means a *chassis*-legal-but-lower value still
    // needs the per-device range to catch it; this is the backstop, not the policy.
    ensure!(
        (crate::state::FAN_RPM_MIN_ANY..=crate::state::FAN_RPM_MAX_ANY).contains(&rpm),
        "Fan RPM {} is out of range ({}..={} across all known chassis); below the minimum \
         the EC silently holds its floor instead of reporting an error, so this would \
         appear to succeed while changing nothing",
        rpm,
        crate::state::FAN_RPM_MIN_ANY,
        crate::state::FAN_RPM_MAX_ANY
    );
    // The wire carries rpm/100, so 2050 used to go out as 20 and read back as 2000: a
    // set point that could never be confirmed, which Enforce then saw as permanent drift.
    ensure!(
        rpm.is_multiple_of(100),
        "Fan RPM {rpm} is not a multiple of 100; the EC stores rpm/100, so it would be \
         written as {} and never read back as {rpm}",
        rpm / 100 * 100
    );
    if check_mode {
        ensure!(
            matches!(get_perf_mode(device)?, (_, FanMode::Manual)),
            "Fan mode must be set to {:?}",
            FanMode::Manual
        );
    }
    [FanZone::Zone1, FanZone::Zone2]
        .into_iter()
        .try_for_each(|zone| {
            _send_command(device, 0x0d01, &[0, zone as u8, (rpm / 100) as u8]).map(|_| ())
        })
}

pub fn get_fan_rpm(device: &impl HidTransport, fan_zone: FanZone) -> Result<u16> {
    let response = device.send(Packet::new(0x0d81, &[0, fan_zone as u8, 0]))?;
    ensure!(response.get_args()[1] == fan_zone as u8);
    Ok(response.get_args()[2] as u16 * 100)
}

/// Whether the manual set points have left `intended_rpm`, given that a zone 1 read just
/// made matched it (`DeviceState::read` reports only zone 1).
///
/// Every write here sets both zones to the same value, so a difference means something
/// else wrote one zone (another tool), or a two-zone write was cut off between the zones
/// (a process killed mid-write). A zone-1-only read cannot see either, so Enforce never
/// repaired it. Reported by upstream (blauzim/razer-ctl 435c870).
///
/// A difference is re-read once, both zones, before it is believed: another process's
/// two-zone write can land between our reads, as with the perf zones in `get_perf_mode`.
/// The re-read is compared with `intended_rpm`, not only zone against zone, so a write
/// that moved both zones together between our reads is drift too.
/// Costs one extra read when the zones agree. Returns a bool rather than a typed error on
/// purpose: unlike the perf zones, divergence here is treated as drift (repaired only
/// where the caller already writes), because zone 2's set-point read-back is measured on
/// `0x029F` only (2026-10-14, echoes zone 1).
pub fn fan_setpoint_zones_differ(device: &impl HidTransport, intended_rpm: u16) -> Result<bool> {
    if get_fan_rpm(device, FanZone::Zone2)? == intended_rpm {
        return Ok(false);
    }
    let zone1 = get_fan_rpm(device, FanZone::Zone1)?;
    let zone2 = get_fan_rpm(device, FanZone::Zone2)?;
    let drifted = zone1 != intended_rpm || zone2 != intended_rpm;
    if drifted {
        log::warn!(
            "fan set points off target {intended_rpm} RPM: zone 1 = {zone1} RPM, zone 2 = {zone2} RPM"
        );
    }
    Ok(drifted)
}

pub fn get_fan_actual_rpm(device: &impl HidTransport, fan_zone: FanZone) -> Result<u16> {
    let response = device.send(Packet::new(0x0d88, &[0, fan_zone as u8, 0]))?;
    ensure!(response.get_args()[1] == fan_zone as u8);
    Ok(response.get_args()[2] as u16 * 100)
}

/// What powers the laptop, as the EC reports it (`0x078c`); see [`PowerSource`]. Read-only.
pub fn get_power_source(device: &impl HidTransport) -> Result<PowerSource> {
    Ok(PowerSource::from_args(
        device.send(Packet::new(0x078c, &[0, 0]))?.get_args(),
    ))
}

/// The CPU-side and GPU temperatures the EC reports (`0x0d85`); see [`EcTemperatures`].
/// Read-only. A model without the command answers "not supported" (0x05).
pub fn get_ec_temperatures(device: &impl HidTransport) -> Result<EcTemperatures> {
    let response = device.send(Packet::new(0x0d85, &[0, 0, 0]))?;
    EcTemperatures::from_args(response.get_args())
}

pub fn send_command(device: &impl HidTransport, command: u16, args: &[u8]) -> Result<Packet> {
    let response = device.send(Packet::new(command, args))?;
    Ok(response)
}

pub fn set_max_fan_speed_mode(device: &impl HidTransport, mode: MaxFanSpeedMode) -> Result<()> {
    ensure!(
        get_perf_mode(device)?.0 == PerfMode::Custom,
        "Performance mode must be {:?}",
        PerfMode::Custom
    );
    send_max_fan_speed_mode(device, mode)
}

/// The Max Fan write alone, for a caller that has read the perf mode itself: so a "not
/// supported" answer here is about Max Fan, never about the perf read.
pub fn send_max_fan_speed_mode(device: &impl HidTransport, mode: MaxFanSpeedMode) -> Result<()> {
    _send_command(device, 0x070f, &[mode as u8]).map(|_| ())
}

pub fn get_max_fan_speed_mode(device: &impl HidTransport) -> Result<MaxFanSpeedMode> {
    device.send(Packet::new(0x078f, &[0]))?.get_args()[0].try_into()
}

pub fn set_fan_mode(device: &impl HidTransport, mode: FanMode) -> Result<()> {
    _set_perf_mode(device, get_perf_mode(device)?.0, mode)
}

/// How long [`confirm_perf_mode`] waits before its one re-read.
const CONFIRM_REREAD_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Read the perf/fan mode back and fail with [`crate::error::PerfNotApplied`] unless it is
/// `perf_mode`/`fan_mode`. A write's own answer proves nothing: `0x029F` answered `0x0d02`
/// with success and its args echoed for writes it ignored (hours into Modern Standby,
/// 2026-10-19). One re-read after a short pause covers a mode that lands a moment late.
/// A laptop that can't read the mode back can't confirm a write, so there the write
/// stands, as it did before this check existed.
pub fn confirm_perf_mode(
    device: &impl HidTransport,
    perf_mode: PerfMode,
    fan_mode: FanMode,
) -> Result<()> {
    let wanted = (perf_mode, fan_mode);
    let mut read = match get_perf_mode(device) {
        Ok(read) => read,
        Err(e) if crate::error::is_not_supported(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    if read != wanted {
        device.pause(CONFIRM_REREAD_DELAY);
        read = get_perf_mode(device)?;
    }
    if read == wanted {
        Ok(())
    } else {
        Err(anyhow::Error::new(crate::error::PerfNotApplied {
            wanted,
            read,
        }))
    }
}

/// [`set_perf_mode`], then [`confirm_perf_mode`]. A perf mode write also sets the fan to
/// Auto.
pub fn set_perf_mode_confirmed(device: &impl HidTransport, perf_mode: PerfMode) -> Result<()> {
    set_perf_mode(device, perf_mode)?;
    confirm_perf_mode(device, perf_mode, FanMode::Auto)
}

/// [`set_fan_mode`], then [`confirm_perf_mode`].
pub fn set_fan_mode_confirmed(device: &impl HidTransport, mode: FanMode) -> Result<()> {
    let perf_mode = get_perf_mode(device)?.0;
    _set_perf_mode(device, perf_mode, mode)?;
    confirm_perf_mode(device, perf_mode, mode)
}

pub fn custom_command(device: &impl HidTransport, command: u16, args: &[u8]) -> Result<()> {
    // try_new: `args` comes straight from the CLI, so an over-long list must be a clean
    // error rather than a panic in the fixed 80-byte arg buffer.
    let report = Packet::try_new(command, args)?;
    println!("Report   {:?}", report);
    // send_once: an arbitrary command is not known to be idempotent, so a Failure answer
    // is reported rather than re-sent four more times.
    let response = device.send_once(report)?;
    println!("Response {:?}", response);
    Ok(())
}

/// Like [`custom_command`], but send at a caller-chosen transaction id (for probing
/// commands the reference drivers issue at an id other than our default 0x1F).
pub fn custom_command_tx(
    device: &impl HidTransport,
    command: u16,
    args: &[u8],
    tx: u8,
) -> Result<()> {
    let report = Packet::try_new_with_tx(command, args, tx)?;
    println!("Report   {:?}", report);
    let response = device.send_once(report)?;
    println!("Response {:?}", response);
    Ok(())
}

fn _set_logo_power(device: &impl HidTransport, mode: LogoMode) -> Result<Packet> {
    match mode {
        LogoMode::Off => _send_command(device, 0x0300, &[1, 4, 0]),
        LogoMode::Static | LogoMode::Breathing => _send_command(device, 0x0300, &[1, 4, 1]),
    }
}

fn _set_logo_mode(device: &impl HidTransport, mode: LogoMode) -> Result<Packet> {
    match mode {
        LogoMode::Static => _send_command(device, 0x0302, &[1, 4, 0]),
        LogoMode::Breathing => _send_command(device, 0x0302, &[1, 4, 2]),
        _ => bail!("Invalid logo mode"),
    }
}

fn _get_logo_power(device: &impl HidTransport) -> Result<bool> {
    match device.send(Packet::new(0x0380, &[1, 4, 0]))?.get_args()[2] {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!("Invalid logo power state"),
    }
}

fn _get_logo_mode(device: &impl HidTransport) -> Result<LogoMode> {
    match device.send(Packet::new(0x0382, &[1, 4, 0]))?.get_args()[2] {
        0 => Ok(LogoMode::Static),
        2 => Ok(LogoMode::Breathing),
        _ => bail!("Invalid logo power state"),
    }
}

pub fn get_logo_mode(device: &impl HidTransport) -> Result<LogoMode> {
    let power = _get_logo_power(device)?;
    match power {
        true => _get_logo_mode(device),
        false => Ok(LogoMode::Off),
    }
}

pub fn set_logo_mode(device: &impl HidTransport, mode: LogoMode) -> Result<()> {
    if mode != LogoMode::Off {
        _set_logo_mode(device, mode)?;
    }
    _set_logo_power(device, mode)?;
    Ok(())
}

pub fn get_keyboard_brightness(device: &impl HidTransport) -> Result<u8> {
    let response = device.send(Packet::new(0x0383, &[1, 5, 0]))?;
    ensure!(response.get_args()[1] == 5);
    Ok(response.get_args()[2])
}

pub fn set_keyboard_brightness(device: &impl HidTransport, brightness: u8) -> Result<()> {
    let args = &[1, 5, brightness];
    ensure!(device
        .send(Packet::new(0x0303, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

// ---- keyboard RGB effect (0x0f02 extended-matrix effect; LED region 0x05 = backlight) ----
//
// HW-verified on 0x029F (2026-07-10): applies in Normal device mode (Fn keys survive), no
// driver mode needed. The effect can be read back through 0x0f82 (see `get_keyboard_effect`,
// HW-verified 2026-07-25), but it is still held as intent and re-applied rather than
// reconciled (see `DeviceState::apply_keyboard_lighting`).
//
// These are the EC-animated effects. The EC's own static-color effect ignores the color and
// shows Razer green, but a custom color frame does work in Normal mode (`set_keyboard_frame`,
// HW-verified 2026-09-23); this project wrongly called that impossible until then. All four
// effects HW-confirmed on 0x029F at our default 0x1F transaction.

/// Effect-command LED region: openrazer "backlight" (the whole keyboard).
const KBD_LED_BACKLIGHT: u8 = 0x05;
/// Variable-storage byte: VARSTORE writes the EC's persisted slot so the effect *sticks* and
/// survives idle/wake. (NOSTORE only stages a volatile buffer the EC doesn't display, so the
/// stored default -- rainbow -- returns; HW-confirmed 2026-07-10.)
const KBD_STORE_VAR: u8 = 0x01;
/// Default Wave direction (0x00..=0x02 accepted; the EC clamps).
const KBD_WAVE_DIR: u8 = 0x01;

/// Set the keyboard backlight effect. Write-only: the response is not echo-checked because the
/// effect command does not mirror its args back the way the `set_*`/`get_*` register pairs do
/// (P0 read status 0x02 = success, not an arg echo). Effect ids are the extended-matrix values
/// (openrazer `razerchromacommon.c`): Off=0x00, Breathing(random)=0x02, Spectrum=0x03, Wave=0x04.
pub fn set_keyboard_effect(device: &impl HidTransport, effect: KeyboardEffect) -> Result<()> {
    let args: Vec<u8> = match effect {
        KeyboardEffect::Off => vec![KBD_STORE_VAR, KBD_LED_BACKLIGHT, 0x00],
        KeyboardEffect::Breathing => vec![KBD_STORE_VAR, KBD_LED_BACKLIGHT, 0x02],
        KeyboardEffect::Spectrum => vec![KBD_STORE_VAR, KBD_LED_BACKLIGHT, 0x03],
        KeyboardEffect::Wave => vec![KBD_STORE_VAR, KBD_LED_BACKLIGHT, 0x04, KBD_WAVE_DIR],
    };
    device.send(Packet::new(0x0f02, &args))?;
    Ok(())
}

/// Read the keyboard backlight effect back from the EC (`0x0f82`, the get-mirror of `0x0f02`).
///
/// **This project previously documented Chroma as write-only with no getter on this device,
/// and that was wrong.** HW-verified on PID 0x029F (2026-07-25): `0x0f82` with args
/// `[VARSTORE, backlight, 0]` returns `[0x01, 0x05, <effect id>, <param>, ...]` echoing
/// exactly what was last written -- Spectrum -> 3, Wave -> 4 (plus its direction byte),
/// Breathing -> 2, Off -> 0. Confirmed to be a real EC read rather than a process-local
/// cache by writing in one `razer-cli` process and reading in another.
///
/// A `None` return means the EC reported an effect id we do not model (e.g. Reactive or
/// Static, which need driver mode and a host color, so we deliberately don't offer them).
/// That is not an error -- it just means "something we didn't set", and callers should
/// treat it as unknown rather than fighting it.
pub fn get_keyboard_effect(device: &impl HidTransport) -> Result<Option<KeyboardEffect>> {
    let response = device.send(Packet::new(
        0x0f82,
        &[KBD_STORE_VAR, KBD_LED_BACKLIGHT, 0x00],
    ))?;
    let args = response.get_args();
    ensure!(
        args[1] == KBD_LED_BACKLIGHT,
        "effect read answered for LED region {:#04x}, expected backlight {:#04x}",
        args[1],
        KBD_LED_BACKLIGHT
    );
    Ok(match args[2] {
        0x00 => Some(KeyboardEffect::Off),
        0x02 => Some(KeyboardEffect::Breathing),
        0x03 => Some(KeyboardEffect::Spectrum),
        0x04 => Some(KeyboardEffect::Wave),
        _ => None,
    })
}

/// Show a custom color frame (see [`crate::keyboard`]): one `0x030b` write per row,
/// `[0xff, row, first col, last col, RGB x 16]`, then `0x030a [custom, no-store]` to display
/// them. Works in Normal device mode, so the Fn keys keep working; not stored in the EC.
pub fn set_keyboard_frame(
    device: &impl HidTransport,
    frame: &crate::keyboard::Frame,
) -> Result<()> {
    use crate::keyboard::COLS;
    for (row, colors) in frame.iter().enumerate() {
        let mut args = Vec::with_capacity(4 + 3 * COLS);
        args.extend_from_slice(&[0xff, row as u8, 0, (COLS - 1) as u8]);
        for c in colors {
            args.extend_from_slice(&[c.r, c.g, c.b]);
        }
        device.send(Packet::new(0x030b, &args))?;
    }
    device.send(Packet::new(
        0x030a,
        &[KBD_EFFECT_CUSTOM_FRAME, KBD_NO_STORE],
    ))?;
    Ok(())
}

/// `0x030a` effect id that displays the frame written by `0x030b`.
const KBD_EFFECT_CUSTOM_FRAME: u8 = 0x05;
/// Storage byte: the frame is volatile. Storing frames wears the EC's flash, and OpenRazer
/// stopped doing it for that reason.
const KBD_NO_STORE: u8 = 0x00;

/// Read the Razer **device mode** (the `0x0084` get-mirror of `0x0004`). See the module
/// docs: `Disable` (0x00) is Normal/hardware mode, `Enable` (0x03) is Driver mode.
pub fn get_lights_always_on(device: &impl HidTransport) -> Result<LightsAlwaysOn> {
    device.send(Packet::new(0x0084, &[0, 0]))?.get_args()[0].try_into()
}

/// Set the Razer **device mode** -- this is the `0x0004` command, NOT a lighting toggle.
/// `LightsAlwaysOn::Disable` (0x00) = Normal/hardware mode; `Enable` (0x03) = Driver mode,
/// which disables the EC's native Fn media keys (see module docs). The tray only ever sets
/// Normal mode; "keyboard always-on" is a Normal-mode keep-alive, not Driver mode.
pub fn set_lights_always_on(
    device: &impl HidTransport,
    lights_always_on: LightsAlwaysOn,
) -> Result<()> {
    let args = &[lights_always_on as u8, 0];
    ensure!(device
        .send(Packet::new(0x0004, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

/// Read the charge limit. An "off" answer is read a second time before it's believed.
///
/// On `0x029F` (2026-10-15), `auto json` read the limit as off three times in an hour while
/// the battery sat at 74% on AC for 19 hours, so the limit was in force: those were false
/// reads. About 17,000 later reads didn't reproduce one, so the byte behind them is unknown.
/// Every false read seen so far said off, so an off that the second read contradicts is
/// taken as false. Either way a mismatch is logged with both raw bytes, so the next one is
/// caught. Costs one more read and 50 ms when the limit really is off.
pub fn get_battery_care(device: &impl HidTransport) -> Result<BatteryCare> {
    let read = || -> Result<(BatteryCare, u8, String)> {
        let response = device.send(Packet::new(0x0792, &[0]))?;
        let byte = response.get_args()[0];
        Ok((byte.try_into()?, byte, response.header()))
    };
    let (first, first_byte, first_header) = read()?;
    if !first.is_disabled() {
        return Ok(first);
    }
    device.pause(std::time::Duration::from_millis(50));
    match read() {
        Ok((_, second_byte, _)) if second_byte == first_byte => Ok(first),
        Ok((second, second_byte, second_header)) => {
            log::warn!(
                "charge limit read as off ({first_byte:#04x}; {first_header}), then \
                 {second} ({second_byte:#04x}; {second_header}); using the second"
            );
            Ok(second)
        }
        // A model without the command answers that way every time; let the caller learn it.
        Err(e) if crate::error::is_not_supported(&e) => Err(e),
        // The confirming read failed; the first answer is all there is.
        Err(e) => {
            log::warn!(
                "charge limit read as off ({first_byte:#04x}; {first_header}); \
                 the confirming read failed: {e}"
            );
            Ok(first)
        }
    }
}

pub fn set_battery_care(device: &impl HidTransport, mode: BatteryCare) -> Result<()> {
    let args = &[mode.wire_byte()];
    ensure!(device
        .send(Packet::new(0x0712, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockTransport;

    /// Build a canned-response packet whose args buffer begins with `args`
    /// (everything past that stays zero), mimicking a register read.
    fn reply(args: &[u8]) -> Packet {
        let mut p = Packet::new(0, &[]);
        p.set_args(args);
        p
    }

    // ---- write path: the bytes we put on the wire -------------------------------

    #[test]
    fn set_perf_mode_writes_both_fan_zones() {
        let mock = MockTransport::echo();
        set_perf_mode(&mock, PerfMode::Performance).unwrap();
        // One 0x0d02 per zone, carrying [enable, zone, perf=2, fan=Auto=0].
        assert_eq!(
            mock.sent(),
            vec![
                (
                    0x0d02,
                    vec![0x01, 1, PerfMode::Performance as u8, FanMode::Auto as u8]
                ),
                (
                    0x0d02,
                    vec![0x01, 2, PerfMode::Performance as u8, FanMode::Auto as u8]
                ),
            ]
        );
    }

    #[test]
    fn set_keyboard_brightness_emits_keyboard_payload() {
        let mock = MockTransport::echo();
        set_keyboard_brightness(&mock, 128).unwrap();
        assert_eq!(mock.sent(), vec![(0x0303, vec![1, 5, 128])]);
    }

    #[test]
    fn set_fan_rpm_writes_both_zones_as_rpm_over_100() {
        let mock = MockTransport::echo();
        set_fan_rpm(&mock, 2400, false).unwrap();
        assert_eq!(
            mock.sent(),
            vec![(0x0d01, vec![0, 1, 24]), (0x0d01, vec![0, 2, 24])]
        );
    }

    #[test]
    fn set_fan_rpm_rejects_below_the_floor_without_reaching_the_wire() {
        // The lower bound is not cosmetic. Measured on 0x029F (2026-08-15): the EC accepts
        // a sub-floor write, keeps it in the read-back register, and holds 2000 RPM anyway.
        // So sending it would return Ok(()) while changing nothing, and every layer above
        // would then display a speed the fan is ignoring. Nothing may go out.
        for rpm in [0, 100, 800, crate::state::FAN_RPM_MIN_ANY - 100] {
            let mock = MockTransport::echo();
            let err = set_fan_rpm(&mock, rpm, false).unwrap_err();
            assert!(
                err.to_string().contains("out of range"),
                "rpm {rpm}: unexpected error {err}"
            );
            assert!(
                mock.sent().is_empty(),
                "rpm {rpm} was rejected but still hit the wire: {:?}",
                mock.sent()
            );
        }
    }

    #[test]
    fn set_fan_rpm_accepts_both_ends_of_the_global_range() {
        // Boundary values are legal, not off-by-one victims: FAN_RPM_MIN_ANY is a real
        // measured set point on 0x029F (2000 -> exactly 2000 RPM), not a safety margin.
        for rpm in [crate::state::FAN_RPM_MIN_ANY, crate::state::FAN_RPM_MAX_ANY] {
            let mock = MockTransport::echo();
            set_fan_rpm(&mock, rpm, false).unwrap();
            assert_eq!(
                mock.sent().len(),
                2,
                "rpm {rpm}: both zones must be written"
            );
        }
    }

    #[test]
    fn a_false_off_reading_is_corrected_by_the_second_read() {
        let reads = std::cell::Cell::new(0);
        let mock = MockTransport::with_responder(move |p| {
            assert_eq!(p.command(), 0x0792);
            reads.set(reads.get() + 1);
            // First answer: a zeroed byte (decodes as off). Second: enabled at 60%.
            reply(&[if reads.get() == 1 { 0x00 } else { 0xBC }])
        });
        assert_eq!(
            get_battery_care(&mock).unwrap(),
            BatteryCare::from_percent(60).unwrap()
        );
        assert_eq!(mock.sent_count(), 2);
    }

    #[test]
    fn a_real_off_reads_off_twice_and_an_enabled_limit_reads_once() {
        let off = MockTransport::with_responder(|_| reply(&[0x50]));
        assert_eq!(get_battery_care(&off).unwrap(), BatteryCare::DISABLE);
        assert_eq!(off.sent_count(), 2);

        let on = MockTransport::with_responder(|_| reply(&[0xBC]));
        assert_eq!(
            get_battery_care(&on).unwrap(),
            BatteryCare::from_percent(60).unwrap()
        );
        assert_eq!(on.sent_count(), 1);
    }

    #[test]
    fn a_perf_write_stands_on_a_laptop_that_cant_read_the_mode() {
        let mock = MockTransport::with_fallible_responder(|req| {
            if req.command() == 0x0d82 {
                Err(anyhow::Error::new(
                    crate::packet::ResponseError::NotSupported,
                ))
            } else {
                Ok(crate::packet::Packet::new(req.command(), req.get_args()))
            }
        });
        set_perf_mode_confirmed(&mock, PerfMode::Silent).unwrap();
        assert!(
            mock.sent().iter().any(|(c, _)| *c == 0x0d02),
            "the write was sent"
        );
    }

    #[test]
    fn a_confirming_not_supported_is_passed_up_for_the_caller_to_learn() {
        let reads = std::cell::Cell::new(0);
        let mock = MockTransport::with_fallible_responder(move |_| {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                Ok(reply(&[0x50]))
            } else {
                Err(anyhow::Error::new(
                    crate::packet::ResponseError::NotSupported,
                ))
            }
        });
        assert!(crate::error::is_not_supported(
            &get_battery_care(&mock).unwrap_err()
        ));
    }

    #[test]
    fn set_battery_care_emits_single_byte_register() {
        let mock = MockTransport::echo();
        set_battery_care(&mock, BatteryCare::from_percent(80).unwrap()).unwrap();
        assert_eq!(
            mock.sent(),
            vec![(
                0x0712,
                vec![BatteryCare::from_percent(80).unwrap().wire_byte()]
            )]
        );
    }

    #[test]
    fn set_logo_mode_off_only_touches_power_register() {
        // Off must not send a logo-*mode* (0x0302) write, just power off (0x0300).
        let mock = MockTransport::echo();
        set_logo_mode(&mock, LogoMode::Off).unwrap();
        assert_eq!(mock.sent(), vec![(0x0300, vec![1, 4, 0])]);
    }

    #[test]
    fn set_logo_mode_breathing_sets_mode_then_power() {
        let mock = MockTransport::echo();
        set_logo_mode(&mock, LogoMode::Breathing).unwrap();
        assert_eq!(
            mock.sent(),
            vec![(0x0302, vec![1, 4, 2]), (0x0300, vec![1, 4, 1])]
        );
    }

    #[test]
    fn set_keyboard_effect_emits_backlight_effect_command() {
        use crate::types::KeyboardEffect;
        // Every effect targets LED region 0x05 (backlight) with VARSTORE (0x01) storage,
        // command 0x0f02. Effect ids: Off=0x00, Breathing=0x02, Spectrum=0x03, Wave=0x04
        // (Wave carries a trailing direction byte). None carry a host color (effects-only v1).
        let off = MockTransport::echo();
        set_keyboard_effect(&off, KeyboardEffect::Off).unwrap();
        assert_eq!(off.sent(), vec![(0x0f02, vec![0x01, 0x05, 0x00])]);

        let breathing = MockTransport::echo();
        set_keyboard_effect(&breathing, KeyboardEffect::Breathing).unwrap();
        assert_eq!(breathing.sent(), vec![(0x0f02, vec![0x01, 0x05, 0x02])]);

        let spectrum = MockTransport::echo();
        set_keyboard_effect(&spectrum, KeyboardEffect::Spectrum).unwrap();
        assert_eq!(spectrum.sent(), vec![(0x0f02, vec![0x01, 0x05, 0x03])]);

        let wave = MockTransport::echo();
        set_keyboard_effect(&wave, KeyboardEffect::Wave).unwrap();
        assert_eq!(wave.sent(), vec![(0x0f02, vec![0x01, 0x05, 0x04, 0x01])]);
    }

    // ---- read path: parsing the firmware's response -----------------------------

    #[test]
    fn get_perf_mode_parses_matching_zones() {
        // Both zones agree -> the parsed (PerfMode, FanMode) is returned.
        let mock = MockTransport::with_responder(|_| {
            reply(&[0, 0, PerfMode::Custom as u8, FanMode::Manual as u8])
        });
        assert_eq!(
            get_perf_mode(&mock).unwrap(),
            (PerfMode::Custom, FanMode::Manual)
        );
    }

    #[test]
    fn get_perf_mode_errors_when_zones_disagree() {
        // Respond per zone (args[1]) so the two reads conflict -> ensure! trips.
        let mock = MockTransport::with_responder(|req| {
            let zone = req.get_args()[1];
            let perf = if zone == 1 {
                PerfMode::Performance
            } else {
                PerfMode::Silent
            };
            reply(&[0, 0, perf as u8, FanMode::Auto as u8])
        });
        assert!(get_perf_mode(&mock).is_err());
    }

    #[test]
    fn get_keyboard_brightness_reads_third_arg() {
        // args[1] must be the keyboard LED id (5); the value lives in args[2].
        let mock = MockTransport::with_responder(|_| reply(&[1, 5, 200]));
        assert_eq!(get_keyboard_brightness(&mock).unwrap(), 200);
    }

    #[test]
    fn get_keyboard_brightness_rejects_wrong_led_id() {
        let mock = MockTransport::with_responder(|_| reply(&[1, 4, 200]));
        assert!(get_keyboard_brightness(&mock).is_err());
    }

    #[test]
    fn set_keyboard_frame_writes_six_rows_then_shows_them() {
        use crate::keyboard::{Frame, Rgb, COLS, ROWS};
        let mut frame: Frame = [[Rgb::BLACK; COLS]; ROWS];
        frame[1][2] = Rgb::new(0x11, 0x22, 0x33);
        let mock = MockTransport::echo();
        set_keyboard_frame(&mock, &frame).unwrap();
        let sent = mock.sent();
        assert_eq!(sent.len(), ROWS + 1);
        for (row, (cmd, args)) in sent[..ROWS].iter().enumerate() {
            assert_eq!(*cmd, 0x030b);
            assert_eq!(args.len(), 52);
            assert_eq!(args[..4], [0xff, row as u8, 0, 15]);
        }
        // Row 1, column 2: header (4) + 2 columns x 3.
        assert_eq!(sent[1].1[10..13], [0x11, 0x22, 0x33]);
        assert_eq!(sent[ROWS], (0x030a, vec![0x05, 0x00]));
    }

    #[test]
    fn get_keyboard_effect_decodes_the_effect_register() {
        use crate::types::KeyboardEffect;
        // 0x0f82 answers [store, led_region, effect_id, ...]. HW-verified ids on 0x029F.
        for (id, expected) in [
            (0x00, KeyboardEffect::Off),
            (0x02, KeyboardEffect::Breathing),
            (0x03, KeyboardEffect::Spectrum),
            (0x04, KeyboardEffect::Wave),
        ] {
            let mock = MockTransport::with_responder(move |_| reply(&[1, 5, id]));
            assert_eq!(
                get_keyboard_effect(&mock).unwrap(),
                Some(expected),
                "id {id:#04x}"
            );
        }

        // An effect we deliberately don't model (Static/Reactive need driver mode and a
        // host color) must read as "unknown", NOT as an error and NOT as a wrong variant
        // -- otherwise a Synapse-set effect would look like a failed read.
        let exotic = MockTransport::with_responder(|_| reply(&[1, 5, 0x07]));
        assert_eq!(get_keyboard_effect(&exotic).unwrap(), None);
    }

    #[test]
    fn get_keyboard_effect_rejects_a_reply_for_another_led_region() {
        // Region 4 is the lid logo. Accepting it would silently report the logo's effect
        // as the keyboard's.
        let mock = MockTransport::with_responder(|_| reply(&[1, 4, 0x03]));
        assert!(get_keyboard_effect(&mock).is_err());
    }

    #[test]
    fn get_keyboard_effect_queries_the_backlight_region() {
        let mock = MockTransport::with_responder(|_| reply(&[1, 5, 0x03]));
        get_keyboard_effect(&mock).unwrap();
        assert_eq!(mock.sent(), vec![(0x0f82, vec![0x01, 0x05, 0x00])]);
    }

    #[test]
    fn get_battery_care_decodes_wire_byte() {
        let mock = MockTransport::with_responder(|_| {
            reply(&[BatteryCare::from_percent(80).unwrap().wire_byte()])
        });
        assert_eq!(
            get_battery_care(&mock).unwrap(),
            BatteryCare::from_percent(80).unwrap()
        );
    }

    #[test]
    fn get_fan_rpm_scales_register_by_100() {
        // Register holds rpm/100; getter must rescale. args[1] must echo the zone.
        let mock = MockTransport::with_responder(|req| {
            let zone = req.get_args()[1];
            reply(&[0, zone, 30])
        });
        assert_eq!(get_fan_rpm(&mock, FanZone::Zone1).unwrap(), 3000);
    }

    #[test]
    fn fan_setpoint_zones_agreeing_cost_one_read() {
        let mock = MockTransport::with_responder(|req| reply(&[0, req.get_args()[1], 30]));
        assert!(!fan_setpoint_zones_differ(&mock, 3000).unwrap());
        assert_eq!(mock.sent(), vec![(0x0d81, vec![0, 2, 0])], "zone 2 only");
    }

    #[test]
    fn fan_setpoint_zones_persistently_diverged_are_reported() {
        // Zone 1 at 3000, zone 2 left at 4000 by a write cut off between the zones.
        let mock = MockTransport::with_responder(|req| {
            let zone = req.get_args()[1];
            reply(&[0, zone, if zone == 1 { 30 } else { 40 }])
        });
        assert!(fan_setpoint_zones_differ(&mock, 3000).unwrap());
        assert_eq!(mock.sent_count(), 3, "zone 2, then both zones to confirm");
    }

    #[test]
    fn fan_setpoint_zones_moved_together_off_target_are_reported() {
        // Another process moves both zones from 3000 to 4000 between the caller's zone 1
        // read and our zone 2 read; the re-read sees both at 4000, which is not the
        // intended 3000.
        let mock = MockTransport::with_responder(|req| reply(&[0, req.get_args()[1], 40]));
        assert!(fan_setpoint_zones_differ(&mock, 3000).unwrap());
        assert_eq!(mock.sent_count(), 3);
    }

    #[test]
    fn fan_setpoint_zones_back_on_target_after_a_race_are_not_reported() {
        // Zone 2 reads 4000 once (a write in flight), then both zones settle at 3000.
        let n = std::cell::Cell::new(0);
        let mock = MockTransport::with_responder(move |req| {
            n.set(n.get() + 1);
            reply(&[0, req.get_args()[1], if n.get() == 1 { 40 } else { 30 }])
        });
        assert!(!fan_setpoint_zones_differ(&mock, 3000).unwrap());
    }

    #[test]
    fn get_logo_mode_reports_off_without_reading_mode() {
        // Power register (0x0380) reads 0 -> Off, and the mode register is never read.
        let mock = MockTransport::with_responder(|_| reply(&[1, 4, 0]));
        assert_eq!(get_logo_mode(&mock).unwrap(), LogoMode::Off);
        assert_eq!(mock.sent(), vec![(0x0380, vec![1, 4, 0])]);
    }

    #[test]
    fn get_logo_mode_reads_mode_when_powered() {
        // Power on (0x0380 -> 1), then mode register (0x0382 -> 2) decodes to Breathing.
        let mock = MockTransport::with_responder(|req| match req.command() {
            0x0380 => reply(&[1, 4, 1]),
            0x0382 => reply(&[1, 4, 2]),
            other => panic!("unexpected command {other:#06x}"),
        });
        assert_eq!(get_logo_mode(&mock).unwrap(), LogoMode::Breathing);
    }

    // ---- a guard that combines a read precondition with a write -----------------

    #[test]
    fn set_cpu_boost_requires_custom_perf_mode() {
        // _set_boost first reads perf mode and demands Custom before writing 0x0d07.
        let custom = MockTransport::with_responder(|req| match req.command() {
            0x0d82 => reply(&[0, 0, PerfMode::Custom as u8, FanMode::Auto as u8]),
            _ => reply(&[0x01, Cluster::Cpu as u8, CpuBoost::Boost as u8]),
        });
        set_cpu_boost(&custom, CpuBoost::Boost).unwrap();
        assert!(custom.sent().iter().any(|(cmd, args)| *cmd == 0x0d07
            && args == &[0x01, Cluster::Cpu as u8, CpuBoost::Boost as u8]));

        // Not in Custom mode -> the boost write must be refused.
        let balanced = MockTransport::with_responder(|_| {
            reply(&[0, 0, PerfMode::Balanced as u8, FanMode::Auto as u8])
        });
        assert!(set_cpu_boost(&balanced, CpuBoost::Boost).is_err());
        assert!(!balanced.sent().iter().any(|(cmd, _)| *cmd == 0x0d07));
    }

    // ---- get_perf_mode: tolerate a mode change landing between the two zone reads ----

    #[test]
    fn get_perf_mode_retries_once_when_zones_disagree() {
        use std::cell::Cell;
        // Zone reads alternate 1,2,1,2. The first pair straddles a mode change
        // (Silent then Balanced); the re-read is consistent.
        let calls = Cell::new(0);
        let mock = MockTransport::with_responder(move |_req| {
            let n = calls.get();
            calls.set(n + 1);
            let perf = match n {
                0 => PerfMode::Silent as u8,
                1 => PerfMode::Balanced as u8,
                _ => PerfMode::Balanced as u8,
            };
            reply(&[0, 0, perf, FanMode::Auto as u8])
        });

        let (perf, fan) = get_perf_mode(&mock).expect("a mid-read mode change must not be fatal");
        assert_eq!(perf, PerfMode::Balanced);
        assert_eq!(fan, FanMode::Auto);
        assert_eq!(
            mock.sent_count(),
            4,
            "two zone reads, then two more to settle it"
        );
    }

    #[test]
    fn get_perf_mode_still_fails_when_zones_persistently_disagree() {
        use std::cell::Cell;
        // Genuinely divergent zones: zone 1 always Silent, zone 2 always Balanced.
        let calls = Cell::new(0);
        let mock = MockTransport::with_responder(move |_req| {
            let n = calls.get();
            calls.set(n + 1);
            let perf = if n % 2 == 0 {
                PerfMode::Silent as u8
            } else {
                PerfMode::Balanced as u8
            };
            reply(&[0, 0, perf, FanMode::Auto as u8])
        });

        let err = get_perf_mode(&mock).unwrap_err();
        assert!(
            err.downcast_ref::<crate::error::PerfZonesDiverged>()
                .is_some(),
            "divergence must be typed so the tray can repair it: {err}"
        );
    }

    #[test]
    fn get_perf_mode_reads_both_zones_once_when_they_agree() {
        let mock = MockTransport::with_responder(|_req| {
            reply(&[0, 0, PerfMode::Balanced as u8, FanMode::Auto as u8])
        });
        assert_eq!(
            get_perf_mode(&mock).unwrap(),
            (PerfMode::Balanced, FanMode::Auto)
        );
        assert_eq!(
            mock.sent_count(),
            2,
            "agreement must not cost an extra round-trip"
        );
    }

    // ---- a mode write the EC acknowledges and ignores (0x029F, Modern Standby) ----

    /// An EC whose perf/fan mode starts Balanced/Auto and takes `0x0d02` writes only if
    /// `applies`. Every write is acknowledged with its args echoed either way.
    fn ec_with_mode(applies: bool) -> MockTransport {
        let mode = std::cell::Cell::new((PerfMode::Balanced as u8, FanMode::Auto as u8));
        MockTransport::with_responder(move |req| match req.command() {
            0x0d02 => {
                if applies {
                    mode.set((req.get_args()[2], req.get_args()[3]));
                }
                Packet::new(req.command(), req.get_args())
            }
            0x0d82 => {
                let (perf, fan) = mode.get();
                reply(&[0, req.get_args()[1], perf, fan])
            }
            _ => Packet::new(req.command(), req.get_args()),
        })
    }

    #[test]
    fn a_confirmed_perf_write_passes_when_the_ec_takes_it() {
        set_perf_mode_confirmed(&ec_with_mode(true), PerfMode::Custom).unwrap();
        set_fan_mode_confirmed(&ec_with_mode(true), FanMode::Manual).unwrap();
    }

    #[test]
    fn a_confirmed_perf_write_reports_an_acknowledged_write_the_ec_ignored() {
        let ignoring = ec_with_mode(false);
        // The gap this closes: the plain write's own answer looks like success.
        set_perf_mode(&ignoring, PerfMode::Custom).unwrap();

        let err = set_perf_mode_confirmed(&ignoring, PerfMode::Custom).unwrap_err();
        assert_eq!(
            err.downcast_ref::<crate::error::PerfNotApplied>(),
            Some(&crate::error::PerfNotApplied {
                wanted: (PerfMode::Custom, FanMode::Auto),
                read: (PerfMode::Balanced, FanMode::Auto),
            })
        );
        assert!(crate::error::is_perf_not_applied(&err));
        assert!(crate::error::is_retryable(&err));
        assert_eq!(
            crate::error::ExitCode::classify(&err),
            crate::error::ExitCode::DeviceError
        );
        assert!(set_fan_mode_confirmed(&ignoring, FanMode::Manual).is_err());
    }

    #[test]
    fn confirm_perf_mode_rereads_once_for_a_mode_that_lands_late() {
        // The first two-zone read still shows Balanced; the re-read shows Custom.
        let reads = std::cell::Cell::new(0);
        let mock = MockTransport::with_responder(move |req| {
            reads.set(reads.get() + 1);
            let perf = if reads.get() <= 2 {
                PerfMode::Balanced
            } else {
                PerfMode::Custom
            };
            reply(&[0, req.get_args()[1], perf as u8, FanMode::Auto as u8])
        });
        confirm_perf_mode(&mock, PerfMode::Custom, FanMode::Auto).unwrap();
        assert_eq!(
            mock.sent_count(),
            4,
            "one read, one re-read, two zones each"
        );
    }

    #[test]
    fn a_failed_perf_read_keeps_the_cause_that_decides_retry_and_exit_code() {
        use crate::packet::ResponseError;
        for (answer, retryable, code) in [
            (
                ResponseError::Busy,
                true,
                crate::error::ExitCode::DeviceError,
            ),
            (
                ResponseError::NotSupported,
                false,
                crate::error::ExitCode::Unsupported,
            ),
        ] {
            let mock = MockTransport::with_fallible_responder(move |req| match req.command() {
                0x0d82 => Err(anyhow::Error::new(answer)),
                _ => Ok(Packet::new(req.command(), req.get_args())),
            });
            let err = get_perf_mode(&mock).unwrap_err();
            assert_eq!(crate::error::is_retryable(&err), retryable, "{answer:?}");
            assert_eq!(crate::error::ExitCode::classify(&err), code, "{answer:?}");
            // Confirming passes a Busy up; a laptop with no mode read keeps its write.
            let confirmed = confirm_perf_mode(&mock, PerfMode::Balanced, FanMode::Auto);
            assert_eq!(confirmed.is_ok(), !retryable, "{answer:?}");
        }
    }
}
