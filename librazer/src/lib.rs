// The hardware-facing modules depend on hidapi/winreg, which are only available
// on Windows and Linux. Gate them out elsewhere (e.g. a macOS dev host) so the
// pure protocol/logic modules below can still be compiled and unit-tested natively.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod command;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod device;

pub mod feature;
pub mod matching;
pub mod types;

pub mod descriptor;
mod packet;
