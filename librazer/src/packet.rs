use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

/// Packet is the structure of the packet that is sent to the Razer HID device and received back.
/// Source https://github.com/Razer-Linux/razer-laptop-control-no-dkms/blob/main/razer_control_gui/src/device.rs.
#[repr(C)]
#[derive(Serialize, Deserialize, Debug)]
pub struct Packet {
    status: u8,
    id: u8,
    remaining_packets: u16,
    protocol_type: u8,
    data_size: u8,
    command_class: u8,
    command_id: u8,
    #[serde(with = "BigArray")]
    args: [u8; 80],
    crc: u8,
    reserved: u8,
}

enum CommandStatus {
    New = 0x00,
    Busy = 0x01,
    Successful = 0x02,
    Failure = 0x03,
    NotSupported = 0x05,
}

/// Why a response failed to match its request. `Device::send` retries on any of
/// these, but the reason picks the backoff: a `Busy` EC is asking us to come back
/// shortly, whereas the rest are either hard rejections or a desynchronised bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    /// Status 0x01 -- firmware is busy and the command was not actioned. Transient
    /// under write-heavy sequences (`apply()` issues six-plus writes back to back).
    Busy,
    /// Status 0x03 -- firmware actioned and rejected the command.
    Failure,
    /// Status 0x05 -- command unsupported on this device.
    NotSupported,
    /// Header mismatch, or a status byte outside the documented set. The response
    /// may belong to a different transaction, so the bus may be out of step.
    Mismatch,
}

impl ResponseError {
    /// Whether the EC is merely asking us to retry rather than refusing outright.
    pub fn is_busy(self) -> bool {
        self == ResponseError::Busy
    }
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseError::Busy => write!(f, "Device busy (status 0x01)"),
            ResponseError::Failure => write!(f, "Command failed (status 0x03)"),
            ResponseError::NotSupported => write!(f, "Command not supported (status 0x05)"),
            ResponseError::Mismatch => write!(f, "Response does not match the report"),
        }
    }
}

/// `ResponseError` is a real `std::error::Error`, not just something printable.
///
/// `Device::send` used to return `anyhow!("{}", err)`, which flattened this enum into a
/// string and destroyed the only thing a caller could branch on. A CLI that wants to exit
/// 3 for "this device doesn't implement that command" and 4 for "the bus is confused" has
/// to be able to `downcast_ref` its way back to the variant, so the type must survive
/// propagation.
impl std::error::Error for ResponseError {}

impl Packet {
    /// Transaction id placed in every outgoing packet. Razer firmware uses this field
    /// to route responses, and the reference drivers use a FIXED id per device family
    /// (razer-laptop-control: 0x1F; OpenRazer: 0xFF) rather than a random one. A fixed
    /// id matches the reference and reduces retries in `Device::send`.
    ///
    /// This was once `Option<u8>`, falling back to a random id when `None` -- which is
    /// why this crate depended on `rand`. It has been a fixed `0x1f` since we
    /// standardized on the reference behavior, so that fallback was unreachable and the
    /// dependency (plus `rand_chacha`, `rand_core`, `ppv-lite86`, `zerocopy`,
    /// `getrandom`) was dead weight in a binary people are asked to trust. If a device
    /// ever needs a different id, use [`Packet::new_with_tx`] -- per-command and
    /// explicit, which is what the probe path already does.
    const TRANSACTION_ID: u8 = 0x1f;

    /// Capacity of the wire `args` buffer. The report is a fixed 90 bytes, of which 80
    /// carry arguments, and `data_size` is a single byte -- so anything longer can be
    /// neither stored nor described. Callers taking *untrusted* argument lengths (the
    /// CLI `cmd` probe) must go through [`Packet::try_new`].
    pub const MAX_ARGS: usize = 80;

    /// Build a packet, rejecting an over-long argument list instead of panicking.
    ///
    /// [`Packet::new`] indexes a fixed 80-byte buffer and casts the length to `u8`, so
    /// an oversized slice would panic (`copy_from_slice` range) or silently wrap
    /// `data_size` to 0 at 256 args. Every in-crate caller passes a compile-time-known
    /// short array, but `custom_command` forwards user-supplied bytes -- that path uses
    /// this constructor so a bad invocation is a clean error, not a crash.
    pub fn try_new(command: u16, args: &[u8]) -> Result<Packet> {
        ensure!(
            args.len() <= Self::MAX_ARGS,
            "Too many arguments: {} (max {})",
            args.len(),
            Self::MAX_ARGS
        );
        Ok(Self::new(command, args))
    }

    /// [`Packet::try_new`] with an explicit transaction id -- the checked counterpart of
    /// [`Packet::new_with_tx`], used by the CLI `cmd --tx` probe.
    pub fn try_new_with_tx(command: u16, args: &[u8], tx: u8) -> Result<Packet> {
        let mut p = Self::try_new(command, args)?;
        p.id = tx;
        Ok(p)
    }

    /// Build a packet from a **statically known** argument list.
    ///
    /// # Panics
    /// If `args.len() > `[`Packet::MAX_ARGS`]. Every caller inside this crate passes a
    /// fixed short array, so this is unreachable there; for caller-supplied lengths use
    /// [`Packet::try_new`].
    pub fn new(command: u16, args: &[u8]) -> Packet {
        assert!(
            args.len() <= Self::MAX_ARGS,
            "Packet args too long: {} (max {}); use Packet::try_new for untrusted input",
            args.len(),
            Self::MAX_ARGS
        );
        let mut args_buffer = [0x00; 80];
        args_buffer[..args.len()].copy_from_slice(args);

        Packet {
            status: CommandStatus::New as u8,
            id: Self::TRANSACTION_ID,
            remaining_packets: 0x0000,
            protocol_type: 0x00,
            data_size: args.len() as u8,
            command_class: (command >> 8) as u8,
            command_id: (command & 0xff) as u8,
            args: args_buffer,
            crc: 0x00,
            reserved: 0x00,
        }
    }

    /// Build a packet like [`Packet::new`] but force a specific transaction id, overriding
    /// [`Packet::TRANSACTION_ID`]. Some reference drivers issue certain commands at a
    /// different id than our default 0x1F -- notably OpenRazer drives keyboard *matrix
    /// effects* at 0xFF on several Blade laptops. Used by the CLI `cmd --tx` probe.
    pub fn new_with_tx(command: u16, args: &[u8], tx: u8) -> Packet {
        let mut p = Packet::new(command, args);
        p.id = tx;
        p
    }

    /// Overwrite the leading argument bytes.
    ///
    /// # Panics
    /// If `args.len() > `[`Packet::MAX_ARGS`]. Only used with fixed short arrays (tests
    /// and canned responses); untrusted lengths belong in [`Packet::try_new`].
    pub fn set_args(&mut self, args: &[u8]) {
        assert!(
            args.len() <= Self::MAX_ARGS,
            "Packet args too long: {} (max {})",
            args.len(),
            Self::MAX_ARGS
        );
        self.args[..args.len()].copy_from_slice(args)
    }

    pub fn get_args(&self) -> &[u8] {
        &self.args
    }

    /// Stamp a raw status byte, so tests can synthesise the firmware replies
    /// (`0x01` busy, `0x03` failure, ...) that only real hardware produces.
    #[cfg(test)]
    pub fn set_status_for_test(&mut self, status: u8) {
        self.status = status;
    }

    /// The 16-bit command (class<<8 | id) this packet carries -- the inverse of the
    /// `command` argument to `new`. Lets tests/inspection recover which command a
    /// packet represents without reaching into the private wire fields.
    pub fn command(&self) -> u16 {
        ((self.command_class as u16) << 8) | self.command_id as u16
    }

    /// Logical argument length (the `data_size` field). The full args buffer is
    /// always 80 bytes; this is how many of them are meaningful for this packet.
    pub fn data_len(&self) -> usize {
        self.data_size as usize
    }

    /// Classify a response against the request that produced it.
    ///
    /// Split out from [`Packet::ensure_matches_report`] so the retry loop can tell a
    /// *busy* EC (come back in a moment) apart from a hard rejection -- previously
    /// every non-success collapsed into one opaque error and drew the same flat
    /// backoff. `Ok(())` means the response is a genuine success for this request.
    pub fn classify_response(&self, report: &Packet) -> Result<(), ResponseError> {
        if (report.command_class, report.command_id, report.id)
            != (self.command_class, self.command_id, self.id)
        {
            return Err(ResponseError::Mismatch);
        }

        // 0x0792 (battery health optimizer) and 0x078f (max fan speed mode) legitimately
        // answer with a different remaining_packets than they were asked with.
        let remaining_ok = self.remaining_packets == report.remaining_packets
            || (self.command_class, self.command_id) == (0x07, 0x92)
            || (self.command_class, self.command_id) == (0x07, 0x8f);
        if !remaining_ok {
            return Err(ResponseError::Mismatch);
        }

        match self.status {
            s if s == CommandStatus::Successful as u8 => Ok(()),
            s if s == CommandStatus::Busy as u8 => Err(ResponseError::Busy),
            s if s == CommandStatus::Failure as u8 => Err(ResponseError::Failure),
            s if s == CommandStatus::NotSupported as u8 => Err(ResponseError::NotSupported),
            _ => Err(ResponseError::Mismatch),
        }
    }

    pub fn ensure_matches_report(&self, report: &Packet) -> Result<()> {
        self.classify_response(report)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    /// Byte offset of the `crc` field in the serialized 90-byte report.
    const CRC_OFFSET: usize = 88;
    /// Range of bytes the checksum covers: everything from the transaction id up to
    /// (not including) the checksum itself.
    const CRC_RANGE: std::ops::Range<usize> = 2..88;

    /// The report checksum: a plain XOR of bytes 2..88.
    ///
    /// Matches OpenRazer's `razer_calculate_crc` (`driver/razercommon.c`) and
    /// razer-laptop-control's `calc_crc`, which are independent decodes of the same
    /// protocol and agree on both the algorithm and the range.
    ///
    /// We previously shipped a hard-coded `crc: 0x00` on every outgoing packet.
    /// That is *accepted* by the Blade 16 (2023) firmware -- HW-verified 2026-07-25
    /// on PID 0x029F: a zero-CRC write returns status 0x02 (success) -- so this is
    /// not a fix for any observed failure. It is a portability fix: both reference
    /// drivers compute it for every Razer device, so a model whose firmware *does*
    /// validate the field would have rejected every command we sent, and on hardware
    /// we cannot test that would look like "razer-ctl just doesn't work on my Blade".
    /// Sending a correct checksum costs one XOR pass and removes the whole class of
    /// failure.
    fn compute_crc(bytes: &[u8]) -> u8 {
        bytes[Self::CRC_RANGE].iter().fold(0u8, |crc, b| crc ^ b)
    }
}

impl From<&Packet> for Vec<u8> {
    fn from(packet: &Packet) -> Vec<u8> {
        // The wire layout is a fixed 90-byte `#[repr(C)]` struct of plain integers, so
        // bincode cannot fail here; if it somehow did, an all-zero report would be
        // silently wrong on the bus, and a panic is the honest outcome.
        let mut bytes = bincode::serialize(packet)
            .expect("Packet is a fixed-size POD struct; serialization cannot fail");
        // Stamp the checksum over the serialized form -- this is the single choke
        // point where a packet becomes wire bytes, so nothing can bypass it.
        bytes[Packet::CRC_OFFSET] = Packet::compute_crc(&bytes);
        bytes
    }
}

impl TryFrom<&[u8]> for Packet {
    type Error = anyhow::Error;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        ensure!(
            data.len() == std::mem::size_of::<Packet>(),
            "Invalid raw data size"
        );

        Ok(bincode::deserialize::<Packet>(data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_id_constant_is_fixed_0x1f() {
        // Reference drivers (razer-laptop-control 0x1F, OpenRazer 0xFF) use a fixed
        // id; we standardized on 0x1F. Per-command overrides go through new_with_tx.
        assert_eq!(Packet::TRANSACTION_ID, 0x1f);
    }

    #[test]
    fn wire_layout_matches_protocol() {
        let pkt = Packet::new(0x0d02, &[0x01, 0x02, 0x00, 0x00]);
        let bytes: Vec<u8> = (&pkt).into();

        // Fixed 90-byte report (matches the size Device::send relies on).
        assert_eq!(bytes.len(), std::mem::size_of::<Packet>());
        assert_eq!(bytes.len(), 90);

        assert_eq!(bytes[0], 0x00, "status = New");
        assert_eq!(
            bytes[1],
            Packet::TRANSACTION_ID,
            "transaction id is wired into the packet"
        );
        // bytes[2..4] = remaining_packets (u16 LE) = 0
        assert_eq!(&bytes[2..4], &[0x00, 0x00]);
        assert_eq!(bytes[4], 0x00, "protocol_type");
        assert_eq!(bytes[5], 4, "data_size = args.len()");
        assert_eq!(bytes[6], 0x0d, "command_class = high byte of command");
        assert_eq!(bytes[7], 0x02, "command_id = low byte of command");
        assert_eq!(
            &bytes[8..12],
            &[0x01, 0x02, 0x00, 0x00],
            "args copied verbatim"
        );
        // bytes[88] is the checksum, asserted in the crc tests below.
        assert_eq!(bytes[89], 0x00, "reserved");
    }

    #[test]
    fn crc_is_xor_of_bytes_2_to_88() {
        // Independently recompute the checksum the way OpenRazer's
        // razer_calculate_crc does, and require the serialized packet to carry it.
        let pkt = Packet::new(0x0d02, &[0x01, 0x02, 0x03, 0x04]);
        let bytes: Vec<u8> = (&pkt).into();

        let expected = bytes[2..88].iter().fold(0u8, |c, b| c ^ b);
        assert_eq!(bytes[88], expected, "crc must be the XOR of bytes 2..88");

        // Sanity: this packet has non-zero payload, so a zero crc would mean the
        // field simply wasn't stamped -- the pre-fix behavior this test guards.
        assert_ne!(
            bytes[88], 0x00,
            "a packet with payload must have a non-zero crc"
        );

        // The checksum must not be computed over itself or the trailing reserved byte.
        assert_eq!(bytes[89], 0x00, "reserved stays zero");
    }

    #[test]
    fn crc_changes_with_the_payload() {
        // A checksum that ignored the args would be worse than useless -- it would
        // look correct while failing to detect exactly the corruption it exists for.
        let a: Vec<u8> = (&Packet::new(0x0d02, &[0x01])).into();
        let b: Vec<u8> = (&Packet::new(0x0d02, &[0x02])).into();
        assert_ne!(a[88], b[88], "differing payloads must yield differing crcs");

        // Same for the command id.
        let c: Vec<u8> = (&Packet::new(0x0d03, &[0x01])).into();
        assert_ne!(a[88], c[88], "differing commands must yield differing crcs");
    }

    #[test]
    fn crc_of_an_all_zero_body_is_zero() {
        // Degenerate case: a command of 0x0000 with no args XORs to zero. Pinned so
        // the "non-zero crc" assertion above is understood as payload-dependent, not
        // a universal invariant.
        let bytes: Vec<u8> = (&Packet::new(0x0000, &[])).into();
        assert_eq!(bytes[2..88].iter().fold(0u8, |c, b| c ^ b), bytes[88]);
    }

    #[test]
    fn data_size_tracks_args_len() {
        let pkt = Packet::new(0x0084, &[0, 0]);
        let bytes: Vec<u8> = (&pkt).into();
        assert_eq!(bytes[5], 2);
    }

    #[test]
    fn roundtrips_through_bytes() {
        let pkt = Packet::new(0x0d82, &[0, 1, 0, 0]);
        let bytes: Vec<u8> = (&pkt).into();
        let back: Packet = bytes.as_slice().try_into().unwrap();
        let rebytes: Vec<u8> = (&back).into();
        assert_eq!(bytes, rebytes);
    }

    #[test]
    fn try_new_rejects_oversized_args() {
        // The CLI `cmd` probe forwards user-supplied bytes. The args buffer is 80 wide,
        // so 81 used to panic in copy_from_slice; it must now be a clean error.
        assert!(
            Packet::try_new(0x0d82, &[0u8; 80]).is_ok(),
            "80 args is the limit"
        );
        assert!(
            Packet::try_new(0x0d82, &[0u8; 81]).is_err(),
            "81 args must error"
        );
        // 256 would also have wrapped data_size (u8) to 0 rather than overflowing.
        assert!(Packet::try_new(0x0d82, &[0u8; 256]).is_err());
        assert!(Packet::try_new_with_tx(0x0d82, &[0u8; 81], 0xff).is_err());
    }

    #[test]
    fn try_new_preserves_args_and_tx() {
        let p = Packet::try_new(0x0d02, &[1, 2, 3]).unwrap();
        assert_eq!(p.command(), 0x0d02);
        assert_eq!(p.data_len(), 3);
        assert!(p.get_args().starts_with(&[1, 2, 3]));

        let tx = Packet::try_new_with_tx(0x0f02, &[9], 0xff).unwrap();
        let bytes: Vec<u8> = (&tx).into();
        assert_eq!(
            bytes[1], 0xff,
            "explicit transaction id overrides the default"
        );
    }

    /// Build a response that echoes `req`'s routing fields, then stamp `status`.
    fn response_to(req: &Packet, status: u8) -> Packet {
        let mut resp = Packet::new(req.command(), &req.get_args()[..req.data_len()]);
        resp.set_status_for_test(status);
        resp
    }

    #[test]
    fn classify_response_distinguishes_busy_from_failure() {
        let req = Packet::new(0x0d02, &[0x00, 0x01, 0x02, 0x00]);

        // Before the fix these three collapsed into one "unknown status" error, so the
        // retry loop could not tell "come back shortly" from a hard rejection.
        assert_eq!(
            response_to(&req, 0x01).classify_response(&req),
            Err(ResponseError::Busy)
        );
        assert_eq!(
            response_to(&req, 0x03).classify_response(&req),
            Err(ResponseError::Failure)
        );
        assert_eq!(
            response_to(&req, 0x05).classify_response(&req),
            Err(ResponseError::NotSupported)
        );
        assert_eq!(response_to(&req, 0x02).classify_response(&req), Ok(()));

        // Only Busy should trigger the fast-retry path.
        assert!(ResponseError::Busy.is_busy());
        assert!(!ResponseError::Failure.is_busy());
        assert!(!ResponseError::NotSupported.is_busy());
    }

    #[test]
    fn classify_response_reports_undocumented_status_as_mismatch() {
        let req = Packet::new(0x0d02, &[0x00, 0x01, 0x02, 0x00]);
        assert_eq!(
            response_to(&req, 0x7f).classify_response(&req),
            Err(ResponseError::Mismatch)
        );
    }

    #[test]
    fn classify_response_rejects_a_different_command() {
        let req = Packet::new(0x0d02, &[0x00, 0x01, 0x02, 0x00]);
        let other = response_to(&Packet::new(0x0d82, &[0, 1, 0, 0]), 0x02);
        assert_eq!(
            other.classify_response(&req),
            Err(ResponseError::Mismatch),
            "a response for another command must never be accepted"
        );
    }

    #[test]
    fn ensure_matches_report_still_wraps_classification() {
        let req = Packet::new(0x0d02, &[0x00, 0x01, 0x02, 0x00]);
        assert!(response_to(&req, 0x02).ensure_matches_report(&req).is_ok());

        let err = response_to(&req, 0x01)
            .ensure_matches_report(&req)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("busy"),
            "busy should be named in the error: {err}"
        );
    }
}
