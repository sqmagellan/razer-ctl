use crate::descriptor::{Descriptor, SUPPORTED};
use crate::error::DetectError;
use crate::packet::{Packet, ResponseError};
use crate::transport::HidTransport;

use anyhow::{anyhow, Context, Result};
use std::{thread, time};

pub struct Device {
    device: hidapi::HidDevice,
    pub info: Descriptor,
}

// Read the model id and clip to conform with https://mysupport.razer.com/app/answers/detail/a_id/5481
#[cfg(target_os = "windows")]
fn read_device_model() -> Result<String> {
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    let bios = hklm.open_subkey("HARDWARE\\DESCRIPTION\\System\\BIOS")?;
    let system_sku: String = bios.get_value("SystemSKU")?;
    Ok(system_sku.chars().take(10).collect())
}

#[cfg(target_os = "linux")]
fn read_device_model() -> Result<String> {
    let sku = std::fs::read_to_string("/sys/devices/virtual/dmi/id/product_sku")
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to read product SKU: {}", e))?;

    log::debug!("Linux product SKU: {}", sku);

    if sku.starts_with("RZ") {
        Ok(sku.chars().take(10).collect())
    } else {
        anyhow::bail!("Invalid SKU format: {}", sku)
    }
}

impl Device {
    const RAZER_VID: u16 = 0x1532;

    pub fn info(&self) -> &Descriptor {
        &self.info
    }

    pub fn new(descriptor: Descriptor) -> Result<Device> {
        let api = hidapi::HidApi::new().context("Failed to create hid api")?;

        // Identify the Razer control interface. razer-laptop-control/OpenRazer
        // pick it deterministically (interface 0 / vendor-defined usage page
        // 0xFF00) rather than guessing, so we try those first. We still *confirm*
        // each candidate by probing with a full-sized GET report (0x0084, read
        // device mode -- side-effect free; the keyboard interface rejects 91-byte
        // reports on Windows), and we fall back to the remaining interfaces. So if
        // Windows reports interface_number as -1 / usage_page as 0 (which it can),
        // the sort is a no-op and behavior is exactly as before.
        let mut candidates: Vec<_> = api
            .device_list()
            .filter(|info| {
                (info.vendor_id(), info.product_id()) == (Device::RAZER_VID, descriptor.pid)
            })
            .collect();
        candidates.sort_by_key(|info| (info.interface_number() != 0, info.usage_page() != 0xff00));

        for info in candidates {
            let device = api.open_path(info.path())?;
            let probe: Vec<u8> = std::iter::once(0u8)
                .chain(Into::<Vec<u8>>::into(&Packet::new(0x0084, &[0, 0])))
                .collect();
            if device.send_feature_report(&probe).is_ok() {
                return Ok(Device {
                    device,
                    info: descriptor.clone(),
                });
            }
        }
        // Typed, and readable: this used to dump the whole Descriptor's Debug output at
        // the user, and returned an unclassifiable string so `manual --pid` on a bad PID
        // exited 1 instead of "no device" (verified on hardware).
        Err(anyhow::Error::new(DetectError::InterfaceUnavailable {
            pid: descriptor.pid,
            name: descriptor.name.to_string(),
        }))
    }

    pub fn send(&self, report: Packet) -> Result<Packet> {
        // extra byte for report id
        let mut response_buf: Vec<u8> = vec![0x00; 1 + std::mem::size_of::<Packet>()];
        //println!("Report {:?}", report);

        const MAX_RETRIES: usize = 5;
        // A busy EC (status 0x01) is asking us to come back shortly, so retry fast;
        // anything else that's still worth retrying gets the original slow backoff.
        const BUSY_BACKOFF: time::Duration = time::Duration::from_millis(20);
        const RETRY_BACKOFF: time::Duration = time::Duration::from_millis(500);

        for attempt in 0..MAX_RETRIES {
            thread::sleep(time::Duration::from_micros(1000));

            self.device
                .send_feature_report(
                    [0_u8; 1] // report id
                        .iter()
                        .copied()
                        .chain(Into::<Vec<u8>>::into(&report))
                        .collect::<Vec<_>>()
                        .as_slice(),
                )
                .context("Failed to send feature report")?;

            thread::sleep(time::Duration::from_micros(2000));

            let response_size = self.device.get_feature_report(&mut response_buf)?;
            if response_buf.len() != response_size {
                return Err(anyhow!("Response size != {}", response_buf.len()));
            }

            // skip report id byte
            let response = <&[u8] as TryInto<Packet>>::try_into(&response_buf[1..])?;
            //println!("Response {:?}", response);

            let err = match response.classify_response(&report) {
                Ok(()) => return Ok(response),
                Err(e) => e,
            };

            // NotSupported is a definitive answer from the firmware -- the command will
            // never succeed on this device, so retrying just burns ~2.5s before failing.
            // Propagate the ERROR VALUE, not its Display string: callers (the CLI's exit
            // codes, `feature` probing) need to distinguish "this firmware lacks the
            // command" from "the bus is out of step", and `anyhow!("{}", err)` erased that.
            if err == ResponseError::NotSupported {
                return Err(anyhow::Error::new(err));
            }

            if attempt == MAX_RETRIES - 1 {
                return Err(anyhow::Error::new(err).context(format!(
                    "failed to match report after {MAX_RETRIES} attempts"
                )));
            }

            thread::sleep(if err.is_busy() {
                BUSY_BACKOFF
            } else {
                RETRY_BACKOFF
            });
        }

        Err(anyhow!("Failed to send feature report"))
    }

    pub fn enumerate() -> Result<(Vec<u16>, String)> {
        let razer_pid_list: Vec<_> = hidapi::HidApi::new()?
            .device_list()
            .filter(|info| info.vendor_id() == Device::RAZER_VID)
            .map(|info| info.product_id())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if razer_pid_list.is_empty() {
            return Err(anyhow::Error::new(DetectError::NoRazerDevices));
        }

        match read_device_model() {
            Ok(model) if model.starts_with("RZ09-") => Ok((razer_pid_list, model)),
            Ok(model) => Err(anyhow::Error::new(DetectError::NotARazerLaptop(model))),
            Err(e) => Err(anyhow::Error::new(DetectError::ModelUnreadable(
                e.to_string(),
            ))),
        }
    }

    pub fn detect() -> Result<Device> {
        let (pid_list, model_number_prefix) = Device::enumerate()?;

        if let Some(supported) = crate::matching::find_descriptor(&model_number_prefix, SUPPORTED) {
            return Device::new(supported.clone());
        }

        // Unknown SKU. The Razer laptop command set is shared across the line -- the
        // reference drivers cover 50 models through one code path -- so an unlisted
        // `RZ09-` machine is far more likely to be uncatalogued than unsupported.
        // Refusing to start made those users unreachable; instead, try each PID the
        // machine actually exposes with a generic profile. `Device::new` still probes
        // the interface before accepting it, so a PID that isn't the control interface
        // is rejected here exactly as it would be for a known model.
        log::warn!(
            "Model {} is not in the supported list; continuing with a generic profile \
             (fan range {:?}, all features offered). Controls this chassis lacks will \
             report 'not supported'. Please open an issue with this SKU so it can be added.",
            model_number_prefix,
            crate::matching::FALLBACK_FAN_RPM_RANGE
        );
        let mut last_err = None;
        for pid in &pid_list {
            match Device::new(crate::matching::fallback_descriptor(*pid)) {
                Ok(device) => return Ok(device),
                Err(e) => last_err = Some(e),
            }
        }

        Err(anyhow::Error::new(DetectError::NoControlInterface {
            model: model_number_prefix.clone(),
            pids: pid_list.clone(),
            detail: match last_err {
                Some(e) => format!(": {e}"),
                None => String::new(),
            },
        }))
    }
}

impl HidTransport for Device {
    /// Thin adapter: delegate to the inherent `Device::send` (the real hidapi
    /// exchange). Method resolution prefers the inherent method, so this is not
    /// recursive. Lets `command::*` accept any `HidTransport` while production code
    /// keeps passing a `Device` unchanged.
    fn send(&self, packet: Packet) -> Result<Packet> {
        Device::send(self, packet)
    }
}
