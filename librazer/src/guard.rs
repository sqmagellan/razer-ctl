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

/// How long the readings must stay cool before the set point comes back, measured between
/// the observations themselves (when each was taken), not between looks at a cached one.
/// Long enough that one cool sample in a game's load does not drop the fans; blade-lite
/// holds 60 s.
pub const FAN_GUARD_COOL_HOLD: Duration = Duration::from_secs(30);

/// An observation older than this is no observation: the guard neither engages nor releases
/// on it. Two cool observations further apart than this start the hold over. The tray reads
/// every 5 s while it watches, so this is about three missed reads (a failed read pauses
/// them for 60 s).
pub const FAN_GUARD_STALE: Duration = Duration::from_secs(15);

/// A sensor that read hot and then has no fresh observation for this long stops holding the
/// guard, whether or not the other sensor still reports; with every sensor lost, that gives
/// the set point back. Holding full fan on no evidence would turn a lost sensor (a bus fault, an EC
/// that stopped answering `0x0d85`) into full fan until the user leaves Manual, with no sign
/// of why. Ten minutes covers the reads' own 60 s pause many times over, and what protects
/// the chips without the guard is still there: both throttle at their own limits, and on
/// `0x029F` the EC itself lifted a 2000 RPM set point to 4500 under load (2026-11-19). The
/// first fresh hot reading engages the guard again.
pub const FAN_GUARD_BLIND_RELEASE: Duration = Duration::from_secs(600);

/// One sensor's temperature and when it was taken (by the sensor read, not by whoever
/// looked at it last).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub c: u8,
    pub at: Instant,
}

/// What the guard can know at one look. A sensor without an observation is `None`: a byte
/// that isn't believable, a failed read, or a GPU that isn't there. `gpu_asleep` says the
/// dGPU is known to be powered down, which counts as cool; an awake GPU without a reading
/// is only missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuardReading {
    pub cpu: Option<Observation>,
    pub gpu: Option<Observation>,
    pub gpu_asleep: bool,
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
    /// Engaged, and no fresh observation for [`FAN_GUARD_STALE`]: still holding. Once per
    /// loss.
    Blind,
    /// No fresh observation for [`FAN_GUARD_BLIND_RELEASE`]: the set point comes back.
    ReleasedBlind,
}

/// One sensor's history for the guard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Track {
    /// It read hot while the guard was engaged: only its own readings can say it cooled.
    hot: bool,
    /// The newest observation taken in. One seen again is not taken in twice.
    last: Option<Observation>,
    /// The observation time its current run of cool readings started at.
    cool_since: Option<Instant>,
    /// The time of the last cool observation in that run.
    last_cool: Option<Instant>,
}

impl Track {
    /// Take in `o` if it is fresh and newer than the last one. A cool one extends the run
    /// of cool readings (a gap over [`FAN_GUARD_STALE`] starts it over); any other ends it.
    fn see(&mut self, o: Option<Observation>, now: Instant, cool_line: u8) {
        let Some(o) = fresh(o, now) else {
            return;
        };
        if self.last.is_some_and(|last| o.at <= last.at) {
            return;
        }
        if o.c < cool_line {
            if self
                .last_cool
                .is_some_and(|last| o.at.saturating_duration_since(last) > FAN_GUARD_STALE)
            {
                self.cool_since = None;
            }
            self.cool_since.get_or_insert(o.at);
            self.last_cool = Some(o.at);
        } else {
            self.cool_since = None;
            self.last_cool = None;
        }
        self.last = Some(o);
    }

    /// Its newest temperature, if that is still fresh.
    fn fresh_c(&self, now: Instant) -> Option<u8> {
        fresh(self.last, now).map(|o| o.c)
    }

    /// How long since its newest observation was taken (forever if it never had one).
    fn silent_for(&self, now: Instant) -> Duration {
        self.last
            .map_or(Duration::MAX, |o| now.saturating_duration_since(o.at))
    }

    /// Its own readings have been cool across [`FAN_GUARD_COOL_HOLD`], and the last of
    /// them is still fresh.
    fn cooled(&self, now: Instant) -> bool {
        match (self.cool_since, self.last_cool) {
            (Some(since), Some(last)) => {
                now.saturating_duration_since(last) <= FAN_GUARD_STALE
                    && last.saturating_duration_since(since) >= FAN_GUARD_COOL_HOLD
            }
            _ => false,
        }
    }
}

/// What one sensor says about releasing the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Cool,
    /// It read hot and has been silent for [`FAN_GUARD_BLIND_RELEASE`].
    GaveUp,
    Hold,
}

/// Whether the fans should be at full speed for the heat, with hysteresis. Each sensor is
/// tracked on its own: the hold that releases the guard is measured on the readings of the
/// sensor that read hot, never advanced by the other's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FanGuard {
    engaged: bool,
    cpu: Track,
    gpu: Track,
    /// Since when the dGPU has been known asleep with no reading (counts as cool after
    /// [`FAN_GUARD_COOL_HOLD`]).
    gpu_asleep_since: Option<Instant>,
    /// [`FanGuardChange::Blind`] was reported for this loss.
    blind_logged: bool,
}

fn fresh(o: Option<Observation>, now: Instant) -> Option<Observation> {
    o.filter(|o| now.saturating_duration_since(o.at) <= FAN_GUARD_STALE)
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

    /// Take a look at the sensors.
    ///
    /// - Engages on any fresh observation at or above its hot line.
    /// - Releases when every sensor that read hot has its own cool readings (under its cool
    ///   line) spanning [`FAN_GUARD_COOL_HOLD`], taken no more than [`FAN_GUARD_STALE`]
    ///   apart, the last still fresh; a dGPU known asleep that long counts as cool. A sensor
    ///   that didn't read hot only has to not read warm (at or above its cool line) now.
    /// - An observation seen again counts once; one older than [`FAN_GUARD_STALE`] counts
    ///   for nothing.
    /// - A sensor that read hot and then went silent holds the guard, reported once
    ///   ([`FanGuardChange::Blind`]), until [`FAN_GUARD_BLIND_RELEASE`] after its last
    ///   observation; then it no longer holds it ([`FanGuardChange::ReleasedBlind`] when
    ///   that is what lets the guard go). The same rule covers losing every sensor.
    pub fn update(
        &mut self,
        now: Instant,
        reading: GuardReading,
        limits: FanGuardLimits,
    ) -> Option<FanGuardChange> {
        self.cpu.see(reading.cpu, now, limits.cpu_cool_c);
        self.gpu.see(reading.gpu, now, limits.gpu_cool_c);
        // A dGPU known asleep has no temperature, whatever was read before it slept.
        let cpu_c = self.cpu.fresh_c(now);
        let gpu_c = (!reading.gpu_asleep)
            .then(|| self.gpu.fresh_c(now))
            .flatten();
        if reading.gpu_asleep {
            self.gpu_asleep_since.get_or_insert(now);
        } else {
            self.gpu_asleep_since = None;
        }
        let hot_cpu = cpu_c.is_some_and(|c| c >= limits.cpu_hot_c);
        let hot_gpu = gpu_c.is_some_and(|c| c >= limits.gpu_hot_c);
        if !self.engaged {
            if hot_cpu || hot_gpu {
                self.engaged = true;
                self.cpu.hot = hot_cpu;
                self.gpu.hot = hot_gpu;
                self.blind_logged = false;
                return Some(FanGuardChange::Engaged { cpu_c, gpu_c });
            }
            return None;
        }
        self.cpu.hot |= hot_cpu;
        self.gpu.hot |= hot_gpu;

        let verdict =
            |track: &Track, c: Option<u8>, cool_line: u8, asleep_for: Option<Duration>| {
                if !track.hot {
                    return if c.is_some_and(|c| c >= cool_line) {
                        Verdict::Hold
                    } else {
                        Verdict::Cool
                    };
                }
                if track.cooled(now) || asleep_for.is_some_and(|d| d >= FAN_GUARD_COOL_HOLD) {
                    Verdict::Cool
                } else if track.silent_for(now) >= FAN_GUARD_BLIND_RELEASE {
                    Verdict::GaveUp
                } else {
                    Verdict::Hold
                }
            };
        let asleep_for = self
            .gpu_asleep_since
            .map(|since| now.saturating_duration_since(since));
        let v_cpu = verdict(&self.cpu, cpu_c, limits.cpu_cool_c, None);
        let v_gpu = verdict(&self.gpu, gpu_c, limits.gpu_cool_c, asleep_for);
        if v_cpu != Verdict::Hold && v_gpu != Verdict::Hold {
            let blind = v_cpu == Verdict::GaveUp || v_gpu == Verdict::GaveUp;
            *self = FanGuard::default();
            return Some(if blind {
                FanGuardChange::ReleasedBlind
            } else {
                FanGuardChange::Released { cpu_c, gpu_c }
            });
        }
        // A sensor that read hot and has gone quiet (not a sleeping dGPU): say so once.
        let silent = (self.cpu.hot && self.cpu.silent_for(now) > FAN_GUARD_STALE)
            || (self.gpu.hot && asleep_for.is_none() && self.gpu.silent_for(now) > FAN_GUARD_STALE);
        if !silent {
            self.blind_logged = false;
        } else if !self.blind_logged {
            self.blind_logged = true;
            return Some(FanGuardChange::Blind);
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

    fn obs(c: u8, at: Instant) -> Option<Observation> {
        Some(Observation { c, at })
    }

    /// A CPU observation taken at `at`, and a GPU one at the same time when given.
    fn read(at: Instant, cpu: u8, gpu: Option<u8>) -> GuardReading {
        GuardReading {
            cpu: obs(cpu, at),
            gpu: gpu.and_then(|g| obs(g, at)),
            gpu_asleep: false,
        }
    }

    fn none() -> GuardReading {
        GuardReading::default()
    }

    const L: FanGuardLimits = FanGuardLimits::DEFAULT;

    #[test]
    fn a_hot_cpu_or_gpu_engages_and_a_warm_one_does_not() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        let warm = read(t0, L.cpu_hot_c - 1, Some(L.gpu_hot_c - 1));
        assert_eq!(g.update(t0, warm, L), None);
        assert!(!g.engaged());
        assert!(matches!(
            g.update(at(t0, 1), read(at(t0, 1), L.cpu_hot_c, None), L),
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
        for s in [5, 10, 15, 20] {
            assert_eq!(g.update(at(t0, s), read(at(t0, s), band, None), L), None);
            assert!(g.engaged());
        }
        // Cool, but not for long enough.
        let cool = L.cpu_cool_c - 1;
        for s in [25, 35, 45, 54] {
            assert_eq!(
                g.update(at(t0, s), read(at(t0, s), cool, None), L),
                None,
                "{s}"
            );
        }
        // One warm reading starts the hold over.
        assert_eq!(g.update(at(t0, 56), read(at(t0, 56), band, None), L), None);
        for s in [60, 70, 80] {
            assert_eq!(
                g.update(at(t0, s), read(at(t0, s), cool, None), L),
                None,
                "{s}"
            );
        }
        assert_eq!(
            g.update(at(t0, 90), read(at(t0, 90), cool, None), L),
            Some(FanGuardChange::Released {
                cpu_c: Some(cool),
                gpu_c: None
            })
        );
        assert!(!g.engaged());
    }

    #[test]
    fn a_reused_observation_counts_once() {
        // The tray looks every second; the EC is read every 5 s. Looking at the same
        // observation again must not move the hold along.
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c, None), L);
        let cool = L.cpu_cool_c - 1;
        for s in [5, 10] {
            g.update(at(t0, s), read(at(t0, s), cool, None), L);
        }
        // Each observation looked at every second until the next one: at second 30 the
        // newest observation is from 25, only 20 s after the first cool one, so no release
        // (stamping a look with the time of the look would have released at 30).
        for (taken, until) in [(16, 24), (25, 34)] {
            let last = read(at(t0, taken), cool, None);
            for s in taken..=until {
                assert_eq!(g.update(at(t0, s), last, L), None, "{s}");
            }
        }
        assert!(g.engaged());
        // A new observation at 35 is 30 s after the first cool one at 5.
        assert!(matches!(
            g.update(at(t0, 35), read(at(t0, 35), cool, None), L),
            Some(FanGuardChange::Released { .. })
        ));
    }

    #[test]
    fn the_other_sensors_readings_never_advance_the_hot_ones_hold() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c, Some(50)), L);
        let cool = L.cpu_cool_c - 1;
        // The CPU reads cool at 5 and 20 only; the GPU reports every 5 s.
        for s in (5..=25).step_by(5) {
            let cpu = [5, 20].contains(&s).then(|| Observation {
                c: cool,
                at: at(t0, s),
            });
            let r = GuardReading {
                cpu,
                gpu: obs(50, at(t0, s)),
                gpu_asleep: false,
            };
            assert_eq!(g.update(at(t0, s), r, L), None, "{s}");
        }
        // At 35 the GPU is fresh and the CPU's 20 s reading still counts as fresh, but
        // the CPU's own readings span only 15 s: no release.
        let gpu_only = GuardReading {
            gpu: obs(50, at(t0, 35)),
            ..none()
        };
        assert_eq!(g.update(at(t0, 35), gpu_only, L), None);
        assert!(g.engaged());
        // CPU readings at 30 and 35 complete its own 30 s span.
        g.update(at(t0, 36), read(at(t0, 30), cool, Some(50)), L);
        assert!(matches!(
            g.update(at(t0, 36), read(at(t0, 35), cool, Some(50)), L),
            Some(FanGuardChange::Released { .. })
        ));
    }

    #[test]
    fn a_hot_sensor_gone_quiet_stops_holding_after_ten_minutes() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        // The CPU triggers it, then only the GPU reports (cool) from then on.
        g.update(t0, read(t0, L.cpu_hot_c, Some(50)), L);
        let gpu_only = |s| GuardReading {
            gpu: obs(50, at(t0, s)),
            ..none()
        };
        let end = FAN_GUARD_BLIND_RELEASE.as_secs();
        let mut blind = 0;
        for s in (5..end).step_by(5) {
            match g.update(at(t0, s), gpu_only(s), L) {
                Some(FanGuardChange::Blind) => blind += 1,
                None => {}
                other => panic!("{s}: {other:?}"),
            }
        }
        assert_eq!(blind, 1);
        assert!(g.engaged());
        assert_eq!(
            g.update(at(t0, end), gpu_only(end), L),
            Some(FanGuardChange::ReleasedBlind)
        );
        // A warm GPU at that point keeps it, though the CPU has given up.
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c, Some(50)), L);
        let warm = GuardReading {
            gpu: obs(L.gpu_cool_c, at(t0, end)),
            ..none()
        };
        // (The CPU's silence is reported, once; nothing is released.)
        assert_eq!(g.update(at(t0, end), warm, L), Some(FanGuardChange::Blind));
        assert!(g.engaged());
    }

    #[test]
    fn a_warm_gpu_keeps_it_engaged_and_a_sleeping_gpu_is_cool() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        g.update(t0, read(t0, 60, Some(L.gpu_hot_c)), L);
        for s in (5..=60).step_by(5) {
            let r = read(at(t0, s), 50, Some(L.gpu_cool_c));
            assert_eq!(g.update(at(t0, s), r, L), None);
        }
        assert!(g.engaged());
        // The dGPU powers down when the game ends: known asleep counts as cool.
        let asleep = |s| GuardReading {
            gpu_asleep: true,
            ..read(at(t0, s), 50, None)
        };
        for s in (65..=90).step_by(5) {
            assert_eq!(g.update(at(t0, s), asleep(s), L), None, "{s}");
        }
        assert!(matches!(
            g.update(at(t0, 95), asleep(95), L),
            Some(FanGuardChange::Released { .. })
        ));
    }

    #[test]
    fn the_other_sensor_alone_never_releases_it() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        // The GPU triggered it, then its reading is lost (not asleep: just missing).
        g.update(t0, read(t0, 60, Some(L.gpu_hot_c + 1)), L);
        for s in (5..=300).step_by(5) {
            let change = g.update(at(t0, s), read(at(t0, s), 40, None), L);
            assert!(
                matches!(change, None | Some(FanGuardChange::Blind)),
                "{s}: {change:?}"
            );
        }
        assert!(g.engaged(), "a cool CPU is no news about the GPU");
        // The GPU is read again, cool: the hold starts from there.
        let both = |s| read(at(t0, s), 40, Some(L.gpu_cool_c - 1));
        for s in (305..=330).step_by(5) {
            assert_eq!(g.update(at(t0, s), both(s), L), None, "{s}");
        }
        assert!(g.update(at(t0, 335), both(335), L).is_some());

        // The CPU triggered it and its byte goes away; a cool GPU doesn't release it.
        let mut g = FanGuard::default();
        g.update(t0, read(t0, L.cpu_hot_c, Some(50)), L);
        for s in (5..=120).step_by(5) {
            let r = GuardReading {
                gpu: obs(50, at(t0, s)),
                ..none()
            };
            let change = g.update(at(t0, s), r, L);
            assert!(
                matches!(change, None | Some(FanGuardChange::Blind)),
                "{s}: {change:?}"
            );
        }
        assert!(g.engaged());
    }

    #[test]
    fn a_stale_reading_engages_nothing_and_a_lost_sensor_holds_then_gives_up() {
        let t0 = Instant::now();
        let mut g = FanGuard::default();
        // Hot, but taken too long ago: no engage.
        let later = t0 + FAN_GUARD_STALE + Duration::from_secs(1);
        assert_eq!(g.update(later, read(t0, L.cpu_hot_c + 5, None), L), None);
        assert!(!g.engaged());
        assert_eq!(g.update(later, none(), L), None);
        // Engaged, then the readings stop: held, reported once, released after
        // FAN_GUARD_BLIND_RELEASE without an observation.
        g.update(t0, read(t0, L.cpu_hot_c, None), L);
        let mut blind = 0;
        let mut s = 1;
        let blind_end = FAN_GUARD_BLIND_RELEASE.as_secs();
        while s < blind_end {
            match g.update(at(t0, s), none(), L) {
                Some(FanGuardChange::Blind) => blind += 1,
                None => {}
                other => panic!("{s}: {other:?}"),
            }
            assert!(g.engaged(), "{s}");
            s += 7;
        }
        assert_eq!(blind, 1, "reported once");
        assert_eq!(
            g.update(at(t0, blind_end), none(), L),
            Some(FanGuardChange::ReleasedBlind)
        );
        assert!(!g.engaged());
        // A fresh hot reading engages again.
        let back = at(t0, blind_end + 5);
        assert!(g.update(back, read(back, L.cpu_hot_c, None), L).is_some());
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
