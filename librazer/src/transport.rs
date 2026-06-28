//! The seam between the command protocol and the physical HID device.
//!
//! `command::*` talk to the laptop purely by exchanging [`Packet`]s, so they only
//! need *something that can send a packet and hand back the response* -- not the
//! concrete hidapi-backed [`crate::device::Device`]. `HidTransport` is that
//! something. The real implementation is `Device` (Windows/Linux only); a
//! `#[cfg(test)] MockTransport` records the packets it's asked to send and replays
//! canned responses, which lets the command + tray logic be exercised on any host
//! (e.g. a macOS dev box) with no hardware attached.

use crate::packet::Packet;
use anyhow::Result;

/// Send a request packet and return the device's matched response packet.
///
/// Implementors own the wire details (retries, report-id framing, response
/// matching); callers above this trait deal only in `Packet`s.
pub trait HidTransport {
    fn send(&self, packet: Packet) -> Result<Packet>;
}

/// Test-only `HidTransport` that records every request and replies via a
/// caller-supplied responder. It never touches hardware, so it lets the `command`
/// layer (and any logic layered on it) be exercised on any host.
///
/// The command layer validates responses by inspecting `get_args()` only -- packet
/// *matching* (transaction id, status) lives in `Device::send`, below this seam --
/// so a response just needs the right arg bytes; `Packet::new`'s status is ignored.
#[cfg(test)]
pub struct MockTransport {
    sent: std::cell::RefCell<Vec<(u16, Vec<u8>)>>,
    responder: Box<dyn Fn(&Packet) -> Packet>,
}

#[cfg(test)]
impl MockTransport {
    /// Echoing transport: each response repeats the request's args, mimicking the
    /// firmware's success ack. Satisfies the `starts_with(args)` checks the set_*
    /// paths use, so it's the right mock for write-path tests.
    pub fn echo() -> Self {
        Self::with_responder(|req| Packet::new(req.command(), req.get_args()))
    }

    /// Transport whose response is computed from each request -- use for read-path
    /// (get_*) tests that must return specific register values.
    pub fn with_responder(f: impl Fn(&Packet) -> Packet + 'static) -> Self {
        Self {
            sent: std::cell::RefCell::new(Vec::new()),
            responder: Box::new(f),
        }
    }

    /// (command, args) of every packet sent so far, in order.
    pub fn sent(&self) -> Vec<(u16, Vec<u8>)> {
        self.sent.borrow().clone()
    }

    /// Number of packets sent so far.
    pub fn sent_count(&self) -> usize {
        self.sent.borrow().len()
    }
}

#[cfg(test)]
impl HidTransport for MockTransport {
    fn send(&self, packet: Packet) -> Result<Packet> {
        let response = (self.responder)(&packet);
        self.sent
            .borrow_mut()
            .push((packet.command(), packet.get_args().to_vec()));
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_repeats_request_args_and_command() {
        let mock = MockTransport::echo();
        let resp = mock.send(Packet::new(0x0d02, &[1, 2, 3])).unwrap();
        assert_eq!(resp.command(), 0x0d02);
        assert!(resp.get_args().starts_with(&[1, 2, 3]));
    }

    #[test]
    fn records_sent_packets_in_order() {
        let mock = MockTransport::echo();
        mock.send(Packet::new(0x0001, &[9])).unwrap();
        mock.send(Packet::new(0x0002, &[8, 7])).unwrap();
        let sent = mock.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(mock.sent_count(), 2);
        assert_eq!(sent[0].0, 0x0001);
        assert_eq!(&sent[1].1[..2], &[8, 7]);
    }

    #[test]
    fn responder_can_return_canned_values() {
        // Stand in for a get_* register read: respond with a fixed arg byte.
        let mock = MockTransport::with_responder(|req| {
            let mut p = Packet::new(req.command(), &[]);
            p.set_args(&[0, 0, 0x42]);
            p
        });
        let resp = mock.send(Packet::new(0x0d87, &[0, 0, 0])).unwrap();
        assert_eq!(resp.get_args()[2], 0x42);
    }
}
