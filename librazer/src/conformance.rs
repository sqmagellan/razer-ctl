//! Every EC model under `tests/ec/` run through the same scenarios.
//!
//! The results are compared with `tests/ec/expected.txt`, so a change to the code or to
//! a model that changes what a laptop would see shows up as a diff in review. Regenerate
//! the file with `UPDATE_EXPECT=1 cargo test -p librazer conformance` and read the diff.
//!
//! A gap is a command a model has no observation for. Each fixture's gaps, with the
//! rule's note, are the questions still open for that laptop.

use crate::capture::{Machine, Outcome, Recording, StepResult, Verdict};
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

/// A catalogued laptop gets its own profile and only its `KNOWN_LACKING` entries; any other
/// uses the generic profile, which learns what the EC lacks (as `Device::detect` does).
fn transport(m: &Model) -> SimTransport {
    let catalogued = sim::catalogued(m).is_some();
    let sim = SimTransport::new(m.clone());
    if catalogued {
        sim.with_known_lacking()
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

/// Apply `intent`, then check the EC ended where it was asked: `apply` returning Ok only
/// means every write was answered.
fn applied(d: &SimTransport, intent: DeviceState) -> anyhow::Result<()> {
    intent.apply(d)?;
    let now = DeviceState::read(d)?;
    anyhow::ensure!(
        (now.perf_mode, now.fan_speed, now.max_fan)
            == (intent.perf_mode, intent.fan_speed, intent.max_fan),
        "applied, but the EC reads {:?}/{:?}/max fan {}",
        now.perf_mode,
        now.fan_speed,
        now.max_fan
    );
    Ok(())
}

type Scenario = fn(&SimTransport) -> anyhow::Result<()>;

const SCENARIOS: &[(&str, Scenario)] = &[
    ("read", |d| DeviceState::read(d).map(|_| ())),
    ("apply balanced", |d| {
        applied(d, profile(PerfMode::Balanced, FanSpeed::Auto, false))
    }),
    ("apply silent", |d| {
        applied(d, profile(PerfMode::Silent, FanSpeed::Auto, false))
    }),
    ("apply custom", |d| {
        applied(d, profile(CUSTOM, FanSpeed::Auto, false))
    }),
    ("apply custom + max fan", |d| {
        applied(d, profile(CUSTOM, FanSpeed::Auto, true))
    }),
    ("apply manual 3000", |d| {
        applied(
            d,
            profile(PerfMode::Balanced, FanSpeed::Manual(3000), false),
        )
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
        let (rpm, trusted) = crate::state::sample_fan_rpm(d, 3)?;
        anyhow::ensure!(trusted, "no two agreeing reads (last {rpm:?})");
        Ok(())
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
        let catalogued = sim::catalogued(&m).is_some();
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
            // Every 0x05 answer, not only the first error: one scenario can hit several.
            let mut rejected: Vec<String> = sim
                .drain_exchanges()
                .iter()
                .filter(|e| {
                    matches!(
                        e.outcome,
                        Outcome::Response {
                            verdict: Verdict::NotSupported,
                            ..
                        }
                    )
                })
                .map(|e| format!("{:#06x} {:?}", e.command, e.args))
                .collect();
            rejected.dedup();
            let rejected = if rejected.is_empty() {
                String::new()
            } else {
                format!("  [0x05: {}]", rejected.join(", "))
            };
            out.push_str(&format!(
                "{scenario}: {}{rejected}{gaps}\n",
                outcome(&result)
            ));
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
        for g in sim.gaps() {
            let q = format!("{:#06x} {:?}: {}", g.command, g.args, g.why);
            if !questions.contains(&q) {
                questions.push(q);
            }
        }
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
/// can't tell, as every write is acknowledged; `confirm_modes`, which the tray runs after
/// it, must not report success while the EC kept its old mode.
#[test]
fn an_acknowledged_but_ignored_perf_write_is_not_success() {
    let sim = blade16();
    sim.inject(Fault::IgnoreWrites {
        from_ms: 0,
        until_ms: u64::MAX,
    });
    let state = profile(PerfMode::Silent, FanSpeed::Auto, false);
    state.apply(&sim).unwrap();
    let err = state.confirm_modes(&sim).unwrap_err();
    assert!(
        crate::error::is_perf_not_applied(&err),
        "the EC is still Balanced: {err:?}"
    );
}

/// 2026-10-20 curve runs: in Custom/Manual the fans run at the larger of the set point
/// and 3800, whatever the boosts; a 3000 set point reads back as 3000 meanwhile.
#[test]
fn custom_manual_runs_at_least_3800() {
    use crate::types::FanZone;
    let sim = blade16();
    profile(CUSTOM, FanSpeed::Manual(3000), false)
        .apply(&sim)
        .unwrap();
    sim.advance(std::time::Duration::from_secs(120));
    assert_eq!(
        crate::command::get_fan_rpm(&sim, FanZone::Zone1).unwrap(),
        3000
    );
    assert_eq!(
        crate::command::get_fan_actual_rpm(&sim, FanZone::Zone1).unwrap(),
        3800
    );

    profile(CUSTOM, FanSpeed::Manual(4500), false)
        .apply(&sim)
        .unwrap();
    sim.advance(std::time::Duration::from_secs(120));
    assert_eq!(
        crate::command::get_fan_actual_rpm(&sim, FanZone::Zone1).unwrap(),
        4500
    );
}

/// The tray and CLI take a perf mode's fan floor from `unsupported::MANUAL_FLOOR_BY_PERF`,
/// the simulator from each EC model. Both record the same hardware runs, so they agree on
/// every fixture, and every row in the table has a fixture behind it.
#[test]
fn the_manual_floor_table_matches_the_ec_models() {
    use crate::types::PerfMode as Wire;
    use crate::unsupported::{manual_fan_floor, MANUAL_FLOOR_BY_PERF};
    use strum::IntoEnumIterator;

    let models: Vec<Model> = FIXTURES.iter().map(|(_, json)| model(json)).collect();
    for m in &models {
        for perf in Wire::iter() {
            assert_eq!(
                manual_fan_floor(m.pid, perf),
                m.fan.manual_floor_by_mode.get(&(perf as u8)).copied(),
                "{} in {perf:?}",
                m.name
            );
        }
    }
    for (pid, perf, rpm) in MANUAL_FLOOR_BY_PERF {
        assert!(
            models
                .iter()
                .any(|m| m.pid == *pid
                    && m.fan.manual_floor_by_mode.get(&(*perf as u8)) == Some(rpm)),
            "{pid:#06x} {perf:?} {rpm}: no EC model records this floor"
        );
    }
}

/// What the tray and CLI say the fans run at is what the simulated Blade 16 (2023) fans do:
/// the floor below it in Custom, the set point above it, and the set point in Balanced.
#[test]
fn the_predicted_manual_speed_is_what_the_fans_settle_at() {
    use crate::types::FanZone;
    use crate::unsupported::manual_fan_runs_at;
    let sim = blade16();
    let pid = model(FIXTURES[0].1).pid;
    for (perf, rpm) in [
        (CUSTOM, 3000),
        (CUSTOM, 2000),
        (CUSTOM, 3800),
        (CUSTOM, 4500),
        (PerfMode::Balanced, 3000),
        (PerfMode::Balanced, 2000),
    ] {
        profile(perf, FanSpeed::Manual(rpm), false)
            .apply(&sim)
            .unwrap();
        sim.advance(std::time::Duration::from_secs(180));
        let predicted = manual_fan_runs_at(pid, perf.wire(), rpm).unwrap_or(rpm);
        for zone in [FanZone::Zone1, FanZone::Zone2] {
            assert_eq!(
                crate::command::get_fan_rpm(&sim, zone).unwrap(),
                rpm,
                "{perf:?} {rpm}: the set point reads back as written"
            );
            assert_eq!(
                crate::command::get_fan_actual_rpm(&sim, zone).unwrap(),
                predicted,
                "{perf:?} {rpm}"
            );
        }
    }
}
