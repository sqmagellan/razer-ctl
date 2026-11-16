#![cfg_attr(not(target_os = "windows"), allow(dead_code))]
//! What the "Rules window..." dialogs show and how their fields map to a rule, apart from
//! the Win32 code (rules_window.rs), so it is tested on any host.
//!
//! The dialogs edit only the settings they show. Everything else a rule holds (lighting,
//! charge limit, more process names, a battery-only rule) is kept as it is in the file.

use librazer::types::{CpuBoost, GpuBoost};

use crate::state::{AppProfile, FanSpeed, PerfMode};

/// The "Power mode" choices, in list order. `None` is "Don't change".
pub const PERF_CHOICES: [(Option<PerfKind>, &str); 7] = [
    (None, "Don't change"),
    (Some(PerfKind::Battery), "Battery"),
    (Some(PerfKind::Silent), "Silent"),
    (Some(PerfKind::Balanced), "Balanced"),
    (Some(PerfKind::Performance), "Performance"),
    (Some(PerfKind::Hyperboost), "Hyperboost"),
    (Some(PerfKind::Custom), "Custom"),
];

/// A perf mode without Custom's boosts, which have their own lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfKind {
    Battery,
    Silent,
    Balanced,
    Performance,
    Hyperboost,
    Custom,
}

pub const CPU_BOOSTS: [CpuBoost; 5] = [
    CpuBoost::Low,
    CpuBoost::Medium,
    CpuBoost::High,
    CpuBoost::Boost,
    CpuBoost::Overclock,
];
pub const GPU_BOOSTS: [GpuBoost; 3] = [GpuBoost::Low, GpuBoost::Medium, GpuBoost::High];

/// The "Fans" choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fans {
    DontChange,
    Auto,
    /// The fastest manual speed this laptop offers.
    Max,
    /// A fan setting the dialog has no choice for (a manual speed below the top, or Max
    /// Fan), kept as the file has it.
    AsInFile,
}

/// The dialog's fields for one rule.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleForm {
    pub name: String,
    pub app: String,
    pub perf: Option<PerfKind>,
    pub cpu_boost: CpuBoost,
    pub gpu_boost: GpuBoost,
    pub fans: Fans,
    pub only_plugged_in: bool,
    pub priority: String,
}

impl RuleForm {
    /// A new rule's form: Performance while plugged in.
    pub fn new_rule() -> Self {
        Self {
            name: String::new(),
            app: String::new(),
            perf: Some(PerfKind::Performance),
            cpu_boost: CpuBoost::Boost,
            gpu_boost: GpuBoost::High,
            fans: Fans::DontChange,
            only_plugged_in: true,
            priority: "0".into(),
        }
    }

    /// The form for an existing rule. `fan_max` is the fastest manual speed of this laptop.
    pub fn from_rule(rule: &AppProfile, fan_max: u16) -> Self {
        let (perf, cpu_boost, gpu_boost) = match rule.perf_mode {
            None => (None, CpuBoost::Boost, GpuBoost::High),
            Some(PerfMode::Custom(c, g)) => (Some(PerfKind::Custom), c, g),
            Some(p) => (Some(kind_of(p)), CpuBoost::Boost, GpuBoost::High),
        };
        let fans = match (rule.fan_speed, rule.max_fan) {
            // Max Fan, on or off, is a setting the dialog doesn't show: keep it.
            (_, Some(_)) => Fans::AsInFile,
            (None, _) => Fans::DontChange,
            (Some(FanSpeed::Auto), _) => Fans::Auto,
            (Some(FanSpeed::Manual(rpm)), _) if rpm == fan_max => Fans::Max,
            (Some(FanSpeed::Manual(_)), _) => Fans::AsInFile,
        };
        Self {
            name: rule.name.clone().unwrap_or_default(),
            app: rule.process.clone(),
            perf,
            cpu_boost,
            gpu_boost,
            fans,
            only_plugged_in: rule.require_ac == Some(true),
            priority: rule.priority.to_string(),
        }
    }

    /// `base` with the form's fields written in: the rule to save. `Err` is a message for
    /// the user.
    pub fn to_rule(&self, base: &AppProfile, fan_max: u16) -> Result<AppProfile, String> {
        let app = self.app.trim();
        if app.is_empty() {
            return Err("Pick the app, or type its file name, like game.exe.".into());
        }
        let priority: i32 = self
            .priority
            .trim()
            .parse()
            .map_err(|_| "Priority must be a whole number, like 0 or 10.".to_string())?;
        let mut rule = base.clone();
        rule.process = app.to_string();
        let name = self.name.trim();
        rule.name = (!name.is_empty()).then(|| name.to_string());
        rule.perf_mode = self.perf.map(|kind| match kind {
            PerfKind::Battery => PerfMode::Battery,
            PerfKind::Silent => PerfMode::Silent,
            PerfKind::Balanced => PerfMode::Balanced,
            PerfKind::Performance => PerfMode::Performance,
            PerfKind::Hyperboost => PerfMode::Hyperboost,
            PerfKind::Custom => PerfMode::Custom(self.cpu_boost, self.gpu_boost),
        });
        match self.fans {
            Fans::DontChange => {
                rule.fan_speed = None;
                rule.max_fan = None;
            }
            Fans::Auto => {
                rule.fan_speed = Some(FanSpeed::Auto);
                rule.max_fan = None;
            }
            Fans::Max => {
                rule.fan_speed = Some(FanSpeed::Manual(fan_max));
                rule.max_fan = None;
            }
            Fans::AsInFile => {}
        }
        // Unticked clears "plugged in only"; a battery-only rule from the file stays one.
        rule.require_ac = match (self.only_plugged_in, base.require_ac) {
            (true, _) => Some(true),
            (false, Some(false)) => Some(false),
            (false, _) => None,
        };
        rule.priority = priority;
        Ok(rule)
    }

    /// The "Fans" choices this form offers: "As in the file" only when the rule has it.
    pub fn fan_choices(&self) -> Vec<(Fans, &'static str)> {
        let mut v = vec![
            (Fans::DontChange, "Don't change"),
            (Fans::Auto, "Auto"),
            (Fans::Max, "Max"),
        ];
        if self.fans == Fans::AsInFile {
            v.push((Fans::AsInFile, "As in the file"));
        }
        v
    }
}

fn kind_of(p: PerfMode) -> PerfKind {
    match p {
        PerfMode::Battery => PerfKind::Battery,
        PerfMode::Silent => PerfKind::Silent,
        PerfMode::Balanced => PerfKind::Balanced,
        PerfMode::Performance => PerfKind::Performance,
        PerfMode::Hyperboost => PerfKind::Hyperboost,
        PerfMode::Custom(..) => PerfKind::Custom,
    }
}

/// A perf mode as the window shows it: "Performance", "Custom (Boost, High)".
pub fn perf_label(p: PerfMode) -> String {
    match p {
        PerfMode::Custom(c, g) => format!("Custom ({c:?}, {g:?})"),
        other => format!("{other:?}"),
    }
}

/// The three columns of a rule's row: Rule, App, Power mode.
pub fn row(rule: &AppProfile, running: bool) -> [String; 3] {
    let mut name = rule.label().to_string();
    if running {
        name.push_str(" (running)");
    }
    let apps = rule.all_processes().collect::<Vec<_>>().join(", ");
    let power = match rule.perf_mode {
        Some(p) => perf_label(p),
        None => "Don't change".into(),
    };
    [name, apps, power]
}

/// The line under the list.
pub fn status_line(rules: &[AppProfile], running: Option<usize>, idle: PerfMode) -> String {
    match running.and_then(|i| rules.get(i)) {
        Some(rule) => format!("Running now: {}", rule.label()),
        None => format!("No rule running: {}", perf_label(idle)),
    }
}

/// The apps the "When this app runs" list offers: programs using the graphics card first,
/// then the rest, each group A to Z, one entry per file name whatever its case. `keep` (the
/// rule's own app) is always in the list. System processes are left out.
pub fn app_choices(running: &[String], gpu: &[String], keep: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut usable = |name: &str| {
        let name = name.trim();
        !name.is_empty()
            && name.to_ascii_lowercase().ends_with(".exe")
            && !librazer::process_guard::is_protected_process(name)
            && seen.insert(name.to_ascii_lowercase())
    };
    let sorted = |mut v: Vec<String>| {
        v.sort_by_key(|n| n.to_ascii_lowercase());
        v
    };
    let mut first = Vec::new();
    if !keep.trim().is_empty() && usable(keep) {
        first.push(keep.trim().to_string());
    }
    let gpu_users: Vec<String> = gpu
        .iter()
        .filter(|n| usable(n))
        .map(|n| n.trim().to_string())
        .collect();
    let rest: Vec<String> = running
        .iter()
        .filter(|n| usable(n))
        .map(|n| n.trim().to_string())
        .collect();
    first.extend(sorted(gpu_users));
    first.extend(sorted(rest));
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAN_MAX: u16 = 5000;

    fn longshot() -> AppProfile {
        AppProfile {
            name: Some("Longshot OoT: Performance mode".into()),
            process: "longshot.exe".into(),
            require_ac: Some(true),
            perf_mode: Some(PerfMode::Performance),
            ..Default::default()
        }
    }

    #[test]
    fn a_rule_round_trips_through_the_form_unchanged() {
        let helldivers = AppProfile {
            name: Some("Helldivers 2".into()),
            process: "helldivers2.exe".into(),
            processes: vec!["launcher.exe".into()],
            require_ac: Some(true),
            priority: 3,
            perf_mode: Some(PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)),
            logo_mode: Some(librazer::types::LogoMode::Breathing),
            battery_care: Some(librazer::types::BatteryCare::from_percent(80).unwrap()),
            fan_speed: Some(FanSpeed::Manual(3500)),
            ..Default::default()
        };
        for rule in [longshot(), helldivers, AppProfile::default()] {
            let mut rule = rule;
            rule.process = if rule.process.is_empty() {
                "x.exe".into()
            } else {
                rule.process
            };
            let form = RuleForm::from_rule(&rule, FAN_MAX);
            assert_eq!(form.to_rule(&rule, FAN_MAX).unwrap(), rule);
        }
    }

    #[test]
    fn the_form_maps_each_field() {
        let form = RuleForm::from_rule(&longshot(), FAN_MAX);
        assert_eq!(form.name, "Longshot OoT: Performance mode");
        assert_eq!(form.app, "longshot.exe");
        assert_eq!(form.perf, Some(PerfKind::Performance));
        assert_eq!(form.fans, Fans::DontChange);
        assert!(form.only_plugged_in);
        assert_eq!(form.priority, "0");

        let edited = RuleForm {
            name: "  ".into(),
            app: " game.exe ".into(),
            perf: Some(PerfKind::Custom),
            cpu_boost: CpuBoost::High,
            gpu_boost: GpuBoost::Medium,
            fans: Fans::Max,
            only_plugged_in: false,
            priority: "-2".into(),
        }
        .to_rule(&longshot(), FAN_MAX)
        .unwrap();
        assert_eq!(edited.name, None);
        assert_eq!(edited.process, "game.exe");
        assert_eq!(
            edited.perf_mode,
            Some(PerfMode::Custom(CpuBoost::High, GpuBoost::Medium))
        );
        assert_eq!(edited.fan_speed, Some(FanSpeed::Manual(FAN_MAX)));
        assert_eq!(edited.require_ac, None);
        assert_eq!(edited.priority, -2);
        assert!(edited.enabled);
    }

    #[test]
    fn settings_the_form_lacks_are_kept() {
        let mut rule = longshot();
        rule.require_ac = Some(false);
        rule.max_fan = Some(true);
        let mut off = longshot();
        off.max_fan = Some(false);
        let form = RuleForm::from_rule(&off, FAN_MAX);
        assert_eq!(form.fans, Fans::AsInFile);
        assert_eq!(form.to_rule(&off, FAN_MAX).unwrap(), off);
        rule.perf_mode = Some(PerfMode::Custom(CpuBoost::Low, GpuBoost::Low));
        rule.enabled = false;
        let form = RuleForm::from_rule(&rule, FAN_MAX);
        assert_eq!(form.fans, Fans::AsInFile);
        assert!(form.fan_choices().iter().any(|(f, _)| *f == Fans::AsInFile));
        assert!(!form.only_plugged_in);
        let back = form.to_rule(&rule, FAN_MAX).unwrap();
        assert_eq!(back.require_ac, Some(false), "battery-only stays");
        assert_eq!(back.max_fan, Some(true));
        assert!(
            !back.enabled,
            "the check mark is the list's, not the form's"
        );
        assert!(!RuleForm::new_rule()
            .fan_choices()
            .iter()
            .any(|(f, _)| *f == Fans::AsInFile));
    }

    #[test]
    fn a_form_without_an_app_or_with_a_bad_priority_is_refused() {
        let mut form = RuleForm::new_rule();
        assert!(form.to_rule(&AppProfile::default(), FAN_MAX).is_err());
        form.app = "game.exe".into();
        form.priority = "high".into();
        assert!(form.to_rule(&AppProfile::default(), FAN_MAX).is_err());
        form.priority = " 5 ".into();
        let rule = form.to_rule(&AppProfile::default(), FAN_MAX).unwrap();
        assert_eq!(
            (rule.priority, rule.perf_mode, rule.require_ac),
            (5, Some(PerfMode::Performance), Some(true))
        );
    }

    #[test]
    fn rows_and_status_read_plainly() {
        let rules = vec![longshot()];
        assert_eq!(
            row(&rules[0], true),
            [
                "Longshot OoT: Performance mode (running)".to_string(),
                "longshot.exe".into(),
                "Performance".into()
            ]
        );
        let custom = AppProfile {
            process: "a.exe".into(),
            processes: vec!["b.exe".into()],
            perf_mode: Some(PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)),
            ..Default::default()
        };
        assert_eq!(
            row(&custom, false),
            [
                "a.exe".to_string(),
                "a.exe, b.exe".into(),
                "Custom (Boost, High)".into()
            ]
        );
        assert_eq!(
            status_line(&rules, None, PerfMode::Balanced),
            "No rule running: Balanced"
        );
        assert_eq!(
            status_line(&rules, Some(0), PerfMode::Balanced),
            "Running now: Longshot OoT: Performance mode"
        );
    }

    #[test]
    fn apps_list_graphics_card_users_first_then_the_rest_a_to_z() {
        let running: Vec<String> = [
            "zoom.exe",
            "Wow.exe",
            "svchost.exe",
            "explorer.exe",
            "alpha.exe",
            "WOW.EXE",
            "System",
            "longshot.exe",
        ]
        .map(String::from)
        .to_vec();
        let gpu: Vec<String> = ["longshot.exe", "Wow.exe"].map(String::from).to_vec();
        assert_eq!(
            app_choices(&running, &gpu, ""),
            ["longshot.exe", "Wow.exe", "alpha.exe", "zoom.exe"]
        );
        assert_eq!(
            app_choices(&running, &gpu, "helldivers2.exe"),
            [
                "helldivers2.exe",
                "longshot.exe",
                "Wow.exe",
                "alpha.exe",
                "zoom.exe"
            ]
        );
    }
}
