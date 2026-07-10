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
}
