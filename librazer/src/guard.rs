//! Two guards the tray lays over the state it aims the laptop at, without changing what the
//! user saved: the weak-charger clamp and the fan guard. Both ideas are from
//! nezinomas/blade-lite (MIT, `crates/razer-proto/src/charger.rs` and
//! `crates/ui/src/fan_guard.rs`, read 2026-11-19); the code and the numbers here are ours.
//!
//! Pure: the tray feeds them readings and asks for the guarded state, so they are tested on
//! any host.

use std::time::{Duration, Instant};

use crate::state::{DeviceState, FanSpeed, PerfMode};
use crate::types::PowerSource;

// ---- weak charger ------------------------------------------------------------------------

/// Whether `source` is a USB-C charger (`0x078c` codes 1-7, 40-100 W). Synapse offers only
/// the battery presets on one, and so does blade-lite: the laptop draws more than such a
/// charger gives at Performance and up, and the battery makes up the rest.
pub fn is_weak_charger(source: PowerSource) -> bool {
    source.kind() == "usb-c"
}

/// The perf mode a weak charger holds `perf` to, or `None` when it is left alone: every
/// mode above Balanced (Performance, Hyperboost) and Custom go to Balanced; Battery, Silent
/// and Balanced stay.
pub fn weak_charger_perf(perf: PerfMode) -> Option<PerfMode> {
    match perf {
        PerfMode::Battery | PerfMode::Silent | PerfMode::Balanced => None,
        PerfMode::Performance | PerfMode::Hyperboost | PerfMode::Custom(..) => {
            Some(PerfMode::Balanced)
        }
    }
}

/// `state` as a weak charger allows it. Max Fan exists only in Custom, so it goes too.
pub fn clamp_for_weak_charger(state: DeviceState) -> DeviceState {
    match weak_charger_perf(state.perf_mode) {
        Some(perf_mode) => DeviceState {
            perf_mode,
            max_fan: false,
            ..state
        },
        None => state,
    }
}

// ---- fan guard ---------------------------------------------------------------------------

/// A manual fan set point is a promise the tray can't check: nothing in razer-ctl speeds
/// the fans up when the laptop gets hot. The fan guard raises a manual fan to the model's
/// top manual speed while the laptop is hot, and gives the set point back once it has
/// cooled. It never touches Auto, and Max Fan already runs the fans flat out. It is a
/// backstop: on `0x029F` the EC itself ran the fans past a 2000 RPM set point under load
/// (2026-11-19: 3400 RPM in Balanced at half load, 4500/3900 in Performance at full load),
/// but nobody has measured that on other models, or how hot it lets the GPU get.
///
/// The temperatures it can have (see README "Quirks"):
/// - CPU: the EC's CPU-side sensor (`0x0d85`), NOT the die. On `0x029F` it reads 51-67 °C
///   at idle and moves slowly: +7 °C in 40 s at half load (2026-10-22), and the EC's own
///   Auto curve starts the fans at about 62 °C.
/// - GPU: the dGPU's own temperature, only while the dGPU is awake (asleep it is cool by
///   definition). On `0x029F` the RTX 4090 Laptop GPU held about 86 °C at its 180 W limit
///   in a game (2026-06-29) -- power-limited, not thermal-limited -- and NVIDIA's slowdown
///   target for it is 87 °C.
///
/// See [`FanGuardLimits::DEFAULT`] for the lines and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanGuardLimits {
    /// At or above this the CPU is hot.
    pub cpu_hot_c: u8,
    /// Below this the CPU is cool.
    pub cpu_cool_c: u8,
    pub gpu_hot_c: u8,
    pub gpu_cool_c: u8,
}

/// How far under its trigger the CPU must read to count as cool: under the 60-63 °C the
/// sensor held at full load with the fans at 4500 RPM (see [`FanGuardLimits::DEFAULT`]), so
/// a guard that engaged under load holds until the load ends instead of cycling.
pub const FAN_GUARD_CPU_MARGIN: u8 = 14;
/// How far under its trigger the GPU must read to count as cool. Wider than the CPU's:
/// blade-lite measured a steady GPU load at full fans settling at 74-76 °C (Blade 18, their
/// ticket 175), so a 10 °C margin released the guard mid-load and the fans went full,
/// slow, full. 20 °C below 87 is under that.
pub const FAN_GUARD_GPU_MARGIN: u8 = 20;

impl FanGuardLimits {
    /// CPU 72 °C (cool under 58), GPU 87 °C (cool under 67).
    ///
    /// GPU: 87 °C is NVIDIA's slowdown target for this GPU and where blade-lite triggers.
    /// A manual set point that lets a game reach it is costing clocks.
    ///
    /// CPU: the EC sensor reads far under the die (TjMax 100 °C on the i9-13950HX), so a
    /// die-style 95 °C line would never be reached. Measured on `0x029F` (2026-11-19,
    /// `razer-cli auto json` every 4-8 s): idle 51-58 °C; half of 32 threads busy for 3 min
    /// in Balanced, at most 60; all 32 threads at full load in Performance with a 2000 RPM
    /// set point, 69-70 within 35 s, then 60-63 once the EC had lifted the fans to 4500,
    /// and 55 within 10 s of the load ending. So 72 is above every reading the EC left alone,
    /// and the cool line sits under the full-fan plateau.
    pub const DEFAULT: FanGuardLimits = FanGuardLimits::with_triggers(CPU_HOT_C, GPU_HOT_C);

    /// The limits for these triggers, each cool line its margin below.
    pub const fn with_triggers(cpu_hot_c: u8, gpu_hot_c: u8) -> FanGuardLimits {
        FanGuardLimits {
            cpu_hot_c,
            cpu_cool_c: cpu_hot_c.saturating_sub(FAN_GUARD_CPU_MARGIN),
            gpu_hot_c,
            gpu_cool_c: gpu_hot_c.saturating_sub(FAN_GUARD_GPU_MARGIN),
        }
    }

    /// The defaults with a config's triggers where it sets them. A trigger outside
    /// 40-110 °C is ignored (`None` back for the log): below that the guard would hold the
    /// fans at full speed at idle, above it it could never fire.
    pub fn from_config(cpu: Option<u8>, gpu: Option<u8>) -> (FanGuardLimits, Option<String>) {
        let mut ignored = Vec::new();
        let mut pick = |name: &str, value: Option<u8>, default: u8| match value {
            Some(c) if (FAN_GUARD_TRIGGER_MIN..=FAN_GUARD_TRIGGER_MAX).contains(&c) => c,
            Some(c) => {
                ignored.push(format!("{name} {c}"));
                default
            }
            None => default,
        };
        let cpu = pick("fan_guard_cpu_c", cpu, CPU_HOT_C);
        let gpu = pick("fan_guard_gpu_c", gpu, GPU_HOT_C);
        let note = (!ignored.is_empty()).then(|| {
            format!(
                "{} outside {FAN_GUARD_TRIGGER_MIN}-{FAN_GUARD_TRIGGER_MAX} °C; using the default",
                ignored.join(", ")
            )
        });
        (FanGuardLimits::with_triggers(cpu, gpu), note)
    }
}

const CPU_HOT_C: u8 = 72;
const GPU_HOT_C: u8 = 87;
/// The range a config's trigger must fall in.
pub const FAN_GUARD_TRIGGER_MIN: u8 = 40;
pub const FAN_GUARD_TRIGGER_MAX: u8 = 110;

/// How long every reading must stay cool before the set point comes back. Long enough that
/// one cool sample in a game's load does not drop the fans; blade-lite holds 60 s.
pub const FAN_GUARD_COOL_HOLD: Duration = Duration::from_secs(30);

/// A reading older than this is no reading: the guard neither engages nor releases on it,
/// and the cool hold starts over. The tray reads every 5 s while it watches, so this is
/// about three missed reads (a failed read pauses them for 60 s).
pub const FAN_GUARD_STALE: Duration = Duration::from_secs(15);

/// One reading for the guard: the CPU and GPU temperatures taken at `at`. A sensor that
/// isn't there (a GPU asleep, a byte that isn't believable) is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardReading {
    pub at: Instant,
    pub cpu_c: Option<u8>,
    pub gpu_c: Option<u8>,
}

/// What [`FanGuard::update`] changed, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanGuardChange {
    /// Hot: the fans go to full speed.
    Engaged {
        cpu_c: Option<u8>,
        gpu_c: Option<u8>,
    },
    /// Cool for [`FAN_GUARD_COOL_HOLD`]: the set point comes back.
    Released {
        cpu_c: Option<u8>,
        gpu_c: Option<u8>,
    },
}

/// Whether the fans should be at full speed for the heat, with hysteresis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FanGuard {
    engaged: bool,
    /// Since when every fresh reading has been cool, while engaged.
    cool_since: Option<Instant>,
    /// The last cool reading counted toward the hold. A gap longer than
    /// [`FAN_GUARD_STALE`] after it starts the hold over: 30 s of cool means 30 s of readings.
    last_cool: Option<Instant>,
}

impl FanGuard {
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    /// Stop guarding (the fan is no longer manual, or the guard was turned off). Returns
    /// whether it was engaged.
    pub fn reset(&mut self) -> bool {
        std::mem::take(self).engaged
    }

    /// Take a reading. Engages on any sensor at or above its hot line; releases once some
    /// sensor is read and every one read is under its cool line, and has been for
    /// [`FAN_GUARD_COOL_HOLD`]. No reading, or one older than [`FAN_GUARD_STALE`], changes
    /// nothing, and the cool hold starts over.
    pub fn update(
        &mut self,
        now: Instant,
        reading: Option<GuardReading>,
        limits: FanGuardLimits,
    ) -> Option<FanGuardChange> {
        let Some(r) = reading.filter(|r| now.saturating_duration_since(r.at) <= FAN_GUARD_STALE)
        else {
            self.cool_since = None;
            self.last_cool = None;
            return None;
        };
        let hot = r.cpu_c.is_some_and(|c| c >= limits.cpu_hot_c)
            || r.gpu_c.is_some_and(|c| c >= limits.gpu_hot_c);
        let cool = (r.cpu_c.is_some() || r.gpu_c.is_some())
            && r.cpu_c.is_none_or(|c| c < limits.cpu_cool_c)
            && r.gpu_c.is_none_or(|c| c < limits.gpu_cool_c);
        let (cpu_c, gpu_c) = (r.cpu_c, r.gpu_c);
        if !self.engaged {
            if hot {
                *self = FanGuard {
                    engaged: true,
                    ..FanGuard::default()
                };
                return Some(FanGuardChange::Engaged { cpu_c, gpu_c });
            }
            return None;
        }
        if !cool {
            self.cool_since = None;
            self.last_cool = None;
            return None;
        }
        if self
            .last_cool
            .is_some_and(|last| r.at.saturating_duration_since(last) > FAN_GUARD_STALE)
        {
            self.cool_since = None;
        }
        self.last_cool = Some(r.at);
        let since = *self.cool_since.get_or_insert(r.at);
        if now.saturating_duration_since(since) >= FAN_GUARD_COOL_HOLD {
            *self = FanGuard::default();
            return Some(FanGuardChange::Released { cpu_c, gpu_c });
        }
        None
    }
}

/// Whether the fan guard has anything to raise in `state`: a manual set point under the
/// top of `range`, without Max Fan. Auto is never touched.
pub fn fan_guard_applies(state: &DeviceState, range: (u16, u16)) -> bool {
    !state.max_fan && matches!(state.fan_speed, FanSpeed::Manual(rpm) if rpm < range.1)
}

/// `state` with the guard engaged: a manual set point raised to the top of `range`.
pub fn raise_fan_for_heat(state: DeviceState, range: (u16, u16)) -> DeviceState {
    if fan_guard_applies(&state, range) {
        DeviceState {
            fan_speed: FanSpeed::Manual(range.1),
            ..state
        }
    } else {
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CpuBoost, GpuBoost};

    const RANGE: (u16, u16) = (2000, 5000);

    fn custom() -> PerfMode {
        PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
    }

    #[test]
    fn a_usb_c_charger_is_weak_and_the_barrel_and_battery_are_not() {
        for code in 1..=7u8 {
            assert!(
                is_weak_charger(PowerSource::from_args(&[code, 0x12])),
                "{code}"
            );
        }
        // 0x029F on its 330 W barrel charger reads [0x12, 0x12].
        assert!(!is_weak_charger(PowerSource::from_args(&[0x12, 0x12])));
        assert!(!is_weak_charger(PowerSource::from_args(&[0x08])));
        assert!(!is_weak_charger(PowerSource::from_args(&[0, 0])));
    }

    #[test]
    fn modes_above_balanced_and_custom_are_held_to_balanced() {
        for perf in [PerfMode::Performance, PerfMode::Hyperboost, custom()] {
            assert_eq!(
                weak_charger_perf(perf),
                Some(PerfMode::Balanced),
                "{perf:?}"
            );
        }
        for perf in [PerfMode::Battery, PerfMode::Silent, PerfMode::Balanced] {
            assert_eq!(weak_charger_perf(perf), None, "{perf:?}");
        }
    }

    #[test]
    fn the_clamp_drops_max_fan_and_keeps_everything_else() {
        let state = DeviceState {
            perf_mode: custom(),
            max_fan: true,
            fan_speed: FanSpeed::Manual(3400),
            ..DeviceState::default()
        };
        let held = clamp_for_weak_charger(state);
        assert_eq!(held.perf_mode, PerfMode::Balanced);
        assert!(!held.max_fan);
        assert_eq!(held.fan_speed, FanSpeed::Manual(3400));
        assert_eq!(held.lights_mode, state.lights_mode);
        assert_eq!(held.battery_care, state.battery_care);
        let silent = DeviceState {
            perf_mode: PerfMode::Silent,
            ..DeviceState::default()
        };
        assert_eq!(clamp_for_weak_charger(silent), silent);
    }

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    fn read(at: Instant, cpu: u8, gpu: Option<u8>) -> Option<GuardReading> {
        Some(GuardReading {
            at,
            cpu_c: Some(cpu),
            gpu_c: gpu,
        })
    }

    const L: FanGuardLimits = FanGuardLimits::DEFAULT;

    #[test]
    fn a_hot_cpu_or_gpu_engages_and_a_warm_one_does_not() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        assert_eq!(
            g.update(t0, read(t0, L.cpu_hot_c - 1, Some(L.gpu_hot_c - 1)), L),
            None
        );
        assert!(!g.engaged());
        assert!(matches!(
            g.update(t0, read(t0, L.cpu_hot_c, None), L),
            Some(FanGuardChange::Engaged { .. })
        ));
        assert!(g.engaged());
        let mut g = FanGuard::default();
        assert!(matches!(
            g.update(t0, read(t0, 50, Some(L.gpu_hot_c)), L),
            Some(FanGuardChange::Engaged { .. })
        ));
    }

    #[test]
    fn it_holds_through_the_band_and_releases_after_the_cool_hold() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c + 2, None), L);
        // Between the lines: held, and no cool time accrues.
        let band = L.cpu_cool_c + 1;
        for s in [5, 10, 40, 80] {
            assert_eq!(g.update(at(t0, s), read(at(t0, s), band, None), L), None);
            assert!(g.engaged());
        }
        // Cool, but not for long enough.
        let cool = L.cpu_cool_c - 1;
        for s in [85, 95, 105, 114] {
            assert_eq!(
                g.update(at(t0, s), read(at(t0, s), cool, None), L),
                None,
                "{s}"
            );
        }
        // One warm reading starts the hold over.
        assert_eq!(
            g.update(at(t0, 116), read(at(t0, 116), band, None), L),
            None
        );
        for s in [120, 130, 140] {
            assert_eq!(
                g.update(at(t0, s), read(at(t0, s), cool, None), L),
                None,
                "{s}"
            );
        }
        assert_eq!(
            g.update(at(t0, 150), read(at(t0, 150), cool, None), L),
            Some(FanGuardChange::Released {
                cpu_c: Some(cool),
                gpu_c: None
            })
        );
        assert!(!g.engaged());
    }

    #[test]
    fn a_warm_gpu_keeps_it_engaged_while_the_cpu_is_cool() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, 60, Some(L.gpu_hot_c)), L);
        for s in (5..=120).step_by(5) {
            let r = read(at(t0, s), 50, Some(L.gpu_cool_c));
            assert_eq!(g.update(at(t0, s), r, L), None);
        }
        assert!(g.engaged());
        // The GPU going to sleep (no reading) counts as cool.
        for s in (125..=150).step_by(5) {
            assert_eq!(g.update(at(t0, s), read(at(t0, s), 50, None), L), None);
        }
        assert!(matches!(
            g.update(at(t0, 155), read(at(t0, 155), 50, None), L),
            Some(FanGuardChange::Released { .. })
        ));
    }

    #[test]
    fn a_gap_in_the_cool_readings_starts_the_hold_over() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c, None), L);
        let cool = L.cpu_cool_c - 1;
        assert_eq!(g.update(at(t0, 5), read(at(t0, 5), cool, None), L), None);
        // 20 s with no reading (a stalled loop), then cool again: not 30 s of evidence yet.
        assert_eq!(g.update(at(t0, 36), read(at(t0, 36), cool, None), L), None);
        assert!(g.engaged());
        for s in [41, 51, 61] {
            assert_eq!(
                g.update(at(t0, s), read(at(t0, s), cool, None), L),
                None,
                "{s}"
            );
        }
        assert!(g
            .update(at(t0, 66), read(at(t0, 66), cool, None), L)
            .is_some());
    }

    #[test]
    fn a_stale_or_missing_reading_changes_nothing() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        // Hot, but read too long ago: no engage.
        let old = read(t0, L.cpu_hot_c + 5, None);
        let later = t0 + FAN_GUARD_STALE + Duration::from_secs(1);
        assert_eq!(g.update(later, old, L), None);
        assert!(!g.engaged());
        assert_eq!(g.update(later, None, L), None);
        // Engaged, then the readings stop: it stays engaged however long that lasts.
        g.update(t0, read(t0, L.cpu_hot_c, None), L);
        g.update(at(t0, 5), read(at(t0, 5), 40, None), L);
        for s in [20, 60, 600] {
            assert_eq!(g.update(at(t0, s), read(at(t0, 5), 40, None), L), None);
            assert_eq!(g.update(at(t0, s), None, L), None);
            assert!(g.engaged(), "{s}");
        }
        // And the hold starts over from the next fresh cool reading.
        for s in [601, 611, 621] {
            assert_eq!(
                g.update(at(t0, s), read(at(t0, s), 40, None), L),
                None,
                "{s}"
            );
        }
        assert!(g
            .update(at(t0, 631), read(at(t0, 631), 40, None), L)
            .is_some());
    }

    #[test]
    fn no_sensor_at_all_is_not_cool() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c, None), L);
        let blind = Some(GuardReading {
            at: at(t0, 100),
            cpu_c: None,
            gpu_c: None,
        });
        for s in [100, 140, 200] {
            let r = blind.map(|r| GuardReading { at: at(t0, s), ..r });
            assert_eq!(g.update(at(t0, s), r, L), None);
        }
        assert!(g.engaged());
    }

    #[test]
    fn reset_says_whether_it_was_engaged() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        assert!(!g.reset());
        g.update(t0, read(t0, L.cpu_hot_c, None), L);
        assert!(g.reset());
        assert!(!g.engaged());
    }

    #[test]
    fn only_a_manual_set_point_under_the_top_is_raised() {
        let manual = DeviceState {
            fan_speed: FanSpeed::Manual(3000),
            perf_mode: PerfMode::Balanced,
            ..DeviceState::default()
        };
        assert_eq!(
            raise_fan_for_heat(manual, RANGE).fan_speed,
            FanSpeed::Manual(5000)
        );
        let auto = DeviceState {
            fan_speed: FanSpeed::Auto,
            ..manual
        };
        assert_eq!(raise_fan_for_heat(auto, RANGE), auto);
        let top = DeviceState {
            fan_speed: FanSpeed::Manual(5000),
            ..manual
        };
        assert!(!fan_guard_applies(&top, RANGE));
        let max_fan = DeviceState {
            perf_mode: custom(),
            max_fan: true,
            ..manual
        };
        assert_eq!(raise_fan_for_heat(max_fan, RANGE), max_fan);
    }

    #[test]
    fn config_triggers_set_both_lines_and_silly_ones_are_ignored() {
        let (l, note) = FanGuardLimits::from_config(Some(80), Some(85));
        assert_eq!(note, None);
        assert_eq!((l.cpu_hot_c, l.cpu_cool_c), (80, 66));
        assert_eq!((l.gpu_hot_c, l.gpu_cool_c), (85, 65));
        let (l, note) = FanGuardLimits::from_config(Some(20), None);
        assert_eq!(l, FanGuardLimits::DEFAULT);
        assert!(note.unwrap().contains("fan_guard_cpu_c 20"));
        let (l, note) = FanGuardLimits::from_config(None, Some(200));
        assert_eq!(l, FanGuardLimits::DEFAULT);
        assert!(note.is_some());
    }
}
