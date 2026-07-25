//! Typed failures that callers need to branch on.
//!
//! Everything in this crate returns `anyhow::Result`, which is right for messages but
//! useless for decisions: a script wants to know *why* it failed, not read prose. These
//! types are attached with `anyhow::Error::new` (never `anyhow!("{}", e)`, which flattens
//! them to a string) so a caller can recover the reason with `downcast_ref`.
//!
//! Lives outside the `device` module on purpose. `device` is gated to Windows and Linux
//! because it needs `hidapi`, but the CLI's exit-code mapping is pure logic that should
//! compile and be testable on any host.

/// Why device detection failed.
///
/// The distinction that matters to a script: "no Razer hardware here at all" is a
/// different situation from "this is a Razer laptop we couldn't open", and both differ
/// from "a command isn't implemented on this firmware" ([`crate::packet::ResponseError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectError {
    /// No device with Razer's USB vendor id is present.
    NoRazerDevices,
    /// A Razer device answered, but the machine's model number isn't an `RZ09-` laptop
    /// SKU -- e.g. this is a desktop with a Razer mouse attached.
    NotARazerLaptop(String),
    /// The model number couldn't be read from the platform at all.
    ModelUnreadable(String),
    /// A specific USB product id could not be opened as a control interface -- either it
    /// isn't present, or it is present but rejected the probe.
    InterfaceUnavailable { pid: u16, name: String },
    /// An `RZ09-` laptop, but none of its USB product ids exposed a usable control
    /// interface even with the generic fallback profile.
    NoControlInterface {
        model: String,
        pids: Vec<u16>,
        detail: String,
    },
}

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::NoRazerDevices => write!(f, "No Razer devices found"),
            DetectError::NotARazerLaptop(model) => {
                write!(f, "Detected model but it's not a Razer laptop: {model}")
            }
            DetectError::ModelUnreadable(detail) => {
                write!(f, "Failed to detect model: {detail}")
            }
            DetectError::InterfaceUnavailable { pid, name } => write!(
                f,
                "Could not open a Razer control interface for {name} (PID 0x{pid:04x}). \
                 Is the device present, and is another tool holding it?"
            ),
            DetectError::NoControlInterface {
                model,
                pids,
                detail,
            } => write!(
                f,
                "Model {model} with PIDs {pids:0>4x?} is not supported, and no generic \
                 fallback could open a control interface{detail}"
            ),
        }
    }
}

impl std::error::Error for DetectError {}

/// Process exit statuses, so a script can branch without parsing stderr.
///
/// Stable contract: these numbers are documented in the README and in `--help`, and must
/// not be renumbered. Only 0 means the requested change actually happened.
///
/// **2 is deliberately skipped.** `clap` exits with 2 on a usage error (unknown
/// subcommand, bad argument) and does so from inside `get_matches()`, before any of our
/// code runs -- so 2 can never be reliably ours. An earlier draft assigned 2 to
/// `NoDevice`, which made a typo and absent hardware indistinguishable to a script;
/// verified on hardware, both returned 2. Aligning with clap's convention is better than
/// fighting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Success = 0,
    /// Anything we couldn't classify.
    Generic = 1,
    // 2 = usage error, emitted by clap itself. Not ours to assign.
    /// No usable Razer laptop: absent, not a laptop, or no control interface.
    /// A retry won't help until the hardware situation changes.
    NoDevice = 3,
    /// The device answered "I don't implement that" (status 0x05). Definitive: this
    /// command will never work on this model, so a script should stop asking.
    Unsupported = 4,
    /// The exchange failed for a transient or protocol reason -- busy firmware, a
    /// rejected command, a desynchronised bus. Worth retrying.
    DeviceError = 5,
}

impl ExitCode {
    /// Classify a failure into an exit status.
    ///
    /// Walks the whole `anyhow` cause chain rather than inspecting only the outermost
    /// error, because the interesting type is usually wrapped in context by the time it
    /// reaches `main` (`"failed to match report after 3 attempts"` over a `ResponseError`).
    pub fn classify(err: &anyhow::Error) -> Self {
        for cause in err.chain() {
            if let Some(response) = cause.downcast_ref::<crate::packet::ResponseError>() {
                return match response {
                    crate::packet::ResponseError::NotSupported => ExitCode::Unsupported,
                    _ => ExitCode::DeviceError,
                };
            }
            if cause.downcast_ref::<DetectError>().is_some() {
                return ExitCode::NoDevice;
            }
        }
        ExitCode::Generic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::ResponseError;

    #[test]
    fn unsupported_is_distinguished_from_other_device_errors() {
        assert_eq!(
            ExitCode::classify(&anyhow::Error::new(ResponseError::NotSupported)),
            ExitCode::Unsupported
        );
        for e in [
            ResponseError::Busy,
            ResponseError::Failure,
            ResponseError::Mismatch,
        ] {
            assert_eq!(
                ExitCode::classify(&anyhow::Error::new(e)),
                ExitCode::DeviceError,
                "{e:?} should be retryable, not unsupported"
            );
        }
    }

    #[test]
    fn detection_failures_all_map_to_no_device() {
        for e in [
            DetectError::NoRazerDevices,
            DetectError::NotARazerLaptop("Foo".into()),
            DetectError::ModelUnreadable("nope".into()),
            DetectError::NoControlInterface {
                model: "RZ09-0000".into(),
                pids: vec![0x1234],
                detail: String::new(),
            },
            DetectError::InterfaceUnavailable {
                pid: 0x1234,
                name: "Unknown".into(),
            },
        ] {
            assert_eq!(
                ExitCode::classify(&anyhow::Error::new(e.clone())),
                ExitCode::NoDevice,
                "{e:?}"
            );
        }
    }

    /// The regression that made all of this necessary: a typed error wrapped in context
    /// must still be classifiable. Inspecting only the outermost error returns Generic.
    #[test]
    fn a_typed_error_survives_being_wrapped_in_context() {
        let wrapped = anyhow::Error::new(ResponseError::NotSupported)
            .context("failed to match report after 3 attempts")
            .context("while setting the fan speed");
        assert_eq!(ExitCode::classify(&wrapped), ExitCode::Unsupported);
    }

    #[test]
    fn an_unclassifiable_error_is_generic() {
        assert_eq!(
            ExitCode::classify(&anyhow::anyhow!("something else went wrong")),
            ExitCode::Generic
        );
    }
}
