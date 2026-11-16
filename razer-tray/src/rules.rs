//! The "Game rules" submenu: a check mark per app rule to turn it on or off, the rule that is
//! running marked, and "Edit rules..." to open the rules by hand.
//!
//! The tray saves the config with serde, which drops every comment, so the explanation of the
//! rule format can't live in the config. It lives in [`HELP_FILE`], beside the config: the tray
//! writes a fresh copy each time "Edit rules..." is picked and opens it next to the config, so
//! it always matches the build that wrote it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::state::AppProfile;

/// Menu id prefix of a rule's check mark: `rule_toggle:<index>:<fingerprint>`.
const TOGGLE_PREFIX: &str = "rule_toggle:";

/// Menu id of "Edit rules...".
pub const EDIT_ID: &str = "edit_rules";

/// The guide "Edit rules..." opens beside the config.
pub const HELP_FILE: &str = "game-rules-help.txt";

/// The menu id of rule `index`. It carries a fingerprint of the whole rule too, so a click on
/// a menu built before a hand edit moved or changed the rules can't toggle a different rule,
/// even one for the same program.
pub fn toggle_id(index: usize, rule: &AppProfile) -> String {
    format!("{TOGGLE_PREFIX}{index}:{:016x}", fingerprint(rule))
}

fn fingerprint(rule: &AppProfile) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    toml::to_string(rule).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// Whether `id` is a rule check mark (current or stale).
pub fn is_toggle_id(id: &str) -> bool {
    id.starts_with(TOGGLE_PREFIX)
}

/// Flip `enabled` on the rule `id` names. Returns the rule's label and its new state, or
/// `None` when the id no longer names that rule (the rules changed since the menu was built).
pub fn toggle(rules: &mut [AppProfile], id: &str) -> Option<(String, bool)> {
    let (index, print) = id.strip_prefix(TOGGLE_PREFIX)?.split_once(':')?;
    let rule = rules.get_mut(index.parse::<usize>().ok()?)?;
    if format!("{:016x}", fingerprint(rule)) != print {
        return None;
    }
    rule.enabled = !rule.enabled;
    Some((rule.label().to_string(), rule.enabled))
}

/// A rule's menu text: its name, and "(running)" while it is the rule in force. An `&` is
/// doubled, since Windows menus read a single one as a keyboard shortcut marker.
pub fn menu_label(rule: &AppProfile, running: bool) -> String {
    let name = rule.label().replace('&', "&&");
    if running {
        format!("{name} (running)")
    } else {
        name
    }
}

/// Write [`HELP_FILE`] into `dir`, replacing any older copy, and return its path.
pub fn write_help(dir: &Path, config: &Path, log: &Path) -> Result<PathBuf> {
    let path = dir.join(HELP_FILE);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // CRLF: the file is for Notepad, and older Notepads show a bare LF file as one line.
    let text = help_text(config, log).replace('\n', "\r\n");
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The guide's text. `config` and `log` are the paths it names.
pub fn help_text(config: &Path, log: &Path) -> String {
    HELP_TEMPLATE
        .replace("{config_name}", &file_name(config))
        .replace("{config}", &config.display().to_string())
        .replace("{log}", &log.display().to_string())
        .replace("{examples}", EXAMPLES.trim_end())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The worked examples, as they would appear in the config. A test parses them, so they
/// can't drift from what the tray accepts.
const EXAMPLES: &str = r#"[[app_profiles]]
name = "Helldivers 2: Custom boost"
process = "helldivers2.exe"
require_ac = true
perf_mode = { Custom = ["Boost", "High"] }

[[app_profiles]]
name = "Longshot OoT: Performance mode"
process = "longshot.exe"
require_ac = true
perf_mode = "Performance"
"#;

const HELP_TEMPLATE: &str = r##"RAZER TRAY: HOW TO WRITE GAME RULES
====================================

A game rule changes the laptop's settings while a program runs, and puts them
back when the program closes.

The rules live in {config_name}, the other file that just opened:
    {config}

This file is only a guide. The tray writes a new copy of it each time you pick
"Edit rules...", so anything you change here is not kept.


TURNING A RULE ON OR OFF
------------------------
Use the check marks in the tray menu under "Game rules". You don't need to
edit anything for that. The rule that is in force right now says "(running)".


ADDING OR CHANGING A RULE
-------------------------
1. Go to the very end of {config_name}.
2. Copy one of the examples below, starting at its [[app_profiles]] line.
3. Change the values. Keep the double quotes around text.
4. Save the file (Ctrl+S).
5. The tray checks the file every 5 seconds and picks up the change. The
   rule then shows in the tray menu under "Game rules".

Each rule starts with the line [[app_profiles]]. The lines under it belong to
that rule, up to the next line that starts with [ or [[.

To remove a rule, delete its [[app_profiles]] line and the lines under it.


EXAMPLES
--------
Two rules, ready to copy:

{examples}

The first one switches to Custom (CPU boost "Boost", GPU boost "High") while
helldivers2.exe runs, only when the laptop is plugged in. The second one
switches to Performance while longshot.exe runs, also only when plugged in.


WHAT EACH LINE MEANS
--------------------
Only "process" is required. Leave a line out and the rule leaves that setting
as it is.

process = "game.exe"
    The program's file name, as the Details tab of Task Manager shows it.
    Capital letters don't matter.

processes = ["launcher.exe", "game_dx12.exe"]
    More file names for the same rule, for example a launcher, or a game
    that has two versions. Leave it out, or use [], for none.

name = "My game: Performance mode"
    The name shown in the tray menu and in the log. Without it, the menu
    shows the process name.

enabled = true
    true uses the rule. false keeps it in the file but doesn't use it. The
    check mark in the tray menu changes this line for you.

priority = 0
    When the programs of two rules run at the same time, the rule with the
    higher number wins. With equal numbers, the rule higher up in the file
    wins. Any whole number; 0 if left out.

require_ac = true
    true: only while plugged in. false: only on battery. Leave the line out
    to use the rule either way.

perf_mode = "Performance"
    The performance mode while the program runs. One of:
        "Battery"  "Silent"  "Balanced"  "Performance"  "Hyperboost"
    or Custom, which takes a CPU boost and a GPU boost, in that order:
        perf_mode = { Custom = ["Boost", "High"] }
    CPU boost: "Low"  "Medium"  "High"  "Boost"  "Overclock"
    ("Overclock" was called "Undervolt" before 0.9.8; both work.)
    GPU boost: "Low"  "Medium"  "High"
    A mode this laptop doesn't have does nothing.

fan_speed = "Auto"
    "Auto" lets the laptop pick. For a fixed speed, give RPM:
        fan_speed = { Manual = 3500 }
    Use a speed the tray's Fan menu offers.

max_fan = true
    Max fan speed. It only works together with a Custom perf_mode.

battery_care = 80
    Charge limit in percent, from 50 to 100. 100 means no limit.

logo_mode = "Static"
    The lid logo: "Off", "Static" or "Breathing".

keyboard_effect = "Spectrum"
    The keyboard lighting: "Off", "Spectrum", "Wave" or "Breathing".

keyboard_color = "FollowPerfMode"
    A keyboard color instead of an effect, on laptops whose menu offers
    keyboard colors. "FollowPerfMode" uses the color of the tray icon. For
    your own colors, give one per keyboard row, top to bottom (six rows):
        keyboard_color = { Rows = ["#ff0000", "#ff0000", "#ff0000", "#ff0000", "#ff0000", "#ff0000"] }


IF SOMETHING GOES WRONG
-----------------------
- If you save a mistake (a missing quote, a misspelled value) while the tray
  runs, it keeps the rules it had and doesn't save over the file until the
  mistake is fixed. If the tray starts with a mistake in the file, it uses
  no rules and says so at the top of its menu. Either way the log says what
  is wrong:
      {log}
- Don't keep notes in {config_name}. The tray saves that file again when
  you change a setting from its menu, and lines that start with # are lost.
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ConfigState, FanSpeed, PerfMode};
    use librazer::keyboard::KeyboardColor;
    use librazer::types::{BatteryCare, CpuBoost, GpuBoost, KeyboardEffect, LogoMode};

    fn rule(process: &str, name: Option<&str>) -> AppProfile {
        AppProfile {
            process: process.to_string(),
            name: name.map(str::to_string),
            ..Default::default()
        }
    }

    fn help() -> String {
        help_text(
            Path::new(r"C:\cfg\default-config.toml"),
            Path::new(r"C:\log\razer-tray.log"),
        )
    }

    #[test]
    fn a_check_mark_flips_its_own_rule_only() {
        let mut rules = vec![rule("a.exe", Some("A")), rule("b.exe", None)];
        let id = toggle_id(1, &rules[1]);
        assert!(is_toggle_id(&id));
        assert_eq!(toggle(&mut rules, &id), Some(("b.exe".to_string(), false)));
        assert!(rules[0].enabled);
        assert!(!rules[1].enabled);
        // The rebuilt menu carries the rule as it is now.
        let id = toggle_id(1, &rules[1]);
        assert_eq!(toggle(&mut rules, &id), Some(("b.exe".to_string(), true)));
        assert!(rules[1].enabled);
    }

    #[test]
    fn a_check_mark_from_before_the_rules_changed_does_nothing() {
        let mut rules = vec![rule("a.exe", None), rule("b.exe", None)];
        let stale = toggle_id(1, &rules[1]);
        rules.remove(0);
        rules.push(rule("c.exe", None));
        assert_eq!(toggle(&mut rules, &stale), None);
        assert!(rules.iter().all(|r| r.enabled));
        assert_eq!(toggle(&mut rules, "rule_toggle:7:a.exe"), None);
        assert_eq!(toggle(&mut rules, "rule_toggle:x:a.exe"), None);
        assert_eq!(toggle(&mut rules, "Balanced"), None);
    }

    #[test]
    fn the_running_rule_says_so() {
        let r = rule("longshot.exe", Some("Longshot OoT: Performance mode"));
        assert_eq!(
            menu_label(&r, true),
            "Longshot OoT: Performance mode (running)"
        );
        assert_eq!(menu_label(&r, false), "Longshot OoT: Performance mode");
        assert_eq!(menu_label(&rule("game.exe", None), false), "game.exe");
        assert_eq!(menu_label(&rule("x.exe", Some("R&D")), false), "R&&D");
    }

    /// Every setting a rule can hold is explained in the guide, so a new field can't ship
    /// without its line. The list comes from the struct itself, with every field set.
    #[test]
    fn the_guide_explains_every_rule_setting() {
        let full = AppProfile {
            process: "a.exe".into(),
            processes: vec!["b.exe".into()],
            name: Some("n".into()),
            enabled: true,
            priority: 1,
            require_ac: Some(true),
            perf_mode: Some(PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)),
            fan_speed: Some(FanSpeed::Manual(3500)),
            logo_mode: Some(LogoMode::Static),
            keyboard_effect: Some(KeyboardEffect::Spectrum),
            keyboard_color: Some(KeyboardColor::FollowPerfMode),
            battery_care: Some(BatteryCare::from_percent(80).unwrap()),
            max_fan: Some(true),
        };
        let toml::Value::Table(fields) = toml::Value::try_from(&full).unwrap() else {
            panic!("a rule serializes as a table");
        };
        let text = help();
        for key in fields.keys() {
            assert!(
                text.contains(&format!("\n{key} = ")),
                "the guide doesn't explain {key}"
            );
        }
    }

    #[derive(serde::Deserialize)]
    struct Rules {
        app_profiles: Vec<AppProfile>,
    }

    /// The guide says to paste an example at the very end of the config. That has to work
    /// on a config the tray saved with no rules in it, too.
    #[test]
    fn an_example_pasted_at_the_end_of_a_saved_config_with_no_rules_loads() {
        for saved in [ConfigState::default(), ConfigState::first_run()] {
            let text = toml::to_string_pretty(&saved).unwrap();
            let pasted = format!("{text}\n{EXAMPLES}");
            let back: ConfigState =
                toml::from_str(&pasted).unwrap_or_else(|e| panic!("{e}\n--- config ---\n{pasted}"));
            assert_eq!(back.app_profiles.len(), 2);
        }
    }

    /// Two rules for the same program, swapped by hand while the menu was open: the old
    /// check mark names a rule that is no longer at its place, so it does nothing.
    #[test]
    fn a_check_mark_for_a_swapped_rule_of_the_same_program_does_nothing() {
        let mut a = rule("game.exe", Some("A"));
        a.require_ac = Some(true);
        let b = rule("game.exe", Some("B"));
        let mut rules = vec![a.clone(), b.clone()];
        let stale = toggle_id(0, &rules[0]);
        rules = vec![b, a];
        assert_eq!(toggle(&mut rules, &stale), None);
        assert!(rules.iter().all(|r| r.enabled));
    }

    #[test]
    fn the_examples_are_rules_the_tray_reads() {
        let text = help();
        assert!(text.contains(EXAMPLES.trim_end()), "the guide shows them");
        let parsed: Rules = toml::from_str(EXAMPLES).unwrap();
        let [helldivers, longshot] = &parsed.app_profiles[..] else {
            panic!("two examples");
        };
        assert_eq!(helldivers.process, "helldivers2.exe");
        assert_eq!(
            helldivers.perf_mode,
            Some(PerfMode::Custom(CpuBoost::Boost, GpuBoost::High))
        );
        assert_eq!(helldivers.require_ac, Some(true));
        assert!(helldivers.enabled);
        assert_eq!(longshot.process, "longshot.exe");
        assert_eq!(longshot.perf_mode, Some(PerfMode::Performance));
        assert_eq!(longshot.label(), "Longshot OoT: Performance mode");
    }

    /// The value lines the guide shows inside its explanations parse too.
    #[test]
    fn the_guide_values_are_ones_the_tray_reads() {
        for line in [
            r#"perf_mode = { Custom = ["Boost", "High"] }"#,
            r#"fan_speed = { Manual = 3500 }"#,
            r##"keyboard_color = { Rows = ["#ff0000", "#ff0000", "#ff0000", "#ff0000", "#ff0000", "#ff0000"] }"##,
            r#"keyboard_color = "FollowPerfMode""#,
            r#"processes = ["launcher.exe", "game_dx12.exe"]"#,
        ] {
            assert!(help().contains(line), "the guide shows {line}");
            let text = format!("process = \"a.exe\"\n{line}\n");
            toml::from_str::<AppProfile>(&text).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
        for mode in ["Battery", "Silent", "Balanced", "Performance", "Hyperboost"] {
            let text = format!("process = \"a.exe\"\nperf_mode = \"{mode}\"\n");
            toml::from_str::<AppProfile>(&text).unwrap_or_else(|e| panic!("{mode}: {e}"));
        }
    }

    #[test]
    fn the_guide_names_the_files_and_is_rewritten_each_time() {
        let text = help();
        assert!(text.contains(r"C:\cfg\default-config.toml"));
        assert!(text.contains("the other file that just opened"));
        assert!(text.contains(r"C:\log\razer-tray.log"));
        for placeholder in ["{config", "{log}", "{examples}"] {
            assert!(
                !text.contains(placeholder),
                "{placeholder} left in the guide"
            );
        }

        let dir = std::env::temp_dir().join(format!("razer-tray-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = dir.join("default-config.toml");
        let log = dir.join("razer-tray.log");
        let path = write_help(&dir, &cfg, &log).unwrap();
        assert_eq!(path, dir.join(HELP_FILE));
        std::fs::write(&path, "changed by hand").unwrap();
        write_help(&dir, &cfg, &log).unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.starts_with("RAZER TRAY: HOW TO WRITE GAME RULES\r\n"));
        assert!(!back.contains("changed by hand"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
