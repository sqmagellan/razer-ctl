//! A record of what the EC did, one entry per attempt.
//!
//! `Device` records below its retry loop, so a capture shows each answer the EC gave
//! (status byte, args, timing) rather than what the loop concluded from them.
//! `razer-cli probe` writes a [`Capture`] as JSON for a device-support issue; the
//! simulator ([`crate::sim`]) builds EC models from captures.

use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::packet::{Packet, ResponseError};

/// The `format` field of a capture file.
pub const FORMAT: &str = "razer-ctl-capture/1";

/// One attempt at one command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// Milliseconds since recording started.
    pub t_ms: u64,
    #[serde(with = "hex_u16")]
    pub command: u16,
    pub args: Vec<u8>,
    /// 0 for the first try; the retry loop's later attempts count up.
    pub attempt: u8,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The EC answered, and the answer was classified as `verdict`.
    Response {
        status: u8,
        args: Vec<u8>,
        verdict: Verdict,
    },
    /// The OS failed to send the report or read the reply.
    TransportError { error: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Ok,
    Busy,
    Failure,
    NotSupported,
    Mismatch,
}

impl From<Result<(), ResponseError>> for Verdict {
    fn from(r: Result<(), ResponseError>) -> Self {
        match r {
            Ok(()) => Verdict::Ok,
            Err(ResponseError::Busy) => Verdict::Busy,
            Err(ResponseError::Failure) => Verdict::Failure,
            Err(ResponseError::NotSupported) => Verdict::NotSupported,
            Err(ResponseError::Mismatch) => Verdict::Mismatch,
        }
    }
}

/// The args that matter in a request or reply: `data_size` bytes, or the request's when
/// a reply's `data_size` is zero.
pub fn meaningful_args(packet: &Packet, request: &Packet) -> Vec<u8> {
    let len = match packet.data_len() {
        0 => request.data_len(),
        n => n,
    };
    packet.get_args()[..len.min(Packet::MAX_ARGS)].to_vec()
}

/// Collects exchanges as a transport makes them. `Sync`, like `Device`.
#[derive(Debug)]
pub struct Recorder {
    start: Instant,
    log: Mutex<Vec<Exchange>>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            start: Instant::now(),
            log: Mutex::new(Vec::new()),
        }
    }
}

impl Recorder {
    /// Record one attempt at `request`, timed by the wall clock.
    pub fn record(&self, request: &Packet, attempt: usize, outcome: Outcome) {
        let t_ms = self.start.elapsed().as_millis() as u64;
        self.record_at(t_ms, request, attempt, outcome);
    }

    /// Record one attempt at a given time (the simulator keeps its own clock).
    pub fn record_at(&self, t_ms: u64, request: &Packet, attempt: usize, outcome: Outcome) {
        let entry = Exchange {
            t_ms,
            command: request.command(),
            args: request.get_args()[..request.data_len()].to_vec(),
            attempt: attempt as u8,
            outcome,
        };
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    /// Record the EC's reply to `request`.
    pub fn record_response(
        &self,
        request: &Packet,
        response: &Packet,
        attempt: usize,
        t_ms: Option<u64>,
    ) {
        let outcome = Outcome::Response {
            status: response.status(),
            args: meaningful_args(response, request),
            verdict: response.classify_response(request).into(),
        };
        match t_ms {
            Some(t) => self.record_at(t, request, attempt, outcome),
            None => self.record(request, attempt, outcome),
        }
    }

    /// Everything recorded since the last drain.
    pub fn drain(&self) -> Vec<Exchange> {
        std::mem::take(
            &mut *self
                .log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// A transport that keeps a record of its exchanges.
pub trait Recording {
    fn drain_exchanges(&self) -> Vec<Exchange>;
}

/// What `razer-cli probe` writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub format: String,
    /// The tool and version that made it, e.g. `razer-cli 0.8.9`.
    pub tool: String,
    pub machine: Machine,
    /// `read-only`, or the write set's name.
    pub mode: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    #[serde(with = "hex_u16")]
    pub pid: u16,
    /// The BIOS model number (SKU), e.g. `RZ09-0483`. Not a serial number.
    pub model_number: String,
    /// The profile the tool picked: a catalogued model's name, or the generic one.
    pub profile: String,
    pub catalogued: bool,
}

/// One probe step: a command sent once through the normal retry loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub label: String,
    #[serde(with = "hex_u16")]
    pub command: u16,
    pub args: Vec<u8>,
    pub result: StepResult,
    /// Every attempt the step took, in order.
    pub exchanges: Vec<Exchange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepResult {
    Ok { args: Vec<u8> },
    Error { error: String },
}

/// `u16` command codes as `"0x0d82"` in JSON, which is how every issue and note writes them.
pub mod hex_u16 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u16, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:#06x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Str(String),
            Num(u16),
        }
        match Either::deserialize(d)? {
            Either::Num(n) => Ok(n),
            Either::Str(s) => {
                let hex = s.trim_start_matches("0x").trim_start_matches("0X");
                u16::from_str_radix(hex, 16).map_err(serde::de::Error::custom)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_reply_keeps_its_status_and_verdict() {
        let rec = Recorder::default();
        let req = Packet::new(0x070f, &[2]);
        let reply = req.response(&[2], 0x05);
        rec.record_response(&req, &reply, 0, Some(7));
        let log = rec.drain();
        assert_eq!(
            log,
            vec![Exchange {
                t_ms: 7,
                command: 0x070f,
                args: vec![2],
                attempt: 0,
                outcome: Outcome::Response {
                    status: 0x05,
                    args: vec![2],
                    verdict: Verdict::NotSupported
                },
            }]
        );
        assert!(rec.drain().is_empty(), "drained");
    }

    #[test]
    fn commands_are_hex_strings_in_json() {
        let e = Exchange {
            t_ms: 0,
            command: 0x0d82,
            args: vec![0, 1, 0, 0],
            attempt: 0,
            outcome: Outcome::TransportError { error: "x".into() },
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"command\":\"0x0d82\""), "{json}");
        assert_eq!(serde_json::from_str::<Exchange>(&json).unwrap(), e);
    }
}
