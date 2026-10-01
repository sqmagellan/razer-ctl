//! What is keeping the NVIDIA dGPU awake.
//!
//! On a hybrid (Optimus) laptop the dGPU powers fully off (D3cold) when nothing holds it.
//! Any process with a D3D/CUDA context or memory on it keeps it awake, and so does anything
//! that polls it (`nvidia-smi`, monitoring tools). This module answers "who" without waking
//! the GPU itself:
//!
//! - the GPU's power state comes from the PnP manager (`platform::nvidia_gpu_asleep`);
//! - the dGPU's adapter LUID comes from its device node (`DEVPKEY_Gpu_Luid`);
//! - per-process GPU memory comes from the `GPU Process Memory` performance counters, which
//!   the Windows graphics kernel keeps itself; reading them does not talk to the GPU.
//!
//! In dGPU-only (MUX) mode the NVIDIA GPU drives the display, so it can never sleep and
//! the holder list is everything on screen; the report says so instead of listing it.

/// Processes with memory on the dGPU, grouped by executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// File name of the executable, e.g. `pythonw.exe`.
    pub exe: String,
    /// Full path when it could be read (needed to set a per-app GPU preference).
    pub path: Option<String>,
    pub pids: Vec<u32>,
    pub dedicated_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Some(true) when an Intel/AMD display adapter is present next to the NVIDIA one.
    pub hybrid: Option<bool>,
    /// Some(true) when the dGPU is in D3 (powered down).
    pub asleep: Option<bool>,
    pub holders: Vec<Holder>,
    /// Why the holder list could not be read, if it could not.
    pub error: Option<String>,
}

/// Parse a `GPU Process Memory` / `GPU Engine` counter instance name, e.g.
/// `pid_1234_luid_0x00000000_0x0000C3F5_phys_0` (engine instances carry more after `phys_0`).
/// Returns the pid and the adapter LUID as `(HighPart << 32) | LowPart`, the same layout as
/// the `DEVPKEY_Gpu_Luid` property.
pub fn parse_instance(name: &str) -> Option<(u32, u64)> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once("_luid_")?;
    let pid = pid.parse::<u32>().ok()?;
    let mut parts = rest.splitn(3, '_');
    let high = u32::from_str_radix(parts.next()?.strip_prefix("0x")?, 16).ok()?;
    let low = u32::from_str_radix(parts.next()?.strip_prefix("0x")?, 16).ok()?;
    Some((pid, (u64::from(high) << 32) | u64::from(low)))
}

/// Group per-pid usage into holders, biggest first. `name_of` maps a pid to
/// (file name, full path); pids that exited or that `skip` names are dropped.
pub fn group_holders(
    usage: &[(u32, u64)],
    name_of: impl Fn(u32) -> Option<(String, Option<String>)>,
    skip: impl Fn(u32, &str) -> bool,
) -> Vec<Holder> {
    let mut out: Vec<Holder> = Vec::new();
    for &(pid, bytes) in usage {
        if bytes == 0 {
            continue;
        }
        let Some((exe, path)) = name_of(pid) else { continue };
        if skip(pid, &exe) {
            continue;
        }
        match out.iter_mut().find(|h| h.exe.eq_ignore_ascii_case(&exe)) {
            Some(h) => {
                h.pids.push(pid);
                h.dedicated_bytes += bytes;
                if h.path.is_none() {
                    h.path = path;
                }
            }
            None => out.push(Holder { exe, path, pids: vec![pid], dedicated_bytes: bytes }),
        }
    }
    out.sort_by(|a, b| b.dedicated_bytes.cmp(&a.dedicated_bytes).then(a.exe.cmp(&b.exe)));
    out
}

/// Plain-text report for the dialog and for `--gpu-report`.
pub fn format_report(r: &Report) -> String {
    let mut s = String::new();
    match r.hybrid {
        Some(false) => {
            s.push_str(
                "dGPU-only mode: the NVIDIA GPU drives the display, so it cannot power down.\n\
                 To let it sleep, switch to Optimus/hybrid in NVIDIA Control Panel > \
                 Manage Display Mode (needs a restart), then check again.\n",
            );
            return s;
        }
        None => s.push_str("Display mode: unknown (could not list display adapters).\n"),
        Some(true) => s.push_str("Display mode: hybrid (Intel graphics drives the display).\n"),
    }
    s.push_str(match r.asleep {
        Some(true) => "dGPU: asleep (powered down).\n",
        Some(false) => "dGPU: AWAKE.\n",
        None => "dGPU: power state unknown.\n",
    });
    if let Some(e) = &r.error {
        s.push_str(&format!("Could not read GPU usage per app: {e}\n"));
        return s;
    }
    if r.holders.is_empty() {
        s.push_str(if r.asleep == Some(false) {
            "No app holds memory on it right now; it should power down within seconds \
             unless something is polling it (monitoring tools, nvidia-smi).\n"
        } else {
            "No app is using it.\n"
        });
        return s;
    }
    s.push_str("Apps keeping it awake:\n");
    for h in &r.holders {
        let n = h.pids.len();
        s.push_str(&format!(
            "  {}{}  {:.0} MB\n",
            h.exe,
            if n > 1 { format!(" x{n}") } else { String::new() },
            h.dedicated_bytes as f64 / 1_048_576.0
        ));
    }
    s.push_str(
        "\nClose them, or for games/apps that do not need the NVIDIA GPU set \"Power saving\" \
         in Windows Settings > Display > Graphics. CUDA programs (AI tools, Python) always \
         use the NVIDIA GPU and keep it awake until they exit.\n",
    );
    s
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{group_holders, parse_instance, Holder, Report};
    use windows::core::{w, GUID, PCWSTR, PWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW,
        CM_Locate_DevNodeW, CM_GETIDLIST_FILTER_CLASS, CM_GETIDLIST_FILTER_PRESENT,
        CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
    };
    use windows::Win32::Devices::Properties::{DEVPROPKEY, DEVPROPTYPE};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_MORE_DATA,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// DEVPKEY_Gpu_Luid (devpkey.h), DEVPROP_TYPE_UINT64.
    const DEVPKEY_GPU_LUID: DEVPROPKEY = DEVPROPKEY {
        fmtid: GUID::from_u128(0x60b193cb_5276_4d0f_96fc_f173abad3ec6),
        pid: 2,
    };

    /// Present display adapters as (upper-cased device ID, devinst).
    fn display_adapters() -> Vec<(String, u32)> {
        let class = w!("{4d36e968-e325-11ce-bfc1-08002be10318}");
        let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
        let mut out = Vec::new();
        // SAFETY: Configuration Manager calls with buffers sized as the API reports; the
        // list is NUL-separated wide strings ending in an empty one.
        unsafe {
            let mut len = 0u32;
            if CM_Get_Device_ID_List_SizeW(&mut len, class, flags) != CR_SUCCESS || len == 0 {
                return out;
            }
            let mut list = vec![0u16; len as usize];
            if CM_Get_Device_ID_ListW(class, &mut list, flags) != CR_SUCCESS {
                return out;
            }
            for id in list.split(|&c| c == 0).filter(|id| !id.is_empty()) {
                let mut id_z = id.to_vec();
                id_z.push(0);
                let mut devinst = 0u32;
                if CM_Locate_DevNodeW(&mut devinst, PCWSTR(id_z.as_ptr()), CM_LOCATE_DEVNODE_NORMAL)
                    == CR_SUCCESS
                {
                    out.push((String::from_utf16_lossy(id).to_ascii_uppercase(), devinst));
                }
            }
        }
        out
    }

    fn luid_of(devinst: u32) -> Option<u64> {
        let mut data = [0u8; 8];
        let mut size = data.len() as u32;
        let mut kind = DEVPROPTYPE::default();
        // SAFETY: an 8-byte buffer for a UINT64 property, size passed in and out.
        let ok = unsafe {
            CM_Get_DevNode_PropertyW(devinst, &DEVPKEY_GPU_LUID, &mut kind, Some(data.as_mut_ptr()), &mut size, 0)
        } == CR_SUCCESS;
        (ok && size == 8).then(|| u64::from_le_bytes(data))
    }

    /// (hybrid?, NVIDIA LUID)
    pub fn adapters() -> (Option<bool>, Option<u64>) {
        let list = display_adapters();
        if list.is_empty() {
            return (None, None);
        }
        let nvidia = list.iter().find(|(id, _)| id.starts_with("PCI\\VEN_10DE"));
        let other = list
            .iter()
            .any(|(id, _)| id.starts_with("PCI\\VEN_8086") || id.starts_with("PCI\\VEN_1002"));
        match nvidia {
            None => (None, None),
            Some((_, devinst)) => (Some(other), luid_of(*devinst)),
        }
    }

    /// Dedicated GPU memory per (pid, luid) from the `GPU Process Memory` counters.
    fn process_memory() -> Result<Vec<(u32, u64, u64)>, String> {
        // SAFETY: PDH handles are opened and closed here; the item buffer is sized by the
        // first call and its strings point into that same buffer.
        unsafe {
            let mut query = 0isize;
            let rc = PdhOpenQueryW(PCWSTR::null(), 0, &mut query);
            if rc != 0 {
                return Err(format!("PdhOpenQuery 0x{rc:08X}"));
            }
            let result = (|| {
                let mut counter = 0isize;
                let rc = PdhAddEnglishCounterW(query, w!("\\GPU Process Memory(*)\\Dedicated Usage"), 0, &mut counter);
                if rc != 0 {
                    return Err(format!("PdhAddEnglishCounter 0x{rc:08X}"));
                }
                let rc = PdhCollectQueryData(query);
                if rc != 0 {
                    return Err(format!("PdhCollectQueryData 0x{rc:08X}"));
                }
                let (mut bytes, mut count) = (0u32, 0u32);
                let rc = PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut bytes, &mut count, None);
                if rc != PDH_MORE_DATA {
                    return if rc == 0 { Ok(Vec::new()) } else { Err(format!("PdhGetFormattedCounterArray 0x{rc:08X}")) };
                }
                // u64-aligned backing store; the items come first, their names after.
                let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
                let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                let rc = PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut bytes, &mut count, Some(items));
                if rc != 0 {
                    return Err(format!("PdhGetFormattedCounterArray 0x{rc:08X}"));
                }
                let mut out = Vec::new();
                for i in 0..count as usize {
                    let item = *items.add(i);
                    if item.FmtValue.CStatus != 0 {
                        continue;
                    }
                    let name = item.szName.to_string().unwrap_or_default();
                    if let Some((pid, luid)) = parse_instance(&name) {
                        out.push((pid, luid, item.FmtValue.Anonymous.largeValue.max(0) as u64));
                    }
                }
                Ok(out)
            })();
            PdhCloseQuery(query);
            result
        }
    }

    fn exe_of(pid: u32) -> Option<(String, Option<String>)> {
        // SAFETY: the handle is closed on every path; the buffer is sized in and out.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(h);
            r.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let exe = path.rsplit('\\').next().unwrap_or(&path).to_string();
            Some((exe, Some(path)))
        }
    }

    pub fn holders(luid: u64) -> Result<Vec<Holder>, String> {
        let me = std::process::id();
        let usage: Vec<(u32, u64)> = process_memory()?
            .into_iter()
            .filter(|&(_, l, _)| l == luid)
            .map(|(pid, _, b)| (pid, b))
            .collect();
        Ok(group_holders(&usage, exe_of, |pid, exe| {
            pid == me || exe.eq_ignore_ascii_case("nvidia-smi.exe")
        }))
    }

    /// `all`: list holders even in dGPU-only mode (for `--gpu-report`; the dialog skips them
    /// there because every on-screen app holds the GPU).
    pub fn report(all: bool) -> Report {
        let (hybrid, luid) = adapters();
        let asleep = crate::platform::nvidia_gpu_asleep();
        let mut r = Report { hybrid, asleep, ..Default::default() };
        if hybrid == Some(false) && !all {
            return r;
        }
        match luid {
            None => r.error = Some("no NVIDIA adapter LUID".into()),
            Some(l) => match holders(l) {
                Ok(h) => r.holders = h,
                Err(e) => r.error = Some(e),
            },
        }
        r
    }

    /// For the telemetry poller: Some(true) if something other than us holds the dGPU,
    /// Some(false) if nothing does, None in dGPU-only mode or when it can't be read.
    pub fn others_hold_dgpu() -> Option<bool> {
        let (hybrid, luid) = adapters();
        if hybrid != Some(true) {
            return None;
        }
        holders(luid?).ok().map(|h| !h.is_empty())
    }
}

#[cfg(target_os = "windows")]
pub use imp::{others_hold_dgpu, report};

#[cfg(not(target_os = "windows"))]
pub fn report(_all: bool) -> Report {
    Report { error: Some("only implemented on Windows".into()), ..Default::default() }
}

#[cfg(not(target_os = "windows"))]
pub fn others_hold_dgpu() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_process_memory_and_engine_instances() {
        assert_eq!(
            parse_instance("pid_1234_luid_0x00000000_0x0000C3F5_phys_0"),
            Some((1234, 0xC3F5))
        );
        assert_eq!(
            parse_instance("pid_42_luid_0x00000001_0x0000D1A2_phys_0_eng_3_engtype_Copy"),
            Some((42, (1u64 << 32) | 0xD1A2))
        );
        assert_eq!(parse_instance("_Total"), None);
        assert_eq!(parse_instance("pid_x_luid_0x0_0x1_phys_0"), None);
        assert_eq!(parse_instance("pid_7_luid_00000000_0x1_phys_0"), None);
    }

    #[test]
    fn groups_by_exe_and_drops_skipped_and_exited() {
        let usage = [(10, 500), (11, 300), (12, 0), (13, 900), (14, 100), (99, 50)];
        let names = |pid| match pid {
            10 | 11 => Some(("pythonw.exe".to_string(), Some("C:\\py\\pythonw.exe".to_string()))),
            13 => Some(("dwm.exe".to_string(), None)),
            14 => Some(("nvidia-smi.exe".to_string(), None)),
            _ => None, // 12 has no memory, 99 exited
        };
        let h = group_holders(&usage, names, |_, exe| exe == "nvidia-smi.exe");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].exe, "dwm.exe"); // 900 bytes beats pythonw's 300 + 500
        assert_eq!(h[1].exe, "pythonw.exe");
        assert_eq!(h[1].pids, vec![10, 11]);
        assert_eq!(h[1].dedicated_bytes, 800);
        assert_eq!(h[1].path.as_deref(), Some("C:\\py\\pythonw.exe"));
    }

    #[test]
    fn dgpu_only_report_explains_instead_of_listing() {
        let r = Report { hybrid: Some(false), ..Default::default() };
        let s = format_report(&r);
        assert!(s.contains("dGPU-only mode"));
        assert!(!s.contains("Apps keeping it awake"));
    }

    #[test]
    fn hybrid_report_lists_holders_with_counts() {
        let r = Report {
            hybrid: Some(true),
            asleep: Some(false),
            holders: vec![Holder { exe: "pythonw.exe".into(), path: None, pids: vec![1, 2], dedicated_bytes: 2 * 1_048_576 }],
            error: None,
        };
        let s = format_report(&r);
        assert!(s.contains("dGPU: AWAKE"));
        assert!(s.contains("pythonw.exe x2  2 MB"));
    }
}
