//! `razer-cli probe`: every getter the tools use, sent once each through the normal retry
//! loop, with each attempt recorded. The result ([`crate::capture::Capture`]) is what a
//! device-support issue needs, and what [`crate::sim`] builds an EC model from.
//!
//! Read-only by construction: every command here is a getter (command id bit 0x80 set),
//! which a test checks. Nothing is written, so nothing needs restoring.

use std::time::Duration;

use crate::capture::{Capture, Machine, Recording, Step, StepResult, FORMAT};
use crate::transport::HidTransport;

pub struct Read {
    pub label: &'static str,
    pub command: u16,
    pub args: &'static [u8],
}

const fn read(label: &'static str, command: u16, args: &'static [u8]) -> Read {
    Read {
        label,
        command,
        args,
    }
}

/// The getters, with the args the tools send them.
pub const READS: &[Read] = &[
    read("perf and fan mode, zone 1", 0x0d82, &[0, 1, 0, 0]),
    read("perf and fan mode, zone 2", 0x0d82, &[0, 2, 0, 0]),
    read("fan set point, zone 1", 0x0d81, &[0, 1, 0]),
    read("fan set point, zone 2", 0x0d81, &[0, 2, 0]),
    read("CPU boost", 0x0d87, &[0, 1, 0]),
    read("GPU boost", 0x0d87, &[0, 2, 0]),
    read("Max Fan", 0x078f, &[0]),
    read("charge limit", 0x0792, &[0]),
    read("logo power", 0x0380, &[1, 4, 0]),
    read("logo mode", 0x0382, &[1, 4, 0]),
    read("keyboard brightness", 0x0383, &[1, 5, 0]),
    read("keyboard effect", 0x0f82, &[1, 5, 0]),
    read("lights always on", 0x0084, &[0, 0]),
];

/// Actual fan speed is read in rounds: one round can't tell a lone-zone bogus read
/// from a real one.
pub const FAN_ACTUAL_ROUNDS: usize = 3;
pub const FAN_ACTUAL_SPACING: Duration = Duration::from_millis(500);

/// Run the read-only probe. `device` should be recording; anything it recorded before
/// is dropped first.
pub fn run<T: HidTransport + Recording>(device: &T, machine: Machine, tool: &str) -> Capture {
    device.drain_exchanges();
    let mut steps: Vec<Step> = READS
        .iter()
        .map(|r| step(device, r.label.to_string(), r.command, r.args))
        .collect();
    for round in 1..=FAN_ACTUAL_ROUNDS {
        if round > 1 {
            device.pause(FAN_ACTUAL_SPACING);
        }
        for zone in [1u8, 2] {
            let label = format!("fan actual, zone {zone}, round {round}");
            steps.push(step(device, label, 0x0d88, &[0, zone, 0]));
        }
    }
    Capture {
        format: FORMAT.into(),
        tool: tool.into(),
        machine,
        mode: "read-only".into(),
        steps,
        restored: None,
    }
}

fn step<T: HidTransport + Recording>(device: &T, label: String, command: u16, args: &[u8]) -> Step {
    let result = match device.send(crate::packet::Packet::new(command, args)) {
        Ok(reply) => StepResult::Ok {
            args: reply.get_args()[..reply.data_len().max(args.len())].to_vec(),
        },
        Err(e) => StepResult::Error {
            error: format!("{e:#}"),
        },
    };
    Step {
        label,
        command,
        args: args.to_vec(),
        result,
        check: None,
        exchanges: device.drain_exchanges(),
    }
}

// ---- probe --writes ----
//
// Recovery contract: the settings it changes are read into a [`Snapshot`] before the
// first write (the CLI saves it to a recovery file first), restored in dependency order
// (a perf mode write resets the fan to Auto; leaving Custom clears Max Fan), and read back
// to verify. If the snapshot can't be read cleanly, nothing is written.

/// The settings `probe --writes` changes, as wire values, read before the first write.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub perf_mode: u8,
    pub fan_mode: u8,
    /// Per zone; only restored when the fan was in Manual.
    pub setpoint_rpm: [u16; 2],
    /// Read only in Custom, where they exist.
    pub cpu_boost: Option<u8>,
    pub gpu_boost: Option<u8>,
    pub max_fan: Option<u8>,
}

/// Read a snapshot. Fails, so nothing gets written, if any read fails or the zones
/// disagree (there would be no single state to restore).
pub fn snapshot(device: &impl HidTransport) -> anyhow::Result<Snapshot> {
    use crate::command;
    use crate::types::{FanMode, FanZone, PerfMode};
    let (perf, fan) = command::get_perf_mode(device)?;
    let custom = perf == PerfMode::Custom;
    Ok(Snapshot {
        perf_mode: perf as u8,
        fan_mode: fan as u8,
        setpoint_rpm: if fan == FanMode::Manual {
            [
                command::get_fan_rpm(device, FanZone::Zone1)?,
                command::get_fan_rpm(device, FanZone::Zone2)?,
            ]
        } else {
            [0, 0]
        },
        cpu_boost: if custom {
            Some(command::get_cpu_boost(device)? as u8)
        } else {
            None
        },
        gpu_boost: if custom {
            Some(command::get_gpu_boost(device)? as u8)
        } else {
            None
        },
        max_fan: if custom {
            Some(command::get_max_fan_speed_mode(device)? as u8)
        } else {
            None
        },
    })
}

/// Put back what differs from `snap`, then verify. Writes only what changed, so a
/// setting the EC rejects (e.g. Max Fan on a model without it) is left alone when it
/// never moved.
pub fn restore(device: &impl HidTransport, snap: &Snapshot) -> anyhow::Result<()> {
    use crate::command;
    use crate::types::{CpuBoost, FanMode, GpuBoost, MaxFanSpeedMode, PerfMode};
    let now = snapshot(device).ok();
    let perf = PerfMode::try_from(snap.perf_mode)?;
    let fan = FanMode::try_from(snap.fan_mode)?;

    let perf_moved = now.as_ref().is_none_or(|n| {
        n.perf_mode != snap.perf_mode
            || n.cpu_boost != snap.cpu_boost
            || n.gpu_boost != snap.gpu_boost
    });
    if perf_moved {
        command::set_perf_mode(device, perf)?;
        if let (Some(cpu), Some(gpu)) = (snap.cpu_boost, snap.gpu_boost) {
            command::set_cpu_boost(device, CpuBoost::try_from(cpu)?)?;
            command::set_gpu_boost(device, GpuBoost::try_from(gpu)?)?;
        }
    }
    let now = snapshot(device).ok();
    let fan_moved = now.as_ref().is_none_or(|n| {
        n.fan_mode != snap.fan_mode
            || (fan == FanMode::Manual && n.setpoint_rpm != snap.setpoint_rpm)
    });
    if fan_moved {
        command::set_fan_mode(device, fan)?;
        if fan == FanMode::Manual {
            // Raw, per zone: the set point to restore may be one the CLI wouldn't accept.
            for (i, rpm) in snap.setpoint_rpm.iter().enumerate() {
                command::send_command(device, 0x0d01, &[0, i as u8 + 1, (rpm / 100) as u8])?;
            }
        }
    }
    if let Some(max) = snap.max_fan {
        let now = snapshot(device).ok();
        if now.as_ref().is_none_or(|n| n.max_fan != Some(max)) {
            command::set_max_fan_speed_mode(device, MaxFanSpeedMode::try_from(max)?)?;
        }
    }
    let after = snapshot(device)?;
    anyhow::ensure!(
        &after == snap,
        "restore didn't verify: before {snap:?}, now {after:?}"
    );
    Ok(())
}

/// How long to watch the fans settle at each manual set point.
pub const SETTLE_POLL: Duration = Duration::from_secs(5);

/// The write set. `fan_range` is the chassis's manual range (the descriptor's): the set
/// points tried are its ends and middle. Restores and verifies before returning, even
/// after a failed step; `restored` in the capture says how that went.
pub fn run_writes<T: HidTransport + Recording>(
    device: &T,
    machine: Machine,
    tool: &str,
    snap: &Snapshot,
    fan_range: (u16, u16),
    settle: Duration,
) -> Capture {
    use crate::command;
    use crate::types::{CpuBoost, FanMode, FanZone, GpuBoost, MaxFanSpeedMode, PerfMode};

    device.drain_exchanges();
    let mut steps = Vec::new();
    let mut write = |label: String,
                     command: u16,
                     args: &[u8],
                     run: &mut dyn FnMut() -> anyhow::Result<String>| {
        let (result, check) = match run() {
            Ok(check) => (StepResult::Ok { args: Vec::new() }, Some(check)),
            Err(e) => (
                StepResult::Error {
                    error: format!("{e:#}"),
                },
                None,
            ),
        };
        steps.push(Step {
            label,
            command,
            args: args.to_vec(),
            result,
            check,
            exchanges: device.drain_exchanges(),
        });
    };
    let modes = |d: &T| -> anyhow::Result<String> {
        let (p, f) = command::get_perf_mode(d)?;
        Ok(format!("read back {p:?}/{f:?} on both zones"))
    };

    for mode in [
        PerfMode::Balanced,
        PerfMode::Silent,
        PerfMode::Performance,
        PerfMode::Custom,
    ] {
        write(
            format!("set perf {mode:?}"),
            0x0d02,
            &[1, 1, mode as u8, 0],
            &mut || {
                command::set_perf_mode(device, mode)?;
                modes(device)
            },
        );
    }
    write(
        "set CPU boost Medium, GPU boost High".into(),
        0x0d07,
        &[1, 1, CpuBoost::Medium as u8],
        &mut || {
            command::set_cpu_boost(device, CpuBoost::Medium)?;
            command::set_gpu_boost(device, GpuBoost::High)?;
            Ok(format!(
                "read back {:?}/{:?}",
                command::get_cpu_boost(device)?,
                command::get_gpu_boost(device)?
            ))
        },
    );
    for max in [
        MaxFanSpeedMode::Disable,
        MaxFanSpeedMode::Enable,
        MaxFanSpeedMode::Disable,
    ] {
        write(
            format!("set Max Fan {max:?}"),
            0x070f,
            &[max as u8],
            &mut || {
                command::set_max_fan_speed_mode(device, max)?;
                Ok(format!(
                    "read back {:?}",
                    command::get_max_fan_speed_mode(device)?
                ))
            },
        );
    }
    // The fan sweep runs in Balanced: Hyperboost (0x029F, 2026-10-02) treats a manual set
    // point as a floor under its own curve, and Custom may too.
    write(
        "set perf Balanced".into(),
        0x0d02,
        &[1, 1, 0, 0],
        &mut || {
            command::set_perf_mode(device, PerfMode::Balanced)?;
            modes(device)
        },
    );
    write("set fan Manual".into(), 0x0d02, &[1, 1, 0, 1], &mut || {
        command::set_fan_mode(device, FanMode::Manual)?;
        modes(device)
    });
    let (lo, hi) = fan_range;
    let mid = (lo / 2 + hi / 2) / 100 * 100;
    for rpm in [hi, mid, lo] {
        write(
            format!("set fan {rpm} RPM"),
            0x0d01,
            &[0, 1, (rpm / 100) as u8],
            &mut || {
                command::set_fan_rpm(device, rpm, false)?;
                let set = [
                    command::get_fan_rpm(device, FanZone::Zone1)?,
                    command::get_fan_rpm(device, FanZone::Zone2)?,
                ];
                // Watch the fans until two polls agree, or `settle` runs out.
                let mut curve = Vec::new();
                let mut waited = Duration::ZERO;
                loop {
                    let a = [
                        command::get_fan_actual_rpm(device, FanZone::Zone1)?,
                        command::get_fan_actual_rpm(device, FanZone::Zone2)?,
                    ];
                    let settled = curve.last() == Some(&a);
                    curve.push(a);
                    if settled || waited >= settle {
                        break;
                    }
                    device.pause(SETTLE_POLL);
                    waited += SETTLE_POLL;
                }
                let curve: Vec<String> =
                    curve.iter().map(|a| format!("{}/{}", a[0], a[1])).collect();
                Ok(format!(
                    "set point read back {}/{}; actual every {}s: {}",
                    set[0],
                    set[1],
                    SETTLE_POLL.as_secs(),
                    curve.join(" ")
                ))
            },
        );
    }
    write("set fan Auto".into(), 0x0d02, &[1, 1, 0, 0], &mut || {
        command::set_fan_mode(device, FanMode::Auto)?;
        modes(device)
    });

    let restored = match restore(device, snap) {
        Ok(()) => "verified".to_string(),
        Err(e) => format!("FAILED: {e:#}"),
    };
    device.drain_exchanges();
    Capture {
        format: FORMAT.into(),
        tool: tool.into(),
        machine,
        mode: "writes".into(),
        steps,
        restored: Some(restored),
    }
}

/// One line per step for a terminal: the full JSON is for the issue.
pub fn summary(cap: &Capture) -> String {
    let mut out = format!(
        "{} ({:#06x}, {}), profile {}\n",
        cap.machine.model_number,
        cap.machine.pid,
        if cap.machine.catalogued {
            "catalogued"
        } else {
            "generic"
        },
        cap.machine.profile
    );
    for s in &cap.steps {
        let exchanges = s.exchanges.len();
        let result = match &s.result {
            StepResult::Ok { args } if args.is_empty() => "ok".to_string(),
            StepResult::Ok { args } => format!("ok {args:?}"),
            StepResult::Error { error } => format!("error: {error}"),
        };
        let retried = if exchanges > 1 {
            format!(" ({exchanges} exchanges)")
        } else {
            String::new()
        };
        let check = match &s.check {
            Some(c) => format!(": {c}"),
            None => String::new(),
        };
        out.push_str(&format!("  {:<34} {result}{retried}{check}\n", s.label));
    }
    if let Some(r) = &cap.restored {
        out.push_str(&format!("restore: {r}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sim::{Fault, Model, SimTransport};

    fn machine(m: &Model) -> Machine {
        Machine {
            pid: m.pid,
            model_number: "simulated".into(),
            profile: m.name.clone(),
            catalogued: true,
        }
    }

    fn blade16() -> Model {
        Model::from_json(include_str!("../tests/ec/0x029f-blade16-2023.json")).unwrap()
    }

    #[test]
    fn a_capture_becomes_a_model_with_the_same_starting_state() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        let cap = run(&sim, machine(&m), "test");
        let json = serde_json::to_string(&cap).unwrap();
        let back: Capture = serde_json::from_str(&json).unwrap();
        let rebuilt = Model::from_capture(&back, "cap");
        assert_eq!(rebuilt.state, m.state);
        assert!(rebuilt
            .rules
            .iter()
            .all(|r| r.support == crate::sim::Support::Works));
    }

    #[test]
    fn the_write_set_puts_everything_back_and_says_so() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        let snap = snapshot(&sim).unwrap();
        let cap = run_writes(
            &sim,
            machine(&m),
            "test",
            &snap,
            (2000, 5000),
            Duration::from_secs(120),
        );
        assert_eq!(cap.restored.as_deref(), Some("verified"));
        let failed: Vec<_> = cap
            .steps
            .iter()
            .filter(|s| matches!(s.result, StepResult::Error { .. }))
            .map(|s| &s.label)
            .collect();
        assert!(failed.is_empty(), "{failed:?}");
        let end = sim.state();
        assert_eq!(
            (end.perf_mode, end.fan_mode, end.max_fan),
            (m.state.perf_mode, m.state.fan_mode, m.state.max_fan)
        );
        let sweep = cap
            .steps
            .iter()
            .find(|s| s.label == "set fan 5000 RPM")
            .unwrap();
        assert!(
            sweep.check.as_deref().unwrap().contains("5000/5000"),
            "{:?}",
            sweep.check
        );
    }

    #[test]
    fn a_failed_write_midway_is_still_restored() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        sim.inject(Fault::FailZone2 {
            command: 0x0d02,
            times: 5,
        });
        let snap = snapshot(&sim).unwrap();
        let cap = run_writes(
            &sim,
            machine(&m),
            "test",
            &snap,
            (2000, 5000),
            Duration::ZERO,
        );
        assert!(matches!(cap.steps[0].result, StepResult::Error { .. }));
        assert_eq!(cap.restored.as_deref(), Some("verified"));
    }

    #[test]
    fn a_rejected_max_fan_is_recorded_and_not_restored() {
        let m = Model::from_json(include_str!("../tests/ec/0x028b-blade17-2022.json")).unwrap();
        let sim = SimTransport::new(m.clone());
        let snap = snapshot(&sim).unwrap();
        let cap = run_writes(
            &sim,
            machine(&m),
            "test",
            &snap,
            (3500, 5000),
            Duration::ZERO,
        );
        let enable = cap
            .steps
            .iter()
            .find(|s| s.label == "set Max Fan Enable")
            .unwrap();
        match &enable.result {
            StepResult::Error { error } => assert!(error.contains("0x05"), "{error}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(cap.restored.as_deref(), Some("verified"));
    }

    #[test]
    fn no_snapshot_when_the_zones_disagree() {
        let mut m = blade16();
        m.state.perf_mode = [0, 5];
        let sim = SimTransport::new(m);
        assert!(snapshot(&sim).is_err());
    }

    #[test]
    fn the_probe_only_reads() {
        for r in READS {
            assert_ne!(
                r.command & 0x0080,
                0,
                "{} ({:#06x}) is not a getter",
                r.label,
                r.command
            );
        }
    }
}
