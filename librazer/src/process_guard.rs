//! Guard list for the dGPU-terminate ("Close GPU apps" / CLI `taskkill`) feature.
//!
//! `nvidia-smi --query-compute-apps` lists every process holding a GPU compute
//! context. On a laptop whose dGPU also renders the desktop, that set includes
//! the Windows compositor and shell (dwm, explorer, the Start/Search/Shell
//! hosts, ...). Killing those tears down the user's whole session, so the
//! terminate feature must skip them. This is the single source of truth shared
//! by both the CLI (`taskkill`) and the tray (`gpu_taskkill`).

/// Session-critical Windows processes that must never be terminated, even if
/// nvidia-smi reports them as GPU users. Matched case-insensitively against the
/// executable name, with or without the `.exe` suffix and tolerant of a full
/// path (see [`is_protected_process`]).
pub const PROTECTED_PROCESSES: &[&str] = &[
    // Compositor + core shell.
    "dwm.exe",
    "explorer.exe",
    // Modern shell surfaces (Start, Search, action center, settings, lock, IME).
    "StartMenuExperienceHost.exe",
    "SearchHost.exe",
    "SearchApp.exe",
    "ShellExperienceHost.exe",
    "ShellHost.exe",
    "ApplicationFrameHost.exe",
    "SystemSettings.exe",
    "LockApp.exe",
    "TextInputHost.exe",
    "sihost.exe",
    "ctfmon.exe",
    "fontdrvhost.exe",
    "taskhostw.exe",
    // Cross-device / Phone-Link helpers observed holding GPU contexts.
    "PhoneExperienceHost.exe",
    "CrossDeviceResume.exe",
    // OS-critical. nvidia-smi shouldn't list these, but never kill them.
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "smss.exe",
    "svchost.exe",
    // Never terminate ourselves.
    "razer-tray.exe",
    "razer-cli.exe",
];

/// True if `name` is a session-critical process that the dGPU-terminate feature
/// must skip. `name` may be a bare executable name (`"explorer.exe"`), a stem
/// without the suffix (`"explorer"`), or a full path (`r"C:\Windows\explorer.exe"`);
/// all are matched case-insensitively against [`PROTECTED_PROCESSES`].
pub fn is_protected_process(name: &str) -> bool {
    // Reduce to the executable's base name so a full path still matches.
    let base = name.rsplit(['\\', '/']).next().unwrap_or(name);
    PROTECTED_PROCESSES.iter().any(|p| {
        p.eq_ignore_ascii_case(base)
            || p.strip_suffix(".exe")
                .is_some_and(|stem| stem.eq_ignore_ascii_case(base))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_shell_and_compositor_case_insensitively() {
        assert!(is_protected_process("explorer.exe"));
        assert!(is_protected_process("EXPLORER.EXE"));
        assert!(is_protected_process("dwm.exe"));
        assert!(is_protected_process("StartMenuExperienceHost.exe"));
        assert!(is_protected_process("searchhost.exe"));
    }

    #[test]
    fn matches_with_or_without_exe_suffix() {
        assert!(is_protected_process("explorer"));
        assert!(is_protected_process("dwm"));
    }

    #[test]
    fn matches_a_full_path() {
        assert!(is_protected_process(r"C:\Windows\explorer.exe"));
        assert!(is_protected_process("/usr/bin/dwm.exe"));
    }

    #[test]
    fn does_not_protect_ordinary_gpu_apps() {
        assert!(!is_protected_process("game.exe"));
        assert!(!is_protected_process("python.exe"));
        assert!(!is_protected_process("NVIDIA Overlay.exe"));
        // A substring of a protected name must not false-match.
        assert!(!is_protected_process("notexplorer.exe"));
    }

    #[test]
    fn never_kills_self() {
        assert!(is_protected_process("razer-tray.exe"));
        assert!(is_protected_process("razer-cli.exe"));
    }
}
