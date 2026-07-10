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
        init_cmds : &[],
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
        init_cmds : &[],
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
        init_cmds : &[],
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
        init_cmds : &[],
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
        init_cmds : &[0x0081,0x0086,0x0f90,0x0086,0x0f10,0x0087],
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
        init_cmds : &[0x0081,0x0086,0x0f90,0x0086,0x0f10,0x0087],
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
        init_cmds : &[0x0081,0x0086,0x0f90,0x0086,0x0f10,0x0087],
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
        init_cmds : &[],
        fan_rpm_range: (3500, 5000),
    }
];

const _VALIDATE_FEATURES: () = {
    crate::const_for! { device in SUPPORTED => {
        feature::validate_features(device.features);
    }}
};
