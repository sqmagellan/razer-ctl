//! "Rules window...": two plain Win32 dialogs for the game rules, a list (check box per
//! rule, Add, Edit, Delete) and a rule editor. No UI framework: dialog templates built in
//! memory, the stock ListView, combo box, edit and button controls, and comctl32 v6 for
//! the current Windows look (razer-tray's manifest, see build.rs).
//!
//! The window runs on its own thread, so the tray's loop keeps applying rules while it is
//! open. It never writes the config. It sends the whole new rule list to the main loop,
//! which saves it the way every menu change is saved (`ProgramState::persist`: atomic, and
//! never over a hand edit), and publishes the rules back through [`Shared`]. A change made
//! from a list older than the tray's (a hand edit landed meanwhile) is dropped, and the
//! window shows the newer list.

// Off Windows only the shared types and the template builder are compiled (for tests).
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::state::{AppProfile, PerfMode};

/// Menu id of "Rules window...".
pub const OPEN_ID: &str = "rules_window";

/// What the main loop publishes for the window.
#[derive(Debug, Default)]
pub struct Shared {
    /// Bumped each time `rules` changes.
    pub generation: u64,
    pub rules: Vec<AppProfile>,
    /// The rule in force now, an index into `rules`.
    pub running: Option<usize>,
    /// The saved profile's perf mode, shown while no rule runs.
    pub idle: Option<PerfMode>,
    /// The fastest manual fan speed, for "Fans: Max".
    pub fan_max: u16,
    /// The open window, if any (an `HWND` as an integer).
    pub window: Option<isize>,
    /// A window is being opened: its thread runs, its `HWND` isn't known yet.
    pub opening: bool,
}

impl Shared {
    /// Publish the tray's current rules. Returns whether they changed.
    pub fn publish(
        &mut self,
        rules: &[AppProfile],
        running: Option<usize>,
        idle: PerfMode,
        fan_max: u16,
    ) -> bool {
        self.running = running;
        self.idle = Some(idle);
        self.fan_max = fan_max;
        if self.rules != rules {
            self.rules = rules.to_vec();
            self.generation += 1;
            true
        } else {
            false
        }
    }
}

/// A new rule list from the window, made from the list of `based_on`.
#[derive(Debug)]
pub struct Change {
    pub based_on: u64,
    pub rules: Vec<AppProfile>,
}

/// Everything the window needs from the tray.
#[derive(Clone)]
pub struct Link {
    pub shared: Arc<Mutex<Shared>>,
    pub changes: Sender<Change>,
    /// Wakes the tray's loop so a change is saved at once.
    pub wake: Arc<dyn Fn() + Send + Sync>,
}

impl Link {
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Open the window, or bring it to the front if it is open.
#[cfg(target_os = "windows")]
pub fn open(link: Link) {
    win::open(link)
}

#[cfg(not(target_os = "windows"))]
pub fn open(_link: Link) {
    log::info!("the rules window is Windows-only; use \"Edit rules...\"");
}

/// A dialog template built in memory (`DLGTEMPLATEEX`), in dialog units, so Windows
/// scales it with the dialog font.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod template {
    pub const BUTTON: u16 = 0x0080;
    pub const EDIT: u16 = 0x0081;
    pub const STATIC: u16 = 0x0082;
    pub const COMBOBOX: u16 = 0x0085;

    pub enum Class {
        Atom(u16),
        Name(&'static str),
    }

    pub struct Template {
        words: Vec<u16>,
        count: u16,
    }

    fn push_u32(w: &mut Vec<u16>, v: u32) {
        w.push(v as u16);
        w.push((v >> 16) as u16);
    }

    fn push_str(w: &mut Vec<u16>, s: &str) {
        w.extend(s.encode_utf16());
        w.push(0);
    }

    fn align(w: &mut Vec<u16>) {
        if w.len() % 2 == 1 {
            w.push(0);
        }
    }

    impl Template {
        pub fn new(title: &str, style: u32, ex_style: u32, cx: i16, cy: i16) -> Self {
            let mut w = Vec::new();
            w.push(1); // dlgVer
            w.push(0xffff); // signature: DLGTEMPLATEEX
            push_u32(&mut w, 0); // helpID
            push_u32(&mut w, ex_style);
            push_u32(&mut w, style);
            w.push(0); // cDlgItems, filled in by `bytes`
            w.extend([0, 0, cx as u16, cy as u16]);
            w.push(0); // no menu
            w.push(0); // the standard dialog class
            push_str(&mut w, title);
            // DS_SETFONT: 9 pt, normal weight, not italic, DEFAULT_CHARSET.
            w.extend([9, 400, 0x0100]);
            push_str(&mut w, "MS Shell Dlg 2");
            Self { words: w, count: 0 }
        }

        #[allow(clippy::too_many_arguments)]
        pub fn item(
            &mut self,
            class: Class,
            text: &str,
            id: u32,
            style: u32,
            ex_style: u32,
            (x, y, cx, cy): (i16, i16, i16, i16),
        ) -> &mut Self {
            let w = &mut self.words;
            align(w);
            push_u32(w, 0); // helpID
            push_u32(w, ex_style);
            push_u32(w, style);
            w.extend([x as u16, y as u16, cx as u16, cy as u16]);
            push_u32(w, id);
            match class {
                Class::Atom(atom) => w.extend([0xffff, atom]),
                Class::Name(name) => push_str(w, name),
            }
            push_str(w, text);
            w.push(0); // no creation data
            self.count += 1;
            self
        }

        /// The template, in a buffer aligned for `DialogBoxIndirectParamW`.
        pub fn finish(&self) -> Vec<u32> {
            let mut words = self.words.clone();
            words[8] = self.count;
            align(&mut words);
            words
                .chunks(2)
                .map(|c| c[0] as u32 | (c[1] as u32) << 16)
                .collect()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn items_start_on_a_dword_and_are_counted() {
            let mut t = Template::new("T", 0x80c8_0040, 0, 100, 50);
            t.item(Class::Atom(BUTTON), "OK", 1, 0x5001_0001, 0, (1, 2, 3, 4));
            t.item(
                Class::Name("SysListView32"),
                "",
                7,
                0x5081_000d,
                0,
                (5, 6, 7, 8),
            );
            let buf = t.finish();
            let words: Vec<u16> = buf
                .iter()
                .flat_map(|d| [*d as u16, (*d >> 16) as u16])
                .collect();
            assert_eq!(&words[..2], &[1, 0xffff]);
            assert_eq!(words[8], 2, "two items");
            assert_eq!(&words[9..13], &[0, 0, 100, 50]);
            // The first item's style follows its helpID and exStyle on a DWORD boundary.
            let at = words
                .windows(2)
                .position(|p| p == [0x0001, 0x5001])
                .unwrap();
            assert_eq!(at % 2, 0);
            let at = words
                .windows(2)
                .position(|p| p == [0x000d, 0x5081])
                .unwrap();
            assert_eq!(at % 2, 0);
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::template::{Class, Template, BUTTON, COMBOBOX, EDIT, STATIC};
    use super::{Change, Link};
    use crate::rules_form::{self, Fans, RuleForm, CPU_BOOSTS, GPU_BOOSTS, PERF_CHOICES};
    use crate::state::AppProfile;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };
    use windows::Win32::UI::Controls::{
        InitCommonControlsEx, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DialogBoxIndirectParamW, EndDialog, GetDlgItem, GetWindowLongPtrW, GetWindowTextLengthW,
        GetWindowTextW, KillTimer, MessageBoxW, SendMessageW, SetForegroundWindow, SetTimer,
        SetWindowLongPtrW, SetWindowTextW, ShowWindow, DLGTEMPLATE, GWLP_USERDATA, IDYES,
        MB_ICONWARNING, MB_OK, MB_YESNO, MESSAGEBOX_STYLE, SW_HIDE, SW_SHOW,
    };

    // Window styles, messages and notifications, as plain numbers (winuser.h, commctrl.h).
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_TABSTOP: u32 = 0x0001_0000;
    const WS_GROUP: u32 = 0x0002_0000;
    const WS_BORDER: u32 = 0x0080_0000;
    const WS_VSCROLL: u32 = 0x0020_0000;
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_CAPTION: u32 = 0x00c0_0000;
    const WS_SYSMENU: u32 = 0x0008_0000;
    const WS_MINIMIZEBOX: u32 = 0x0002_0000;
    const WS_EX_APPWINDOW: u32 = 0x0004_0000;
    const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
    const DS_SHELLFONT: u32 = 0x40 | 0x08;
    const DS_MODALFRAME: u32 = 0x80;
    const DS_CENTER: u32 = 0x0800;
    const BS_DEFPUSHBUTTON: u32 = 1;
    const BS_AUTOCHECKBOX: u32 = 3;
    const BS_GROUPBOX: u32 = 7;
    const ES_AUTOHSCROLL: u32 = 0x80;
    const SS_RIGHT: u32 = 2;
    const CBS_DROPDOWN: u32 = 2;
    const CBS_DROPDOWNLIST: u32 = 3;
    const CBS_AUTOHSCROLL: u32 = 0x40;
    const LVS_REPORT: u32 = 1;
    const LVS_SINGLESEL: u32 = 4;
    const LVS_SHOWSELALWAYS: u32 = 8;
    const LVS_NOSORTHEADER: u32 = 0x8000;

    const WM_INITDIALOG: u32 = 0x0110;
    const WM_COMMAND: u32 = 0x0111;
    const WM_NOTIFY: u32 = 0x004e;
    const WM_TIMER: u32 = 0x0113;
    const WM_CLOSE: u32 = 0x0010;
    const BM_GETCHECK: u32 = 0x00f0;
    const BM_SETCHECK: u32 = 0x00f1;
    const CB_ADDSTRING: u32 = 0x0143;
    const CB_GETCURSEL: u32 = 0x0147;
    const CB_SETCURSEL: u32 = 0x014e;
    const CBN_SELCHANGE: u32 = 1;
    const BN_CLICKED: u32 = 0;
    const LVM_GETITEMCOUNT: u32 = 0x1004;
    const LVM_DELETEALLITEMS: u32 = 0x1009;
    const LVM_GETNEXTITEM: u32 = 0x100c;
    const LVM_SETITEMSTATE: u32 = 0x102b;
    const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = 0x1036;
    const LVM_INSERTITEMW: u32 = 0x104d;
    const LVM_INSERTCOLUMNW: u32 = 0x1061;
    const LVM_SETITEMTEXTW: u32 = 0x1074;
    const LVM_SETCOLUMNWIDTH: u32 = 0x101e;
    const LVSCW_AUTOSIZE_USEHEADER: isize = -2;
    const LVS_EX_CHECKBOXES: isize = 0x4;
    const LVS_EX_FULLROWSELECT: isize = 0x20;
    const LVS_EX_DOUBLEBUFFER: isize = 0x0001_0000;
    const LVCF_WIDTH: u32 = 2;
    const LVCF_TEXT: u32 = 4;
    const LVIF_TEXT: u32 = 1;
    const LVIF_STATE: u32 = 8;
    const LVIS_FOCUSED: u32 = 1;
    const LVIS_SELECTED: u32 = 2;
    const LVIS_STATEIMAGEMASK: u32 = 0xf000;
    const LVNI_SELECTED: usize = 2;
    const LVN_ITEMCHANGED: u32 = (-101i32) as u32;
    const NM_DBLCLK: u32 = (-3i32) as u32;
    const IDOK: u32 = 1;
    const IDCANCEL: u32 = 2;

    // The list dialog's controls.
    const ID_LIST: u32 = 100;
    const ID_ADD: u32 = 101;
    const ID_EDIT: u32 = 102;
    const ID_DELETE: u32 = 103;
    const ID_STATUS: u32 = 104;
    // The rule dialog's controls.
    const ID_NAME: u32 = 200;
    const ID_APP: u32 = 201;
    const ID_BROWSE: u32 = 202;
    const ID_PERF: u32 = 210;
    const ID_CPU_LABEL: u32 = 211;
    const ID_CPU: u32 = 212;
    const ID_GPU_LABEL: u32 = 213;
    const ID_GPU: u32 = 214;
    const ID_FANS: u32 = 215;
    const ID_PLUGGED: u32 = 220;
    const ID_PRIORITY: u32 = 221;

    const TIMER: usize = 1;

    #[repr(C)]
    struct NmHdr {
        hwnd_from: isize,
        id_from: usize,
        code: u32,
    }

    #[repr(C)]
    struct NmListView {
        hdr: NmHdr,
        item: i32,
        sub_item: i32,
        new_state: u32,
        old_state: u32,
        changed: u32,
        action: [i32; 2],
        lparam: isize,
    }

    #[repr(C)]
    #[derive(Default)]
    struct LvColumn {
        mask: u32,
        fmt: i32,
        cx: i32,
        text: usize,
        text_max: i32,
        sub_item: i32,
        image: i32,
        order: i32,
        cx_min: i32,
        cx_default: i32,
        cx_ideal: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct LvItem {
        mask: u32,
        item: i32,
        sub_item: i32,
        state: u32,
        state_mask: u32,
        text: usize,
        text_max: i32,
        image: i32,
        lparam: isize,
        indent: i32,
        group_id: i32,
        columns: u32,
        pu_columns: usize,
        pi_col_fmt: usize,
        group: i32,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn item(dlg: HWND, id: u32) -> HWND {
        GetDlgItem(dlg, id as i32)
    }

    unsafe fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
        SendMessageW(hwnd, msg, WPARAM(w), LPARAM(l)).0
    }

    unsafe fn text_of(hwnd: HWND) -> String {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n])
    }

    unsafe fn set_text(hwnd: HWND, text: &str) {
        let w = wide(text);
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }

    unsafe fn message(owner: HWND, text: &str, style: MESSAGEBOX_STYLE) -> i32 {
        let t = wide(text);
        let c = wide("Game rules");
        MessageBoxW(owner, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), style).0
    }

    unsafe fn checked(hwnd: HWND) -> bool {
        send(hwnd, BM_GETCHECK, 0, 0) == 1
    }

    unsafe fn combo_fill(hwnd: HWND, items: &[String], select: usize) {
        for s in items {
            let w = wide(s);
            send(hwnd, CB_ADDSTRING, 0, w.as_ptr() as isize);
        }
        send(hwnd, CB_SETCURSEL, select, 0);
    }

    unsafe fn combo_pick(hwnd: HWND) -> usize {
        send(hwnd, CB_GETCURSEL, 0, 0).max(0) as usize
    }

    /// The dialog's context, as a shared reference only: the dialog procedures are re-entered
    /// (a control sends a notification while a handler runs), so state that changes lives in
    /// a `RefCell` and is borrowed briefly, never across a call that runs a message loop.
    unsafe fn ctx<T>(hwnd: HWND) -> Option<&'static T> {
        (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const T).as_ref()
    }

    fn modal(template: &Template, owner: HWND, proc_: DlgProc, param: isize) -> isize {
        let buf = template.finish();
        unsafe {
            let hinstance = GetModuleHandleW(None).unwrap_or_default();
            DialogBoxIndirectParamW(
                hinstance,
                buf.as_ptr() as *const DLGTEMPLATE,
                owner,
                Some(proc_),
                LPARAM(param),
            )
        }
    }

    type DlgProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize;

    const CHILD: u32 = WS_CHILD | WS_VISIBLE;

    // ---- the list ------------------------------------------------------------------

    struct ListCtx {
        link: Link,
        state: std::cell::RefCell<ListState>,
    }

    #[derive(Default)]
    struct ListState {
        generation: u64,
        rules: Vec<AppProfile>,
        running: Option<usize>,
        status: String,
        fan_max: u16,
    }

    pub fn open(link: Link) {
        {
            let mut shared = link.shared();
            if let Some(hwnd) = shared.window {
                unsafe {
                    let _ = SetForegroundWindow(HWND(hwnd));
                }
                return;
            }
            if shared.opening {
                return;
            }
            shared.opening = true;
        }
        let thread_link = link.clone();
        let spawned = std::thread::Builder::new()
            .name("rules-window".into())
            .spawn(move || {
                let link = thread_link;
                unsafe {
                    let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                        dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
                    });
                }
                let mut t = Template::new(
                    "Game rules",
                    WS_POPUP
                        | WS_CAPTION
                        | WS_SYSMENU
                        | WS_MINIMIZEBOX
                        | DS_SHELLFONT
                        | DS_MODALFRAME
                        | DS_CENTER,
                    WS_EX_APPWINDOW,
                    384,
                    214,
                );
                t.item(
                    Class::Atom(STATIC),
                    "Each rule switches the laptop's mode while its app runs. Untick a rule to pause it.",
                    0xffff,
                    CHILD,
                    0,
                    (7, 7, 370, 10),
                )
                .item(
                    Class::Name("SysListView32"),
                    "",
                    ID_LIST,
                    CHILD
                        | WS_TABSTOP
                        | WS_BORDER
                        | LVS_REPORT
                        | LVS_SINGLESEL
                        | LVS_SHOWSELALWAYS
                        | LVS_NOSORTHEADER,
                    0,
                    (7, 21, 306, 150),
                )
                .item(
                    Class::Atom(BUTTON),
                    "&Add\u{2026}",
                    ID_ADD,
                    CHILD | WS_TABSTOP,
                    0,
                    (320, 21, 57, 14),
                )
                .item(
                    Class::Atom(BUTTON),
                    "&Edit\u{2026}",
                    ID_EDIT,
                    CHILD | WS_TABSTOP,
                    0,
                    (320, 39, 57, 14),
                )
                .item(
                    Class::Atom(BUTTON),
                    "&Delete",
                    ID_DELETE,
                    CHILD | WS_TABSTOP,
                    0,
                    (320, 57, 57, 14),
                )
                .item(
                    Class::Atom(STATIC),
                    "",
                    ID_STATUS,
                    CHILD,
                    0,
                    (7, 176, 306, 10),
                )
                .item(
                    Class::Atom(BUTTON),
                    "Close",
                    IDCANCEL,
                    CHILD | WS_TABSTOP | BS_DEFPUSHBUTTON,
                    0,
                    (320, 193, 57, 14),
                );
                let ctx = Box::into_raw(Box::new(ListCtx {
                    link: link.clone(),
                    state: Default::default(),
                }));
                modal(&t, HWND(0), list_proc, ctx as isize);
                {
                    let mut shared = link.shared();
                    shared.window = None;
                    shared.opening = false;
                }
                // SAFETY: the dialog is closed; nothing else holds the pointer.
                drop(unsafe { Box::from_raw(ctx) });
            });
        if let Err(e) = spawned {
            log::warn!("could not open the rules window: {e}");
            link.shared().opening = false;
        }
    }

    unsafe extern "system" fn list_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
        match msg {
            WM_INITDIALOG => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, lp.0);
                let Some(c) = ctx::<ListCtx>(hwnd) else {
                    return 0;
                };
                {
                    let mut shared = c.link.shared();
                    shared.window = Some(hwnd.0);
                    shared.opening = false;
                }
                let list = item(hwnd, ID_LIST);
                let ex = LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
                send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex);
                // Column widths in pixels, from the dialog units the template uses.
                let mut rect = windows::Win32::Foundation::RECT {
                    left: 0,
                    top: 0,
                    right: 120,
                    bottom: 0,
                };
                let _ = windows::Win32::UI::WindowsAndMessaging::MapDialogRect(hwnd, &mut rect);
                let unit = rect.right as f32 / 120.0;
                for (i, (title, dlu)) in [("Rule", 140.0), ("App", 70.0), ("Power mode", 80.0)]
                    .into_iter()
                    .enumerate()
                {
                    let w = wide(title);
                    let col = LvColumn {
                        mask: LVCF_TEXT | LVCF_WIDTH,
                        cx: (dlu * unit) as i32,
                        text: w.as_ptr() as usize,
                        ..Default::default()
                    };
                    send(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
                }
                // The last column takes the rest of the list's width.
                send(list, LVM_SETCOLUMNWIDTH, 2, LVSCW_AUTOSIZE_USEHEADER);
                refresh(hwnd, c, true);
                SetTimer(hwnd, TIMER, 1000, None);
                1
            }
            WM_TIMER => {
                if let Some(c) = ctx::<ListCtx>(hwnd) {
                    refresh(hwnd, c, false);
                }
                1
            }
            WM_NOTIFY => {
                let Some(c) = ctx::<ListCtx>(hwnd) else {
                    return 0;
                };
                let hdr = &*(lp.0 as *const NmHdr);
                if hdr.id_from != ID_LIST as usize {
                    return 0;
                }
                if hdr.code == LVN_ITEMCHANGED {
                    let nm = &*(lp.0 as *const NmListView);
                    let was = (nm.old_state & LVIS_STATEIMAGEMASK) >> 12;
                    let now = (nm.new_state & LVIS_STATEIMAGEMASK) >> 12;
                    if was == 0 || now == 0 || was == now {
                        return 0;
                    }
                    // Borrowed means `refresh` is filling the list: not the user's click.
                    let Ok(st) = c.state.try_borrow() else {
                        return 0;
                    };
                    let i = nm.item as usize;
                    let Some(rule) = st.rules.get(i) else {
                        return 0;
                    };
                    let mut rules = st.rules.clone();
                    rules[i].enabled = now == 2;
                    log::info!(
                        "rules window: '{}' {}",
                        rule.label(),
                        if now == 2 { "enabled" } else { "disabled" }
                    );
                    let generation = st.generation;
                    drop(st);
                    submit(hwnd, c, generation, rules);
                } else if hdr.code == NM_DBLCLK {
                    edit_selected(hwnd, c);
                }
                0
            }
            WM_COMMAND => {
                let Some(c) = ctx::<ListCtx>(hwnd) else {
                    return 0;
                };
                let id = (wp.0 & 0xffff) as u32;
                let code = ((wp.0 >> 16) & 0xffff) as u32;
                if code != BN_CLICKED {
                    return 0;
                }
                match id {
                    ID_ADD => {
                        let Some((generation, rules, fan_max)) = snapshot(c) else {
                            return 1;
                        };
                        if let Some(rule) = while_modal(hwnd, || edit_rule(hwnd, None, fan_max)) {
                            let mut rules = rules;
                            log::info!("rules window: added '{}'", rule.label());
                            rules.push(rule);
                            submit(hwnd, c, generation, rules);
                        }
                    }
                    ID_EDIT => edit_selected(hwnd, c),
                    ID_DELETE => {
                        let (Some(i), Some((generation, rules, _))) = (selected(hwnd), snapshot(c))
                        else {
                            return 1;
                        };
                        let Some(rule) = rules.get(i).cloned() else {
                            return 1;
                        };
                        let ask = format!("Delete the rule \"{}\"?", rule.label());
                        let answer =
                            while_modal(hwnd, || message(hwnd, &ask, MB_YESNO | MB_ICONWARNING));
                        if answer == IDYES.0 {
                            let mut rules = rules;
                            log::info!("rules window: deleted '{}'", rule.label());
                            rules.remove(i);
                            submit(hwnd, c, generation, rules);
                        }
                    }
                    IDCANCEL => {
                        let _ = KillTimer(hwnd, TIMER);
                        let _ = EndDialog(hwnd, 0);
                    }
                    _ => {}
                }
                1
            }
            WM_CLOSE => {
                let _ = KillTimer(hwnd, TIMER);
                let _ = EndDialog(hwnd, 0);
                1
            }
            _ => 0,
        }
    }

    unsafe fn selected(hwnd: HWND) -> Option<usize> {
        let i = send(
            item(hwnd, ID_LIST),
            LVM_GETNEXTITEM,
            usize::MAX,
            LVNI_SELECTED as isize,
        );
        (i >= 0).then_some(i as usize)
    }

    /// The list as shown, and its generation: what a change is made from.
    fn snapshot(c: &ListCtx) -> Option<(u64, Vec<AppProfile>, u16)> {
        let st = c.state.try_borrow().ok()?;
        Some((st.generation, st.rules.clone(), st.fan_max))
    }

    /// Run a modal dialog over the list with the list's refresh stopped, so the list a
    /// change is made from can't change under it. The change still carries the generation
    /// it was made from, and the tray drops it if a hand edit landed meanwhile.
    unsafe fn while_modal<R>(hwnd: HWND, f: impl FnOnce() -> R) -> R {
        let _ = KillTimer(hwnd, TIMER);
        let r = f();
        SetTimer(hwnd, TIMER, 1000, None);
        r
    }

    unsafe fn edit_selected(hwnd: HWND, c: &ListCtx) {
        let (Some(i), Some((generation, rules, fan_max))) = (selected(hwnd), snapshot(c)) else {
            return;
        };
        let Some(old) = rules.get(i).cloned() else {
            return;
        };
        if let Some(rule) = while_modal(hwnd, || edit_rule(hwnd, Some(&old), fan_max)) {
            if rule != old {
                let mut rules = rules;
                log::info!("rules window: edited '{}'", rule.label());
                rules[i] = rule;
                submit(hwnd, c, generation, rules);
            }
        }
    }

    /// Send a new list to the tray, then wait (briefly) for it to come back as the tray's,
    /// so the next change is made from it.
    unsafe fn submit(hwnd: HWND, c: &ListCtx, based_on: u64, rules: Vec<AppProfile>) {
        let _ = c.link.changes.send(Change { based_on, rules });
        (c.link.wake)();
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(3) {
            if c.link.shared().generation != based_on {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        refresh(hwnd, c, true);
    }

    /// Show the tray's rules, when they changed or `force`. The running mark and the status
    /// line follow every second.
    unsafe fn refresh(hwnd: HWND, c: &ListCtx, force: bool) {
        let (generation, rules, running, idle, fan_max) = {
            let s = c.link.shared();
            (s.generation, s.rules.clone(), s.running, s.idle, s.fan_max)
        };
        // Held while the list is filled: the notifications that sends find it borrowed.
        let Ok(mut st) = c.state.try_borrow_mut() else {
            return;
        };
        st.fan_max = fan_max;
        let idle = idle.unwrap_or(crate::state::PerfMode::Balanced);
        let status = rules_form::status_line(&rules, running, idle);
        if status != st.status {
            set_text(item(hwnd, ID_STATUS), &status);
            st.status = status;
        }
        if !force && generation == st.generation && running == st.running {
            return;
        }
        let list = item(hwnd, ID_LIST);
        let keep = selected(hwnd);
        send(list, LVM_DELETEALLITEMS, 0, 0);
        for (i, rule) in rules.iter().enumerate() {
            let cols = rules_form::row(rule, running == Some(i));
            let w0 = wide(&cols[0]);
            let row = LvItem {
                mask: LVIF_TEXT,
                item: i as i32,
                text: w0.as_ptr() as usize,
                ..Default::default()
            };
            send(list, LVM_INSERTITEMW, 0, &row as *const _ as isize);
            for (sub, text) in cols.iter().enumerate().skip(1) {
                let w = wide(text);
                let cell = LvItem {
                    sub_item: sub as i32,
                    text: w.as_ptr() as usize,
                    ..Default::default()
                };
                send(list, LVM_SETITEMTEXTW, i, &cell as *const _ as isize);
            }
            let check = LvItem {
                mask: LVIF_STATE,
                state: (if rule.enabled { 2 } else { 1 }) << 12,
                state_mask: LVIS_STATEIMAGEMASK,
                ..Default::default()
            };
            send(list, LVM_SETITEMSTATE, i, &check as *const _ as isize);
        }
        let count = send(list, LVM_GETITEMCOUNT, 0, 0).max(0) as usize;
        if let Some(i) = keep.filter(|i| *i < count) {
            let sel = LvItem {
                mask: LVIF_STATE,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                state_mask: LVIS_SELECTED | LVIS_FOCUSED,
                ..Default::default()
            };
            send(list, LVM_SETITEMSTATE, i, &sel as *const _ as isize);
        }
        st.generation = generation;
        st.rules = rules;
        st.running = running;
    }

    // ---- one rule ------------------------------------------------------------------

    struct EditCtx {
        form: RuleForm,
        base: AppProfile,
        apps: Vec<String>,
        fans: Vec<(Fans, &'static str)>,
        fan_max: u16,
        result: std::cell::RefCell<Option<AppProfile>>,
    }

    /// The rule dialog. `None` from it: cancelled.
    fn edit_rule(owner: HWND, rule: Option<&AppProfile>, fan_max: u16) -> Option<AppProfile> {
        let base = rule.cloned().unwrap_or_default();
        let form = match rule {
            Some(r) => RuleForm::from_rule(r, fan_max),
            None => RuleForm::new_rule(),
        };
        let apps = rules_form::app_choices(
            &crate::platform::running_process_names(),
            &crate::platform::gpu_process_names(),
            &form.app,
        );
        let fans = form.fan_choices();
        let title = if rule.is_some() {
            "Edit rule"
        } else {
            "Add rule"
        };
        let mut t = Template::new(
            title,
            WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_SHELLFONT | DS_MODALFRAME | DS_CENTER,
            0,
            274,
            236,
        );
        let label = |t: &mut Template, text: &str, id: u32, y: i16| {
            t.item(
                Class::Atom(STATIC),
                text,
                id,
                CHILD | SS_RIGHT,
                0,
                (13, y + 2, 74, 8),
            );
        };
        // Rule
        t.item(
            Class::Atom(BUTTON),
            "Rule",
            0xffff,
            CHILD | BS_GROUPBOX,
            0,
            (7, 5, 260, 62),
        );
        label(&mut t, "&Name:", 0xffff, 17);
        t.item(
            Class::Atom(EDIT),
            "",
            ID_NAME,
            CHILD | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
            WS_EX_CLIENTEDGE,
            (91, 17, 170, 12),
        );
        label(&mut t, "When this &app runs:", 0xffff, 33);
        t.item(
            Class::Atom(COMBOBOX),
            "",
            ID_APP,
            CHILD | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWN | CBS_AUTOHSCROLL,
            0,
            (91, 33, 118, 140),
        )
        .item(
            Class::Atom(BUTTON),
            "&Browse\u{2026}",
            ID_BROWSE,
            CHILD | WS_TABSTOP,
            0,
            (213, 32, 48, 14),
        )
        .item(
            Class::Atom(STATIC),
            "Running apps; ones using the graphics card first.",
            0xffff,
            CHILD,
            0,
            (91, 50, 170, 8),
        );
        // While it runs
        t.item(
            Class::Atom(BUTTON),
            "While it runs",
            0xffff,
            CHILD | BS_GROUPBOX,
            0,
            (7, 72, 260, 66),
        );
        label(&mut t, "&Power mode:", 0xffff, 85);
        t.item(
            Class::Atom(COMBOBOX),
            "",
            ID_PERF,
            CHILD | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST,
            0,
            (91, 85, 100, 120),
        );
        t.item(
            Class::Atom(STATIC),
            "CPU boost:",
            ID_CPU_LABEL,
            CHILD | SS_RIGHT,
            0,
            (13, 103, 74, 8),
        )
        .item(
            Class::Atom(COMBOBOX),
            "",
            ID_CPU,
            CHILD | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST,
            0,
            (91, 101, 64, 100),
        )
        .item(
            Class::Atom(STATIC),
            "GPU boost:",
            ID_GPU_LABEL,
            CHILD | SS_RIGHT,
            0,
            (159, 103, 40, 8),
        )
        .item(
            Class::Atom(COMBOBOX),
            "",
            ID_GPU,
            CHILD | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST,
            0,
            (203, 101, 58, 100),
        );
        label(&mut t, "&Fans:", 0xffff, 117);
        t.item(
            Class::Atom(COMBOBOX),
            "",
            ID_FANS,
            CHILD | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST,
            0,
            (91, 117, 80, 100),
        );
        // When to apply
        t.item(
            Class::Atom(BUTTON),
            "When to apply",
            0xffff,
            CHILD | BS_GROUPBOX,
            0,
            (7, 143, 260, 62),
        )
        .item(
            Class::Atom(BUTTON),
            "Only when p&lugged in",
            ID_PLUGGED,
            CHILD | WS_TABSTOP | BS_AUTOCHECKBOX,
            0,
            (91, 156, 120, 10),
        );
        label(&mut t, "P&riority:", 0xffff, 172);
        t.item(
            Class::Atom(EDIT),
            "",
            ID_PRIORITY,
            CHILD | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL,
            WS_EX_CLIENTEDGE,
            (91, 172, 30, 12),
        )
        .item(
            Class::Atom(STATIC),
            "Higher wins if two rules' apps run at once.",
            0xffff,
            CHILD,
            0,
            (91, 188, 170, 8),
        )
        .item(
            Class::Atom(BUTTON),
            "OK",
            IDOK,
            CHILD | WS_TABSTOP | WS_GROUP | BS_DEFPUSHBUTTON,
            0,
            (155, 214, 54, 14),
        )
        .item(
            Class::Atom(BUTTON),
            "Cancel",
            IDCANCEL,
            CHILD | WS_TABSTOP,
            0,
            (213, 214, 54, 14),
        );
        let ctx = EditCtx {
            form,
            base,
            apps,
            fans,
            fan_max,
            result: Default::default(),
        };
        modal(&t, owner, edit_proc, &ctx as *const EditCtx as isize);
        ctx.result.into_inner()
    }

    unsafe fn show_boosts(hwnd: HWND, show: bool) {
        for id in [ID_CPU_LABEL, ID_CPU, ID_GPU_LABEL, ID_GPU] {
            let _ = ShowWindow(item(hwnd, id), if show { SW_SHOW } else { SW_HIDE });
        }
    }

    fn custom_picked(i: usize) -> bool {
        PERF_CHOICES.get(i).and_then(|c| c.0) == Some(rules_form::PerfKind::Custom)
    }

    unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
        match msg {
            WM_INITDIALOG => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, lp.0);
                let Some(c) = ctx::<EditCtx>(hwnd) else {
                    return 0;
                };
                let f = &c.form;
                set_text(item(hwnd, ID_NAME), &f.name);
                combo_fill(item(hwnd, ID_APP), &c.apps, 0);
                set_text(item(hwnd, ID_APP), &f.app);
                let perf = PERF_CHOICES.iter().position(|p| p.0 == f.perf).unwrap_or(0);
                let names: Vec<String> = PERF_CHOICES.iter().map(|p| p.1.to_string()).collect();
                combo_fill(item(hwnd, ID_PERF), &names, perf);
                let cpu: Vec<String> = CPU_BOOSTS.iter().map(|b| format!("{b:?}")).collect();
                let at = CPU_BOOSTS
                    .iter()
                    .position(|b| *b == f.cpu_boost)
                    .unwrap_or(3);
                combo_fill(item(hwnd, ID_CPU), &cpu, at);
                let gpu: Vec<String> = GPU_BOOSTS.iter().map(|b| format!("{b:?}")).collect();
                let at = GPU_BOOSTS
                    .iter()
                    .position(|b| *b == f.gpu_boost)
                    .unwrap_or(2);
                combo_fill(item(hwnd, ID_GPU), &gpu, at);
                show_boosts(hwnd, custom_picked(perf));
                let fan_names: Vec<String> = c.fans.iter().map(|f| f.1.to_string()).collect();
                let at = c.fans.iter().position(|x| x.0 == f.fans).unwrap_or(0);
                combo_fill(item(hwnd, ID_FANS), &fan_names, at);
                send(
                    item(hwnd, ID_PLUGGED),
                    BM_SETCHECK,
                    f.only_plugged_in as usize,
                    0,
                );
                set_text(item(hwnd, ID_PRIORITY), &f.priority);
                1
            }
            WM_COMMAND => {
                let Some(c) = ctx::<EditCtx>(hwnd) else {
                    return 0;
                };
                let id = (wp.0 & 0xffff) as u32;
                let code = ((wp.0 >> 16) & 0xffff) as u32;
                match (id, code) {
                    (ID_PERF, CBN_SELCHANGE) => {
                        show_boosts(hwnd, custom_picked(combo_pick(item(hwnd, ID_PERF))));
                    }
                    (ID_BROWSE, BN_CLICKED) => {
                        if let Some(exe) = browse(hwnd) {
                            set_text(item(hwnd, ID_APP), &exe);
                        }
                    }
                    (IDOK, BN_CLICKED) => {
                        let form = RuleForm {
                            name: text_of(item(hwnd, ID_NAME)),
                            app: text_of(item(hwnd, ID_APP)),
                            perf: PERF_CHOICES
                                [combo_pick(item(hwnd, ID_PERF)).min(PERF_CHOICES.len() - 1)]
                            .0,
                            cpu_boost: CPU_BOOSTS
                                [combo_pick(item(hwnd, ID_CPU)).min(CPU_BOOSTS.len() - 1)],
                            gpu_boost: GPU_BOOSTS
                                [combo_pick(item(hwnd, ID_GPU)).min(GPU_BOOSTS.len() - 1)],
                            fans: c.fans[combo_pick(item(hwnd, ID_FANS)).min(c.fans.len() - 1)].0,
                            only_plugged_in: checked(item(hwnd, ID_PLUGGED)),
                            priority: text_of(item(hwnd, ID_PRIORITY)),
                        };
                        match form.to_rule(&c.base, c.fan_max) {
                            Ok(rule) => {
                                *c.result.borrow_mut() = Some(rule);
                                let _ = EndDialog(hwnd, 1);
                            }
                            Err(why) => {
                                message(hwnd, &why, MB_OK | MB_ICONWARNING);
                            }
                        }
                    }
                    (IDCANCEL, BN_CLICKED) => {
                        let _ = EndDialog(hwnd, 0);
                    }
                    _ => return 0,
                }
                1
            }
            WM_CLOSE => {
                let _ = EndDialog(hwnd, 0);
                1
            }
            _ => 0,
        }
    }

    /// "Browse...": pick a program; the rule needs only its file name.
    unsafe fn browse(owner: HWND) -> Option<String> {
        let mut file = vec![0u16; 1024];
        let filter: Vec<u16> = "Programs (*.exe)\0*.exe\0All files\0*.*\0\0"
            .encode_utf16()
            .collect();
        let title = wide("Pick the app");
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: owner,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(file.as_mut_ptr()),
            nMaxFile: file.len() as u32,
            lpstrTitle: PCWSTR(title.as_ptr()),
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR,
            ..Default::default()
        };
        if !GetOpenFileNameW(&mut ofn).as_bool() {
            return None;
        }
        let n = file.iter().position(|c| *c == 0).unwrap_or(file.len());
        let path = String::from_utf16_lossy(&file[..n]);
        std::path::Path::new(&path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
    }
}
