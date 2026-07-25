use anyhow::{ensure, Result};
use rand::Rng;
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
    Successful = 0x02,
    NotSupported = 0x05,
}

impl Packet {
    /// Transaction id placed in every outgoing packet. Razer firmware uses this
    /// field to route responses, and the reference drivers use a FIXED id per
    /// device family (razer-laptop-control: 0x1F; OpenRazer: 0xFF) rather than a
    /// random one. A fixed id matches the reference and tends to reduce the retries
    /// in `Device::send`. Set to `None` to restore the original random-id behavior.
    const TRANSACTION_ID: Option<u8> = Some(0x1f);

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
            id: Self::TRANSACTION_ID.unwrap_or_else(|| rand::thread_rng().gen()),
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

    pub fn ensure_matches_report(&self, report: &Packet) -> Result<()> {
        ensure!(
            (report.command_class, report.command_id, report.id)
                == (self.command_class, self.command_id, self.id),
            "Response does not match the report"
        );

        ensure!(
            self.remaining_packets == report.remaining_packets
            || (self.command_class, self.command_id) == (0x07, 0x92) /* 0x0792 (bho) has special handling */
            || (self.command_class, self.command_id) == (0x07, 0x8f), /* 0x078f max fan speed mode has special handling */
            "Response command does not match the report"
        );

        ensure!(
            self.status != CommandStatus::NotSupported as u8,
            "Command not supported"
        );

        ensure!(
            self.status == CommandStatus::Successful as u8,
            "Command failed with unknown status: {:02X?}",
            self.status
        );

        Ok(())
    }
}

impl From<&Packet> for Vec<u8> {
    fn from(packet: &Packet) -> Vec<u8> {
        bincode::serialize(packet).unwrap()
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
        // id; we standardized on 0x1F. Flip TRANSACTION_ID to None to restore random.
        assert_eq!(Packet::TRANSACTION_ID, Some(0x1f));
    }

    #[test]
    fn wire_layout_matches_protocol() {
        let pkt = Packet::new(0x0d02, &[0x01, 0x02, 0x00, 0x00]);
        let bytes: Vec<u8> = (&pkt).into();

        // Fixed 90-byte report (matches the size Device::send relies on).
        assert_eq!(bytes.len(), std::mem::size_of::<Packet>());
        assert_eq!(bytes.len(), 90);

        assert_eq!(bytes[0], 0x00, "status = New");
        if let Some(id) = Packet::TRANSACTION_ID {
            assert_eq!(bytes[1], id, "transaction id is wired into the packet");
        }
        // bytes[2..4] = remaining_packets (u16 LE) = 0
        assert_eq!(&bytes[2..4], &[0x00, 0x00]);
        assert_eq!(bytes[4], 0x00, "protocol_type");
        assert_eq!(bytes[5], 4, "data_size = args.len()");
        assert_eq!(bytes[6], 0x0d, "command_class = high byte of command");
        assert_eq!(bytes[7], 0x02, "command_id = low byte of command");
        assert_eq!(&bytes[8..12], &[0x01, 0x02, 0x00, 0x00], "args copied verbatim");
        assert_eq!(bytes[88], 0x00, "crc");
        assert_eq!(bytes[89], 0x00, "reserved");
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
        assert!(Packet::try_new(0x0d82, &[0u8; 80]).is_ok(), "80 args is the limit");
        assert!(Packet::try_new(0x0d82, &[0u8; 81]).is_err(), "81 args must error");
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
        assert_eq!(bytes[1], 0xff, "explicit transaction id overrides the default");
    }
}

