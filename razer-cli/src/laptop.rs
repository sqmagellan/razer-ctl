//! The laptop a command talks to: its EC over HID, or, in a `--features sim` build, a
//! simulated EC ([`librazer::sim`]) answering from a model file.
//!
//! `command::*` take `&impl HidTransport`, which is `Sized`, so a `&dyn HidTransport`
//! can't stand in for either side. [`Laptop`] delegates to whichever it holds instead,
//! and carries the descriptor the handlers read (fan range, PID, name).
//!
//! The simulated side is also compiled into test builds (the dev-dependency on
//! `librazer` turns its `sim` feature on), so the handler tests run in a plain
//! `cargo test`. Only the `--sim` option needs the feature.

use anyhow::Result;
use librazer::capture::{Exchange, Recording};
use librazer::descriptor::Descriptor;
use librazer::packet::Packet;
use librazer::transport::HidTransport;
use librazer::unsupported::UnsupportedLog;

#[cfg(not(any(target_os = "windows", target_os = "linux", feature = "sim", test)))]
compile_error!(
    "razer-cli reaches the hardware on Windows and Linux only; elsewhere, build it with \
     `--features sim` and run it with `--sim MODEL.json`"
);

pub struct Laptop {
    pub info: Descriptor,
    link: Link,
}

enum Link {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    Hid(librazer::device::Device),
    /// Boxed: it is the larger by far, and a laptop is made once per run.
    #[cfg(any(test, feature = "sim"))]
    Sim(Box<librazer::sim::SimTransport>),
}

/// Run `$body` with `$t` bound to whichever link the laptop has.
macro_rules! on_link {
    ($laptop:expr, $t:ident => $body:expr) => {
        match &$laptop.link {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            Link::Hid($t) => $body,
            #[cfg(any(test, feature = "sim"))]
            Link::Sim($t) => $body,
        }
    };
}

impl Laptop {
    #[cfg(any(test, feature = "sim"))]
    pub fn sim(info: Descriptor, ec: librazer::sim::SimTransport) -> Laptop {
        Laptop {
            info,
            link: Link::Sim(Box::new(ec)),
        }
    }

    /// Log every exchange from now on, for `probe`. A simulated EC always does.
    pub fn recording(self) -> Laptop {
        let link = match self.link {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            Link::Hid(device) => Link::Hid(device.recording()),
            #[cfg(any(test, feature = "sim"))]
            sim @ Link::Sim(_) => sim,
        };
        Laptop {
            info: self.info,
            link,
        }
    }

    /// The requests a simulated EC had no observation for so far; none on hardware.
    #[cfg(any(test, feature = "sim"))]
    pub fn gaps(&self) -> Vec<librazer::sim::Unmodeled> {
        match &self.link {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            Link::Hid(_) => Vec::new(),
            Link::Sim(ec) => ec.gaps(),
        }
    }
}

impl HidTransport for Laptop {
    fn send(&self, packet: Packet) -> Result<Packet> {
        on_link!(self, t => t.send(packet))
    }

    fn send_once(&self, packet: Packet) -> Result<Packet> {
        on_link!(self, t => t.send_once(packet))
    }

    fn pause(&self, duration: std::time::Duration) {
        on_link!(self, t => t.pause(duration))
    }

    fn unsupported(&self) -> Option<&UnsupportedLog> {
        on_link!(self, t => t.unsupported())
    }
}

impl Recording for Laptop {
    fn drain_exchanges(&self) -> Vec<Exchange> {
        on_link!(self, t => t.drain_exchanges())
    }
}

/// Where laptops come from: this machine's HID devices, or a simulated EC.
pub enum Backend {
    Hardware,
    // Boxed: a model is far larger than the empty hardware variant.
    #[cfg(feature = "sim")]
    Sim(Box<Simulation>),
}

impl Backend {
    /// `Device::detect`: the laptop `auto` and `probe` talk to.
    pub fn detect(&self) -> Result<Laptop> {
        match self {
            Backend::Hardware => hardware::detect(),
            #[cfg(feature = "sim")]
            Backend::Sim(sim) => Ok(sim.detect()),
        }
    }

    /// `Device::new`: the laptop `manual --pid` talks to.
    pub fn open(&self, descriptor: Descriptor) -> Result<Laptop> {
        match self {
            Backend::Hardware => hardware::open(descriptor),
            #[cfg(feature = "sim")]
            Backend::Sim(sim) => sim.open(descriptor),
        }
    }

    /// `Device::enumerate`: the Razer PIDs and the model number (SKU).
    pub fn enumerate(&self) -> Result<(Vec<u16>, String)> {
        match self {
            Backend::Hardware => hardware::enumerate(),
            #[cfg(feature = "sim")]
            Backend::Sim(sim) => Ok(sim.enumerate()),
        }
    }

    /// Read `--sim MODEL` from the command line, before clap parses it: `auto` builds its
    /// subcommands from the detected laptop's features, so the laptop comes first.
    #[cfg(feature = "sim")]
    pub fn from_args(args: &[std::ffi::OsString]) -> Result<Backend> {
        match sim_arg(args) {
            Some(path) => {
                Simulation::load(std::path::Path::new(path?)).map(|s| Backend::Sim(Box::new(s)))
            }
            None => Ok(Backend::Hardware),
        }
    }
}

/// The first argument: the subcommand, when one leads.
#[cfg(not(feature = "sim"))]
pub fn subcommand(args: &[std::ffi::OsString]) -> Option<&std::ffi::OsStr> {
    args.get(1).map(|arg| arg.as_os_str())
}

/// The first argument that isn't `--sim MODEL`: the subcommand, when one leads.
#[cfg(feature = "sim")]
pub fn subcommand(args: &[std::ffi::OsString]) -> Option<&std::ffi::OsStr> {
    let mut rest = args.iter().skip(1);
    loop {
        let arg = rest.next()?;
        match arg.to_str() {
            Some("--sim") => {
                rest.next();
            }
            Some(a) if a.starts_with("--sim=") => {}
            _ => return Some(arg),
        }
    }
}

/// `Some` when `--sim` is given (global, so it may follow a subcommand) before any `--`:
/// its value, or why it has none. A `--sim` without a usable value must not fall back to the
/// hardware, which `auto` would open before clap gets to reject the arguments.
#[cfg(feature = "sim")]
fn sim_arg(args: &[std::ffi::OsString]) -> Option<Result<&str>> {
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        let bytes = arg.as_encoded_bytes();
        let value = match bytes {
            b"--" => return None,
            b"--sim" => rest.next().map(|value| value.to_str()),
            _ if bytes.starts_with(b"--sim=") => {
                Some(arg.to_str().map(|arg| &arg["--sim=".len()..]))
            }
            _ => continue,
        };
        return Some(match value {
            Some(Some(path)) if !path.is_empty() => Ok(path),
            Some(None) => Err(anyhow::anyhow!("--sim: the model path is not UTF-8")),
            _ => Err(anyhow::anyhow!("--sim needs a model file")),
        });
    }
    None
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
mod hardware {
    use super::{Laptop, Link};
    use anyhow::Result;
    use librazer::descriptor::Descriptor;
    use librazer::device::Device;

    fn laptop(device: Device) -> Laptop {
        Laptop {
            info: device.info.clone(),
            link: Link::Hid(device),
        }
    }

    pub fn detect() -> Result<Laptop> {
        Device::detect().map(laptop)
    }

    pub fn open(descriptor: Descriptor) -> Result<Laptop> {
        Device::new(descriptor).map(laptop)
    }

    pub fn enumerate() -> Result<(Vec<u16>, String)> {
        Device::enumerate()
    }
}

/// A host without HID access (a macOS dev build with `--features sim`).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod hardware {
    use super::Laptop;
    use anyhow::{bail, Result};
    use librazer::descriptor::Descriptor;

    const NO_HID: &str =
        "this build has no hardware access on this OS; run it with --sim MODEL.json";

    pub fn detect() -> Result<Laptop> {
        bail!(NO_HID)
    }

    pub fn open(_: Descriptor) -> Result<Laptop> {
        bail!(NO_HID)
    }

    pub fn enumerate() -> Result<(Vec<u16>, String)> {
        bail!(NO_HID)
    }
}

/// A simulated EC from a model file, found the way `Device::detect` finds hardware.
#[cfg(any(test, feature = "sim"))]
pub struct Simulation {
    model: librazer::sim::Model,
    /// The catalogued entry for the model's PID, if any.
    catalogued: Option<&'static Descriptor>,
}

#[cfg(any(test, feature = "sim"))]
impl Simulation {
    pub fn new(model: librazer::sim::Model) -> Simulation {
        // Matched by the model's SKU like `Device::detect`; a model without one falls back
        // to the first catalogued entry with its PID.
        let catalogued = librazer::sim::catalogued(&model);
        Simulation { model, catalogued }
    }

    /// Read a model file and say, on stderr, that this run is a simulation.
    #[cfg(feature = "sim")]
    pub fn load(path: &std::path::Path) -> Result<Simulation> {
        use anyhow::Context;
        let json = std::fs::read_to_string(path)
            .with_context(|| format!("reading EC model {}", path.display()))?;
        let model = librazer::sim::Model::from_json(&json)
            .with_context(|| format!("EC model {}", path.display()))?;
        let sim = Simulation::new(model);
        let profile = match sim.catalogued {
            Some(d) => format!("catalogued: {}", d.name),
            None => "generic profile".to_string(),
        };
        eprintln!(
            "razer-cli: SIMULATED EC {:?} from {} (PID {:#06x}, {profile}); no hardware is used",
            sim.model.name,
            path.display(),
            sim.model.pid
        );
        Ok(sim)
    }

    #[cfg(feature = "sim")]
    pub fn name(&self) -> &str {
        &self.model.name
    }

    fn descriptor(&self) -> Descriptor {
        match self.catalogued {
            Some(d) => d.clone(),
            None => librazer::matching::fallback_descriptor(self.model.pid),
        }
    }

    /// `Device::detect` for this PID: a catalogued model gets its descriptor; any other
    /// gets the generic profile, which learns what the EC lacks.
    pub fn detect(&self) -> Laptop {
        let ec = librazer::sim::SimTransport::new(self.model.clone());
        let ec = match self.catalogued {
            Some(_) => ec,
            None => ec.learning(),
        };
        Laptop::sim(self.descriptor(), ec)
    }

    /// `Device::new`: a PID other than the model's has no interface to open.
    pub fn open(&self, descriptor: Descriptor) -> Result<Laptop> {
        if descriptor.pid != self.model.pid {
            return Err(anyhow::Error::new(
                librazer::error::DetectError::InterfaceUnavailable {
                    pid: descriptor.pid,
                    name: descriptor.name.to_string(),
                },
            ));
        }
        let ec = librazer::sim::SimTransport::new(self.model.clone());
        Ok(Laptop::sim(descriptor, ec))
    }

    /// `Device::enumerate`: the model's PID, and a SKU detection matches the same way (the
    /// catalogued prefix, or the generic profile's bare `RZ09-`).
    pub fn enumerate(&self) -> (Vec<u16>, String) {
        let sku = match &self.model.model_number {
            Some(sku) => sku.clone(),
            None => self.descriptor().model_number_prefix.to_string(),
        };
        (vec![self.model.pid], sku)
    }
}
