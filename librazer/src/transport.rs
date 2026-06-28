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
