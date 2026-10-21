//! The retry policy for one EC exchange. `Device` and the simulator both use it, so the
//! simulator answers a status byte the way the real retry loop would.

use std::time::Duration;

use crate::packet::ResponseError;

/// Attempts per exchange, the first included.
pub const MAX_ATTEMPTS: usize = 5;
/// A busy EC (status 0x01) is asking us to come back shortly, so retry fast.
pub const BUSY_BACKOFF: Duration = Duration::from_millis(20);
/// Anything else that's still worth retrying gets the original slow backoff.
pub const RETRY_BACKOFF: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Stop: the answer is final.
    Fail,
    /// Stop: that was the last attempt.
    Exhausted,
    /// Wait, then send again.
    RetryAfter(Duration),
}

/// What to do after the EC rejected attempt `attempt` (0-based) with `err`.
///
/// NotSupported is a definitive answer from the firmware: the command will never succeed
/// on this device, so retrying just burns ~2.5 s before failing. A single-shot send
/// (`retry_failure == false`) retries only Busy, which means the EC did not action the
/// command; any other answer to it is final.
pub fn after_rejection(err: ResponseError, retry_failure: bool, attempt: usize) -> Decision {
    if err == ResponseError::NotSupported || (!retry_failure && !err.is_busy()) {
        Decision::Fail
    } else if attempt + 1 >= MAX_ATTEMPTS {
        Decision::Exhausted
    } else if err.is_busy() {
        Decision::RetryAfter(BUSY_BACKOFF)
    } else {
        Decision::RetryAfter(RETRY_BACKOFF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_supported_is_final_on_the_first_answer() {
        assert_eq!(
            after_rejection(ResponseError::NotSupported, true, 0),
            Decision::Fail
        );
        assert_eq!(
            after_rejection(ResponseError::NotSupported, false, 0),
            Decision::Fail
        );
    }

    #[test]
    fn a_single_shot_send_retries_only_busy() {
        assert_eq!(
            after_rejection(ResponseError::Failure, false, 0),
            Decision::Fail
        );
        assert_eq!(
            after_rejection(ResponseError::Mismatch, false, 0),
            Decision::Fail
        );
        assert_eq!(
            after_rejection(ResponseError::Busy, false, 0),
            Decision::RetryAfter(BUSY_BACKOFF)
        );
    }

    #[test]
    fn a_normal_send_backs_off_until_the_last_attempt() {
        assert_eq!(
            after_rejection(ResponseError::Failure, true, 0),
            Decision::RetryAfter(RETRY_BACKOFF)
        );
        assert_eq!(
            after_rejection(ResponseError::Failure, true, MAX_ATTEMPTS - 1),
            Decision::Exhausted
        );
    }
}
