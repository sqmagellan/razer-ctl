//! Public-API guarantees, tested from *outside* the crate.
//!
//! Files under `tests/` are compiled as their own crate, so they see exactly what a
//! downstream consumer sees. That is the whole point of putting these here: a unit
//! test inside `librazer` can name a private module perfectly well, so it could not
//! catch the visibility regression this file exists to prevent.

use anyhow::Result;
use librazer::packet::Packet;
use librazer::transport::HidTransport;

/// A downstream crate must be able to implement `HidTransport` with its own backing.
///
/// Regression test: `packet` used to be a private module while `HidTransport::send`
/// took and returned a `Packet`, so this impl failed to compile outside the crate
/// with E0603 -- the seam was unusable by the very consumers it was designed for.
/// Compiling this file *is* the assertion.
struct RecordingTransport {
    sent: std::cell::RefCell<Vec<u16>>,
}

impl HidTransport for RecordingTransport {
    fn send(&self, packet: Packet) -> Result<Packet> {
        self.sent.borrow_mut().push(packet.command());
        // Echo the request back, which is what the firmware does on a successful
        // register write -- enough for the `command::set_*` echo checks.
        Ok(Packet::new(
            packet.command(),
            &packet.get_args()[..packet.data_len()],
        ))
    }
}

#[test]
fn external_impl_compiles_and_drives_the_command_layer() {
    let transport = RecordingTransport {
        sent: std::cell::RefCell::new(Vec::new()),
    };

    // The command layer is generic over the seam, so it must accept a foreign
    // transport with no special casing.
    librazer::command::set_keyboard_brightness(&transport, 128).unwrap();

    assert_eq!(transport.sent.borrow().as_slice(), &[0x0303]);
}

#[test]
fn packet_can_be_constructed_and_inspected_externally() {
    // The constructors/accessors a downstream transport or probe tool needs must be
    // reachable; the wire fields themselves stay private to the module.
    let p = Packet::new(0x0d02, &[0x01, 0x02]);
    assert_eq!(p.command(), 0x0d02);
    assert_eq!(p.data_len(), 2);
    assert!(p.get_args().starts_with(&[0x01, 0x02]));

    // The checked constructor is the one untrusted input must go through.
    assert!(Packet::try_new(0x0d02, &[0u8; 81]).is_err());
}
