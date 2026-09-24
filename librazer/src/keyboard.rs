//! Custom keyboard color: the frame model, presets, and the status overlays.
//!
//! The keyboard takes a full color frame in Normal device mode, so the Fn media keys keep
//! working (HW-verified on 0x029F 2026-09-23, after this project had documented it as
//! impossible): six `0x030b` row writes, then `0x030a [0x05, 0x00]` to show them. The frame
//! is not stored in the EC -- it is gone after Modern Standby, and the keyboard falls back to
//! the stored `0x0f02` effect -- so whoever sets a color must paint it again after a wake.
//!
//! Everything here is pure: [`render`] turns a color choice plus live status into a
//! [`Frame`], and `command::set_keyboard_frame` writes one.

use crate::state::PerfMode;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Rows in the color matrix, top (Esc and the function keys) to bottom.
pub const ROWS: usize = 6;
/// Columns in the color matrix. The row write addresses columns 0..=15.
pub const COLS: usize = 16;
/// The number row (`` ` `` 1 2 ... 0 - = Backspace).
pub const NUMBER_ROW: usize = 1;
/// Columns of the 1-0 keys. Mapped by lighting marker columns on 0x029F (2026-09-23):
/// column 0 has no key on the top two rows, `` ` `` is column 1 and `1` is column 2.
pub const DIGIT_COLS: std::ops::RangeInclusive<usize> = 2..=11;

/// Models whose matrix geometry has been mapped. The row write itself is shared with other
/// Blades, but on a different matrix the battery bar would land on the wrong keys.
pub fn supports_custom_frame(pid: u16) -> bool {
    pid == 0x029F
}

/// One LED color. Written and read as `"#rrggbb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// This color at `percent` of its brightness.
    pub fn dimmed(self, percent: u8) -> Self {
        let f = |v: u8| (u16::from(v) * u16::from(percent.min(100)) / 100) as u8;
        Rgb::new(f(self.r), f(self.g), f(self.b))
    }
}

impl FromStr for Rgb {
    type Err = anyhow::Error;

    /// `#rrggbb` or `rrggbb`, case-insensitive.
    fn from_str(s: &str) -> Result<Self> {
        let hex = s.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{s:?} is not a color; expected #rrggbb");
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16);
        Ok(Rgb::new(byte(0)?, byte(2)?, byte(4)?))
    }
}

impl TryFrom<String> for Rgb {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Rgb> for String {
    fn from(c: Rgb) -> String {
        c.to_string()
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// A custom keyboard color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyboardColor {
    /// One color per row, top to bottom. A solid color is six equal rows.
    Rows([Rgb; ROWS]),
    /// The whole keyboard in the color of the current perf mode ([`perf_mode_color`]).
    FollowPerfMode,
}

impl KeyboardColor {
    pub fn solid(c: Rgb) -> Self {
        KeyboardColor::Rows([c; ROWS])
    }
}

/// The color "Follow performance mode" shows for each mode: the color of that mode's tray
/// icon (the dominant opaque pixel of `razer-tray/icons/razer-<color>.png`), so the keyboard
/// and the icon agree. Balanced, Performance and Custom are more saturated than their icons
/// (`#94d82d`, `#fa5252`, `#9c6749`): on the keyboard LEDs those looked washed out.
pub fn perf_mode_color(mode: PerfMode) -> Rgb {
    match mode {
        PerfMode::Battery => Rgb::new(0x00, 0xa2, 0xe8),
        PerfMode::Silent => Rgb::new(0xfc, 0xc4, 0x19),
        PerfMode::Balanced => Rgb::new(0x6c, 0xff, 0x00),
        PerfMode::Performance => Rgb::new(0xff, 0x14, 0x14),
        PerfMode::Hyperboost => Rgb::new(0xff, 0x00, 0xfa),
        PerfMode::Custom(..) => Rgb::new(0xa8, 0x40, 0x00),
    }
}

/// A named color choice offered in the tray menu. `colors` holds one color (solid) or six
/// (one per row, top to bottom); any other count is not offered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyboardPreset {
    pub name: String,
    pub colors: Vec<Rgb>,
}

impl KeyboardPreset {
    pub fn color(&self) -> Option<KeyboardColor> {
        match self.colors.as_slice() {
            [c] => Some(KeyboardColor::solid(*c)),
            rows => <[Rgb; ROWS]>::try_from(rows).ok().map(KeyboardColor::Rows),
        }
    }
}

/// The presets a new config starts with. Written into the config file on the first save,
/// where they can be edited, removed or added to.
pub fn default_presets() -> Vec<KeyboardPreset> {
    let preset = |name: &str, colors: &[&str]| KeyboardPreset {
        name: name.to_string(),
        colors: colors.iter().map(|c| c.parse().unwrap()).collect(),
    };
    vec![
        preset("White", &["#ffffff"]),
        preset("Razer Green", &["#44d62c"]),
        preset("Red", &["#ff0000"]),
        preset("Blue", &["#0050ff"]),
        preset("Purple", &["#8000ff"]),
        preset(
            "Sunset",
            &[
                "#ffd000", "#ffa000", "#ff7000", "#ff4000", "#ff1040", "#d00070",
            ],
        ),
        preset(
            "Ocean",
            &[
                "#00ffd0", "#00d0ff", "#00a0ff", "#0070ff", "#2040ff", "#6020ff",
            ],
        ),
    ]
}

/// Battery state for the number-row bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryLevel {
    pub percent: u8,
    pub charging: bool,
}

pub const BAR_CHARGING: Rgb = Rgb::new(0x00, 0xc8, 0xff);
pub const BAR_HIGH: Rgb = Rgb::new(0x00, 0xff, 0x00);
pub const BAR_MEDIUM: Rgb = Rgb::new(0xff, 0xa0, 0x00);
pub const BAR_LOW: Rgb = Rgb::new(0xff, 0x00, 0x00);

/// One color per key position.
pub type Frame = [[Rgb; COLS]; ROWS];

/// The frame to show for `color`, with the battery bar drawn over it when `battery` is set.
pub fn render(color: KeyboardColor, perf: PerfMode, battery: Option<BatteryLevel>) -> Frame {
    let rows = match color {
        KeyboardColor::Rows(rows) => rows,
        KeyboardColor::FollowPerfMode => [perf_mode_color(perf); ROWS],
    };
    let mut frame = rows.map(|c| [c; COLS]);
    if let Some(level) = battery {
        draw_battery_bar(&mut frame, level);
    }
    frame
}

/// How bright the digit keys beyond the charge are, as a percent of the bar color. Dim
/// rather than off, so the whole ten-key scale stays visible.
pub const BAR_UNLIT_PERCENT: u8 = 20;

/// Light one of the ten digit keys per started 10% (so 1% still shows one key), in a color
/// for the level, or cyan while charging. The rest of the digit keys show the same color
/// dimmed, so the bar reads as a bar against any base color.
fn draw_battery_bar(frame: &mut Frame, level: BatteryLevel) {
    let percent = usize::from(level.percent.min(100));
    let lit = percent.div_ceil(10);
    let on = if level.charging {
        BAR_CHARGING
    } else if percent >= 50 {
        BAR_HIGH
    } else if percent >= 20 {
        BAR_MEDIUM
    } else {
        BAR_LOW
    };
    for (i, col) in DIGIT_COLS.enumerate() {
        frame[NUMBER_ROW][col] = if i < lit {
            on
        } else {
            on.dimmed(BAR_UNLIT_PERCENT)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_and_print_as_hex() {
        assert_eq!(
            "#44d62c".parse::<Rgb>().unwrap(),
            Rgb::new(0x44, 0xd6, 0x2c)
        );
        assert_eq!("FF0080".parse::<Rgb>().unwrap(), Rgb::new(0xff, 0x00, 0x80));
        assert_eq!(Rgb::new(0x0a, 0xb0, 0xff).to_string(), "#0ab0ff");
        for bad in ["", "#fff", "#12345g", "red", "#1234567"] {
            assert!(bad.parse::<Rgb>().is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn colors_round_trip_through_toml_as_strings() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct T {
            c: KeyboardColor,
            p: KeyboardColor,
        }
        let t = T {
            c: KeyboardColor::solid(Rgb::new(1, 2, 3)),
            p: KeyboardColor::FollowPerfMode,
        };
        let text = toml::to_string(&t).unwrap();
        assert!(text.contains("\"#010203\""), "{text}");
        assert_eq!(toml::from_str::<T>(&text).unwrap(), t);
    }

    #[test]
    fn a_preset_needs_one_or_six_colors() {
        let with = |n: usize| KeyboardPreset {
            name: "x".into(),
            colors: vec![Rgb::new(9, 9, 9); n],
        };
        assert_eq!(
            with(1).color(),
            Some(KeyboardColor::solid(Rgb::new(9, 9, 9)))
        );
        assert!(matches!(with(6).color(), Some(KeyboardColor::Rows(_))));
        for n in [0, 2, 5, 7] {
            assert_eq!(with(n).color(), None, "{n} colors");
        }
        assert!(default_presets().iter().all(|p| p.color().is_some()));
    }

    #[test]
    fn rows_fill_every_column() {
        let mut rows = [Rgb::BLACK; ROWS];
        for (i, c) in rows.iter_mut().enumerate() {
            *c = Rgb::new(i as u8, 0, 0);
        }
        let f = render(KeyboardColor::Rows(rows), PerfMode::Balanced, None);
        for (i, row) in f.iter().enumerate() {
            assert!(row.iter().all(|c| *c == Rgb::new(i as u8, 0, 0)));
        }
    }

    #[test]
    fn follow_perf_mode_uses_the_mode_color() {
        let f = render(KeyboardColor::FollowPerfMode, PerfMode::Silent, None);
        assert!(f
            .iter()
            .flatten()
            .all(|c| *c == perf_mode_color(PerfMode::Silent)));
        assert_ne!(
            perf_mode_color(PerfMode::Silent),
            perf_mode_color(PerfMode::Performance)
        );
    }

    fn bar(percent: u8, charging: bool) -> Vec<Rgb> {
        let f = render(
            KeyboardColor::solid(Rgb::new(1, 1, 1)),
            PerfMode::Balanced,
            Some(BatteryLevel { percent, charging }),
        );
        DIGIT_COLS.map(|c| f[NUMBER_ROW][c]).collect()
    }

    #[test]
    fn the_battery_bar_lights_one_key_per_started_ten_percent() {
        let full = [BAR_HIGH, BAR_MEDIUM, BAR_LOW];
        let lit = |p| bar(p, false).iter().filter(|c| full.contains(c)).count();
        assert_eq!(lit(0), 0);
        assert_eq!(lit(1), 1);
        assert_eq!(lit(10), 1);
        assert_eq!(lit(11), 2);
        assert_eq!(lit(80), 8);
        assert_eq!(lit(100), 10);
        assert_eq!(lit(255), 10);
    }

    #[test]
    fn the_keys_beyond_the_charge_are_dimmed_not_off() {
        let keys = bar(80, false);
        assert_eq!(keys[8], BAR_HIGH.dimmed(BAR_UNLIT_PERCENT));
        assert_ne!(keys[8], Rgb::BLACK);
        assert_eq!(Rgb::new(200, 100, 0).dimmed(50), Rgb::new(100, 50, 0));
    }

    #[test]
    fn the_battery_bar_color_follows_level_and_charging() {
        assert_eq!(bar(80, false)[0], BAR_HIGH);
        assert_eq!(bar(30, false)[0], BAR_MEDIUM);
        assert_eq!(bar(10, false)[0], BAR_LOW);
        assert_eq!(bar(10, true)[0], BAR_CHARGING);
    }

    #[test]
    fn the_battery_bar_leaves_the_rest_of_the_keyboard_alone() {
        let base = Rgb::new(1, 1, 1);
        let f = render(
            KeyboardColor::solid(base),
            PerfMode::Balanced,
            Some(BatteryLevel {
                percent: 50,
                charging: false,
            }),
        );
        for (r, row) in f.iter().enumerate() {
            for (c, color) in row.iter().enumerate() {
                if r != NUMBER_ROW || !DIGIT_COLS.contains(&c) {
                    assert_eq!(*color, base, "row {r} col {c}");
                }
            }
        }
    }
}
