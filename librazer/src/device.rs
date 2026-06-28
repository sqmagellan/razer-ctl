use crate::descriptor::{Descriptor, SUPPORTED};
use crate::packet::Packet;

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
        anyhow::bail!("Failed to open device {:?}", descriptor)
    }

    pub fn send(&self, report: Packet) -> Result<Packet> {
        // extra byte for report id
        let mut response_buf: Vec<u8> = vec![0x00; 1 + std::mem::size_of::<Packet>()];
        //println!("Report {:?}", report);

        const MAX_RETRIES: usize = 5;

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

            if response.ensure_matches_report(&report).is_ok() {
                return Ok(response);
            } else if attempt == MAX_RETRIES - 1 {
                return Err(anyhow!("Failed to match report after {} attempts", MAX_RETRIES));
            }

            // Add a small delay before retrying
            thread::sleep(time::Duration::from_millis(500));
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
            anyhow::bail!("No Razer devices found")
        }

        match read_device_model() {
            Ok(model) if model.starts_with("RZ09-") => Ok((razer_pid_list, model)),
            Ok(model) => anyhow::bail!("Detected model but it's not a Razer laptop: {}", model),
            Err(e) => anyhow::bail!("Failed to detect model: {}", e),
        }
    }

    pub fn detect() -> Result<Device> {
        let (pid_list, model_number_prefix) = Device::enumerate()?;

        match crate::matching::find_descriptor(&model_number_prefix, SUPPORTED) {
            Some(supported) => Device::new(supported.clone()),
            None => anyhow::bail!(
                "Model {} with PIDs {:0>4x?} is not supported",
                model_number_prefix,
                pid_list
            ),
        }
    }
}
