use crate::feature;

// model_number_prefix shall conform to https://mysupport.razer.com/app/answers/detail/a_id/5481
#[derive(Debug, Clone)]
pub struct Descriptor {
    pub model_number_prefix: &'static str,
    pub name: &'static str,
    pub pid: u16,
    pub features: &'static [&'static str],
    pub init_cmds: &'static [u16],
    /// Usable manual-fan RPM bounds (min, max) for this chassis. Below min the EC floors the
    /// fan, above max it clamps; the UI/CLI build their presets from this. Sourced from
    /// Vader89/razer-laptop-control-16-2023's per-model `laptops.json` (HW-probed on 0x029F).
    /// 2023+ Blades are (2200, 5000); older chassis floor higher, e.g. 2022 Blade 15 (3500, 5000).
    pub fan_rpm_range: (u16, u16),
}

pub const SUPPORTED: &[Descriptor] = &[
    Descriptor {
        model_number_prefix: "RZ09-0483T",
        name: "Razer Blade 16 (2023) Black",
        pid: 0x029f,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-0482X",
        name: "Razer Blade 14 (2023) Mercury",
        pid: 0x029d,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-0483U",
        name: "Razer Blade 16 (2023)",
        pid: 0x029f,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-0510S",
        name: "Razer Blade 16 (2024)",
        pid: 0x02b7,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-05289",
        name: "Razer Blade 16 (2025) RTX 5090",
        pid: 0x02c6,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[0x0081, 0x0086, 0x0f90, 0x0086, 0x0f10, 0x0087],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-05288",
        name: "Razer Blade 16 (2025) 5080",
        pid: 0x02c6,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[0x0081, 0x0086, 0x0f90, 0x0086, 0x0f10, 0x0087],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-05286",
        name: "Razer Blade 16 (2025) 5070",
        pid: 0x02c6,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[0x0081, 0x0086, 0x0f90, 0x0086, 0x0f10, 0x0087],
        fan_rpm_range: (2200, 5000),
    },
    Descriptor {
        model_number_prefix: "RZ09-0421N",
        name: "Razer Blade 15 (2022)",
        pid: 0x028a,
        features: &[
            "battery-care",
            "fan",
            "kbd-backlight",
            "kbd-lighting",
            "lid-logo",
            "lights-always-on",
            "perf",
        ],
        init_cmds: &[],
        fan_rpm_range: (3500, 5000),
    },
];

const _VALIDATE_FEATURES: () = {
    crate::const_for! { device in SUPPORTED => {
        feature::validate_features(device.features);
    }}
};

/// Per-PID fan envelopes for Blade chassis we have no `SUPPORTED` entry for.
///
/// WHY A SECOND TABLE. `SUPPORTED` is keyed by BIOS **model-number prefix** (`RZ09-…`),
/// which is the precise identifier but is only knowable by reading it off a real
/// machine. The community device tables -- razer-laptop-control's `laptops.json` and
/// the Revived fork's 50-model extension -- are keyed by **USB PID**, which we always
/// have from enumeration. So for a machine we cannot inspect, the PID is the only key
/// we can actually match on. This table is that data, and it exists solely to give the
/// generic fallback a *correct fan range* instead of a conservative guess.
///
/// STATUS: TRANSCRIBED, NOT TESTED. Every row here comes from those reference tables;
/// none has been verified on hardware by this project, which owns exactly one Blade
/// (PID 0x029F). A row being present means "the fan bounds are better than a guess",
/// not "this model is supported". Catalogued models in `SUPPORTED` always win.
///
/// Note the outliers, which are why a flat default is wrong: the Blade Pro 17 tops out
/// at 4300, the Blade 14 2025 reaches 5600, and pre-2023 chassis floor at 3500 rather
/// than 2200 -- a 2200 preset on those is simply a dead menu entry.
pub const FAN_RANGE_BY_PID: &[(u16, (u16, u16))] = &[
    // Pre-2023: higher floor, most cap at 5000.
    (0x0205, (3500, 5000)), // Blade Stealth 2015
    (0x020f, (3500, 5000)), // Blade QHD
    (0x0210, (3500, 5000)), // Blade Pro 2017 v2
    (0x0220, (3500, 5000)), // Blade Stealth Late 2016
    (0x0224, (3500, 5000)), // Blade 15 2016
    (0x0225, (3500, 5000)), // Blade Pro 2017
    (0x022d, (3500, 5000)), // Blade Stealth 2017
    (0x022f, (3500, 5000)), // Blade Pro 2018 FHD
    (0x0232, (3500, 5000)), // Blade Stealth Late 2017
    (0x0233, (3500, 5000)), // Blade 15 2018 Advanced
    (0x0234, (3500, 5300)), // Blade Pro 2019
    (0x0239, (3500, 5300)), // Blade Stealth 2019
    (0x023a, (3500, 5300)), // Blade 15 2019 Advanced
    (0x023b, (3500, 5000)), // Blade 15 2018 Base
    (0x0240, (3500, 5000)), // Blade 15 2018 Mercury
    (0x0245, (3500, 5300)), // Blade 15 2019 Mercury
    (0x0246, (3500, 5000)), // Blade 15 2019 Base
    (0x024a, (3500, 5000)), // Blade Stealth 2019 GTX
    (0x024b, (3500, 5300)), // Blade 15 Late 2019 Advanced
    (0x024c, (3500, 5300)), // Blade Pro Late 2019
    (0x024d, (3500, 5300)), // Blade 15 Studio Edition 2019
    (0x0252, (3500, 5000)), // Blade Stealth 2020
    (0x0253, (3500, 5300)), // Blade 15 2020 Advanced
    (0x0255, (3500, 5000)), // Blade 15 2020 Base
    (0x0256, (3500, 5300)), // Blade Pro 2020
    (0x0259, (3500, 5000)), // Blade Stealth Late 2020
    (0x0268, (3500, 5000)), // Blade 15 Late 2020 Base
    (0x026a, (3500, 5000)), // Razer Book 13 2020
    (0x026d, (3500, 5000)), // Blade 15 Late 2021 Advanced
    (0x026e, (2300, 4300)), // Blade Pro 17 Early 2021 -- low ceiling
    (0x026f, (3500, 5000)), // Blade 15 2021 Base
    (0x0270, (3500, 5000)), // Blade 14 2021
    (0x0276, (2000, 5400)), // Blade 15 2021 Advanced -- widest range
    (0x0279, (2300, 4300)), // Blade Pro 17 Mid 2021 -- low ceiling
    (0x027a, (3500, 5000)), // Blade 15 Late 2021 Base
    (0x028b, (3500, 5000)), // Blade 17 2022
    (0x028c, (3500, 5000)), // Blade 14 2022
    // 2023+: quiet floor at 2200.
    (0x029e, (2200, 5000)), // Blade 15 2023
    (0x02a0, (2200, 5000)), // Blade 18 2023
    (0x02b6, (2200, 5000)), // Blade 14 2024
    (0x02b8, (2200, 5000)), // Blade 18 2024
    (0x02c5, (2200, 5600)), // Blade 14 2025 -- highest ceiling
    (0x02c7, (2200, 5000)), // Blade 18 2025
    (0x02e1, (2200, 5000)), // Blade 18 2026
];

/// Fan bounds for `pid` from [`FAN_RANGE_BY_PID`], if that chassis is listed.
pub fn fan_range_for_pid(pid: u16) -> Option<(u16, u16)> {
    FAN_RANGE_BY_PID
        .iter()
        .find(|(p, _)| *p == pid)
        .map(|(_, range)| *range)
}
