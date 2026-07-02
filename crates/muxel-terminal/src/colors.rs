//! Mapping from `alacritty_terminal` colors to gpui colors, plus a default
//! ANSI palette. Kept self-contained so the terminal crate doesn't depend on
//! the UI theme; the palette can be overridden by the caller later.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, Rgba};

/// A terminal color palette: default fg/bg/cursor + the 16 ANSI colors.
#[derive(Clone, Debug)]
pub struct TerminalPalette {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    /// Text-selection highlight color.
    pub selection: u32,
    /// ANSI colors 0..=7 (normal) and 8..=15 (bright).
    pub ansi: [u32; 16],
}

impl Default for TerminalPalette {
    fn default() -> Self {
        // A modern dark palette (Catppuccin Mocha-ish).
        Self {
            background: 0x1e1e2e,
            foreground: 0xcdd6f4,
            cursor: 0xf5e0dc,
            selection: 0x45475a,
            ansi: [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
            ],
        }
    }
}

fn hex_to_hsla(c: u32) -> Hsla {
    Rgba {
        r: ((c >> 16) & 0xff) as f32 / 255.0,
        g: ((c >> 8) & 0xff) as f32 / 255.0,
        b: (c & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

fn rgb_to_hsla(c: Rgb) -> Hsla {
    Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

impl TerminalPalette {
    pub fn background_hsla(&self) -> Hsla {
        hex_to_hsla(self.background)
    }
    pub fn foreground_hsla(&self) -> Hsla {
        hex_to_hsla(self.foreground)
    }
    pub fn cursor_hsla(&self) -> Hsla {
        hex_to_hsla(self.cursor)
    }
    /// Selection highlight, kept translucent so the text shows through.
    pub fn selection_hsla(&self) -> Hsla {
        let mut c = hex_to_hsla(self.selection);
        c.a = 0.4;
        c
    }

    /// Resolve an alacritty cell color to a concrete gpui color.
    pub fn color(&self, color: &Color) -> Hsla {
        match color {
            Color::Spec(rgb) => rgb_to_hsla(*rgb),
            Color::Named(named) => self.named(*named),
            Color::Indexed(i) => self.indexed(*i),
        }
    }

    /// Whether a background color is the terminal's default (i.e. should not be
    /// painted as a colored rectangle).
    pub fn is_default_bg(&self, color: &Color) -> bool {
        match color {
            Color::Named(NamedColor::Background) => true,
            Color::Spec(rgb) => {
                rgb.r == ((self.background >> 16) & 0xff) as u8
                    && rgb.g == ((self.background >> 8) & 0xff) as u8
                    && rgb.b == (self.background & 0xff) as u8
            }
            _ => false,
        }
    }

    fn named(&self, n: NamedColor) -> Hsla {
        use NamedColor::*;
        let hex = match n {
            Black => self.ansi[0],
            Red => self.ansi[1],
            Green => self.ansi[2],
            Yellow => self.ansi[3],
            Blue => self.ansi[4],
            Magenta => self.ansi[5],
            Cyan => self.ansi[6],
            White => self.ansi[7],
            BrightBlack => self.ansi[8],
            BrightRed => self.ansi[9],
            BrightGreen => self.ansi[10],
            BrightYellow => self.ansi[11],
            BrightBlue => self.ansi[12],
            BrightMagenta => self.ansi[13],
            BrightCyan => self.ansi[14],
            BrightWhite => self.ansi[15],
            Background => self.background,
            Cursor => self.cursor,
            Foreground => self.foreground,
            // Dim/bright-foreground and any future variants fall back to fg.
            _ => self.foreground,
        };
        hex_to_hsla(hex)
    }

    fn indexed(&self, i: u8) -> Hsla {
        match i {
            0..=15 => hex_to_hsla(self.ansi[i as usize]),
            16..=231 => {
                // 6x6x6 color cube with xterm's perceptual levels.
                const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let n = (i - 16) as usize;
                let (r, g, b) = (LEVELS[n / 36], LEVELS[(n / 6) % 6], LEVELS[n % 6]);
                Rgba {
                    r: r as f32 / 255.0,
                    g: g as f32 / 255.0,
                    b: b as f32 / 255.0,
                    a: 1.0,
                }
                .into()
            }
            232..=255 => {
                // 24-step grayscale ramp.
                let l = 8u16 + (i as u16 - 232) * 10;
                let l = l.min(255) as f32 / 255.0;
                Rgba {
                    r: l,
                    g: l,
                    b: l,
                    a: 1.0,
                }
                .into()
            }
        }
    }
}

/// Resolve a color-query index — as vte encodes OSC `4;n;?` (0..=255) and OSC
/// `10/11/12;?` (`NamedColor::Foreground`/`Background`/`Cursor` = 256/257/258)
/// — to the concrete RGB the reply reports back to the querying program. `None`
/// for indices vte never sends. Mirrors [`TerminalPalette::indexed`] in u8
/// space, so the answer matches exactly what the renderer paints.
pub(crate) fn index_to_rgb(palette: &TerminalPalette, index: usize) -> Option<Rgb> {
    let hex = |c: u32| Rgb {
        r: ((c >> 16) & 0xff) as u8,
        g: ((c >> 8) & 0xff) as u8,
        b: (c & 0xff) as u8,
    };
    match index {
        0..=15 => Some(hex(palette.ansi[index])),
        16..=231 => {
            // 6x6x6 color cube with xterm's perceptual levels.
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let n = index - 16;
            Some(Rgb {
                r: LEVELS[n / 36],
                g: LEVELS[(n / 6) % 6],
                b: LEVELS[n % 6],
            })
        }
        232..=255 => {
            // 24-step grayscale ramp.
            let l = (8 + (index - 232) * 10).min(255) as u8;
            Some(Rgb { r: l, g: l, b: l })
        }
        256 => Some(hex(palette.foreground)),
        257 => Some(hex(palette.background)),
        258 => Some(hex(palette.cursor)),
        _ => None,
    }
}

/// Brighten a foreground color for bold text (normal ANSI -> bright variant).
pub fn brighten(color: Color) -> Color {
    match color {
        Color::Named(NamedColor::Black) => Color::Named(NamedColor::BrightBlack),
        Color::Named(NamedColor::Red) => Color::Named(NamedColor::BrightRed),
        Color::Named(NamedColor::Green) => Color::Named(NamedColor::BrightGreen),
        Color::Named(NamedColor::Yellow) => Color::Named(NamedColor::BrightYellow),
        Color::Named(NamedColor::Blue) => Color::Named(NamedColor::BrightBlue),
        Color::Named(NamedColor::Magenta) => Color::Named(NamedColor::BrightMagenta),
        Color::Named(NamedColor::Cyan) => Color::Named(NamedColor::BrightCyan),
        Color::Named(NamedColor::White) => Color::Named(NamedColor::BrightWhite),
        Color::Indexed(idx @ 0..=7) => Color::Indexed(idx + 8),
        other => other,
    }
}

#[cfg(test)]
mod index_to_rgb_tests {
    use super::{Rgb, TerminalPalette, index_to_rgb};

    #[test]
    fn maps_every_query_range() {
        let mut p = TerminalPalette {
            foreground: 0x0a0b0c,
            background: 0x112233,
            cursor: 0xff00ff,
            ..TerminalPalette::default()
        };
        p.ansi[1] = 0xf38ba8;

        // ANSI palette entries.
        assert_eq!(
            index_to_rgb(&p, 1),
            Some(Rgb {
                r: 0xf3,
                g: 0x8b,
                b: 0xa8
            })
        );
        // Cube: index 16 is (0,0,0); index 231 is (255,255,255).
        assert_eq!(index_to_rgb(&p, 16), Some(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(
            index_to_rgb(&p, 231),
            Some(Rgb {
                r: 255,
                g: 255,
                b: 255
            })
        );
        // Gray ramp: 232 → 8, 255 → 238.
        assert_eq!(index_to_rgb(&p, 232), Some(Rgb { r: 8, g: 8, b: 8 }));
        assert_eq!(
            index_to_rgb(&p, 255),
            Some(Rgb {
                r: 238,
                g: 238,
                b: 238
            })
        );
        // OSC 10/11/12 → fg/bg/cursor.
        assert_eq!(
            index_to_rgb(&p, 256),
            Some(Rgb {
                r: 0x0a,
                g: 0x0b,
                b: 0x0c
            })
        );
        assert_eq!(
            index_to_rgb(&p, 257),
            Some(Rgb {
                r: 0x11,
                g: 0x22,
                b: 0x33
            })
        );
        assert_eq!(
            index_to_rgb(&p, 258),
            Some(Rgb {
                r: 0xff,
                g: 0x00,
                b: 0xff
            })
        );
        // vte rejects indices past Cursor — so do we.
        assert_eq!(index_to_rgb(&p, 259), None);
    }
}
