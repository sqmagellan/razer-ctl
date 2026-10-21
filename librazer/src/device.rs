use crate::capture::{Exchange, Outcome, Recorder, Recording};
use crate::descriptor::{Descriptor, SUPPORTED};
use crate::error::{DetectError, TransportError};
use crate::packet::{Packet, ResponseError};
use crate::retry::{self, Decision, MAX_ATTEMPTS, RETRY_BACKOFF};
use crate::transport::HidTransport;
use crate::unsupported::UnsupportedLog;

use anyhow::{anyhow, Context, Result};
use std::{thread, time};

pub struct Device {
    device: hidapi::HidDevice,
    pub info: Descriptor,
    /// Set for a model not in the supported list: see [`crate::unsupported`].
    unsupported: Option<UnsupportedLog>,
    /// Set by [`Device::recording`]: every attempt is logged for `razer-cli probe`.
    recorder: Option<Recorder>,
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

    /// Record the optional settings this laptop turns out not to have, instead of failing
    /// on them. For a model whose features nobody has checked: `detect` turns it on for the
    /// generic profile. Only `DeviceState` reads and writes and
    /// [`crate::state::unsupported_on`] consult the record, so the tray uses it and
    /// `razer-cli` (`manual` included) doesn't need it: each command there reports its own
    /// "not supported". See [`crate::unsupported`].
    pub fn learning_unsupported(mut self) -> Self {
        let known = crate::unsupported::known_lacking(self.info.pid);
        for feature in known.iter() {
            log::info!(
                "PID {:#06x} is known to ignore {} writes; it is left alone",
                self.info.pid,
                feature.name()
            );
        }
        self.unsupported = Some(UnsupportedLog::starting_with(known));
        self
    }

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
            // One interface that won't open (held exclusively, or access denied) must not
            // end the search: another interface of the same PID may be the control one.
            let device = match api.open_path(info.path()) {
                Ok(device) => device,
                Err(e) => {
                    log::debug!(
                        "PID {:#06x}: cannot open {:?}: {e}",
                        descriptor.pid,
                        info.path()
                    );
                    continue;
                }
            };
            let probe: Vec<u8> = std::iter::once(0u8)
                .chain(Into::<Vec<u8>>::into(&Packet::new(0x0084, &[0, 0])))
                .collect();
            if device.send_feature_report(&probe).is_ok() {
                return Ok(Device {
                    device,
                    info: descriptor.clone(),
                    unsupported: None,
                    recorder: None,
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
        self.exchange(report, true)
    }

    /// Log every attempt from now on, for `razer-cli probe`; see [`crate::capture`].
    pub fn recording(mut self) -> Self {
        self.recorder = Some(Recorder::default());
        self
    }

    /// One exchange that does NOT repeat a command the EC answered with `Failure`.
    ///
    /// For caller-supplied raw commands (`razer-cli ... cmd`): a Failure there may mean the
    /// command ran and reported an error, and nothing establishes that an arbitrary command is
    /// idempotent, so sending it four more times is not a safe default. Only Busy (the EC did
    /// not action it) is retried; a mismatch or a transport error is final too.
    pub fn send_once(&self, report: Packet) -> Result<Packet> {
        self.exchange(report, false)
    }

    fn exchange(&self, report: Packet, retry_failure: bool) -> Result<Packet> {
        // extra byte for report id
        let mut response_buf: Vec<u8> = vec![0x00; 1 + std::mem::size_of::<Packet>()];

        let request: Vec<u8> = [0_u8; 1] // report id
            .iter()
            .copied()
            .chain(Into::<Vec<u8>>::into(&report))
            .collect();

        // Each attempt goes into the recording, if one is on, before anything decides
        // what to do with it.
        let note = |attempt: usize, error: String| {
            if let Some(rec) = &self.recorder {
                rec.record(&report, attempt, Outcome::TransportError { error });
            }
        };

        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt == MAX_ATTEMPTS - 1;
            thread::sleep(time::Duration::from_micros(1000));

            // An OS-level failure (the HID stack busy right after resume, a handle racing
            // another process) used to abort on the first attempt and skip the retry loop
            // entirely, and it surfaced as an unclassified error (exit 1) although it is
            // exactly the retryable kind. Now it gets the same backoff as a bad response,
            // and a typed error so it classifies as a device error (exit 5).
            // For a single-shot send this is final too: a failed send does not prove the
            // EC never saw the report.
            if let Err(e) = self.device.send_feature_report(&request) {
                note(attempt, format!("send_feature_report: {e}"));
                if last || !retry_failure {
                    return Err(anyhow::Error::new(TransportError(format!(
                        "send_feature_report: {e}"
                    ))));
                }
                thread::sleep(RETRY_BACKOFF);
                continue;
            }

            thread::sleep(time::Duration::from_micros(2000));

            // For a single-shot send, a failed read-back is final: the EC may already have
            // run the command, and re-sending an unknown command is exactly what send_once
            // exists to avoid.
            let final_read = last || !retry_failure;

            let response_size = match self.device.get_feature_report(&mut response_buf) {
                Ok(n) => n,
                Err(e) => {
                    note(attempt, format!("get_feature_report: {e}"));
                    if final_read {
                        return Err(anyhow::Error::new(TransportError(format!(
                            "get_feature_report: {e}"
                        ))));
                    }
                    thread::sleep(RETRY_BACKOFF);
                    continue;
                }
            };
            if response_buf.len() != response_size {
                note(
                    attempt,
                    format!("response size {response_size} != {}", response_buf.len()),
                );
                if final_read {
                    return Err(anyhow::Error::new(TransportError(format!(
                        "response size {response_size} != {}",
                        response_buf.len()
                    ))));
                }
                thread::sleep(RETRY_BACKOFF);
                continue;
            }

            // skip report id byte
            let response = <&[u8] as TryInto<Packet>>::try_into(&response_buf[1..])?;
            if let Some(rec) = &self.recorder {
                rec.record_response(&report, &response, attempt, None);
            }

            let err = match response.classify_response(&report) {
                Ok(()) => return Ok(response),
                Err(e) => e,
            };
            // A mismatch alone doesn't say which field was off; the header does.
            let with_header = |e: ResponseError| {
                let error = anyhow::Error::new(e);
                if e == ResponseError::Mismatch {
                    error.context(format!(
                        "sent {}, got {}",
                        report.header(),
                        response.header()
                    ))
                } else {
                    error
                }
            };

            // The policy (shared with the simulator) is in `retry::after_rejection`.
            // Propagate the ERROR VALUE, not its Display string: callers (the CLI's exit
            // codes, `feature` probing) need to distinguish "this firmware lacks the
            // command" from "the bus is out of step", and `anyhow!("{}", err)` erased that.
            match retry::after_rejection(err, retry_failure, attempt) {
                Decision::Fail => return Err(with_header(err)),
                Decision::Exhausted => {
                    return Err(with_header(err).context(format!(
                        "failed to match report after {MAX_ATTEMPTS} attempts"
                    )))
                }
                Decision::RetryAfter(backoff) => thread::sleep(backoff),
            }
        }

        Err(anyhow!("Failed to send feature report"))
    }

    pub fn enumerate() -> Result<(Vec<u16>, String)> {
        let razer_pid_list: Vec<_> = hidapi::HidApi::new()?
            .device_list()
            .filter(|info| info.vendor_id() == Device::RAZER_VID)
            .map(|info| info.product_id())
            .collect::<std::collections::BTreeSet<_>>()
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
             (fan range {:?}, all features offered). Logo, keyboard brightness or battery \
             care that this chassis lacks is left alone once it reports 'not supported'; \
             other missing controls report it as an error. Please open an issue with this SKU so it can be added.",
            model_number_prefix,
            crate::matching::FALLBACK_FAN_RPM_RANGE
        );
        let mut last_err = None;
        // Every Razer device on the machine is in `pid_list`, an attached mouse included,
        // and the probe in `Device::new` accepts one of those too. See
        // `fallback_pid_order` for why the order matters.
        for pid in crate::matching::fallback_pid_order(&pid_list) {
            match Device::new(crate::matching::fallback_descriptor(pid)) {
                Ok(device) => {
                    log::info!(
                        "Using PID {pid:#06x} of {pid_list:#06x?} for model {model_number_prefix}"
                    );
                    return Ok(device.learning_unsupported());
                }
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

    fn send_once(&self, packet: Packet) -> Result<Packet> {
        Device::send_once(self, packet)
    }

    fn unsupported(&self) -> Option<&UnsupportedLog> {
        self.unsupported.as_ref()
    }
}

impl Recording for Device {
    fn drain_exchanges(&self) -> Vec<Exchange> {
        self.recorder
            .as_ref()
            .map(Recorder::drain)
            .unwrap_or_default()
    }
}
