// The hardware-facing modules depend on hidapi/winreg, which are only available
// on Windows and Linux. Gate them out elsewhere (e.g. a macOS dev host) so the
// pure protocol/logic modules below can still be compiled and unit-tested natively.
// The command protocol is pure logic over the `HidTransport` seam, so it builds
// (and unit-tests, via a mock transport) on any host. Only `device` -- the real
// hidapi/winreg-backed transport -- is Windows/Linux-only.
pub mod command;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod device;

pub mod feature;
pub mod matching;
pub mod process_guard;
pub mod state;
pub mod transport;
pub mod types;

pub mod descriptor;

// `packet` is public because it is unavoidably part of this crate's public API:
// `HidTransport::send` takes and returns a `Packet`, so an outside crate cannot
// implement the trait -- the seam's whole purpose -- without naming the type.
// While it was private, `impl HidTransport for MyThing` failed to compile outside
// this crate with E0603 (`module packet is private`). The wire *fields* stay
// private to the module; callers go through the constructors and accessors.
pub mod packet;
