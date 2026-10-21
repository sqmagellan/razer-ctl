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
        exchanges: device.drain_exchanges(),
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
        let tries = s.exchanges.len();
        let result = match &s.result {
            StepResult::Ok { args } => format!("ok {args:?}"),
            StepResult::Error { error } => format!("error: {error}"),
        };
        let retried = if tries > 1 {
            format!(" ({tries} tries)")
        } else {
            String::new()
        };
        out.push_str(&format!("  {:<34} {result}{retried}\n", s.label));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
