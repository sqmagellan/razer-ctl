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
    BatteryCare, Cluster, CpuBoost, FanMode, FanZone, GpuBoost, KeyboardEffect, LightsAlwaysOn,
    LogoMode, MaxFanSpeedMode, PerfMode,
};

use anyhow::{bail, ensure, Result};

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

        ensure!(
            r1.is_ok() && r2.is_ok(),
            "Failed to get performance mode and fan mode: r1 = {:?}, r2 = {:?}",
            r1,
            r2
        );

        Ok((r1?, r2?))
    }

    let (r1, r2) = read_zones(device)?;
    if r1 == r2 {
        return Ok(r1);
    }

    // Disagreement: either a mode changed mid-read (transient -- a re-read agrees) or the
    // zones genuinely diverged (persists).
    let (r1, r2) = read_zones(device)?;
    ensure!(r1 == r2, "Modes do not match: r1 = {:?}, r2 = {:?}", r1, r2);

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
    // (`Descriptor::fan_rpm_range`, which is what the UI offers) and the EC clamps
    // anything outside its own limits regardless -- this check only rejects values that
    // could not be meaningful on any Blade.
    ensure!(
        (0..=crate::state::FAN_RPM_MAX_ANY).contains(&rpm),
        "Fan RPM {} is out of range (max {} across all known chassis)",
        rpm,
        crate::state::FAN_RPM_MAX_ANY
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

pub fn get_fan_actual_rpm(device: &impl HidTransport, fan_zone: FanZone) -> Result<u16> {
    let response = device.send(Packet::new(0x0d88, &[0, fan_zone as u8, 0]))?;
    ensure!(response.get_args()[1] == fan_zone as u8);
    Ok(response.get_args()[2] as u16 * 100)
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
    _send_command(device, 0x070f, &[mode as u8]).map(|_| ())
}

pub fn get_max_fan_speed_mode(device: &impl HidTransport) -> Result<MaxFanSpeedMode> {
    device.send(Packet::new(0x078f, &[0]))?.get_args()[0].try_into()
}

pub fn set_fan_mode(device: &impl HidTransport, mode: FanMode) -> Result<()> {
    _set_perf_mode(device, get_perf_mode(device)?.0, mode)
}

pub fn custom_command(device: &impl HidTransport, command: u16, args: &[u8]) -> Result<()> {
    // try_new: `args` comes straight from the CLI, so an over-long list must be a clean
    // error rather than a panic in the fixed 80-byte arg buffer.
    let report = Packet::try_new(command, args)?;
    println!("Report   {:?}", report);
    let response = device.send(report)?;
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
    let response = device.send(report)?;
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
// driver mode needed. Chroma is WRITE-ONLY here (no getter), so this is never read back --
// callers hold the effect as intent and re-apply it (see `DeviceState::apply_keyboard_lighting`).
//
// EFFECTS-ONLY, NO ARBITRARY COLOR (by design): a chosen color needs driver mode (host-streamed
// frames = what Synapse does), which kills the Fn keys. In Normal mode the EC ignores color
// payloads and falls back to Razer green, so we ship only the EC-animated effects that need no
// host color. All four HW-confirmed on 0x029F at our default 0x1F transaction.

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
/// Static, which need driver mode and a host colour, so we deliberately don't offer them).
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

pub fn get_battery_care(device: &impl HidTransport) -> Result<BatteryCare> {
    device.send(Packet::new(0x0792, &[0]))?.get_args()[0].try_into()
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
        // host colour) must read as "unknown", NOT as an error and NOT as a wrong variant
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

        let err = get_perf_mode(&mock).unwrap_err().to_string();
        assert!(
            err.contains("Modes do not match"),
            "unexpected error: {err}"
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
}
