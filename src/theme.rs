//! Colors for the window and the text. One Dark and One Light.
//! Each text color has a contrast of at least 4.5:1 (WCAG AA) on the background. A test checks it.

pub type Rgb = [u8; 3];

#[derive(Clone, Copy, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub name: &'static str,
    /// Terminal background.
    pub bg: Rgb,
    /// Title bar background.
    pub bar: Rgb,
    /// Popup background.
    pub panel: Rgb,
    /// Selected row in a popup, and the active tab.
    pub panel_hi: Rgb,
    pub border: Rgb,
    pub fg: Rgb,
    /// Secondary text.
    pub dim: Rgb,
    pub accent: Rgb,
    pub red: Rgb,
    pub yellow: Rgb,
    pub green: Rgb,
    pub cyan: Rgb,
    pub selection: Rgb,
    /// Background of a find match.
    pub find: Rgb,
    /// The 16 ANSI colors.
    pub ansi: [Rgb; 16],
}

pub const ONE_DARK: Theme = Theme {
    dark: true,
    name: "One Dark",
    bg: [0x28, 0x2c, 0x34],
    bar: [0x21, 0x25, 0x2b],
    panel: [0x21, 0x25, 0x2b],
    panel_hi: [0x2c, 0x31, 0x3a],
    border: [0x3e, 0x44, 0x51],
    fg: [0xc8, 0xcc, 0xd4],
    dim: [0x96, 0x9a, 0xa2],
    accent: [0x61, 0xaf, 0xef],
    red: [0xe3, 0x7b, 0x83],
    yellow: [0xe5, 0xc0, 0x7b],
    green: [0x98, 0xc3, 0x79],
    cyan: [0x56, 0xb6, 0xc2],
    selection: [0x3e, 0x44, 0x51],
    find: [0x60, 0x53, 0x39],
    ansi: [
        [0x3f, 0x44, 0x51],
        [0xe2, 0x73, 0x7c],
        [0x98, 0xc3, 0x79],
        [0xd1, 0x9a, 0x66],
        [0x61, 0xaf, 0xef],
        [0xc6, 0x78, 0xdd],
        [0x56, 0xb6, 0xc2],
        [0xab, 0xb2, 0xbf],
        [0x90, 0x94, 0x9d],
        [0xe2, 0x73, 0x7c],
        [0x98, 0xc3, 0x79],
        [0xe5, 0xc0, 0x7b],
        [0x61, 0xaf, 0xef],
        [0xc6, 0x78, 0xdd],
        [0x56, 0xb6, 0xc2],
        [0xff, 0xff, 0xff],
    ],
};

pub const ONE_LIGHT: Theme = Theme {
    dark: false,
    name: "One Light",
    bg: [0xfa, 0xfa, 0xfa],
    bar: [0xec, 0xec, 0xed],
    panel: [0xff, 0xff, 0xff],
    panel_hi: [0xe5, 0xe5, 0xe6],
    border: [0xd0, 0xd0, 0xd2],
    fg: [0x38, 0x3a, 0x42],
    dim: [0x62, 0x64, 0x6f],
    accent: [0x33, 0x60, 0xc2],
    red: [0xad, 0x41, 0x37],
    yellow: [0x87, 0x5c, 0x01],
    green: [0x38, 0x71, 0x37],
    cyan: [0x01, 0x6c, 0x9a],
    selection: [0xd7, 0xdc, 0xe8],
    find: [0xf2, 0xdc, 0xa0],
    ansi: [
        [0x38, 0x3a, 0x42],
        [0xc2, 0x49, 0x3e],
        [0x3f, 0x7f, 0x3e],
        [0x98, 0x68, 0x01],
        [0x3a, 0x6c, 0xda],
        [0xa6, 0x26, 0xa4],
        [0x01, 0x79, 0xad],
        // "White" must be dark on a light background.
        [0x38, 0x3a, 0x42],
        [0x69, 0x6c, 0x77],
        [0xc2, 0x49, 0x3e],
        [0x3f, 0x7f, 0x3e],
        [0x98, 0x68, 0x01],
        [0x3a, 0x6c, 0xda],
        [0xa6, 0x26, 0xa4],
        [0x01, 0x79, 0xad],
        [0x10, 0x12, 0x16],
    ],
};

impl Theme {
    /// Color for an index of the terminal color table (0..269).
    pub fn palette(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index - 16;
                let level = |n: usize| if n == 0 { 0 } else { (55 + 40 * n) as u8 };
                [level(i / 36), level(i / 6 % 6), level(i % 6)]
            },
            232..=255 => [(8 + 10 * (index - 232)) as u8; 3],
            257 | 268 => self.bg,
            258 => self.accent,
            259..=266 => mix(self.ansi[index - 259], self.bg, 0.4),
            _ => self.fg,
        }
    }
}

/// Mixes `a` with `b`. `t` is the part of `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [0, 1, 2].map(|i| (a[i] as f32 * (1.0 - t) + b[i] as f32 * t) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast ratio of two colors: 1 (same) to 21 (black on white).
    fn contrast(a: Rgb, b: Rgb) -> f32 {
        let lum = |c: Rgb| {
            let ch = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_colors_are_readable() {
        for t in [ONE_DARK, ONE_LIGHT] {
            for (name, color) in [("fg", t.fg), ("dim", t.dim), ("accent", t.accent), ("red", t.red), ("yellow", t.yellow), ("green", t.green), ("cyan", t.cyan)] {
                for (surface, back) in [("bg", t.bg), ("panel_hi", t.panel_hi)] {
                    assert!(contrast(color, back) >= 4.5, "{} {name} on {surface}: {:.2}", t.name, contrast(color, back));
                }
            }
            // ANSI 0, 7, and 15 are black and white. A program uses them also as a background.
            for i in (1..15).filter(|i| *i != 7) {
                assert!(contrast(t.ansi[i], t.bg) >= 4.5, "{} ansi {i}: {:.2}", t.name, contrast(t.ansi[i], t.bg));
            }
            assert!(contrast(t.fg, t.find) >= 4.5 && contrast(t.fg, t.selection) >= 4.5, "{} find or selection", t.name);
        }
    }
}
