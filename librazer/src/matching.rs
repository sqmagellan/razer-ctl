use crate::descriptor::Descriptor;

/// Find the supported descriptor whose `model_number_prefix` is a prefix of the
/// detected model SKU. The prefix may be a stem (e.g. "RZ09-0483"), so a single
/// entry covers every trailing variant (…0483T, …0483U, …). Returns the first
/// match in `supported` order; descriptors that keep a full 10-char SKU stay
/// variant-specific. Kept separate from `device` so it stays host-testable.
pub fn find_descriptor<'a>(model: &str, supported: &'a [Descriptor]) -> Option<&'a Descriptor> {
    supported
        .iter()
        .find(|d| model.starts_with(d.model_number_prefix))
}

/// Conservative fan bounds for a Blade we have no descriptor for.
///
/// Sourced from the union of the reference device tables (razer-laptop-control's
/// `laptops.json`, 37 models, and the Revived fork's 50): pre-2023 chassis floor at
/// 3500 RPM, 2023-and-later at 2200. Taking the *lower* floor is the safe direction
/// -- the EC clamps a too-low request up to its real minimum, so the worst case is a
/// menu entry that behaves like "minimum", whereas guessing too high would hide usable
/// quiet settings. The ceiling is the common 5000.
pub const FALLBACK_FAN_RPM_RANGE: (u16, u16) = (2200, 5000);

/// Build a best-effort descriptor for an unrecognized Razer laptop.
///
/// Why this exists: `Device::detect` used to hard-fail on any SKU missing from
/// `SUPPORTED`, so a Blade we simply hadn't catalogued was indistinguishable from an
/// unsupported machine -- the app refused to start and the only recourse was the
/// obscure `razer-cli manual --pid`. Since the command set is shared across the whole
/// Blade line (the reference drivers drive 50 models through one code path), the
/// overwhelmingly likely truth for an unknown `RZ09-` SKU is "this works, we just
/// haven't listed it".
///
/// This is deliberately *not* silent: `Device::detect` logs a warning naming the SKU
/// (once, not per PID attempted) so an unrecognized model is reported rather than
/// mistaken for a tested one. Features are the full set -- an unsupported command
/// answers `NotSupported` (status 0x05) and now fails fast rather than retrying, so an
/// over-broad feature list degrades to a clean error on the one control that isn't
/// there instead of blocking every control that is.
///
/// `pid` is whatever the USB enumeration actually found, so the HID open path is
/// unaffected; only the *metadata* is guessed.
pub fn fallback_descriptor(pid: u16) -> Descriptor {
    Descriptor {
        // Only used for display; matching has already happened by this point.
        model_number_prefix: "RZ09-",
        name: "Unrecognized Razer laptop (untested)",
        pid,
        features: crate::feature::ALL_FEATURES,
        // Never guess an init sequence: the known ones are model-specific magic
        // (the 2025 Blade 16's 0x0081/0x0086/0x0f90/...), and replaying another
        // model's startup writes at an unknown EC is the one genuinely risky thing
        // we could do here. An empty list is always safe.
        init_cmds: &[],
        // Prefer this chassis's real fan envelope if the PID is one the community
        // tables know, since the outliers are wide (Blade Pro 17 caps at 4300; the
        // Blade 14 2025 reaches 5600) and a flat default would be wrong for both.
        fan_rpm_range: crate::descriptor::fan_range_for_pid(pid).unwrap_or(FALLBACK_FAN_RPM_RANGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(prefix: &'static str, pid: u16) -> Descriptor {
        Descriptor {
            model_number_prefix: prefix,
            name: "test",
            pid,
            features: &[],
            init_cmds: &[],
            fan_rpm_range: (2200, 5000),
        }
    }

    #[test]
    fn stem_matches_all_trailing_variants() {
        let supported = [desc("RZ09-0483", 0x029f)];
        // Real Razer SKUs extend past the stem; every variant maps to the one entry.
        assert!(find_descriptor("RZ09-0483T9R3", &supported).is_some());
        assert!(find_descriptor("RZ09-0483U9R3", &supported).is_some());
        // The 10-char form that read_device_model returns also matches.
        assert!(find_descriptor("RZ09-0483U", &supported).is_some());
    }

    #[test]
    fn different_model_base_does_not_match() {
        let supported = [desc("RZ09-0483", 0x029f)];
        assert!(find_descriptor("RZ09-0510S3", &supported).is_none()); // Blade 16 2024
        assert!(find_descriptor("RZ09-0482X9", &supported).is_none()); // Blade 14 2023
    }

    #[test]
    fn full_sku_prefix_stays_variant_specific() {
        let supported = [desc("RZ09-0483U", 0x029f)];
        assert!(find_descriptor("RZ09-0483U9R3", &supported).is_some());
        assert!(find_descriptor("RZ09-0483T9R3", &supported).is_none());
    }

    #[test]
    fn first_match_wins_in_declaration_order() {
        let supported = [desc("RZ09-0483", 1), desc("RZ09-0483U", 2)];
        assert_eq!(find_descriptor("RZ09-0483U9", &supported).unwrap().pid, 1);
    }

    #[test]
    fn no_match_returns_none() {
        let supported = [desc("RZ09-0483", 0x029f)];
        assert!(find_descriptor("NOTARAZER", &supported).is_none());
    }

    // ---- generic fallback for an uncatalogued Blade ----

    #[test]
    fn fallback_keeps_the_enumerated_pid_and_offers_all_features() {
        // The PID must be the one USB actually reported -- the HID open path depends
        // on it, and guessing there would break a machine that would otherwise work.
        let d = fallback_descriptor(0x02e1); // Blade 18 2026, absent from SUPPORTED
        assert_eq!(d.pid, 0x02e1);

        // Every feature is offered: an absent one answers NotSupported and fails fast,
        // which is a better outcome than hiding a control the chassis does have.
        assert_eq!(d.features, crate::feature::ALL_FEATURES);
        assert!(!d.features.is_empty());
    }

    #[test]
    fn fallback_never_replays_another_models_init_sequence() {
        // The riskiest possible guess: known init_cmds are model-specific magic
        // (the 2025 Blade 16 sends 0x0081/0x0086/0x0f90/...). Firing those at an
        // unknown EC is not something we should ever do on a hunch.
        assert!(
            fallback_descriptor(0x02e1).init_cmds.is_empty(),
            "the generic profile must not send any startup writes"
        );
    }

    #[test]
    fn fallback_fan_range_uses_the_per_pid_table_when_the_chassis_is_known() {
        // A PID the community tables cover gets that chassis's real envelope, not the
        // flat default -- the outliers are what make this worth doing.
        assert_eq!(
            fallback_descriptor(0x026e).fan_rpm_range,
            (2300, 4300),
            "Blade Pro 17 caps at 4300; the default 5000 would offer dead presets"
        );
        assert_eq!(
            fallback_descriptor(0x02c5).fan_rpm_range,
            (2200, 5600),
            "Blade 14 2025 reaches 5600; the default would hide its top end"
        );
        assert_eq!(
            fallback_descriptor(0x0224).fan_rpm_range,
            (3500, 5000),
            "pre-2023 chassis floor at 3500, not 2200"
        );
    }

    #[test]
    fn fallback_fan_range_is_the_permissive_default_for_a_wholly_unknown_pid() {
        // A PID in no table at all (e.g. a Blade released after this build) still has
        // to produce something usable.
        let (min, max) = fallback_descriptor(0xFFFF).fan_rpm_range;
        assert_eq!((min, max), FALLBACK_FAN_RPM_RANGE);
        // Low floor on purpose: the EC clamps a too-low request up to its real
        // minimum, so erring low costs nothing, while erring high would hide the
        // quiet end of the range on a chassis that supports it.
        assert!(min <= 2200, "floor must not exclude modern quiet chassis");
        assert!(max >= 5000, "ceiling must reach the common Blade maximum");
        assert!(min < max);
    }

    #[test]
    fn per_pid_fan_table_is_well_formed_and_does_not_shadow_supported() {
        use crate::descriptor::{FAN_RANGE_BY_PID, SUPPORTED};

        let mut seen = std::collections::HashSet::new();
        for (pid, (min, max)) in FAN_RANGE_BY_PID {
            assert!(
                seen.insert(*pid),
                "duplicate PID {pid:#06x} in the fan table"
            );
            assert!(min < max, "PID {pid:#06x} has an inverted range");
            // Guard against a transcription slip producing a nonsense envelope.
            assert!(
                (1500..=6500).contains(min) && (1500..=6500).contains(max),
                "PID {pid:#06x} range {min}..{max} is outside any plausible Blade fan envelope"
            );
            // The fallback table is only ever consulted for models `SUPPORTED` misses;
            // an entry for a catalogued PID would be dead weight that could silently
            // drift away from the tested value.
            assert!(
                !SUPPORTED.iter().any(|d| d.pid == *pid),
                "PID {pid:#06x} is already in SUPPORTED; the fallback entry is redundant"
            );
        }
    }

    #[test]
    fn a_known_model_still_beats_the_fallback() {
        // The fallback must never shadow a real descriptor: catalogued models keep
        // their tested fan envelope and curated feature list.
        let supported = [desc("RZ09-0483", 0x029f)];
        let found = find_descriptor("RZ09-0483U9R3", &supported).expect("known model");
        assert_eq!(found.pid, 0x029f);
        assert_ne!(found.name, fallback_descriptor(0x029f).name);
    }
}
