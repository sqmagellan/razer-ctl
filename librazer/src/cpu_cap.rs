//! The optional per-app CPU frequency cap of an Action rule.
//!
//! While the running rule sets `cpu_max_mhz`, the tray lowers two Windows power settings
//! and puts them back when the rule stops applying. This module holds the decisions: which
//! cap is wanted, what to save first, what to put back. The Windows calls sit behind
//! [`PowerPlan`], so all of it is tested on any host.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::AppProfile;

/// The lowest `cpu_max_mhz` a rule may set. Anything lower is ignored: Windows reads 0 as
/// "no limit", and a few hundred MHz would leave the laptop barely usable.
pub const CPU_CAP_MIN_MHZ: u32 = 800;

/// The baseline file's name, beside the tray config.
pub const BASELINE_FILE: &str = "cpu-cap-baseline.json";

/// A failed cap is tried again after `RETRY_FIRST`, doubling up to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);

/// The two "maximum processor frequency" settings (MHz, 0 = no limit), one per processor
/// efficiency class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreqMax {
    pub class0: u32,
    pub class1: u32,
}

impl FreqMax {
    pub fn both(mhz: u32) -> Self {
        Self {
            class0: mhz,
            class1: mhz,
        }
    }
}

/// What the settings held on one power scheme before the first cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapBaseline {
    pub scheme: String,
    pub before: FreqMax,
    /// Caps this tray wrote, or was about to write, to `scheme`. A setting holding one of
    /// them is put back; any other value was set by something else. Missing in files from
    /// earlier builds, which put `before` back as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<Vec<u32>>,
}

/// The operating system's side. `scheme` is a power scheme GUID as lowercase text.
pub trait PowerPlan {
    fn active_scheme(&self) -> Result<String>;
    /// The plugged-in values of both settings on `scheme`.
    fn read(&self, scheme: &str) -> Result<FreqMax>;
    /// Write the plugged-in values of both settings on `scheme`, then activate it if it is
    /// the active scheme, so they take effect.
    fn write(&self, scheme: &str, values: FreqMax) -> Result<()>;
}

/// The cap a rule asks for, if its value is usable.
pub fn usable_cap(rule: &AppProfile) -> Option<u32> {
    rule.cpu_max_mhz.filter(|mhz| *mhz >= CPU_CAP_MIN_MHZ)
}

/// The cap wanted right now, given the running rule (if any) and the power source.
///
/// Only the plugged-in values are written, so on battery they would do nothing: unplugging
/// puts the settings back, and a battery-only rule never caps.
pub fn wanted_cap(rule: Option<&AppProfile>, ac_power: bool) -> Option<u32> {
    if !ac_power {
        return None;
    }
    rule.filter(|r| r.allowed_on(ac_power)).and_then(usable_cap)
}

/// A scheme GUID's value, from the text [`PowerPlan`] uses. Checked here because the
/// baseline file can be edited by hand, and the `windows` crate's parser panics on bad text.
pub fn scheme_bits(scheme: &str) -> Option<u128> {
    let groups: Vec<&str> = scheme.split('-').collect();
    let lengths: Vec<usize> = groups.iter().map(|g| g.len()).collect();
    if lengths != [8, 4, 4, 4, 12] || !scheme.chars().all(|c| c == '-' || c.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&groups.concat(), 16).ok()
}

/// The cap's state across rule changes. Call [`Self::reconcile`] whenever the wanted cap may
/// have changed; it does nothing when the settings already agree.
#[derive(Debug)]
pub struct CpuCap {
    /// The baseline file. It holds `baselines` whenever that is not empty, written before
    /// each cap, so a crash can't lose what to put back.
    path: PathBuf,
    /// One entry per power scheme this tray capped, until its values are back.
    baselines: Vec<CapBaseline>,
    /// The cap written and read back, and on which scheme.
    applied: Option<(String, u32)>,
    /// A cap that failed: when to try it again and the delay after that.
    retry: Option<Retry>,
    /// A failed restore was logged; it is retried without logging each time.
    restore_failed: bool,
}

#[derive(Debug)]
struct Retry {
    scheme: Option<String>,
    mhz: u32,
    at: Instant,
    delay: Duration,
}

impl CpuCap {
    /// Start from the baseline file at `path`, if a previous run left one.
    pub fn open(path: PathBuf) -> Self {
        let baselines = load(&path);
        if !baselines.is_empty() {
            log::info!("CPU cap: found settings to put back from an earlier run: {baselines:?}");
        }
        Self {
            path,
            baselines,
            applied: None,
            retry: None,
            restore_failed: false,
        }
    }

    /// Bring the settings in line with `wanted`. Failures are logged and never returned:
    /// a cap that can't be set must not stop the rest of the rule.
    pub fn reconcile(&mut self, wanted: Option<u32>, power: &impl PowerPlan) {
        self.reconcile_at(wanted, power, Instant::now());
    }

    /// [`Self::reconcile`] at a given time, for the retry schedule.
    pub fn reconcile_at(&mut self, wanted: Option<u32>, power: &impl PowerPlan, now: Instant) {
        let Some(mhz) = wanted else {
            self.applied = None;
            self.retry = None;
            self.restore_all(power);
            return;
        };
        // The active scheme is checked every pass: the cap follows a plan switch.
        let scheme = power.active_scheme();
        let key = scheme.as_ref().ok().cloned();
        if let (Some(s), Some((applied_s, applied_mhz))) = (&key, &self.applied) {
            if s == applied_s && *applied_mhz == mhz {
                return;
            }
        }
        let first_failure = match &self.retry {
            Some(r) if r.scheme == key && r.mhz == mhz => {
                if now < r.at {
                    return;
                }
                false
            }
            _ => true,
        };
        self.applied = None;
        let result = scheme.and_then(|s| self.apply(&s, mhz, power).map(|()| s));
        match result {
            Ok(s) => {
                if self.retry.take().is_some() {
                    log::info!("CPU cap: {mhz} MHz applied after earlier failures");
                } else {
                    log::info!("CPU cap: {mhz} MHz");
                }
                self.applied = Some((s, mhz));
            }
            Err(e) => {
                let delay = match &self.retry {
                    Some(r) if !first_failure => (r.delay * 2).min(RETRY_MAX),
                    _ => RETRY_FIRST,
                };
                if first_failure {
                    log::warn!("CPU cap: {mhz} MHz not applied, will retry: {e:#}");
                }
                self.retry = Some(Retry {
                    scheme: key,
                    mhz,
                    at: now + delay,
                    delay,
                });
            }
        }
    }

    /// Before each write the file names the cap about to be written, next to what the
    /// scheme held before the first one. Later caps keep that first baseline.
    fn apply(&mut self, scheme: &str, mhz: u32, power: &impl PowerPlan) -> Result<()> {
        let i = match self.baselines.iter().position(|b| b.scheme == scheme) {
            Some(i) => i,
            None => {
                let before = power.read(scheme)?;
                self.baselines.push(CapBaseline {
                    scheme: scheme.to_string(),
                    before,
                    caps: Some(Vec::new()),
                });
                self.baselines.len() - 1
            }
        };
        let fresh = self.baselines[i].caps.as_ref().is_some_and(Vec::is_empty);
        if let Some(caps) = &mut self.baselines[i].caps {
            if !caps.contains(&mhz) {
                caps.push(mhz);
            }
        }
        if let Err(e) = save(&self.path, &self.baselines) {
            if fresh {
                // Nothing was written to this scheme, so nothing needs putting back.
                self.baselines.remove(i);
            }
            return Err(e);
        }
        write_checked(power, scheme, FreqMax::both(mhz))?;
        // Written and read back: only this cap can be ours now. A failed save here keeps
        // the longer list, which still restores correctly.
        if let Some(caps) = &mut self.baselines[i].caps {
            caps.retain(|c| *c == mhz);
            let _ = save(&self.path, &self.baselines);
        }
        Ok(())
    }

    fn restore_all(&mut self, power: &impl PowerPlan) {
        if self.baselines.is_empty() {
            return;
        }
        let mut errors = Vec::new();
        let mut done = Vec::new();
        let mut left = Vec::new();
        for b in &self.baselines {
            match restore_one(b, power) {
                Ok(target) => done.push((b.scheme.clone(), target)),
                Err(e) => {
                    errors.push(format!("{}: {e:#}", b.scheme));
                    left.push(b.clone());
                }
            }
        }
        // A file left behind would be replayed over later changes at the next start, so a
        // failed save or delete keeps every entry and is retried like a failed write. A
        // scheme already back reads as its target then, so the retry doesn't write it.
        let file = if left.is_empty() {
            delete(&self.path)
        } else if done.is_empty() {
            Ok(())
        } else {
            save(&self.path, &left)
        };
        match file {
            Ok(()) => {
                for (scheme, target) in &done {
                    log::info!("CPU cap: off on {scheme}, settings are {target:?}");
                }
                self.baselines = left;
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        if errors.is_empty() {
            self.restore_failed = false;
        } else if !self.restore_failed {
            self.restore_failed = true;
            log::warn!("CPU cap: could not finish putting settings back, will retry: {errors:?}");
        }
    }
}

/// Put one scheme back. Values this tray may have left (`caps`) go back to `before`;
/// any other value was set by something else while the rule ran, and it stands. A scheme
/// that can't be read is left alone and retried, never written blind.
fn restore_one(b: &CapBaseline, power: &impl PowerPlan) -> Result<FreqMax> {
    let now = power.read(&b.scheme)?;
    let target = match &b.caps {
        Some(caps) => keep_outside_changes(b.before, now, caps),
        // A file from before `caps` was recorded: put the baseline back as it was.
        None => b.before,
    };
    if now != target {
        write_checked(power, &b.scheme, target)?;
    }
    Ok(target)
}

/// `before`, except where `now` holds none of our `caps`.
fn keep_outside_changes(before: FreqMax, now: FreqMax, caps: &[u32]) -> FreqMax {
    let pick = |before, now| if caps.contains(&now) { before } else { now };
    FreqMax {
        class0: pick(before.class0, now.class0),
        class1: pick(before.class1, now.class1),
    }
}

fn write_checked(power: &impl PowerPlan, scheme: &str, values: FreqMax) -> Result<()> {
    power.write(scheme, values)?;
    let back = power.read(scheme)?;
    anyhow::ensure!(back == values, "wrote {values:?}, read back {back:?}");
    Ok(())
}

/// The file holds a list; a file from an earlier build holds one entry without `caps`.
#[derive(Deserialize)]
#[serde(untagged)]
enum BaselineFile {
    Many(Vec<CapBaseline>),
    One(CapBaseline),
}

/// Baselines left by an earlier run. An unusable file can't be acted on, so it is dropped.
fn load(path: &Path) -> Vec<CapBaseline> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            log::warn!("CPU cap: could not read {}: {e}", path.display());
            return Vec::new();
        }
    };
    let parsed = match serde_json::from_str::<BaselineFile>(&text) {
        Ok(BaselineFile::Many(v)) => Some(v),
        Ok(BaselineFile::One(b)) => Some(vec![b]),
        Err(_) => None,
    };
    match parsed {
        Some(v) if !v.is_empty() && v.iter().all(|b| scheme_bits(&b.scheme).is_some()) => v,
        _ => {
            log::warn!("CPU cap: deleting unusable {}", path.display());
            let _ = std::fs::remove_file(path);
            Vec::new()
        }
    }
}

fn delete(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("deleting {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Written like the config: a sibling temp file, flushed, then renamed over.
fn save(path: &Path, baselines: &[CapBaseline]) -> Result<()> {
    use std::io::Write;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(serde_json::to_string(baselines)?.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const BALANCED: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
    const OTHER: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";

    /// A power plan in memory. Each write records whether the baseline file existed then.
    struct FakePlan {
        active: RefCell<String>,
        values: RefCell<HashMap<String, FreqMax>>,
        baseline_path: PathBuf,
        file_at_write: RefCell<Vec<bool>>,
        reads: RefCell<u32>,
        writes: RefCell<Vec<(String, FreqMax)>>,
        fail_read: RefCell<bool>,
        fail_write: RefCell<bool>,
        /// Writes "succeed" but this is what stays in place, as a setting that ignores them.
        stuck: RefCell<Option<FreqMax>>,
    }

    impl FakePlan {
        fn new(path: &Path, start: FreqMax) -> Self {
            Self {
                active: RefCell::new(BALANCED.to_string()),
                values: RefCell::new(HashMap::from([
                    (BALANCED.to_string(), start),
                    (OTHER.to_string(), FreqMax::both(0)),
                ])),
                baseline_path: path.to_path_buf(),
                file_at_write: RefCell::new(Vec::new()),
                reads: RefCell::new(0),
                writes: RefCell::new(Vec::new()),
                fail_read: RefCell::new(false),
                fail_write: RefCell::new(false),
                stuck: RefCell::new(None),
            }
        }
        fn now(&self, scheme: &str) -> FreqMax {
            self.values.borrow()[scheme]
        }
        fn set(&self, scheme: &str, values: FreqMax) {
            self.values.borrow_mut().insert(scheme.to_string(), values);
        }
        fn write_count(&self) -> usize {
            self.writes.borrow().len()
        }
    }

    impl PowerPlan for FakePlan {
        fn active_scheme(&self) -> Result<String> {
            Ok(self.active.borrow().clone())
        }
        fn read(&self, scheme: &str) -> Result<FreqMax> {
            *self.reads.borrow_mut() += 1;
            anyhow::ensure!(!*self.fail_read.borrow(), "read refused");
            Ok(self.now(scheme))
        }
        fn write(&self, scheme: &str, values: FreqMax) -> Result<()> {
            self.file_at_write
                .borrow_mut()
                .push(self.baseline_path.exists());
            self.writes.borrow_mut().push((scheme.to_string(), values));
            anyhow::ensure!(!*self.fail_write.borrow(), "write refused");
            let stored = self.stuck.borrow().unwrap_or(values);
            self.set(scheme, stored);
            Ok(())
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("razer-cap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(BASELINE_FILE)
    }

    fn rule(cap: Option<u32>, require_ac: Option<bool>) -> AppProfile {
        AppProfile {
            process: "game.exe".to_string(),
            cpu_max_mhz: cap,
            require_ac,
            ..Default::default()
        }
    }

    fn saved(path: &Path) -> Vec<CapBaseline> {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn leftover(path: &Path, before: FreqMax) {
        // The single-entry form an earlier build wrote, without `caps`.
        let b = CapBaseline {
            scheme: BALANCED.to_string(),
            before,
            caps: None,
        };
        std::fs::write(path, serde_json::to_string(&b).unwrap()).unwrap();
    }

    #[test]
    fn scheme_text_is_checked() {
        assert_eq!(
            scheme_bits(BALANCED),
            Some(0x381b4222_f694_41f0_9685_ff5bb260df2e)
        );
        for bad in ["", "not a guid", "381b4222f69441f09685ff5bb260df2e"] {
            assert_eq!(scheme_bits(bad), None, "{bad}");
        }
        assert_eq!(scheme_bits("+81b4222-f694-41f0-9685-ff5bb260df2e"), None);
        assert_eq!(scheme_bits("zzzzzzzz-f694-41f0-9685-ff5bb260df2e"), None);
    }

    #[test]
    fn floor_rejects_zero_and_low_values() {
        for bad in [0, 1, 799] {
            assert_eq!(usable_cap(&rule(Some(bad), None)), None, "{bad}");
            assert_eq!(wanted_cap(Some(&rule(Some(bad), None)), true), None);
        }
        assert_eq!(usable_cap(&rule(Some(800), None)), Some(800));
        assert_eq!(usable_cap(&rule(Some(3600), None)), Some(3600));
        assert_eq!(usable_cap(&rule(None, None)), None);
    }

    #[test]
    fn wanted_needs_a_rule_with_a_cap_on_ac() {
        assert_eq!(wanted_cap(None, true), None);
        assert_eq!(wanted_cap(Some(&rule(None, None)), true), None);
        assert_eq!(wanted_cap(Some(&rule(Some(3600), None)), true), Some(3600));
        assert_eq!(
            wanted_cap(Some(&rule(Some(3600), Some(true))), true),
            Some(3600)
        );
    }

    #[test]
    fn nothing_is_wanted_on_battery() {
        // The cap is the plugged-in value only, so on battery it would do nothing.
        assert_eq!(wanted_cap(Some(&rule(Some(3600), None)), false), None);
        assert_eq!(wanted_cap(Some(&rule(Some(3600), Some(true))), false), None);
        // A battery-only rule never caps: it can only run where the setting does nothing.
        assert_eq!(
            wanted_cap(Some(&rule(Some(3600), Some(false))), false),
            None
        );
        assert_eq!(wanted_cap(Some(&rule(Some(3600), Some(false))), true), None);
    }

    #[test]
    fn no_cap_wanted_touches_nothing() {
        let path = scratch("untouched");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(None, &plan);
        cap.reconcile(None, &plan);
        assert_eq!(*plan.reads.borrow(), 0);
        assert_eq!(plan.write_count(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn rule_on_saves_the_baseline_before_writing() {
        let path = scratch("on");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(3600));
        assert_eq!(*plan.file_at_write.borrow(), vec![true]);
        // The file holds the values from before the cap.
        let saved = &saved(&path)[0];
        assert_eq!(saved.before, FreqMax::both(0));
        assert_eq!(saved.scheme, BALANCED);
    }

    #[test]
    fn an_applied_cap_is_not_rewritten_every_pass() {
        let path = scratch("steady");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path);
        cap.reconcile(Some(3600), &plan);
        let writes = plan.write_count();
        cap.reconcile(Some(3600), &plan);
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.write_count(), writes);
    }

    #[test]
    fn rule_off_restores_and_deletes_the_file() {
        let path = scratch("off");
        let before = FreqMax {
            class0: 4200,
            class1: 0,
        };
        let plan = FakePlan::new(&path, before);
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), before);
        assert!(!path.exists());
        // Nothing further happens once it is back.
        let writes = plan.write_count();
        cap.reconcile(None, &plan);
        assert_eq!(plan.write_count(), writes);
    }

    #[test]
    fn switching_capped_rules_keeps_the_original_baseline() {
        let path = scratch("switch");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        cap.reconcile(Some(4000), &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(4000));
        let saved = &saved(&path)[0];
        assert_eq!(saved.before, FreqMax::both(0));
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
    }

    #[test]
    fn unplugging_restores_and_plugging_back_in_caps_again() {
        let path = scratch("unplug");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        let game = rule(Some(3600), None);
        cap.reconcile(wanted_cap(Some(&game), true), &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(3600));
        cap.reconcile(wanted_cap(Some(&game), false), &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert!(!path.exists());
        cap.reconcile(wanted_cap(Some(&game), true), &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(3600));
    }

    #[test]
    fn restore_goes_to_the_saved_scheme_after_a_plan_change() {
        let path = scratch("scheme");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path);
        cap.reconcile(Some(3600), &plan);
        *plan.active.borrow_mut() = OTHER.to_string();
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert_eq!(plan.write_count(), 2);
        assert!(plan.writes.borrow().iter().all(|(s, _)| s == BALANCED));
    }

    #[test]
    fn leftover_baseline_is_restored_when_no_rule_wants_a_cap() {
        let path = scratch("leftover");
        leftover(&path, FreqMax::both(0));
        let plan = FakePlan::new(&path, FreqMax::both(3600));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert!(!path.exists());
    }

    #[test]
    fn leftover_restore_does_not_depend_on_what_the_settings_hold() {
        // A crash during a switch between two rules: the settings can hold either cap.
        let path = scratch("leftover-any");
        leftover(&path, FreqMax::both(0));
        let plan = FakePlan::new(&path, FreqMax::both(4000));
        let mut cap = CpuCap::open(path);
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
    }

    #[test]
    fn leftover_baseline_survives_when_a_rule_wants_a_cap() {
        let path = scratch("leftover-kept");
        leftover(&path, FreqMax::both(0));
        // The live settings still hold the old cap; they must not become the baseline.
        let plan = FakePlan::new(&path, FreqMax::both(3600));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3800), &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(3800));
        let saved = &saved(&path)[0];
        assert_eq!(saved.before, FreqMax::both(0));
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert!(!path.exists());
    }

    #[test]
    fn unreadable_leftover_is_dropped() {
        let path = scratch("garbage");
        std::fs::write(&path, "not json").unwrap();
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(None, &plan);
        assert_eq!(plan.write_count(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn leftover_with_a_bad_scheme_is_dropped() {
        let path = scratch("badscheme");
        std::fs::write(&path, r#"{"scheme":"x","before":{"class0":0,"class1":0}}"#).unwrap();
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(None, &plan);
        assert_eq!(plan.write_count(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn no_write_when_the_baseline_cannot_be_read() {
        let path = scratch("noread");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        *plan.fail_read.borrow_mut() = true;
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.write_count(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn no_write_when_the_baseline_cannot_be_saved() {
        let dir = scratch("nosave");
        // The parent "directory" is a file, so the save fails.
        std::fs::write(&dir, "file").unwrap();
        let path = dir.join(BASELINE_FILE);
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path);
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.write_count(), 0);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
    }

    #[test]
    fn a_failed_cap_is_tried_once_per_value_and_still_restored() {
        let path = scratch("failcap");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        *plan.fail_write.borrow_mut() = true;
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        cap.reconcile(Some(3600), &plan);
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.write_count(), 1);
        // The write may have half landed, so the rule's end still puts the values back.
        *plan.fail_write.borrow_mut() = false;
        plan.set(BALANCED, FreqMax::both(3600));
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_read_back_counts_as_a_failed_cap() {
        let path = scratch("readback");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        *plan.stuck.borrow_mut() = Some(FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.write_count(), 1);
        assert!(path.exists());
    }

    #[test]
    fn a_failed_restore_keeps_the_file_and_retries() {
        let path = scratch("failrestore");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        *plan.fail_write.borrow_mut() = true;
        cap.reconcile(None, &plan);
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(3600));
        assert!(path.exists());
        *plan.fail_write.borrow_mut() = false;
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert!(!path.exists());
    }

    #[test]
    fn a_file_that_cannot_be_deleted_is_retried_without_new_writes() {
        let path = scratch("nodelete");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        // A directory in its place makes the delete fail, as any user or platform.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        cap.reconcile(None, &plan);
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert_eq!(plan.write_count(), 2);
        assert!(!cap.baselines.is_empty());
        std::fs::remove_dir(&path).unwrap();
        cap.reconcile(None, &plan);
        assert_eq!(plan.write_count(), 2);
        assert!(cap.baselines.is_empty());
    }

    #[test]
    fn a_new_rule_during_a_failed_restore_keeps_the_baseline() {
        let path = scratch("failrestore-rule");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path);
        cap.reconcile(Some(3600), &plan);
        *plan.fail_write.borrow_mut() = true;
        cap.reconcile(None, &plan);
        *plan.fail_write.borrow_mut() = false;
        cap.reconcile(Some(4000), &plan);
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
    }

    #[test]
    fn a_value_changed_by_something_else_during_the_rule_is_left_alone() {
        let path = scratch("external");
        let plan = FakePlan::new(&path, FreqMax::both(4200));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        // One setting moved while the cap ran; the other still holds it.
        plan.set(
            BALANCED,
            FreqMax {
                class0: 0,
                class1: 3600,
            },
        );
        cap.reconcile(None, &plan);
        assert_eq!(
            plan.now(BALANCED),
            FreqMax {
                class0: 0,
                class1: 4200,
            }
        );
        assert!(!path.exists());
    }

    /// Another program caps the same settings for the same game and clears them to 0 when
    /// it exits. Whatever the order, nothing stays capped.
    #[test]
    fn a_second_capper_cannot_leave_the_cap_on() {
        for (we_start_first, we_stop_first) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let path = scratch("second");
            let plan = FakePlan::new(&path, FreqMax::both(0));
            let mut cap = CpuCap::open(path.clone());
            if we_start_first {
                cap.reconcile(Some(3600), &plan);
                plan.set(BALANCED, FreqMax::both(3600));
            } else {
                plan.set(BALANCED, FreqMax::both(3600));
                cap.reconcile(Some(3600), &plan);
            }
            if we_stop_first {
                cap.reconcile(None, &plan);
                plan.set(BALANCED, FreqMax::both(0));
            } else {
                plan.set(BALANCED, FreqMax::both(0));
                cap.reconcile(None, &plan);
            }
            assert_eq!(
                plan.now(BALANCED),
                FreqMax::both(0),
                "start first {we_start_first}, stop first {we_stop_first}"
            );
            assert!(!path.exists());
        }
    }

    /// Crash after another tool moved a capped setting: the next start keeps that change.
    #[test]
    fn a_crash_restore_keeps_a_change_made_by_something_else() {
        let path = scratch("crash-outside");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        plan.set(
            BALANCED,
            FreqMax {
                class0: 4200,
                class1: 3600,
            },
        );
        drop(cap); // the crash: nothing is put back
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(None, &plan);
        assert_eq!(
            plan.now(BALANCED),
            FreqMax {
                class0: 4200,
                class1: 0,
            }
        );
        assert!(!path.exists());
    }

    /// Crash in the middle of a switch from 3600 to 4000: either value is ours.
    #[test]
    fn a_crash_during_a_switch_still_restores_either_cap() {
        for left in [3600, 4000] {
            let path = scratch("crash-switch");
            let plan = FakePlan::new(&path, FreqMax::both(0));
            let mut cap = CpuCap::open(path.clone());
            cap.reconcile(Some(3600), &plan);
            *plan.fail_write.borrow_mut() = true; // the 4000 write never lands
            cap.reconcile(Some(4000), &plan);
            *plan.fail_write.borrow_mut() = false;
            plan.set(BALANCED, FreqMax::both(left));
            drop(cap);
            let mut cap = CpuCap::open(path.clone());
            cap.reconcile(None, &plan);
            assert_eq!(plan.now(BALANCED), FreqMax::both(0), "left {left}");
        }
    }

    #[test]
    fn a_restore_that_cannot_read_waits_instead_of_writing_blind() {
        let path = scratch("restore-noread");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        let writes = plan.write_count();
        *plan.fail_read.borrow_mut() = true;
        cap.reconcile(None, &plan);
        assert_eq!(plan.write_count(), writes);
        assert!(path.exists());
        *plan.fail_read.borrow_mut() = false;
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert!(!path.exists());
    }

    #[test]
    fn a_plan_switch_during_the_rule_caps_the_new_plan_and_restores_both() {
        let path = scratch("plan-switch");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        plan.set(OTHER, FreqMax::both(4500));
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(Some(3600), &plan);
        *plan.active.borrow_mut() = OTHER.to_string();
        cap.reconcile(Some(3600), &plan);
        assert_eq!(plan.now(OTHER), FreqMax::both(3600));
        assert_eq!(plan.now(BALANCED), FreqMax::both(3600));
        // A crash now still knows both plans.
        drop(cap);
        let mut cap = CpuCap::open(path.clone());
        cap.reconcile(None, &plan);
        assert_eq!(plan.now(BALANCED), FreqMax::both(0));
        assert_eq!(plan.now(OTHER), FreqMax::both(4500));
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_cap_is_retried_with_a_growing_delay() {
        use std::time::{Duration, Instant};
        let path = scratch("retry");
        let plan = FakePlan::new(&path, FreqMax::both(0));
        *plan.fail_write.borrow_mut() = true;
        let mut cap = CpuCap::open(path.clone());
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut writes_at = Vec::new();
        for s in 0..=40 {
            let before = plan.write_count();
            cap.reconcile_at(Some(3600), &plan, at(s));
            if plan.write_count() > before {
                writes_at.push(s);
            }
        }
        // Tried at once, then 5 s, 10 s and 20 s later: not every pass.
        assert_eq!(writes_at, vec![0, 5, 15, 35]);
        // The delay stops growing at 5 minutes.
        for s in (41..4000).step_by(5) {
            cap.reconcile_at(Some(3600), &plan, at(s));
        }
        let last = plan.write_count();
        cap.reconcile_at(Some(3600), &plan, at(4000 + RETRY_MAX.as_secs()));
        assert_eq!(plan.write_count(), last + 1);
        // Once it works, it stays applied.
        *plan.fail_write.borrow_mut() = false;
        cap.reconcile_at(Some(3600), &plan, at(5000 + RETRY_MAX.as_secs()));
        assert_eq!(plan.now(BALANCED), FreqMax::both(3600));
        let ok = plan.write_count();
        cap.reconcile_at(Some(3600), &plan, at(6000 + RETRY_MAX.as_secs()));
        assert_eq!(plan.write_count(), ok);
    }
}
