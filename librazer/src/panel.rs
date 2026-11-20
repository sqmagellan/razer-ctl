//! The Blade 16 (2023)'s Mini-LED dual-mode panel: which of its two native modes the
//! firmware starts the panel in, 3840x2400 at 120 Hz (UHD+) or 1920x1200 at 240 Hz (FHD+).
//!
//! The protocol is from Paul Rodriguez's razer-helper (MIT, `docs/mini-led-panel.md` and
//! `MiniLedPanelService.cs`, read 2026-11-19), which took it from Synapse's middleware for
//! product 671 and read it on an RZ09-0483 (BIOS 2.02, panel BOE0B90):
//!
//! | Command | Request | Reply |
//! | --- | --- | --- |
//! | `0x0d8f` SKU hardware configuration | `[0]` | byte 0: bit `0x02` = Mini-LED dual-mode panel |
//! | `0x0d8e` Mini-LED panel resolution | `[0]` | byte 0: 0 = UHD+ 120 Hz, 1 = FHD+ 240 Hz |
//! | `0x0d0e` set Mini-LED panel resolution | `[mode]` | success; no promised echo |
//!
//! Their laptop answered `0x0d8f` with `03` and `0x0d8e` with `01` (FHD+, matching the BIOS
//! and Windows). The selection is the BIOS "Max Refresh Rate Mode" and takes effect only
//! after a Windows restart; Windows keeps showing the old mode until then.
//!
//! Gated twice, as razer-helper does: the USB PID must be `0x029F`, the only model whose
//! vendor wrapper and read-back were checked, and that PID also ships IPS panels, so the SKU
//! byte must have the Mini-LED bit too. A write is confirmed with the getter, never by the
//! setter's echo, and an unknown mode byte is an error rather than a guess.
//!
//! On `0x029F` both getters answer `remaining_packets` 256 (wire `[0x00, 0x01]`) where the
//! request had 0, like `0x078f`, so all three commands are on [`crate::packet`]'s list of
//! replies whose `remaining_packets` may differ (seen 2026-11-19 with razer-cli 0.9.2).

use anyhow::{bail, Context, Result};

use crate::packet::Packet;
use crate::transport::HidTransport;

/// The only model the panel commands are offered on.
pub const MINI_LED_PID: u16 = 0x029f;

const GET_SKU_HARDWARE: u16 = 0x0d8f;
const GET_PANEL_MODE: u16 = 0x0d8e;
const SET_PANEL_MODE: u16 = 0x0d0e;

/// The SKU hardware configuration bit for a Mini-LED dual-mode panel.
const SKU_MINI_LED: u8 = 0x02;

/// Getter reads after a write before the write counts as not taken. The charge limit's
/// getter lags its setter by up to 2 s on some Blades (0x029F has no lag there), so a
/// confirm allows a few reads; nobody has timed this one.
const CONFIRM_READS: usize = 4;
const CONFIRM_GAP: std::time::Duration = std::time::Duration::from_millis(250);

/// The panel's two native modes, by the byte the EC uses for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    /// 3840x2400 at 120 Hz.
    Uhd120 = 0,
    /// 1920x1200 at 240 Hz.
    Fhd240 = 1,
}

impl PanelMode {
    pub const ALL: [PanelMode; 2] = [PanelMode::Uhd120, PanelMode::Fhd240];

    /// The mode the EC means by `byte`; anything else is unknown, and an error.
    pub fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            0 => Ok(PanelMode::Uhd120),
            1 => Ok(PanelMode::Fhd240),
            other => bail!("unknown Mini-LED panel mode byte {other:#04x} (0 = UHD+, 1 = FHD+)"),
        }
    }

    /// Resolution and rate, for a menu or a CLI line.
    pub fn label(self) -> &'static str {
        match self {
            PanelMode::Uhd120 => "3840x2400, 120 Hz",
            PanelMode::Fhd240 => "1920x1200, 240 Hz",
        }
    }

    /// The CLI's name for it.
    pub fn name(self) -> &'static str {
        match self {
            PanelMode::Uhd120 => "uhd",
            PanelMode::Fhd240 => "fhd",
        }
    }
}

impl std::str::FromStr for PanelMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "uhd" | "uhd+" => Ok(PanelMode::Uhd120),
            "fhd" | "fhd+" => Ok(PanelMode::Fhd240),
            other => bail!("unknown panel mode {other:?}; use uhd or fhd"),
        }
    }
}

impl std::fmt::Display for PanelMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelWrite {
    /// The mode was already selected; nothing was written.
    AlreadySelected,
    /// Written and read back. It takes effect after a restart.
    Changed,
}

/// Byte 0 of a getter's reply.
fn read_byte(device: &impl HidTransport, command: u16) -> Result<u8> {
    let response = device.send(Packet::new(command, &[0]))?;
    if response.data_len() < 1 {
        bail!("{command:#06x} answered with no data");
    }
    Ok(response.get_args()[0])
}

/// Whether this laptop has the dual-mode panel: PID `0x029F` (no exchange for any other)
/// and the SKU bit. A model that lacks the SKU command answers "not supported", which is
/// a plain no; any other failure is returned.
pub fn supported(device: &impl HidTransport, pid: u16) -> Result<bool> {
    if pid != MINI_LED_PID {
        return Ok(false);
    }
    match read_byte(device, GET_SKU_HARDWARE) {
        Ok(sku) => Ok(sku & SKU_MINI_LED != 0),
        Err(e) if crate::error::is_not_supported(&e) => Ok(false),
        Err(e) => Err(e).context("reading the SKU hardware configuration (0x0d8f)"),
    }
}

/// The SKU hardware byte itself, for a diagnostic line. Not gated.
pub fn sku_hardware_byte(device: &impl HidTransport) -> Result<u8> {
    read_byte(device, GET_SKU_HARDWARE)
}

/// The mode the firmware will start the panel in, or `None` where there is no such panel.
pub fn get_panel_mode(device: &impl HidTransport, pid: u16) -> Result<Option<PanelMode>> {
    if !supported(device, pid)? {
        return Ok(None);
    }
    read_mode(device).map(Some)
}

fn read_mode(device: &impl HidTransport) -> Result<PanelMode> {
    PanelMode::from_byte(read_byte(device, GET_PANEL_MODE)?)
}

/// Select `mode` for the next start. Checks support again, writes nothing when `mode` is
/// already selected, and confirms the write with the getter (the setter does not promise
/// an echo). An unconfirmed write is an error: the caller must not offer a restart then.
pub fn set_panel_mode(device: &impl HidTransport, pid: u16, mode: PanelMode) -> Result<PanelWrite> {
    if !supported(device, pid)? {
        return Err(anyhow::Error::new(
            crate::packet::ResponseError::NotSupported,
        ))
        .context("this laptop has no Mini-LED dual-mode panel");
    }
    if read_mode(device)? == mode {
        return Ok(PanelWrite::AlreadySelected);
    }
    device.send(Packet::new(SET_PANEL_MODE, &[mode as u8]))?;
    let mut last = None;
    for i in 0..CONFIRM_READS {
        if i > 0 {
            device.pause(CONFIRM_GAP);
        }
        match read_mode(device) {
            Ok(read) if read == mode => return Ok(PanelWrite::Changed),
            Ok(read) => last = Some(Ok(read)),
            Err(e) => last = Some(Err(e)),
        }
    }
    match last {
        Some(Ok(read)) => bail!(
            "the laptop did not confirm the panel mode: asked for {mode}, it reads {read}; \
             check the BIOS Max Refresh Rate Mode before restarting"
        ),
        Some(Err(e)) => Err(e).context(
            "the panel mode could not be read back; check the BIOS Max Refresh Rate Mode \
             before restarting",
        ),
        None => unreachable!("CONFIRM_READS is at least 1"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockTransport;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// A 0x029F with the dual-mode panel: SKU byte `sku`, panel mode held in `mode` and
    /// written by 0x0d0e unless `ignore_writes`.
    fn panel(sku: u8, mode: Rc<Cell<u8>>, ignore_writes: bool) -> MockTransport {
        MockTransport::with_responder(move |p| {
            let mut reply = Packet::new(p.command(), &[0]);
            match p.command() {
                GET_SKU_HARDWARE => reply.set_args(&[sku]),
                GET_PANEL_MODE => reply.set_args(&[mode.get()]),
                SET_PANEL_MODE => {
                    if !ignore_writes {
                        mode.set(p.get_args()[0]);
                    }
                    // No echo: the setter promises none.
                    reply.set_args(&[0]);
                }
                other => panic!("unexpected command {other:#06x}"),
            }
            reply
        })
    }

    #[test]
    fn the_getters_send_one_zero_byte_and_the_setter_the_mode() {
        let mode = Rc::new(Cell::new(1));
        let mock = panel(0x03, mode.clone(), false);
        assert_eq!(
            get_panel_mode(&mock, MINI_LED_PID).unwrap(),
            Some(PanelMode::Fhd240)
        );
        assert_eq!(
            mock.sent(),
            vec![(GET_SKU_HARDWARE, vec![0]), (GET_PANEL_MODE, vec![0])]
        );
        let mock = panel(0x03, mode.clone(), false);
        assert_eq!(
            set_panel_mode(&mock, MINI_LED_PID, PanelMode::Uhd120).unwrap(),
            PanelWrite::Changed
        );
        assert_eq!(
            mock.sent(),
            vec![
                (GET_SKU_HARDWARE, vec![0]),
                (GET_PANEL_MODE, vec![0]),
                (SET_PANEL_MODE, vec![0]),
                (GET_PANEL_MODE, vec![0]),
            ]
        );
        assert_eq!(mode.get(), 0);
        let mock = panel(0x03, mode.clone(), false);
        set_panel_mode(&mock, MINI_LED_PID, PanelMode::Fhd240).unwrap();
        assert!(mock.sent().contains(&(SET_PANEL_MODE, vec![1])));
        assert_eq!(mode.get(), 1);
    }

    #[test]
    fn another_pid_is_never_asked() {
        let mock = MockTransport::with_responder(|p| panic!("sent {:#06x}", p.command()));
        for pid in [0x02b6, 0x028a, 0x02c6, 0] {
            assert!(!supported(&mock, pid).unwrap());
            assert_eq!(get_panel_mode(&mock, pid).unwrap(), None);
            let err = set_panel_mode(&mock, pid, PanelMode::Uhd120).unwrap_err();
            assert!(crate::error::is_not_supported(&err), "{err:#}");
        }
        assert_eq!(mock.sent_count(), 0);
    }

    #[test]
    fn the_sku_bit_decides_on_0x029f() {
        // An IPS 0x029F: the bit is clear, so nothing is read or written past the SKU.
        let mode = Rc::new(Cell::new(0));
        let ips = panel(0x01, mode.clone(), false);
        assert_eq!(get_panel_mode(&ips, MINI_LED_PID).unwrap(), None);
        assert!(set_panel_mode(&ips, MINI_LED_PID, PanelMode::Fhd240).is_err());
        assert!(!ips.sent().iter().any(|(c, _)| *c != GET_SKU_HARDWARE));
        assert_eq!(mode.get(), 0);
        // The bit alone, with no others set.
        let only = panel(0x02, mode, false);
        assert!(supported(&only, MINI_LED_PID).unwrap());
    }

    #[test]
    fn a_model_without_the_sku_command_is_unsupported_not_an_error() {
        let mock = MockTransport::with_fallible_responder(|_| {
            Err(anyhow::Error::new(
                crate::packet::ResponseError::NotSupported,
            ))
        });
        assert!(!supported(&mock, MINI_LED_PID).unwrap());
        assert_eq!(get_panel_mode(&mock, MINI_LED_PID).unwrap(), None);
        // Bus trouble is not "unsupported".
        let busy = MockTransport::with_fallible_responder(|_| {
            Err(anyhow::Error::new(crate::packet::ResponseError::Busy))
        });
        assert!(supported(&busy, MINI_LED_PID).is_err());
    }

    #[test]
    fn an_unknown_mode_byte_is_an_error() {
        let mock = panel(0x03, Rc::new(Cell::new(2)), false);
        let err = get_panel_mode(&mock, MINI_LED_PID).unwrap_err();
        assert!(
            err.to_string().contains("unknown Mini-LED panel mode"),
            "{err}"
        );
        assert!(PanelMode::from_byte(0xff).is_err());
        assert_eq!(PanelMode::from_byte(0).unwrap(), PanelMode::Uhd120);
        assert_eq!(PanelMode::from_byte(1).unwrap(), PanelMode::Fhd240);
        // And no write goes out after an unknown read.
        let mode = Rc::new(Cell::new(7));
        let mock = panel(0x03, mode.clone(), false);
        assert!(set_panel_mode(&mock, MINI_LED_PID, PanelMode::Uhd120).is_err());
        assert!(!mock.sent().iter().any(|(c, _)| *c == SET_PANEL_MODE));
        assert_eq!(mode.get(), 7);
    }

    #[test]
    fn the_selected_mode_is_not_written_again() {
        let mode = Rc::new(Cell::new(0));
        let mock = panel(0x03, mode, false);
        assert_eq!(
            set_panel_mode(&mock, MINI_LED_PID, PanelMode::Uhd120).unwrap(),
            PanelWrite::AlreadySelected
        );
        assert!(!mock.sent().iter().any(|(c, _)| *c == SET_PANEL_MODE));
    }

    #[test]
    fn a_write_the_getter_does_not_confirm_is_an_error() {
        let mode = Rc::new(Cell::new(1));
        let mock = panel(0x03, mode, true);
        let err = set_panel_mode(&mock, MINI_LED_PID, PanelMode::Uhd120).unwrap_err();
        assert!(err.to_string().contains("did not confirm"), "{err}");
        // One write, then every confirm read, and no second write.
        let sent = mock.sent();
        assert_eq!(sent.iter().filter(|(c, _)| *c == SET_PANEL_MODE).count(), 1);
        assert_eq!(
            sent.iter().filter(|(c, _)| *c == GET_PANEL_MODE).count(),
            1 + CONFIRM_READS
        );
    }

    #[test]
    fn a_getter_that_lags_the_write_is_waited_for() {
        // Reads the old mode once after the write, then the new one.
        let mode = Rc::new(Cell::new(1u8));
        let stale = Rc::new(RefCell::new(None::<u8>));
        let (m, s) = (mode.clone(), stale.clone());
        let mock = MockTransport::with_responder(move |p| {
            let mut reply = Packet::new(p.command(), &[0]);
            match p.command() {
                GET_SKU_HARDWARE => reply.set_args(&[0x03]),
                GET_PANEL_MODE => {
                    let shown = s.borrow_mut().take().unwrap_or(m.get());
                    reply.set_args(&[shown]);
                }
                SET_PANEL_MODE => {
                    *s.borrow_mut() = Some(m.get());
                    m.set(p.get_args()[0]);
                }
                _ => unreachable!(),
            }
            reply
        });
        assert_eq!(
            set_panel_mode(&mock, MINI_LED_PID, PanelMode::Uhd120).unwrap(),
            PanelWrite::Changed
        );
        assert_eq!(mode.get(), 0);
    }

    #[test]
    fn names_parse_and_print() {
        for m in PanelMode::ALL {
            assert_eq!(m.name().parse::<PanelMode>().unwrap(), m);
        }
        assert_eq!("FHD+".parse::<PanelMode>().unwrap(), PanelMode::Fhd240);
        assert!("4k".parse::<PanelMode>().is_err());
        assert_eq!(PanelMode::Uhd120.to_string(), "3840x2400, 120 Hz");
    }
}
