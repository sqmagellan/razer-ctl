//! A simulated EC, built from what real machines were observed to do.
//!
//! A [`Model`] (JSON, kept under `librazer/tests/ec/`) says, per command, whether an EC
//! works, rejects, ignores or has never been observed answering it, each rule with its
//! evidence: `measured <source>: ...` (a capture, an issue report, a hardware run) or
//! `inferred: ...`. A command with no rule, or a rule marked unknown, fails with
//! [`Unmodeled`] instead of echoing, so a test can't pass on behavior nobody has seen; it
//! shows up as a coverage gap.
//!
//! [`SimTransport`] answers through the same retry policy as `Device` ([`crate::retry`]),
//! keeps per-zone state (perf mode, fan mode, set point, actual RPM, which is separate
//! from the set point), runs on its own clock (advanced by [`HidTransport::pause`], by
//! retry backoffs and by each exchange) and can inject [`Fault`]s. It records every
//! attempt like a recording `Device`, so a simulated probe produces a real [`Capture`].

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::capture::{self, Capture, Exchange, Outcome, Recorder, Recording, StepResult, Verdict};
use crate::packet::Packet;
use crate::retry::{self, Decision};
use crate::transport::HidTransport;
use crate::types::{BatteryCare, FanMode, PerfMode};
use crate::unsupported::UnsupportedLog;

/// The `format` field of a model file.
pub const FORMAT: &str = "razer-ctl-ec-model/1";

/// Simulated time one exchange takes (`Device` sleeps 3 ms per attempt, plus the I/O).
const EXCHANGE_MS: u64 = 4;

const CUSTOM: u8 = PerfMode::Custom as u8;
const MANUAL: u8 = FanMode::Manual as u8;
/// `MaxFanSpeedMode::Enable` on the wire.
const MAX_FAN_ON: u8 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub format: String,
    pub name: String,
    #[serde(with = "capture::hex_u16")]
    pub pid: u16,
    /// Where the evidence came from, by id: an issue, a capture file, a hardware run.
    pub sources: BTreeMap<String, String>,
    /// The state the EC starts in.
    pub state: State,
    pub fan: FanModel,
    /// First match wins: same command, and `args` (when given) a prefix of the request's.
    pub rules: Vec<Rule>,
    /// Faults present from the start (tests can [`SimTransport::inject`] more).
    #[serde(default)]
    pub faults: Vec<Fault>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(with = "capture::hex_u16")]
    pub command: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<u8>>,
    pub support: Support,
    /// `measured <source>: what was seen`, or `inferred: why`.
    pub evidence: String,
    /// A getter with a fixed answer the model doesn't simulate (e.g. a firmware version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Answers 0x02 and behaves: writes change state, reads report it.
    Works,
    /// A write answered 0x02 and echoed, which changes nothing (e.g. the 0x023b logo).
    Ignored,
    /// Answers 0x05.
    NotSupported,
    /// Answers 0x03.
    Failure,
    /// Answers with a status outside the documented set.
    Mismatch,
    /// Never observed: fails with [`Unmodeled`].
    Unknown,
}

/// Wire values, as the getters return them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Per zone (index 0 = zone 1).
    pub perf_mode: [u8; 2],
    pub fan_mode: [u8; 2],
    /// RPM; 0 = never set.
    pub setpoint_rpm: [u16; 2],
    pub actual_rpm: [u16; 2],
    pub cpu_boost: u8,
    pub gpu_boost: u8,
    pub max_fan: u8,
    pub logo_power: u8,
    pub logo_mode: u8,
    pub kbd_brightness: u8,
    pub kbd_effect: u8,
    pub battery_care: u8,
    pub lights_always_on: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanModel {
    /// Where Auto settles, per zone.
    pub auto_rpm: [u16; 2],
    pub manual: ManualFan,
    /// Manual set points are held within these (0x029F: 2000 measured 2026-08-15).
    pub floor_rpm: u16,
    pub ceiling_rpm: u16,
    /// Where Max Fan drives both zones in Custom.
    pub max_fan_rpm: u16,
    /// How fast the actual speed moves (0x029F: ~23-35 manual, ~40 with Max Fan).
    pub slew_rpm_per_s: u16,
    /// For [`ManualFan::Holds`]: what the fans hold whatever the set point.
    #[serde(default)]
    pub hold_rpm: [u16; 2],
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualFan {
    /// The fans settle at the set point, held within [floor, ceiling].
    Honored,
    /// The fans hold `hold_rpm` whatever the set point (0x02A0, issue #1).
    Holds,
    /// Not observed: in Manual the actual speed doesn't move.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
    /// The next `reads` actual-RPM reads of `zone` (1 or 2) answer 0, which is impossible
    /// for one zone alone (0x029F 2026-08-15: ~3 in 10 sparse reads).
    LoneZoneZero { zone: u8, reads: u32 },
    /// Writes between these times are acknowledged and echoed but change nothing
    /// (0x029F 2026-10-19; the cause is not pinned down).
    IgnoreWrites { from_ms: u64, until_ms: u64 },
    /// `command` answers busy `times` times.
    Busy {
        #[serde(with = "capture::hex_u16")]
        command: u16,
        times: u32,
    },
    /// `command`'s zone 2 write answers 0x03 `times` times; zone 1 took.
    FailZone2 {
        #[serde(with = "capture::hex_u16")]
        command: u16,
        times: u32,
    },
    /// The next charge-limit read answers "off".
    ChargeReadsOffOnce,
}

/// A command the model has no observation for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmodeled {
    pub model: String,
    pub command: u16,
    pub args: Vec<u8>,
    pub why: String,
}

impl std::fmt::Display for Unmodeled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the {} model has no observation for {:#06x} {:?}: {}",
            self.model, self.command, self.args, self.why
        )
    }
}

impl std::error::Error for Unmodeled {}

/// Whether `err` is a coverage gap rather than an EC answer.
pub fn unmodeled(err: &anyhow::Error) -> Option<&Unmodeled> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<Unmodeled>())
}

impl Model {
    pub fn from_json(json: &str) -> Result<Model> {
        let model: Model = serde_json::from_str(json)?;
        anyhow::ensure!(
            model.format == FORMAT,
            "model format {:?}, expected {FORMAT:?}",
            model.format
        );
        Ok(model)
    }

    /// A model from a read-only capture. Every rule it can fill is measured from the
    /// capture; writes have no rule (a read-only probe can't say what they do), and the
    /// fan dynamics are unknown. Annotate it by hand from issue reports and hardware runs.
    pub fn from_capture(cap: &Capture, source: &str) -> Model {
        let mut state = State {
            perf_mode: [0; 2],
            fan_mode: [0; 2],
            setpoint_rpm: [0; 2],
            actual_rpm: [0; 2],
            cpu_boost: 0,
            gpu_boost: 0,
            max_fan: 0,
            logo_power: 0,
            logo_mode: 0,
            kbd_brightness: 0,
            kbd_effect: 0,
            battery_care: BatteryCare::DISABLE.wire_byte(),
            lights_always_on: 0,
        };
        let mut rules: Vec<Rule> = Vec::new();
        for step in &cap.steps {
            let support = match step.exchanges.last().map(|e| &e.outcome) {
                Some(Outcome::Response { verdict, .. }) => match verdict {
                    Verdict::Ok => Support::Works,
                    Verdict::NotSupported => Support::NotSupported,
                    Verdict::Failure => Support::Failure,
                    Verdict::Mismatch => Support::Mismatch,
                    Verdict::Busy => Support::Unknown,
                },
                _ => Support::Unknown,
            };
            if let StepResult::Ok { args } = &step.result {
                note_reading(&mut state, step.command, &step.args, args);
            }
            let known = rules
                .iter()
                .any(|r| r.command == step.command && r.args.is_none());
            if !known {
                rules.push(Rule {
                    command: step.command,
                    args: None,
                    support,
                    evidence: format!("measured {source}: {}", step.label),
                    reply: None,
                });
            }
        }
        Model {
            format: FORMAT.into(),
            name: cap.machine.profile.clone(),
            pid: cap.machine.pid,
            sources: BTreeMap::from([(
                source.to_string(),
                format!(
                    "{} capture by {} ({})",
                    cap.mode, cap.tool, cap.machine.model_number
                ),
            )]),
            fan: FanModel {
                auto_rpm: state.actual_rpm,
                manual: ManualFan::Unknown,
                floor_rpm: 0,
                ceiling_rpm: 0,
                max_fan_rpm: 0,
                slew_rpm_per_s: 0,
                hold_rpm: [0; 2],
                evidence: format!("measured {source}: Auto reading only; manual unknown"),
            },
            state,
            rules,
            faults: Vec::new(),
        }
    }
}

/// Fold a getter's reply into the starting state.
fn note_reading(state: &mut State, command: u16, req: &[u8], reply: &[u8]) {
    let zone = || {
        req.get(1)
            .map(|z| (*z as usize).clamp(1, 2) - 1)
            .unwrap_or(0)
    };
    let at = |i: usize| reply.get(i).copied().unwrap_or(0);
    match command {
        0x0d82 => {
            state.perf_mode[zone()] = at(2);
            state.fan_mode[zone()] = at(3);
        }
        0x0d81 => state.setpoint_rpm[zone()] = at(2) as u16 * 100,
        0x0d88 => state.actual_rpm[zone()] = at(2) as u16 * 100,
        0x0d87 if req.get(1) == Some(&1) => state.cpu_boost = at(2),
        0x0d87 => state.gpu_boost = at(2),
        0x078f => state.max_fan = at(0),
        0x0792 => state.battery_care = at(0),
        0x0380 => state.logo_power = at(2),
        0x0382 => state.logo_mode = at(2),
        0x0383 => state.kbd_brightness = at(2),
        0x0f82 => state.kbd_effect = at(2),
        0x0084 => state.lights_always_on = at(0),
        _ => {}
    }
}

/// An EC that answers from a [`Model`]. `Sync`, like `Device`.
pub struct SimTransport {
    inner: Mutex<Inner>,
    recorder: Recorder,
    unsupported: Option<UnsupportedLog>,
}

struct Inner {
    model: Model,
    state: State,
    /// Actual RPM with sub-100 precision; `state.actual_rpm` is what the EC reports.
    actual: [f64; 2],
    now_ms: u64,
    faults: Vec<Fault>,
    gaps: Vec<Unmodeled>,
}

impl SimTransport {
    pub fn new(model: Model) -> SimTransport {
        let state = model.state.clone();
        let actual = state.actual_rpm.map(f64::from);
        let faults = model.faults.clone();
        SimTransport {
            inner: Mutex::new(Inner {
                model,
                state,
                actual,
                now_ms: 0,
                faults,
                gaps: Vec::new(),
            }),
            recorder: Recorder::default(),
            unsupported: None,
        }
    }

    pub fn from_json(json: &str) -> Result<SimTransport> {
        Ok(SimTransport::new(Model::from_json(json)?))
    }

    /// Learn what the EC lacks, as the generic profile does (`Device::learning_unsupported`),
    /// including the per-PID `KNOWN_LACKING` entries.
    pub fn learning(mut self) -> SimTransport {
        let pid = self.lock().model.pid;
        self.unsupported = Some(UnsupportedLog::starting_with(
            crate::unsupported::known_lacking(pid),
        ));
        self
    }

    pub fn inject(&self, fault: Fault) {
        self.lock().faults.push(fault);
    }

    /// Let simulated time pass; the fans move toward their targets.
    pub fn advance(&self, duration: Duration) {
        self.lock().tick(duration.as_millis() as u64);
    }

    pub fn now_ms(&self) -> u64 {
        self.lock().now_ms
    }

    /// What the EC holds now.
    pub fn state(&self) -> State {
        self.lock().state.clone()
    }

    /// Every unmodeled command asked so far: the coverage gaps of a run.
    pub fn gaps(&self) -> Vec<Unmodeled> {
        self.lock().gaps.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn exchange(&self, request: Packet, retry_failure: bool) -> Result<Packet> {
        for attempt in 0..retry::MAX_ATTEMPTS {
            let mut inner = self.lock();
            inner.tick(EXCHANGE_MS);
            let answer = inner.answer(&request);
            let now = inner.now_ms;
            drop(inner);

            let (status, args) = match answer {
                Ok(answer) => answer,
                Err(gap) => {
                    self.recorder.record_at(
                        now,
                        &request,
                        attempt,
                        Outcome::TransportError {
                            error: format!("unmodeled: {}", gap.why),
                        },
                    );
                    self.lock().gaps.push(gap.clone());
                    return Err(anyhow::Error::new(gap));
                }
            };
            let response = request.response(&args, status);
            self.recorder
                .record_response(&request, &response, attempt, Some(now));
            let err = match response.classify_response(&request) {
                Ok(()) => return Ok(response),
                Err(e) => e,
            };
            match retry::after_rejection(err, retry_failure, attempt) {
                Decision::Fail => return Err(anyhow::Error::new(err)),
                Decision::Exhausted => {
                    return Err(anyhow::Error::new(err).context(format!(
                        "failed to match report after {} attempts",
                        retry::MAX_ATTEMPTS
                    )))
                }
                Decision::RetryAfter(backoff) => self.advance(backoff),
            }
        }
        unreachable!("the retry policy ends every exchange")
    }
}

impl Inner {
    fn tick(&mut self, ms: u64) {
        self.now_ms += ms;
        let step = self.model.fan.slew_rpm_per_s as f64 * ms as f64 / 1000.0;
        for z in 0..2 {
            let Some(target) = self.fan_target(z) else {
                continue;
            };
            let target = target as f64;
            let a = self.actual[z];
            self.actual[z] = if (target - a).abs() <= step {
                target
            } else if target > a {
                a + step
            } else {
                a - step
            };
            self.state.actual_rpm[z] = ((self.actual[z] / 100.0).round() * 100.0) as u16;
        }
    }

    fn fan_target(&self, z: usize) -> Option<u16> {
        let fan = &self.model.fan;
        if self.state.perf_mode[z] == CUSTOM && self.state.max_fan == MAX_FAN_ON {
            return Some(fan.max_fan_rpm);
        }
        if self.state.fan_mode[z] != MANUAL {
            return Some(fan.auto_rpm[z]);
        }
        match fan.manual {
            ManualFan::Honored => Some(
                self.state.setpoint_rpm[z]
                    .max(fan.floor_rpm)
                    .min(fan.ceiling_rpm.max(fan.floor_rpm)),
            ),
            ManualFan::Holds => Some(fan.hold_rpm[z]),
            ManualFan::Unknown => None,
        }
    }

    fn gap(&self, request: &[u8], command: u16, why: impl Into<String>) -> Unmodeled {
        Unmodeled {
            model: self.model.name.clone(),
            command,
            args: request.to_vec(),
            why: why.into(),
        }
    }

    fn take_fault(&mut self, pred: impl Fn(&mut Fault) -> bool) -> bool {
        self.faults.iter_mut().any(pred)
    }

    /// (status, reply args) for one attempt.
    fn answer(&mut self, request: &Packet) -> std::result::Result<(u8, Vec<u8>), Unmodeled> {
        let command = request.command();
        let args = request.get_args()[..request.data_len()].to_vec();
        let is_write = command & 0x0080 == 0;
        let zone = args.get(1).copied();

        let busy = self.take_fault(|f| match f {
            Fault::Busy { command: c, times } if *c == command && *times > 0 => {
                *times -= 1;
                true
            }
            _ => false,
        });
        if busy {
            return Ok((0x01, args));
        }

        let rule = self
            .model
            .rules
            .iter()
            .find(|r| {
                r.command == command
                    && r.args
                        .as_ref()
                        .is_none_or(|prefix| args.starts_with(prefix))
            })
            .cloned();
        let Some(rule) = rule else {
            return Err(self.gap(&args, command, "no rule for this command"));
        };
        match rule.support {
            Support::Unknown => return Err(self.gap(&args, command, rule.evidence)),
            Support::NotSupported => return Ok((0x05, args)),
            Support::Failure => return Ok((0x03, args)),
            Support::Mismatch => return Ok((0x00, args)),
            Support::Ignored if is_write => return Ok((0x02, args)),
            Support::Ignored | Support::Works => {}
        }

        if is_write {
            let now = self.now_ms;
            let ignored = self.faults.iter().any(|f| {
                matches!(f, Fault::IgnoreWrites { from_ms, until_ms }
                    if (*from_ms..*until_ms).contains(&now))
            });
            if ignored {
                return Ok((0x02, args));
            }
            let fail_zone2 = zone == Some(2)
                && self.take_fault(|f| match f {
                    Fault::FailZone2 { command: c, times } if *c == command && *times > 0 => {
                        *times -= 1;
                        true
                    }
                    _ => false,
                });
            if fail_zone2 {
                return Ok((0x03, args));
            }
        }

        if let Some(reply) = rule.reply {
            return Ok((0x02, reply));
        }
        self.apply(command, &args)
            .map(|reply| (0x02, reply))
            .ok_or_else(|| {
                self.gap(
                    &args,
                    command,
                    "works, but the simulator has no reply for it",
                )
            })
    }

    /// Do what a working command does; the reply args, or `None` for a command the
    /// simulator doesn't know.
    fn apply(&mut self, command: u16, args: &[u8]) -> Option<Vec<u8>> {
        let at = |i: usize| args.get(i).copied().unwrap_or(0);
        let z = (at(1) as usize).clamp(1, 2) - 1;
        let s = &mut self.state;
        let reply = match command {
            0x0d02 => {
                s.perf_mode[z] = at(2);
                s.fan_mode[z] = at(3);
                // Leaving Custom clears Max Fan in the EC (state.rs, apply_perf_fan_logo).
                if at(2) != CUSTOM {
                    s.max_fan = 0;
                }
                args.to_vec()
            }
            0x0d82 => vec![0, at(1), s.perf_mode[z], s.fan_mode[z]],
            0x0d01 => {
                s.setpoint_rpm[z] = at(2) as u16 * 100;
                args.to_vec()
            }
            0x0d81 => vec![0, at(1), (s.setpoint_rpm[z] / 100) as u8],
            0x0d88 => {
                let zone = at(1);
                let bogus = self.faults.iter_mut().any(|f| match f {
                    Fault::LoneZoneZero { zone: fz, reads } if *fz == zone && *reads > 0 => {
                        *reads -= 1;
                        true
                    }
                    _ => false,
                });
                let rpm = if bogus { 0 } else { self.state.actual_rpm[z] };
                vec![0, zone, (rpm / 100) as u8]
            }
            0x0d07 => {
                match at(1) {
                    1 => s.cpu_boost = at(2),
                    _ => s.gpu_boost = at(2),
                }
                args.to_vec()
            }
            0x0d87 => {
                let level = if at(1) == 1 { s.cpu_boost } else { s.gpu_boost };
                vec![0, at(1), level]
            }
            0x070f => {
                s.max_fan = at(0);
                args.to_vec()
            }
            0x078f => vec![s.max_fan],
            0x0712 => {
                s.battery_care = at(0);
                args.to_vec()
            }
            0x0792 => {
                let off = self
                    .faults
                    .iter()
                    .position(|f| *f == Fault::ChargeReadsOffOnce);
                match off {
                    Some(i) => {
                        self.faults.remove(i);
                        vec![BatteryCare::DISABLE.wire_byte()]
                    }
                    None => vec![self.state.battery_care],
                }
            }
            0x0300 => {
                s.logo_power = at(2);
                args.to_vec()
            }
            0x0380 => vec![at(0), at(1), s.logo_power],
            0x0302 => {
                s.logo_mode = at(2);
                args.to_vec()
            }
            0x0382 => vec![at(0), at(1), s.logo_mode],
            0x0303 => {
                s.kbd_brightness = at(2);
                args.to_vec()
            }
            0x0383 => vec![at(0), at(1), s.kbd_brightness],
            0x0f02 => {
                s.kbd_effect = at(2);
                args.to_vec()
            }
            0x0f82 => vec![at(0), at(1), s.kbd_effect],
            0x0004 => {
                s.lights_always_on = at(0);
                args.to_vec()
            }
            0x0084 => vec![s.lights_always_on, 0],
            // Keyboard custom frame rows and the "show the frame" command echo.
            0x030b | 0x030a => args.to_vec(),
            _ => return None,
        };
        Some(reply)
    }
}

impl HidTransport for SimTransport {
    fn send(&self, packet: Packet) -> Result<Packet> {
        self.exchange(packet, true)
    }

    fn send_once(&self, packet: Packet) -> Result<Packet> {
        self.exchange(packet, false)
    }

    fn unsupported(&self) -> Option<&UnsupportedLog> {
        self.unsupported.as_ref()
    }

    fn pause(&self, duration: Duration) {
        self.advance(duration);
    }
}

impl Recording for SimTransport {
    fn drain_exchanges(&self) -> Vec<Exchange> {
        self.recorder.drain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command;
    use crate::packet::ResponseError;
    use crate::types::FanZone;

    /// A small model: everything the tools use works, the fans are honored from 2000.
    fn model() -> Model {
        let works = |command: u16| Rule {
            command,
            args: None,
            support: Support::Works,
            evidence: "inferred: test model".into(),
            reply: None,
        };
        Model {
            format: FORMAT.into(),
            name: "test".into(),
            pid: 0xfffe,
            sources: BTreeMap::new(),
            state: State {
                perf_mode: [0; 2],
                fan_mode: [0; 2],
                setpoint_rpm: [0; 2],
                actual_rpm: [2000, 2000],
                cpu_boost: 1,
                gpu_boost: 2,
                max_fan: 0,
                logo_power: 1,
                logo_mode: 0,
                kbd_brightness: 128,
                kbd_effect: 3,
                battery_care: BatteryCare::from_percent(80).unwrap().wire_byte(),
                lights_always_on: 0,
            },
            fan: FanModel {
                auto_rpm: [2000, 2000],
                manual: ManualFan::Honored,
                floor_rpm: 2000,
                ceiling_rpm: 5000,
                max_fan_rpm: 4700,
                slew_rpm_per_s: 30,
                hold_rpm: [0; 2],
                evidence: "inferred: test model".into(),
            },
            rules: [
                0x0d02, 0x0d82, 0x0d01, 0x0d81, 0x0d88, 0x0d07, 0x0d87, 0x070f, 0x078f, 0x0712,
                0x0792, 0x0300, 0x0380, 0x0302, 0x0382, 0x0303, 0x0383, 0x0f02, 0x0f82, 0x0004,
                0x0084,
            ]
            .into_iter()
            .map(works)
            .collect(),
            faults: Vec::new(),
        }
    }

    #[test]
    fn a_working_model_round_trips_through_the_real_commands() {
        let sim = SimTransport::new(model());
        command::set_perf_mode(&sim, PerfMode::Custom).unwrap();
        assert_eq!(
            command::get_perf_mode(&sim).unwrap(),
            (PerfMode::Custom, FanMode::Auto)
        );
        command::set_max_fan_speed_mode(&sim, crate::types::MaxFanSpeedMode::Enable).unwrap();
        command::set_perf_mode(&sim, PerfMode::Balanced).unwrap();
        assert_eq!(sim.state().max_fan, 0, "leaving Custom clears Max Fan");
    }

    #[test]
    fn the_fan_moves_at_its_slew_rate_and_holds_the_floor() {
        let sim = SimTransport::new(model());
        command::set_fan_mode(&sim, FanMode::Manual).unwrap();
        command::set_fan_rpm(&sim, 3000, false).unwrap();
        assert_eq!(command::get_fan_rpm(&sim, FanZone::Zone1).unwrap(), 3000);
        sim.advance(Duration::from_secs(10));
        let actual = command::get_fan_actual_rpm(&sim, FanZone::Zone1).unwrap();
        assert!(
            (2200..=2400).contains(&actual),
            "~30 RPM/s for 10 s: {actual}"
        );
        sim.advance(Duration::from_secs(60));
        assert_eq!(
            command::get_fan_actual_rpm(&sim, FanZone::Zone1).unwrap(),
            3000
        );

        // Below the floor: the set point reads back as written, the fans hold the floor.
        sim.inner.lock().unwrap().state.setpoint_rpm = [800, 800];
        sim.advance(Duration::from_secs(60));
        assert_eq!(command::get_fan_rpm(&sim, FanZone::Zone1).unwrap(), 800);
        assert_eq!(
            command::get_fan_actual_rpm(&sim, FanZone::Zone1).unwrap(),
            2000
        );
    }

    #[test]
    fn an_unobserved_command_is_a_gap_not_an_echo() {
        let mut m = model();
        m.rules.retain(|r| r.command != 0x070f);
        let sim = SimTransport::new(m);
        command::set_perf_mode(&sim, PerfMode::Custom).unwrap();
        let err = command::set_max_fan_speed_mode(&sim, crate::types::MaxFanSpeedMode::Disable)
            .unwrap_err();
        let gap = unmodeled(&err).expect("a gap");
        assert_eq!(gap.command, 0x070f);
        assert_eq!(sim.gaps().len(), 1);
    }

    #[test]
    fn not_supported_is_typed_and_not_retried() {
        let mut m = model();
        m.rules.insert(
            0,
            Rule {
                command: 0x070f,
                args: Some(vec![2]),
                support: Support::NotSupported,
                evidence: "inferred: test".into(),
                reply: None,
            },
        );
        let sim = SimTransport::new(m);
        command::set_perf_mode(&sim, PerfMode::Custom).unwrap();
        sim.drain_exchanges();
        let err = command::set_max_fan_speed_mode(&sim, crate::types::MaxFanSpeedMode::Enable)
            .unwrap_err();
        assert!(crate::error::is_not_supported(&err));
        let tries = sim
            .drain_exchanges()
            .into_iter()
            .filter(|e| e.command == 0x070f)
            .count();
        assert_eq!(tries, 1, "a 0x05 is final");
        // The args prefix limited the rule to Enable.
        command::set_max_fan_speed_mode(&sim, crate::types::MaxFanSpeedMode::Disable).unwrap();
    }

    #[test]
    fn busy_is_retried_and_failure_exhausts_the_attempts() {
        let sim = SimTransport::new(model());
        sim.inject(Fault::Busy {
            command: 0x0d82,
            times: 2,
        });
        let t0 = sim.now_ms();
        command::get_perf_mode(&sim).unwrap();
        assert!(sim.now_ms() - t0 >= 40, "two 20 ms busy backoffs");

        let mut m = model();
        m.rules.insert(
            0,
            Rule {
                command: 0x0712,
                args: None,
                support: Support::Failure,
                evidence: "inferred: test".into(),
                reply: None,
            },
        );
        let sim = SimTransport::new(m);
        let err = command::set_battery_care(&sim, BatteryCare::DISABLE).unwrap_err();
        assert!(format!("{err:#}").contains("after 5 attempts"), "{err:#}");
    }

    #[test]
    fn faults_cover_lone_zone_reads_ignored_writes_and_half_applied_writes() {
        let sim = SimTransport::new(model());
        sim.inject(Fault::LoneZoneZero { zone: 2, reads: 1 });
        assert_eq!(
            command::get_fan_actual_rpm(&sim, FanZone::Zone2).unwrap(),
            0
        );
        assert_eq!(
            command::get_fan_actual_rpm(&sim, FanZone::Zone2).unwrap(),
            2000
        );

        sim.inject(Fault::IgnoreWrites {
            from_ms: 0,
            until_ms: u64::MAX,
        });
        command::set_perf_mode(&sim, PerfMode::Silent).unwrap();
        assert_eq!(
            command::get_perf_mode(&sim).unwrap().0,
            PerfMode::Balanced,
            "acknowledged, echoed, ignored"
        );

        let sim = SimTransport::new(model());
        sim.inject(Fault::FailZone2 {
            command: 0x0d02,
            times: 5,
        });
        assert!(command::set_perf_mode(&sim, PerfMode::Silent).is_err());
        assert_eq!(
            sim.state().perf_mode,
            [PerfMode::Silent as u8, 0],
            "zone 1 took"
        );
    }

    #[test]
    fn the_charge_limit_off_read_is_confirmed_away() {
        let sim = SimTransport::new(model());
        sim.inject(Fault::ChargeReadsOffOnce);
        let care = command::get_battery_care(&sim).unwrap();
        assert_eq!(care, BatteryCare::from_percent(80).unwrap());
    }

    #[test]
    fn a_learning_sim_records_what_it_lacks() {
        let mut m = model();
        m.rules.insert(
            0,
            Rule {
                command: 0x0792,
                args: None,
                support: Support::NotSupported,
                evidence: "inferred: test".into(),
                reply: None,
            },
        );
        let sim = SimTransport::new(m).learning();
        crate::state::DeviceState::read(&sim).unwrap();
        assert!(
            crate::state::unsupported_on(&sim).contains(crate::unsupported::Optional::BatteryCare)
        );
        let _ = ResponseError::NotSupported;
    }
}
