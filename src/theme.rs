//! The "obsidian-ember" theme: palette, gradient ramp, block/braille glyph
//! encoders and the easing curve used by the panel animation.
//!
//! Everything here is pure — no terminal I/O, no app state — so the visual
//! primitives are unit-testable.

use ratatui::style::{Color, Modifier, Style};

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

// ------------------------------------------------------------------ palette

/// Terminal background.
pub const BASE: Rgb = Rgb(0x0b, 0x0e, 0x14);
/// Panel / block interior.
pub const PANEL: Rgb = Rgb(0x11, 0x15, 0x1f);
/// Zebra stripe A (even rows).
pub const ZEBRA_A: Rgb = Rgb(0x0e, 0x12, 0x1b);
/// Zebra stripe B (odd rows) — deliberately only a few units apart.
pub const ZEBRA_B: Rgb = Rgb(0x13, 0x18, 0x23);
/// Body text.
pub const TEXT: Rgb = Rgb(0xd7, 0xdc, 0xe5);
/// De-emphasised text (rules, pids, hints).
pub const MUTED: Rgb = Rgb(0x6b, 0x74, 0x88);
/// Block borders.
pub const BORDER: Rgb = Rgb(0x26, 0x2e, 0x40);
/// Selected-row background (accent-tinted).
pub const SEL_BG: Rgb = Rgb(0x14, 0x2e, 0x2b);
/// Backdrop behind a modal.
pub const SCRIM: Rgb = Rgb(0x05, 0x07, 0x0a);

/// Gradient stop 0 — cool / low magnitude.
pub const MINT: Rgb = Rgb(0x2d, 0xd4, 0xa7);
/// Gradient stop 1 — warm / mid magnitude.
pub const AMBER: Rgb = Rgb(0xf5, 0xa5, 0x24);
/// Gradient stop 2 — hot / high magnitude.
pub const EMBER: Rgb = Rgb(0xf0, 0x44, 0x2c);

// ------------------------------------------------------------------- glyphs

/// Left-to-right partial block glyphs, 1/8 through 8/8.
pub const EIGHTHS: [char; 8] = ['\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}', '\u{2588}'];
/// The braille pattern with no dots set (U+2800).
pub const BRAILLE_BLANK: char = '\u{2800}';
/// Left-edge marker drawn beside the selected row.
pub const SELECT_MARK: &str = "\u{258d}";

/// Dot bit for [column][row-from-top] in a braille pattern cell.
const BRAILLE_DOTS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

// -------------------------------------------------------------------- color

pub fn lerp(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round().clamp(0.0, 255.0) as u8;
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// The signature magnitude ramp: mint (0.0) -> amber (0.5) -> ember (1.0).
pub fn ramp(t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        lerp(MINT, AMBER, t * 2.0)
    } else {
        lerp(AMBER, EMBER, (t - 0.5) * 2.0)
    }
}

/// Pull a color toward the panel background — used for the dim half of the
/// GROWING pulse and for scrim'd backdrop cells.
pub fn shade(c: Rgb, toward_bg: f64) -> Rgb {
    lerp(c, PANEL, toward_bg)
}

fn dist2(a: Rgb, b: Rgb) -> i32 {
    let d = |x: u8, y: u8| {
        let v = x as i32 - y as i32;
        v * v
    };
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

/// Nearest xterm-256 index for an RGB triple, considering both the 6x6x6 color
/// cube (16..232) and the 24-step grayscale ramp (232..256).
pub fn nearest_256(c: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let idx = |v: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, &l)| (l as i32 - v as i32).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (idx(c.0), idx(c.1), idx(c.2));
    let cube = Rgb(LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;

    let avg = (c.0 as i32 + c.1 as i32 + c.2 as i32) / 3;
    let step = (((avg - 8) as f64) / 10.0).round().clamp(0.0, 23.0) as i32;
    let gv = (8 + 10 * step) as u8;
    let gray = Rgb(gv, gv, gv);

    if dist2(c, gray) < dist2(c, cube) {
        (232 + step) as u8
    } else {
        cube_idx as u8
    }
}

/// How much color the terminal (or the user) will accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// `--no-color`: no styling at all.
    Plain,
    /// No truecolor advertised: map the palette to the nearest 256 index.
    Indexed,
    /// 24-bit RGB.
    #[default]
    True,
}

/// Palette resolver. Every style in the UI goes through one of these methods so
/// `--no-color` and the 256-color fallback are handled in exactly one place.
#[derive(Debug, Clone, Copy, Default)]
pub struct Theme {
    pub mode: ColorMode,
}

impl Theme {
    /// `--no-color` wins; otherwise truecolor iff `$COLORTERM` says so.
    pub fn detect(no_color: bool) -> Self {
        if no_color {
            return Self { mode: ColorMode::Plain };
        }
        let ct = std::env::var("COLORTERM").unwrap_or_default();
        let truecolor = ct.eq_ignore_ascii_case("truecolor") || ct.eq_ignore_ascii_case("24bit");
        Self { mode: if truecolor { ColorMode::True } else { ColorMode::Indexed } }
    }

    pub fn plain(&self) -> bool {
        self.mode == ColorMode::Plain
    }

    pub fn color(&self, c: Rgb) -> Color {
        match self.mode {
            ColorMode::Plain => Color::Reset,
            ColorMode::Indexed => Color::Indexed(nearest_256(c)),
            ColorMode::True => Color::Rgb(c.0, c.1, c.2),
        }
    }

    /// A style with only a foreground set (no-op when plain).
    pub fn fg(&self, c: Rgb) -> Style {
        if self.plain() {
            Style::default()
        } else {
            Style::default().fg(self.color(c))
        }
    }

    /// Foreground over an explicit background.
    pub fn on(&self, fg: Rgb, bg: Rgb) -> Style {
        if self.plain() {
            Style::default()
        } else {
            Style::default().fg(self.color(fg)).bg(self.color(bg))
        }
    }

    /// Keep a modifier only when styling is enabled.
    pub fn modifier(&self, s: Style, m: Modifier) -> Style {
        if self.plain() {
            s
        } else {
            s.add_modifier(m)
        }
    }

    pub fn dim(&self) -> Style {
        self.fg(MUTED)
    }

    pub fn bold(&self, c: Rgb) -> Style {
        self.modifier(self.fg(c), Modifier::BOLD)
    }
}

// --------------------------------------------------------------------- bars

/// Split a bar of `width` cells filled to `frac` (0..=1) into per-cell
/// (glyph, position) pairs. `position` is the cell's fractional offset across
/// the bar so the caller can color each cell along the gradient.
pub fn bar_cells(frac: f64, width: usize) -> Vec<(char, f64)> {
    if width == 0 {
        return Vec::new();
    }
    let eighths = (frac.clamp(0.0, 1.0) * (width * 8) as f64).round() as i64;
    (0..width)
        .map(|i| {
            let rem = eighths - (i as i64) * 8;
            let ch = if rem >= 8 {
                '\u{2588}'
            } else if rem <= 0 {
                ' '
            } else {
                EIGHTHS[(rem - 1) as usize]
            };
            (ch, (i as f64 + 0.5) / width as f64)
        })
        .collect()
}

// ------------------------------------------------------------------ braille

/// Render `data` into a `width` x `height` grid of braille cells (2 samples per
/// cell column, 4 dot rows per cell row). Each entry is the glyph plus that
/// cell column's normalised magnitude, for gradient coloring.
///
/// The series is scaled between its own min and max so flat-but-large history
/// still shows shape. Columns with no data are blank braille (U+2800), which
/// keeps the row a uniform width.
pub fn braille_grid(data: &[u64], width: usize, height: usize) -> Vec<Vec<(char, f64)>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let cols = width * 2;
    let n = data.len();

    // Newest sample on the right. A longer history is bucket-averaged down;
    // a shorter one is left-padded with gaps.
    let mut vals: Vec<Option<u64>> = Vec::with_capacity(cols);
    if n == 0 {
        vals.resize(cols, None);
    } else if n >= cols {
        for c in 0..cols {
            let s = c * n / cols;
            let e = (((c + 1) * n) / cols).max(s + 1).min(n);
            let sum: u128 = data[s..e].iter().map(|&v| v as u128).sum();
            vals.push(Some((sum / (e - s) as u128) as u64));
        }
    } else {
        let pad = cols - n;
        for c in 0..cols {
            vals.push(if c < pad { None } else { Some(data[c - pad]) });
        }
    }

    let lo = vals.iter().flatten().copied().min().unwrap_or(0);
    let hi = vals.iter().flatten().copied().max().unwrap_or(0);
    let span = (hi - lo) as f64;
    let total = (height * 4) as i64;

    // Per-column dot fill height (0 = no data) and normalised magnitude.
    let levels: Vec<(i64, f64)> = vals
        .iter()
        .map(|v| match v {
            None => (0, 0.0),
            Some(x) => {
                let t = if span <= 0.0 { 0.5 } else { (*x - lo) as f64 / span };
                let lvl = (t * total as f64).round() as i64;
                (lvl.clamp(1, total), t)
            }
        })
        .collect();

    (0..height)
        .map(|row| {
            (0..width)
                .map(|cell| {
                    let mut bits: u8 = 0;
                    let mut mag: f64 = 0.0;
                    for sub in 0..2usize {
                        let (lvl, t) = levels[cell * 2 + sub];
                        if lvl > 0 {
                            mag = mag.max(t);
                        }
                        for dot in 0..4usize {
                            // Global dot row from the top of the chart.
                            let g = (row * 4 + dot) as i64;
                            if lvl > 0 && g >= total - lvl {
                                bits |= BRAILLE_DOTS[sub][dot];
                            }
                        }
                    }
                    (char::from_u32(0x2800 + bits as u32).unwrap_or(BRAILLE_BLANK), mag)
                })
                .collect()
        })
        .collect()
}

/// One-row braille sparkline.
pub fn braille_line(data: &[u64], width: usize) -> Vec<(char, f64)> {
    braille_grid(data, width, 1).pop().unwrap_or_default()
}

// -------------------------------------------------------------------- easing

/// Cubic ease-out, clamped to 0..=1.
pub fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_hits_its_stops_and_stays_monotone_in_warmth() {
        assert_eq!(ramp(0.0), MINT);
        assert_eq!(ramp(0.5), AMBER);
        assert_eq!(ramp(1.0), EMBER);
        // Out-of-range input clamps rather than wrapping or panicking.
        assert_eq!(ramp(-3.0), MINT);
        assert_eq!(ramp(9.0), EMBER);
        // Green falls monotonically across the whole ramp — that is what
        // makes "hotter" read as hotter — and red rises over the cool half.
        let (a, b, c) = (ramp(0.1), ramp(0.5), ramp(0.9));
        assert!(a.1 > b.1 && b.1 > c.1, "green not monotone: {a:?} {b:?} {c:?}");
        assert!(a.0 < b.0, "red does not rise toward amber: {a:?} {b:?}");
        assert!(c.1 < 100 && c.0 > 200, "hot end is not ember-ish: {c:?}");
    }

    #[test]
    fn indexed_fallback_maps_palette_into_the_256_cube() {
        // Pure black/white land on the cube corners, mid-grey on the grey ramp.
        assert_eq!(nearest_256(Rgb(0, 0, 0)), 16);
        assert_eq!(nearest_256(Rgb(255, 255, 255)), 231);
        let grey = nearest_256(Rgb(0x80, 0x80, 0x80));
        assert!((232..=255).contains(&grey), "{grey}");
        // Theme mode selects the representation.
        let t = Theme { mode: ColorMode::True };
        assert_eq!(t.color(MINT), Color::Rgb(0x2d, 0xd4, 0xa7));
        let i = Theme { mode: ColorMode::Indexed };
        assert_eq!(i.color(MINT), Color::Indexed(nearest_256(MINT)));
        let p = Theme { mode: ColorMode::Plain };
        assert_eq!(p.color(MINT), Color::Reset);
        assert_eq!(p.fg(EMBER), Style::default());
        assert_eq!(p.bold(EMBER), Style::default());
    }

    #[test]
    fn bar_cells_fill_left_to_right_with_partial_tail() {
        let full = bar_cells(1.0, 4);
        assert_eq!(full.iter().map(|c| c.0).collect::<String>(), "████");
        let empty = bar_cells(0.0, 4);
        assert_eq!(empty.iter().map(|c| c.0).collect::<String>(), "    ");
        // Half of 4 cells = 2 full blocks then nothing.
        assert_eq!(bar_cells(0.5, 4).iter().map(|c| c.0).collect::<String>(), "██  ");
        // 12/16 eighths = 1 full cell + 4/8 of the next.
        assert_eq!(bar_cells(0.75, 2).iter().map(|c| c.0).collect::<String>(), "█▌");
        // Position runs 0..1 across the bar so colors sweep the ramp.
        let p: Vec<f64> = bar_cells(1.0, 4).iter().map(|c| c.1).collect();
        assert!(p[0] < p[3] && p[0] > 0.0 && p[3] < 1.0);
        assert!(bar_cells(0.5, 0).is_empty());
    }

    #[test]
    fn braille_encodes_dots_from_the_bottom_up() {
        // Every glyph is in the braille block.
        let row = braille_line(&[1, 2, 3, 4, 5, 6, 7, 8], 4);
        assert_eq!(row.len(), 4);
        assert!(row.iter().all(|(c, _)| ('\u{2800}'..='\u{28ff}').contains(c)), "{row:?}");
        // A rising series ends brighter (higher magnitude) than it starts.
        assert!(row[0].1 < row[3].1);

        // A flat series has no span, so every column sits at mid height: dot
        // rows 2 and 3 of both columns => 0x04|0x40 | 0x20|0x80.
        let flat = braille_line(&[7, 7], 1);
        assert_eq!(flat[0].0, char::from_u32(0x2800 + 0xe4).unwrap());
        // Minimum sample keeps one bottom dot (0x40) so the series never
        // disappears; the maximum fills its whole column.
        let hi = braille_line(&[0, 9], 1);
        // Col 0 at min (bottom dot only) + col 1 full column.
        assert_eq!(hi[0].0, char::from_u32(0x2800 + 0x40 + (0x08 | 0x10 | 0x20 | 0x80)).unwrap());

        // No data at all is uniformly blank, still the right width.
        let blank = braille_line(&[], 6);
        assert_eq!(blank.len(), 6);
        assert!(blank.iter().all(|(c, _)| *c == BRAILLE_BLANK));

        // A short history is right-aligned: leading columns stay blank.
        let short = braille_line(&[5], 3);
        assert_eq!(short[0].0, BRAILLE_BLANK);
        assert_ne!(short[2].0, BRAILLE_BLANK);

        // Multi-row charts stack: the bottom row carries dots for every column.
        let grid = braille_grid(&[1, 5, 9, 3], 2, 3);
        assert_eq!(grid.len(), 3);
        assert!(grid[2].iter().all(|(c, _)| *c != BRAILLE_BLANK));
        assert!(braille_grid(&[1, 2], 0, 3).is_empty());
    }

    #[test]
    fn ease_out_cubic_starts_fast_and_settles() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // Clamped outside the unit interval.
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(4.0), 1.0);
        // Ease-OUT: more than half the distance is covered by the halfway point.
        assert!(ease_out_cubic(0.5) > 0.5);
        assert!((ease_out_cubic(0.5) - 0.875).abs() < 1e-9);
        // Monotonically increasing, decelerating.
        let d1 = ease_out_cubic(0.2) - ease_out_cubic(0.1);
        let d2 = ease_out_cubic(0.9) - ease_out_cubic(0.8);
        assert!(d1 > d2 && d2 > 0.0);
    }
}
