//! `razer-cli probe`: every getter the tools use, sent once each through the normal retry
//! loop, with each attempt recorded. The result ([`crate::capture::Capture`]) is what a
//! device-support issue needs, and what [`crate::sim`] builds an EC model from.
//!
//! Read-only by construction: every command here is a getter (command id bit 0x80 set),
//! which a test checks. Nothing is written, so nothing needs restoring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
// Recovery contract (Sol review harness-branch-20261020-193521):
// - Every register the write set changes is read into a [`Snapshot`] before the first
//   write: perf and fan mode, both zones' set points in any mode (the sweep changes them
//   even when the fan was in Auto), both boosts, and Max Fan in Custom. A boost read that
//   fails outside Custom is recorded as unknown, and the boost writes are then skipped,
//   because they couldn't be put back. If anything else can't be read, nothing is written.
// - Every write is read back and compared with what was asked; a write that didn't take
//   fails its step, and a step whose prerequisite failed is skipped (the fan sweep needs
//   Balanced and Manual confirmed: Hyperboost, and maybe Custom, treat a set point as a
//   floor under their own curve).
// - Restore puts back everything that differs, keeps going past a failed write, verifies
//   every snapshotted field, and lists whatever is still different.
// - A cancel flag (Ctrl-C in the CLI) is checked before each step and while the fans
//   settle; cancelling skips straight to the restore.

/// What `probe --writes` changes, as wire values, read before its first write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub perf_mode: u8,
    pub fan_mode: u8,
    /// Both zones, read in any mode.
    pub setpoint_rpm: [u16; 2],
    /// `None` when the EC didn't answer (possible outside Custom).
    pub cpu_boost: Option<u8>,
    pub gpu_boost: Option<u8>,
    /// Exists only in Custom.
    pub max_fan: Option<u8>,
}

/// The format of a recovery file.
pub const RECOVERY_FORMAT: &str = "razer-ctl-recovery/1";

/// A recovery file: the snapshot, and the laptop it belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recovery {
    pub format: String,
    #[serde(with = "crate::capture::hex_u16")]
    pub pid: u16,
    pub model_number: String,
    pub snapshot: Snapshot,
}

/// Read a snapshot. Fails, so nothing gets written, if a needed read fails or the zones
/// disagree (there would be no single state to restore).
pub fn snapshot(device: &impl HidTransport) -> anyhow::Result<Snapshot> {
    use crate::command;
    use crate::types::{FanZone, PerfMode};
    let (perf, fan) = command::get_perf_mode(device)?;
    let custom = perf == PerfMode::Custom;
    let boost = |read: &dyn Fn() -> anyhow::Result<u8>| -> anyhow::Result<Option<u8>> {
        match read() {
            Ok(v) => Ok(Some(v)),
            Err(_) if !custom => Ok(None),
            Err(e) => Err(e),
        }
    };
    Ok(Snapshot {
        perf_mode: perf as u8,
        fan_mode: fan as u8,
        setpoint_rpm: [
            command::get_fan_rpm(device, FanZone::Zone1)?,
            command::get_fan_rpm(device, FanZone::Zone2)?,
        ],
        cpu_boost: boost(&|| Ok(command::get_cpu_boost(device)? as u8))?,
        gpu_boost: boost(&|| Ok(command::get_gpu_boost(device)? as u8))?,
        max_fan: if custom {
            Some(command::get_max_fan_speed_mode(device)? as u8)
        } else {
            None
        },
    })
}

/// The fields of `after` that differ from `before`, skipping boosts `before` couldn't read.
fn differences(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |name: &str, was: String, now: String| {
        if was != now {
            out.push(format!("{name} was {was}, now {now}"));
        }
    };
    check(
        "perf mode",
        before.perf_mode.to_string(),
        after.perf_mode.to_string(),
    );
    check(
        "fan mode",
        before.fan_mode.to_string(),
        after.fan_mode.to_string(),
    );
    check(
        "set points",
        format!("{:?}", before.setpoint_rpm),
        format!("{:?}", after.setpoint_rpm),
    );
    if before.cpu_boost.is_some() {
        check(
            "CPU boost",
            format!("{:?}", before.cpu_boost),
            format!("{:?}", after.cpu_boost),
        );
    }
    if before.gpu_boost.is_some() {
        check(
            "GPU boost",
            format!("{:?}", before.gpu_boost),
            format!("{:?}", after.gpu_boost),
        );
    }
    check(
        "Max Fan",
        format!("{:?}", before.max_fan),
        format!("{:?}", after.max_fan),
    );
    out
}

/// Put back what differs from `snap`, in dependency order, past any failed write, then
/// verify every snapshotted field. The error lists every write that failed and every
/// field still different.
pub fn restore(device: &impl HidTransport, snap: &Snapshot) -> anyhow::Result<()> {
    use crate::command;
    use crate::types::{CpuBoost, FanMode, GpuBoost, MaxFanSpeedMode, PerfMode};
    let perf = PerfMode::try_from(snap.perf_mode)?;
    let fan = FanMode::try_from(snap.fan_mode)?;
    let mut problems: Vec<String> = Vec::new();
    let mut note = |what: &str, r: anyhow::Result<()>| {
        if let Err(e) = r {
            problems.push(format!("{what}: {e:#}"));
        }
    };
    let now = || snapshot(device).ok();

    // Boosts live in Custom, so they go back first, from Custom.
    let boosts_moved = match (snap.cpu_boost, snap.gpu_boost) {
        (Some(c), Some(g)) => {
            now().is_none_or(|n| n.cpu_boost != Some(c) || n.gpu_boost != Some(g))
        }
        _ => false,
    };
    if boosts_moved {
        note(
            "enter Custom",
            command::set_perf_mode(device, PerfMode::Custom),
        );
        if let (Some(c), Some(g)) = (snap.cpu_boost, snap.gpu_boost) {
            note(
                "CPU boost",
                CpuBoost::try_from(c).and_then(|b| command::set_cpu_boost(device, b)),
            );
            note(
                "GPU boost",
                GpuBoost::try_from(g).and_then(|b| command::set_gpu_boost(device, b)),
            );
        }
    }
    // A perf write resets the fan to Auto on both zones.
    if boosts_moved || now().is_none_or(|n| n.perf_mode != snap.perf_mode) {
        note("perf mode", command::set_perf_mode(device, perf));
    }
    // Set points are written in Manual, where what they do is measured, then the fan mode.
    let setpoints_moved = now().is_none_or(|n| n.setpoint_rpm != snap.setpoint_rpm);
    if setpoints_moved {
        note(
            "fan Manual for the set points",
            command::set_fan_mode(device, FanMode::Manual),
        );
        for (i, rpm) in snap.setpoint_rpm.iter().enumerate() {
            // Raw: the value to put back may be one the CLI wouldn't accept (e.g. 0 after
            // boot). The EC held it before.
            let args = [0, i as u8 + 1, (rpm / 100) as u8];
            note(
                &format!("zone {} set point", i + 1),
                command::send_command(device, 0x0d01, &args).map(|_| ()),
            );
        }
    }
    if setpoints_moved || now().is_none_or(|n| n.fan_mode != snap.fan_mode) {
        note("fan mode", command::set_fan_mode(device, fan));
    }
    // Max Fan exists only in Custom; leaving Custom already cleared it.
    if let Some(max) = snap.max_fan {
        if now().is_none_or(|n| n.max_fan != Some(max)) {
            note(
                "Max Fan",
                MaxFanSpeedMode::try_from(max)
                    .and_then(|m| command::set_max_fan_speed_mode(device, m)),
            );
        }
    }

    match snapshot(device) {
        Ok(after) => problems.extend(differences(snap, &after)),
        Err(e) => problems.push(format!("reading the result: {e:#}")),
    }
    if problems.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("not restored: {}", problems.join("; "))
    }
}

/// Set points for a sweep over a measured range: its ends and middle, high first.
pub fn sweep_points(range: (u16, u16)) -> Vec<u16> {
    let (lo, hi) = range;
    let mid = (lo / 2 + hi / 2) / 100 * 100;
    let mut points = vec![hi, mid, lo];
    points.dedup();
    points
}

/// How often the fans are read while they settle.
pub const SETTLE_POLL: Duration = Duration::from_secs(5);

/// The write set. `sweep` is the manual set points to try (the CLI picks them from a
/// measured range only; see [`sweep_points`]). Restores and verifies before returning,
/// whatever happened, including a cancel; `restored` in the capture says how that went.
#[allow(clippy::too_many_arguments)]
pub fn run_writes<T: HidTransport + Recording>(
    device: &T,
    machine: Machine,
    tool: &str,
    snap: &Snapshot,
    sweep: &[u16],
    settle: Duration,
    cancel: &AtomicBool,
) -> Capture {
    use crate::command;
    use crate::types::{CpuBoost, FanMode, FanZone, GpuBoost, MaxFanSpeedMode, PerfMode};

    device.drain_exchanges();
    let mut steps: Vec<Step> = Vec::new();
    // Runs one step unless it was cancelled or `ready` says why it can't; true if it took.
    let mut step = |label: String,
                    command: u16,
                    args: &[u8],
                    ready: Result<(), String>,
                    run: &mut dyn FnMut() -> anyhow::Result<String>|
     -> bool {
        let ready = if cancel.load(Ordering::SeqCst) {
            Err("cancelled".to_string())
        } else {
            ready
        };
        let (result, check, took) = match ready {
            Err(reason) => (StepResult::Skipped { reason }, None, false),
            Ok(()) => match run() {
                Ok(check) => (StepResult::Ok { args: Vec::new() }, Some(check), true),
                Err(e) => (
                    StepResult::Error {
                        error: format!("{e:#}"),
                    },
                    None,
                    false,
                ),
            },
        };
        steps.push(Step {
            label,
            command,
            args: args.to_vec(),
            result,
            check,
            exchanges: device.drain_exchanges(),
        });
        took
    };
    // A perf/fan write took only if both zones read back what was asked.
    let confirm = |d: &T, perf: PerfMode, fan: FanMode| -> anyhow::Result<String> {
        let (p, f) = command::get_perf_mode(d)?;
        anyhow::ensure!(
            (p, f) == (perf, fan),
            "wrote {perf:?}/{fan:?}, read back {p:?}/{f:?}"
        );
        Ok(format!("read back {p:?}/{f:?} on both zones"))
    };

    let mut perf_now: Option<PerfMode> = None;
    for mode in [
        PerfMode::Balanced,
        PerfMode::Silent,
        PerfMode::Performance,
        PerfMode::Custom,
    ] {
        let took = step(
            format!("set perf {mode:?}"),
            0x0d02,
            &[1, 1, mode as u8, 0],
            Ok(()),
            &mut || {
                command::set_perf_mode(device, mode)?;
                confirm(device, mode, FanMode::Auto)
            },
        );
        perf_now = if took { Some(mode) } else { None };
    }
    let custom = perf_now == Some(PerfMode::Custom);
    let in_custom = || {
        if custom {
            Ok(())
        } else {
            Err("Custom not confirmed".to_string())
        }
    };
    let boosts_restorable = match (snap.cpu_boost, snap.gpu_boost) {
        (Some(_), Some(_)) => in_custom(),
        _ => Err("the boosts couldn't be read before, so they couldn't be put back".into()),
    };
    step(
        "set CPU boost Medium, GPU boost High".into(),
        0x0d07,
        &[1, 1, CpuBoost::Medium as u8],
        boosts_restorable,
        &mut || {
            command::set_cpu_boost(device, CpuBoost::Medium)?;
            command::set_gpu_boost(device, GpuBoost::High)?;
            let got = (
                command::get_cpu_boost(device)?,
                command::get_gpu_boost(device)?,
            );
            anyhow::ensure!(
                got == (CpuBoost::Medium, GpuBoost::High),
                "wrote Medium/High, read back {:?}/{:?}",
                got.0,
                got.1
            );
            Ok(format!("read back {:?}/{:?}", got.0, got.1))
        },
    );
    for max in [
        MaxFanSpeedMode::Disable,
        MaxFanSpeedMode::Enable,
        MaxFanSpeedMode::Disable,
    ] {
        step(
            format!("set Max Fan {max:?}"),
            0x070f,
            &[max as u8],
            in_custom(),
            &mut || {
                command::set_max_fan_speed_mode(device, max)?;
                let got = command::get_max_fan_speed_mode(device)?;
                anyhow::ensure!(got == max, "wrote {max:?}, read back {got:?}");
                Ok(format!("read back {got:?}"))
            },
        );
    }

    let balanced = step(
        "set perf Balanced".into(),
        0x0d02,
        &[1, 1, PerfMode::Balanced as u8, 0],
        Ok(()),
        &mut || {
            command::set_perf_mode(device, PerfMode::Balanced)?;
            confirm(device, PerfMode::Balanced, FanMode::Auto)
        },
    );
    let manual = step(
        "set fan Manual".into(),
        0x0d02,
        &[1, 1, PerfMode::Balanced as u8, 1],
        if balanced {
            Ok(())
        } else {
            Err("Balanced not confirmed".into())
        },
        &mut || {
            command::set_fan_mode(device, FanMode::Manual)?;
            confirm(device, PerfMode::Balanced, FanMode::Manual)
        },
    );
    for &rpm in sweep {
        step(
            format!("set fan {rpm} RPM"),
            0x0d01,
            &[0, 1, (rpm / 100) as u8],
            if manual {
                Ok(())
            } else {
                Err("Balanced/Manual not confirmed".into())
            },
            &mut || {
                command::set_fan_rpm(device, rpm, false)?;
                let set = [
                    command::get_fan_rpm(device, FanZone::Zone1)?,
                    command::get_fan_rpm(device, FanZone::Zone2)?,
                ];
                anyhow::ensure!(set == [rpm, rpm], "wrote {rpm}, read back {set:?}");
                // Watch the fans until two polls agree, `settle` runs out, or a cancel.
                let mut curve: Vec<[u16; 2]> = Vec::new();
                let mut waited = Duration::ZERO;
                loop {
                    let a = [
                        command::get_fan_actual_rpm(device, FanZone::Zone1)?,
                        command::get_fan_actual_rpm(device, FanZone::Zone2)?,
                    ];
                    let settled = curve.last() == Some(&a);
                    curve.push(a);
                    if settled || waited >= settle || cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    device.pause(SETTLE_POLL);
                    waited += SETTLE_POLL;
                }
                let last = curve.last().copied().unwrap_or_default();
                let held = last.iter().all(|&a| a.abs_diff(rpm) <= 100);
                let curve: Vec<String> =
                    curve.iter().map(|a| format!("{}/{}", a[0], a[1])).collect();
                Ok(format!(
                    "set point read back {rpm} on both zones; actual every {}s: {}; {}",
                    SETTLE_POLL.as_secs(),
                    curve.join(" "),
                    if held {
                        "held the set point".to_string()
                    } else {
                        format!("ended at {}/{}, not at the set point", last[0], last[1])
                    }
                ))
            },
        );
    }
    step(
        "set fan Auto".into(),
        0x0d02,
        &[1, 1, PerfMode::Balanced as u8, 0],
        if balanced {
            Ok(())
        } else {
            Err("Balanced not confirmed".into())
        },
        &mut || {
            command::set_fan_mode(device, FanMode::Auto)?;
            confirm(device, PerfMode::Balanced, FanMode::Auto)
        },
    );

    // The restore always runs (a cancel included) and is recorded like any step.
    let result = restore(device, snap);
    let mut restored = match &result {
        Ok(()) => "verified".to_string(),
        Err(e) => format!("FAILED: {e:#}"),
    };
    steps.push(Step {
        label: "restore".into(),
        command: 0x0d02,
        args: Vec::new(),
        result: match result {
            Ok(()) => StepResult::Ok { args: Vec::new() },
            Err(e) => StepResult::Error {
                error: format!("{e:#}"),
            },
        },
        check: None,
        exchanges: device.drain_exchanges(),
    });
    if cancel.load(Ordering::SeqCst) {
        restored = format!("{restored} (cancelled before the end)");
    }
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
            StepResult::Skipped { reason } => format!("skipped: {reason}"),
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

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn writes(sim: &SimTransport, m: &Model, range: (u16, u16), cancel: &AtomicBool) -> Capture {
        let snap = snapshot(sim).unwrap();
        run_writes(
            sim,
            machine(m),
            "test",
            &snap,
            &sweep_points(range),
            Duration::from_secs(120),
            cancel,
        )
    }

    fn label_result<'a>(cap: &'a Capture, label: &str) -> &'a StepResult {
        &cap.steps.iter().find(|s| s.label == label).unwrap().result
    }

    #[test]
    fn a_capture_becomes_a_model_with_the_same_starting_state() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        let cap = run(&sim, machine(&m), "test");
        let json = serde_json::to_string(&cap).unwrap();
        let back: Capture = serde_json::from_str(&json).unwrap();
        let rebuilt = Model::from_capture(&back, "cap").unwrap();
        assert_eq!(rebuilt.state, m.state);
        assert!(rebuilt
            .rules
            .iter()
            .all(|r| r.support == crate::sim::Support::Works && r.args.is_some()));
    }

    #[test]
    fn a_writes_capture_does_not_become_a_model() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        let cap = writes(&sim, &m, (2000, 5000), &no_cancel());
        assert!(Model::from_capture(&cap, "cap").is_err());
    }

    #[test]
    fn the_write_set_puts_every_register_back() {
        let mut m = blade16();
        // Auto, with set points left over from an earlier manual run: the sweep changes
        // them, and they must come back although Auto doesn't use them.
        m.state.setpoint_rpm = [2600, 2600];
        let sim = SimTransport::new(m.clone());
        let before = snapshot(&sim).unwrap();
        let cap = writes(&sim, &m, (2000, 5000), &no_cancel());
        assert_eq!(cap.restored.as_deref(), Some("verified"));
        let failed: Vec<_> = cap
            .steps
            .iter()
            .filter(|s| !matches!(s.result, StepResult::Ok { .. }))
            .map(|s| &s.label)
            .collect();
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(snapshot(&sim).unwrap(), before);
        assert_eq!(sim.state().setpoint_rpm, [2600, 2600]);
        let sweep = cap
            .steps
            .iter()
            .find(|s| s.label == "set fan 5000 RPM")
            .unwrap();
        let check = sweep.check.as_deref().unwrap();
        assert!(check.contains("held the set point"), "{check}");
    }

    #[test]
    fn an_ignored_write_fails_its_step_and_skips_what_needs_it() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        sim.inject(Fault::IgnoreWrites {
            from_ms: 0,
            until_ms: u64::MAX,
        });
        let cap = writes(&sim, &m, (2000, 5000), &no_cancel());
        match label_result(&cap, "set perf Silent") {
            StepResult::Error { error } => assert!(error.contains("read back Balanced"), "{error}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            label_result(&cap, "set Max Fan Enable"),
            StepResult::Skipped { .. }
        ));
        assert!(matches!(
            label_result(&cap, "set fan 3500 RPM"),
            StepResult::Skipped { .. }
        ));
        assert_eq!(cap.restored.as_deref(), Some("verified"), "nothing moved");
    }

    #[test]
    fn a_cancel_skips_to_the_restore() {
        let m = blade16();
        let sim = SimTransport::new(m.clone());
        let cancel = AtomicBool::new(true);
        let cap = writes(&sim, &m, (2000, 5000), &cancel);
        assert!(cap
            .steps
            .iter()
            .filter(|s| s.label != "restore")
            .all(|s| matches!(s.result, StepResult::Skipped { .. })));
        assert!(cap.restored.as_deref().unwrap().starts_with("verified"));
    }

    #[test]
    fn a_failed_restore_write_does_not_stop_the_rest() {
        let mut before = blade16();
        before.state.perf_mode = [4, 4];
        before.state.fan_mode = [1, 1];
        before.state.setpoint_rpm = [3000, 3000];
        let snap = snapshot(&SimTransport::new(before.clone())).unwrap();
        // The same EC after a run that moved everything, with boost writes now failing.
        let mut moved = before.clone();
        moved.state.perf_mode = [0, 0];
        moved.state.setpoint_rpm = [4000, 4000];
        moved.state.cpu_boost = 3;
        moved.rules.insert(
            0,
            crate::sim::Rule {
                command: 0x0d07,
                args: None,
                support: crate::sim::Support::Failure,
                evidence: "inferred: test".into(),
                reply: None,
            },
        );
        let sim = SimTransport::new(moved);
        let err = restore(&sim, &snap).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("CPU boost"), "{err}");
        let end = sim.state();
        assert_eq!(end.perf_mode, [4, 4], "the perf mode was still put back");
        assert_eq!(end.fan_mode, [1, 1], "and the fan");
        assert_eq!(end.setpoint_rpm, [3000, 3000]);
    }

    #[test]
    fn no_write_set_without_a_readable_set_point() {
        // The 0x028b model has no observation for the set-point read, so nothing is
        // written: the sweep would change a register the restore couldn't put back.
        let m = Model::from_json(include_str!("../tests/ec/0x028b-blade17-2022.json")).unwrap();
        assert!(snapshot(&SimTransport::new(m)).is_err());
    }

    #[test]
    fn a_rejected_max_fan_is_recorded_and_left_alone() {
        let mut m = Model::from_json(include_str!("../tests/ec/0x028b-blade17-2022.json")).unwrap();
        m.rules
            .retain(|r| r.command != 0x0d81 && r.command != 0x070f);
        for (command, support) in [
            (0x0d81, crate::sim::Support::Works),
            (0x070f, crate::sim::Support::NotSupported),
        ] {
            m.rules.push(crate::sim::Rule {
                command,
                args: None,
                support,
                evidence: "inferred: test".into(),
                reply: None,
            });
        }
        let sim = SimTransport::new(m.clone());
        let cap = writes(&sim, &m, (3500, 5000), &no_cancel());
        for label in ["set Max Fan Disable", "set Max Fan Enable"] {
            match label_result(&cap, label) {
                StepResult::Error { error } => assert!(error.contains("0x05"), "{label}: {error}"),
                other => panic!("{label}: {other:?}"),
            }
        }
        assert_eq!(sim.state().max_fan, 0);
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
