use anyhow::{bail, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use strum_macros::{EnumIter, EnumString};

#[derive(Clone, Copy)]
pub enum Cluster {
    Cpu = 0x01,
    Gpu = 0x02,
}

#[derive(Clone, Copy)]
pub enum FanZone {
    Zone1 = 0x01,
    Zone2 = 0x02,
}

#[derive(EnumIter, Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum PerfMode {
    Balanced = 0,
    Performance = 2,
    Custom = 4,
    Silent = 5,
    Battery = 6,
    Hyperboost = 7,
}

#[derive(EnumIter, Clone, Copy, Debug, ValueEnum, PartialEq, Serialize, Deserialize)]
pub enum MaxFanSpeedMode {
    Enable = 2,
    Disable = 0,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FanMode {
    Auto = 0,
    Manual = 1,
}

#[derive(EnumIter, Clone, Copy, Debug, ValueEnum, PartialEq, Serialize, Deserialize)]
pub enum CpuBoost {
    Low = 0,
    Medium = 1,
    High = 2,
    Boost = 3,
    Undervolt = 4,
}

#[derive(EnumIter, Clone, Copy, Debug, ValueEnum, PartialEq, Serialize, Deserialize)]
pub enum GpuBoost {
    Low = 0,
    Medium = 1,
    High = 2,
}

#[derive(
    EnumString, EnumIter, Clone, Copy, Debug, ValueEnum, PartialEq, Serialize, Deserialize,
)]
pub enum LogoMode {
    Off,
    Breathing,
    Static,
}

/// Keyboard backlight **effect** (the `0x0f02` extended-matrix effect command, LED region
/// 0x05 = backlight). HW-verified on PID 0x029F (2026-07-10): these apply in *Normal* device
/// mode -- the Fn media keys keep working -- so they're Fn-safe and need no driver mode.
///
/// v1 is **effects-only, no arbitrary color, by design.** A chosen static/per-key color on
/// this hardware requires Razer "driver mode" (host-streamed frames, i.e. what Synapse does),
/// which disables the Fn media keys -- our hard "no". In Normal mode the EC only self-animates
/// its built-in effects; any color payload we send is ignored and the board falls back to
/// Razer green. So we ship exactly the EC-animated effects that need no host color, all
/// HW-confirmed: Off (0x00), Spectrum (0x03), Wave (0x04, directional), Breathing (0x02,
/// random-color fade). Static/Reactive are intentionally omitted (they require a color).
#[derive(
    EnumString, EnumIter, Clone, Copy, Debug, ValueEnum, PartialEq, Serialize, Deserialize,
)]
pub enum KeyboardEffect {
    Off,
    Spectrum,
    Wave,
    Breathing,
}

/// Razer **device mode** (the `0x0004` command), misleadingly named for historical reasons.
/// `Enable` (0x03) is Driver mode and `Disable` (0x00) is Normal mode -- this is NOT a backlight
/// toggle. Driver mode disables the keyboard's native Fn media keys; see `command` module docs.
/// The tray keeps the device in Normal mode and does "always-on" via a keep-alive instead.
#[derive(EnumString, ValueEnum, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LightsAlwaysOn {
    Enable = 0x03,
    Disable = 0x00,
}

/// Battery charge limit ("Battery Health Optimizer" in Synapse), as a whole percent.
///
/// The wire byte is `bit7 = BHO enabled | bits0..6 = threshold percent`, confirmed
/// against razer-laptop-control's `bho_to_byte` and BugQuest/razer-blade-bho, and
/// HW-verified on PID 0x029F (2026-07-25).
///
/// **This used to be an 8-variant enum (50/55/.../80 + Disable) and that was our own
/// restriction, not the firmware's.** Probing the EC showed it accepts *every integer
/// from 50 to 100*: 0x85 (5%) and 0xAD (45%) return NotSupported, while 50, 51, 85, 95
/// and 100 all return status 0x02. The exact floor is 50 -- 48 and 49 are refused. So
/// the old enum hid 43 usable values, including the entire 81-99 band (a light 90%
/// limit is common battery-longevity advice and was simply unreachable).
///
/// 100 means "no limit". It is written as `0x50` -- bit 7 clear, i.e. BHO *disabled* --
/// which is what this project has always sent and is HW-verified. (`0xE4`, "enabled at
/// 100%", is also accepted and is what BugQuest's tool uses to defeat a stuck limit;
/// both achieve no-limit, and we keep the encoding already proven here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryCare {
    /// Invariant: always 50..=100. Constructors are the only way in.
    percent: u8,
}

impl BatteryCare {
    /// Lowest limit the EC accepts. HW-verified: 49 and below answer NotSupported.
    pub const MIN_PERCENT: u8 = 50;
    /// 100% == charge without a limit.
    pub const MAX_PERCENT: u8 = 100;

    /// Wire byte meaning "BHO disabled" (no charge limit). Bit 7 clear.
    const WIRE_DISABLED: u8 = 0x50;
    /// Bit 7 of the wire byte: BHO enabled.
    const WIRE_ENABLED_FLAG: u8 = 0x80;

    /// No charge limit (charge to 100%).
    pub const DISABLE: Self = Self { percent: 100 };

    /// A limit at `percent`, rejecting anything the EC will not accept.
    ///
    /// Unlike the old `from_percent`, this does **not** round to a nearby preset: the
    /// firmware honours every integer in range, so silently moving the user's 63% to
    /// 65% would be inventing a limitation and lying about it.
    pub fn from_percent(percent: u8) -> Result<Self> {
        if !(Self::MIN_PERCENT..=Self::MAX_PERCENT).contains(&percent) {
            bail!(
                "Invalid battery care percentage: {} (must be {}-{})",
                percent,
                Self::MIN_PERCENT,
                Self::MAX_PERCENT
            );
        }
        Ok(Self { percent })
    }

    /// The configured limit as a percent. 100 means no limit.
    pub fn to_percent(&self) -> u8 {
        self.percent
    }

    /// True when no limit is in force.
    pub fn is_disabled(&self) -> bool {
        self.percent == Self::MAX_PERCENT
    }

    /// The byte to put on the wire for this limit.
    pub fn wire_byte(&self) -> u8 {
        if self.is_disabled() {
            Self::WIRE_DISABLED
        } else {
            Self::WIRE_ENABLED_FLAG | self.percent
        }
    }
}

impl std::fmt::Display for BatteryCare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_disabled() {
            write!(f, "off (100%)")
        } else {
            write!(f, "{}%", self.percent)
        }
    }
}

/// Serialize as a plain number, so a config reads `battery_care = 80`.
impl Serialize for BatteryCare {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(self.percent)
    }
}

/// Accept either the new number form or the legacy enum-variant strings.
///
/// **This compatibility shim is load-bearing, not politeness.** `BatteryCare` was a
/// fieldless enum, so every config ever written by this app persisted it as a serde
/// variant name -- `battery_care = "Percent80"`, or `"Disable"`. Accepting only a number
/// would make those files fail to deserialize, and because the tray does
/// `confy::load(...).unwrap_or_default()` the failure would be *silent*: the user's saved
/// AC and battery profiles would be replaced by defaults, and the next persist would
/// overwrite them for good. Keep this shim.
impl<'de> Deserialize<'de> for BatteryCare {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Percent(u8),
            Legacy(String),
        }

        match Repr::deserialize(d)? {
            Repr::Percent(p) => BatteryCare::from_percent(p).map_err(serde::de::Error::custom),
            Repr::Legacy(name) => match name.as_str() {
                "Disable" => Ok(BatteryCare::DISABLE),
                // "Percent80" -> 80. The old variants were the only values that could
                // have been written, so anything else is a genuinely malformed config.
                other => other
                    .strip_prefix("Percent")
                    .and_then(|n| n.parse::<u8>().ok())
                    .map(BatteryCare::from_percent)
                    .transpose()
                    .map_err(serde::de::Error::custom)?
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!(
                            "unrecognized battery_care value {other:?}"
                        ))
                    }),
            },
        }
    }
}

impl TryFrom<u8> for GpuBoost {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Low),
            1 => Ok(Self::Medium),
            2 => Ok(Self::High),
            _ => bail!("Failed to convert {} to GpuBoost", value),
        }
    }
}

impl TryFrom<u8> for PerfMode {
    type Error = anyhow::Error;

    fn try_from(perf_mode: u8) -> Result<Self, Self::Error> {
        match perf_mode {
            0 => Ok(Self::Balanced),
            2 => Ok(Self::Performance),
            4 => Ok(Self::Custom),
            5 => Ok(Self::Silent),
            6 => Ok(Self::Battery),
            7 => Ok(Self::Hyperboost),
            _ => bail!("Failed to convert {} to PerformanceMode", perf_mode),
        }
    }
}

impl TryFrom<u8> for FanMode {
    type Error = anyhow::Error;

    fn try_from(fan_mode: u8) -> Result<Self, Self::Error> {
        match fan_mode {
            0 => Ok(Self::Auto),
            1 => Ok(Self::Manual),
            _ => bail!("Failed to convert {} to FanMode", fan_mode),
        }
    }
}

impl TryFrom<u8> for CpuBoost {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Low),
            1 => Ok(Self::Medium),
            2 => Ok(Self::High),
            3 => Ok(Self::Boost),
            4 => Ok(Self::Undervolt),
            _ => bail!("Failed to convert {} to CpuBoost", value),
        }
    }
}

impl TryFrom<u8> for LightsAlwaysOn {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(LightsAlwaysOn::Disable),
            3 => Ok(LightsAlwaysOn::Enable),
            _ => bail!("Failed to convert {} to LightsAlwaysOn", value),
        }
    }
}

impl TryFrom<u8> for BatteryCare {
    type Error = anyhow::Error;

    /// Decode the wire byte the EC reports (`0x0792`).
    ///
    /// Bit 7 set means BHO is enabled and bits 0..6 carry the threshold; bit 7 clear
    /// means it is off, whatever the low bits say (the EC reports 0x50 there).
    /// `0xE4` -- enabled at 100% -- is a no-limit state some tools set, so it decodes
    /// to the same thing as disabled.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value & BatteryCare::WIRE_ENABLED_FLAG == 0 {
            return Ok(BatteryCare::DISABLE);
        }
        let percent = value & 0x7f;
        if percent >= BatteryCare::MAX_PERCENT {
            return Ok(BatteryCare::DISABLE);
        }
        BatteryCare::from_percent(percent)
            .map_err(|e| anyhow::anyhow!("Failed to convert {:#x} to BatteryCare: {}", value, e))
    }
}

impl TryFrom<u8> for MaxFanSpeedMode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x02 => Ok(MaxFanSpeedMode::Enable),
            0x00 => Ok(MaxFanSpeedMode::Disable),
            _ => bail!("Failed to convert {} to MaxFanSpeedMode", value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The EC accepts every integer 50..=100 (HW-verified on 0x029F 2026-07-25: 48 and 49
    // answer NotSupported; 50/51/85/95/100 all return status 0x02). These tests pin that
    // range, because the previous 8-value rounding table was OUR restriction, not the
    // firmware's, and quietly reintroducing one would be a regression users can feel.
    #[test]
    fn battery_care_accepts_every_percent_the_ec_supports() {
        for pct in BatteryCare::MIN_PERCENT..=BatteryCare::MAX_PERCENT {
            let bc = BatteryCare::from_percent(pct)
                .unwrap_or_else(|e| panic!("{pct}% must be accepted: {e}"));
            // No rounding: the value must survive exactly as given.
            assert_eq!(bc.to_percent(), pct, "from_percent({pct}) must not round");
        }
    }

    #[test]
    fn battery_care_rejects_values_the_ec_refuses() {
        // 50 is a hard firmware floor, verified by probe.
        for pct in [0u8, 1, 30, 45, 48, 49] {
            assert!(
                BatteryCare::from_percent(pct).is_err(),
                "{pct}% is below the EC floor and must be rejected"
            );
        }
        for pct in [101u8, 150, 255] {
            assert!(
                BatteryCare::from_percent(pct).is_err(),
                "{pct}% is out of range"
            );
        }
    }

    #[test]
    fn battery_care_wire_encoding_is_enabled_flag_plus_percent() {
        // bit7 = BHO enabled, bits0..6 = threshold. Cross-checked against
        // razer-laptop-control's bho_to_byte and HW-verified byte-for-byte.
        assert_eq!(BatteryCare::from_percent(50).unwrap().wire_byte(), 0xB2);
        assert_eq!(BatteryCare::from_percent(80).unwrap().wire_byte(), 0xD0);
        assert_eq!(BatteryCare::from_percent(95).unwrap().wire_byte(), 0xDF);
        // 100 == no limit, written as bit7-CLEAR (0x50), the encoding this project has
        // always sent and the one verified on hardware.
        assert_eq!(BatteryCare::DISABLE.wire_byte(), 0x50);
        assert!(BatteryCare::DISABLE.is_disabled());
        assert!(!BatteryCare::from_percent(80).unwrap().is_disabled());
    }

    #[test]
    fn battery_care_wire_value_round_trips() {
        for pct in BatteryCare::MIN_PERCENT..=BatteryCare::MAX_PERCENT {
            let bc = BatteryCare::from_percent(pct).unwrap();
            assert_eq!(
                BatteryCare::try_from(bc.wire_byte()).unwrap(),
                bc,
                "wire round-trip for {pct}%"
            );
        }
    }

    #[test]
    fn battery_care_decodes_both_no_limit_encodings() {
        // 0x50: bit7 clear -> BHO off.
        assert_eq!(BatteryCare::try_from(0x50).unwrap(), BatteryCare::DISABLE);
        // 0xE4: "enabled at 100%" -- what BugQuest's tool writes to defeat a stuck
        // limit. Functionally no limit, so it must decode the same way rather than
        // erroring or reporting a 100% "limit" that differs from Disable.
        assert_eq!(BatteryCare::try_from(0xE4).unwrap(), BatteryCare::DISABLE);
        // A bit7-set byte below the floor is genuinely malformed.
        assert!(BatteryCare::try_from(0x80 | 20).is_err());
    }

    /// Existing configs persist `battery_care` as the OLD serde enum-variant name.
    /// Deserialization must still accept them: the tray loads config with
    /// `unwrap_or_default()`, so a parse failure would SILENTLY discard the user's saved
    /// AC/battery profiles and then overwrite them on the next persist.
    #[test]
    fn battery_care_deserializes_legacy_enum_variant_names() {
        for (legacy, expected_pct) in [
            ("\"Percent50\"", 50),
            ("\"Percent65\"", 65),
            ("\"Percent80\"", 80),
            ("\"Disable\"", 100),
        ] {
            let bc: BatteryCare = serde_json::from_str(legacy)
                .unwrap_or_else(|e| panic!("legacy config value {legacy} must load: {e}"));
            assert_eq!(bc.to_percent(), expected_pct, "legacy {legacy}");
        }
    }

    #[test]
    fn battery_care_serializes_as_a_plain_number_and_round_trips() {
        let bc = BatteryCare::from_percent(90).unwrap();
        assert_eq!(serde_json::to_string(&bc).unwrap(), "90");
        // New form loads too, so a config written today reloads tomorrow.
        assert_eq!(serde_json::from_str::<BatteryCare>("90").unwrap(), bc);
        assert_eq!(
            serde_json::from_str::<BatteryCare>("100").unwrap(),
            BatteryCare::DISABLE
        );
        // A number outside the EC's range must fail loudly, not clamp.
        assert!(serde_json::from_str::<BatteryCare>("20").is_err());
    }

    #[test]
    fn enum_wire_values_round_trip() {
        // Each fieldless enum's discriminant is its protocol byte; the TryFrom impls
        // must round-trip every variant so a renumber can't silently desync.
        for v in [
            PerfMode::Balanced,
            PerfMode::Performance,
            PerfMode::Custom,
            PerfMode::Silent,
            PerfMode::Battery,
            PerfMode::Hyperboost,
        ] {
            assert_eq!(PerfMode::try_from(v as u8).unwrap(), v);
        }
        for v in [
            CpuBoost::Low,
            CpuBoost::Medium,
            CpuBoost::High,
            CpuBoost::Boost,
            CpuBoost::Undervolt,
        ] {
            assert_eq!(CpuBoost::try_from(v as u8).unwrap(), v);
        }
        for v in [GpuBoost::Low, GpuBoost::Medium, GpuBoost::High] {
            assert_eq!(GpuBoost::try_from(v as u8).unwrap(), v);
        }
        for v in [FanMode::Auto, FanMode::Manual] {
            assert_eq!(FanMode::try_from(v as u8).unwrap(), v);
        }
        for v in [LightsAlwaysOn::Enable, LightsAlwaysOn::Disable] {
            assert_eq!(LightsAlwaysOn::try_from(v as u8).unwrap(), v);
        }
        for v in [MaxFanSpeedMode::Enable, MaxFanSpeedMode::Disable] {
            assert_eq!(MaxFanSpeedMode::try_from(v as u8).unwrap(), v);
        }
    }

    #[test]
    fn unknown_wire_values_are_rejected() {
        assert!(PerfMode::try_from(99).is_err());
        assert!(CpuBoost::try_from(99).is_err());
        assert!(GpuBoost::try_from(99).is_err());
        assert!(FanMode::try_from(99).is_err());
    }
}
