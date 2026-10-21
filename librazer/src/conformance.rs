//! Every EC model under `tests/ec/` run through the same scenarios.
//!
//! The results are compared with `tests/ec/expected.txt`, so a change to the code or to
//! a model that changes what a laptop would see shows up as a diff in review. Regenerate
//! the file with `UPDATE_EXPECT=1 cargo test -p librazer conformance` and read the diff.
//!
//! A gap is a command a model has no observation for. Each fixture's gaps, with the
//! rule's note, are the questions still open for that laptop.

use crate::capture::{Machine, StepResult};
use crate::descriptor::SUPPORTED;
use crate::sim::{self, Fault, Model, SimTransport};
use crate::state::{DeviceState, FanSpeed, PerfMode};
use crate::types::{CpuBoost, GpuBoost};

const FIXTURES: &[(&str, &str)] = &[
    (
        "0x029f-blade16-2023",
        include_str!("../tests/ec/0x029f-blade16-2023.json"),
    ),
    (
        "0x02a0-blade18-2023",
        include_str!("../tests/ec/0x02a0-blade18-2023.json"),
    ),
    (
        "0x023b-blade15-2018-base",
        include_str!("../tests/ec/0x023b-blade15-2018-base.json"),
    ),
    (
        "0x028b-blade17-2022",
        include_str!("../tests/ec/0x028b-blade17-2022.json"),
    ),
];

fn model(json: &str) -> Model {
    Model::from_json(json).expect("fixture parses")
}

/// A catalogued PID gets its own profile and never learns; any other uses the generic
/// profile, which learns what the EC lacks (as `Device::detect` does).
fn transport(m: &Model) -> SimTransport {
    let catalogued = SUPPORTED.iter().any(|d| d.pid == m.pid);
    let sim = SimTransport::new(m.clone());
    if catalogued {
        sim
    } else {
        sim.learning()
    }
}

fn profile(perf_mode: PerfMode, fan_speed: FanSpeed, max_fan: bool) -> DeviceState {
    DeviceState {
        perf_mode,
        fan_speed,
        max_fan,
        ..DeviceState::default()
    }
}

const CUSTOM: PerfMode = PerfMode::Custom(CpuBoost::Medium, GpuBoost::High);

type Scenario = fn(&SimTransport) -> anyhow::Result<()>;

const SCENARIOS: &[(&str, Scenario)] = &[
    ("read", |d| DeviceState::read(d).map(|_| ())),
    ("apply balanced", |d| {
        profile(PerfMode::Balanced, FanSpeed::Auto, false).apply(d)
    }),
    ("apply silent", |d| {
        profile(PerfMode::Silent, FanSpeed::Auto, false).apply(d)
    }),
    ("apply custom", |d| {
        profile(CUSTOM, FanSpeed::Auto, false).apply(d)
    }),
    ("apply custom + max fan", |d| {
        profile(CUSTOM, FanSpeed::Auto, true).apply(d)
    }),
    ("apply manual 3000", |d| {
        profile(PerfMode::Balanced, FanSpeed::Manual(3000), false).apply(d)
    }),
    ("enforce after drift", |d| {
        crate::command::set_perf_mode(d, crate::types::PerfMode::Silent)?;
        let intended = profile(PerfMode::Balanced, FanSpeed::Auto, false);
        intended.enforce_to(d)?;
        let (now, _) = crate::command::get_perf_mode(d)?;
        anyhow::ensure!(
            now == crate::types::PerfMode::Balanced,
            "still {now:?} after enforcing"
        );
        Ok(())
    }),
    ("fan reads", |d| {
        crate::state::sample_fan_rpm(d, 3).map(|_| ())
    }),
];

fn outcome(result: &anyhow::Result<()>) -> String {
    match result {
        Ok(()) => "ok".into(),
        Err(e) if sim::unmodeled(e).is_some() => "gap".into(),
        Err(e) if crate::error::is_not_supported(e) => "not supported (0x05)".into(),
        Err(e) => format!("error: {e:#}"),
    }
}

fn report() -> String {
    let mut out = String::new();
    for (name, json) in FIXTURES {
        let m = model(json);
        let catalogued = SUPPORTED.iter().any(|d| d.pid == m.pid);
        out.push_str(&format!(
            "## {name} ({}, {})\n",
            m.name,
            if catalogued {
                "catalogued"
            } else {
                "generic, learns"
            }
        ));
        let mut questions: Vec<String> = Vec::new();
        for (scenario, run) in SCENARIOS {
            let sim = transport(&m);
            // The tray reads at startup before it writes anything, so a learning model
            // knows what it lacks by the time it applies; the read's own gaps still count.
            if *scenario != "read" {
                let _ = DeviceState::read(&sim);
            }
            let result = run(&sim);
            let mut gaps: Vec<String> = sim
                .gaps()
                .iter()
                .map(|g| format!("{:#06x} {:?}", g.command, g.args))
                .collect();
            gaps.dedup();
            let gaps = if gaps.is_empty() {
                String::new()
            } else {
                format!("  [gaps: {}]", gaps.join(", "))
            };
            out.push_str(&format!("{scenario}: {}{gaps}\n", outcome(&result)));
            for g in sim.gaps() {
                let q = format!("{:#06x} {:?}: {}", g.command, g.args, g.why);
                if !questions.contains(&q) {
                    questions.push(q);
                }
            }
        }

        // The probe against the model: what a reporter's capture would show.
        let sim = transport(&m);
        let machine = Machine {
            pid: m.pid,
            model_number: "simulated".into(),
            profile: m.name.clone(),
            catalogued,
        };
        let cap = crate::probe::run(&sim, machine, "conformance");
        let failed: Vec<&str> = cap
            .steps
            .iter()
            .filter(|s| matches!(s.result, StepResult::Error { .. }))
            .map(|s| s.label.as_str())
            .collect();
        out.push_str(&format!(
            "probe: {} of {} reads answered{}\n",
            cap.steps.len() - failed.len(),
            cap.steps.len(),
            if failed.is_empty() {
                String::new()
            } else {
                format!(" (no answer: {})", failed.join(", "))
            }
        ));
        if !questions.is_empty() {
            out.push_str("open:\n");
            for q in questions {
                out.push_str(&format!("  {q}\n"));
            }
        }
        out.push('\n');
    }
    out
}

#[test]
fn every_model_matches_the_expected_results() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ec/expected.txt");
    let actual = report();
    if std::env::var_os("UPDATE_EXPECT").is_some() {
        std::fs::write(path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_default();
    if actual != expected {
        panic!(
            "conformance results changed; review, then regenerate with UPDATE_EXPECT=1\n\
             --- expected\n{expected}\n--- actual\n{actual}"
        );
    }
}

#[test]
fn every_rule_says_where_its_evidence_came_from() {
    for (name, json) in FIXTURES {
        let m = model(json);
        for r in &m.rules {
            let source = r
                .evidence
                .strip_prefix("measured ")
                .map(|rest| rest.split([':', ';', ' ']).next().unwrap_or(""));
            match source {
                Some(id) => assert!(
                    m.sources.contains_key(id),
                    "{name}: rule {:#06x} cites unknown source {id:?}",
                    r.command
                ),
                None => assert!(
                    r.evidence.starts_with("inferred") || r.support == sim::Support::Unknown,
                    "{name}: rule {:#06x} is neither measured, inferred nor unknown: {:?}",
                    r.command,
                    r.evidence
                ),
            }
        }
    }
}

// ---- this project's real bugs, replayed on the 0x029F model ----

fn blade16() -> SimTransport {
    transport(&model(FIXTURES[0].1))
}

/// 2026-08-15: about 3 in 10 sparse reads carried one impossible zone, which is the
/// "the app says zero" bug. Two agreeing reads are needed before a stop is believed.
#[test]
fn a_lone_zone_zero_read_is_not_shown_as_a_stopped_fan() {
    let sim = blade16();
    sim.inject(Fault::LoneZoneZero { zone: 2, reads: 1 });
    let (rpm, trusted) = crate::state::sample_fan_rpm(&sim, 3).unwrap();
    assert!(trusted);
    assert_ne!(rpm.fan2, 0, "{rpm:?}");
}

/// 2026-08-15: a set point below the 2000 floor reads back as written while the fans
/// hold 2000, so a UI must show the actual speed, never the set point.
#[test]
fn a_set_point_below_the_floor_reads_back_but_the_fans_hold_the_floor() {
    use crate::types::FanZone;
    let sim = blade16();
    crate::command::set_fan_mode(&sim, crate::types::FanMode::Manual).unwrap();
    for zone in [1, 2] {
        crate::command::send_command(&sim, 0x0d01, &[0, zone, 15]).unwrap();
    }
    sim.advance(std::time::Duration::from_secs(120));
    assert_eq!(
        crate::command::get_fan_rpm(&sim, FanZone::Zone1).unwrap(),
        1500
    );
    assert_eq!(
        crate::command::get_fan_actual_rpm(&sim, FanZone::Zone1).unwrap(),
        2000
    );
}

/// 2026-10-19: perf writes acknowledged (status 0x02, args echoed) and ignored. `apply`
/// must not report success while the EC kept its old mode.
#[test]
#[ignore = "fixed by fix/perf-write-confirm, which reads the mode back after a write"]
fn an_acknowledged_but_ignored_perf_write_is_not_success() {
    let sim = blade16();
    sim.inject(Fault::IgnoreWrites {
        from_ms: 0,
        until_ms: u64::MAX,
    });
    let result = profile(PerfMode::Silent, FanSpeed::Auto, false).apply(&sim);
    assert!(result.is_err(), "the EC is still Balanced");
}
