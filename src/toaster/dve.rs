// SPDX-License-Identifier: GPL-3.0-or-later

//! The Toaster's digital video effects (DVE): moves, squeezes, flips,
//! push-ons and the rest of the 4000FX "Digital", "Trajectory" and
//! "Push-Pull" families.
//!
//! The card has no geometry engine. With EH0's `LINP` (and `LUT`) clear,
//! framestore playback stops counting lines and pixels by itself and
//! follows an address the Amiga sends with every line:
//!
//! - the line comes from the line number `L` in the line's header (sprite
//!   1's B and I words, whatever kind of header the line carries): the
//!   output line shows framestore line `2L + field`, so picture row `y`
//!   (framestore line `y + 1`) shows picture row `2L + f - 1`, `f` the
//!   row's field parity;
//! - across the line, every quad (four card pixels, one subcarrier cycle)
//!   carries an 8-bit source-quad number on the B and I lines, drawn the
//!   same way as the read-back labels (I the high nibble, B the low, first
//!   pixel most significant): output card quad `c` shows framestore
//!   samples `4·Q(c) .. 4·Q(c)+3`.
//!
//! Quad codes 0-191 address the line; `$E8`-`$EF` are the card's own
//! colour quads (borders, the luma-key backgrounds); `$FF` is "no
//! picture here" (the background shows through under QUAF), `$F8`-`$FE`
//! drop-shadow quads under TRANSQUAD. The rest show black.
//!
//! Recovered from the switcher (`ScaleCalc`, `ScaleXOut`, `ScaleYOut`,
//! `InitDVESprite1Buff`, `_BCPalette`) and checked against the 4000FX
//! effect files, whose final frames decode to the identity map. See
//! `docs/internals/video.md`.

use super::composite;
use super::video::{black, AMIGA_LINE0, PICTURE_H, PICTURE_LINE0, PICTURE_W, PICTURE_X0};
use super::Toaster;
use crate::videoslot::FieldPixels;

/// Card quads per line.
const QUADS: usize = 192;

/// Quad code: transparent, the background shows.
pub const QUAF_BYTE: u8 = 0xFF;

/// The card's colour quads `$E8`-`$EF` (`_BCPalette`: Y on the 70-200
/// scale, I and Q signed in the same units): black, white, then the bar
/// colours yellow, cyan, green, magenta, red, blue.
const COLOUR_QUADS: [(f32, f32, f32); 8] = [
    (71.0, 0.0, 0.0),
    (200.0, 0.0, 0.0),
    (157.0, 31.0, -30.0),
    (138.0, -58.0, -20.0),
    (128.0, -27.0, -51.0),
    (110.0, 27.0, 51.0),
    (100.0, 58.0, 20.0),
    (81.0, -31.0, 30.0),
];

fn colour_quad(code: u8) -> u32 {
    let (y, i, q) = COLOUR_QUADS[usize::from(code - 0xE8)];
    let scale = composite::WHITE - composite::BLACK;
    composite::rgba_of_yiq((y - composite::BLACK) / scale, i / scale, q / scale)
}

/// A quad's source number in the Warp palette (`PACO_Warp0-3`): red bit 0
/// of each pixel's low nibble is the B line (the low nibble of Q), green
/// bit 0 the I line (the high nibble), first pixel most significant. Only
/// those two bits: the switcher's interface stays on screen during an
/// effect, and its colours use the other low bits for the overlay and the
/// alpha codes.
fn warp_label(row: &[u32], quad_col: i64, scale: i64) -> Option<u8> {
    let mut hi = 0u8;
    let mut lo = 0u8;
    for j in 0..4i64 {
        let col = usize::try_from(quad_col + j * scale).ok()?;
        let [r, g, _, _] = row.get(col)?.to_le_bytes();
        hi |= (g & 1) << (3 - j);
        lo |= (r & 1) << (3 - j);
    }
    Some((hi << 4) | lo)
}

/// Source rows a line number can reach: the picture and the framestore
/// lines below it (the border lines, `L` = 252).
pub const SOURCE_ROWS: usize = super::LINES - PICTURE_LINE0;

/// One frame's addressing.
pub struct DveMap {
    /// Per picture row, the source row: a picture row, or past the picture
    /// a framestore line below it (`None`: off the framestore).
    rows: Vec<Option<usize>>,
    /// Per picture pixel, the quad code.
    codes: Vec<u8>,
}

impl DveMap {
    /// Read the addressing off a field: the header and quad codes of the
    /// Amiga line under each picture row.
    pub fn from_field(field: &FieldPixels<'_>, x0: i64) -> Self {
        let scale = field.canvas_scale as i64;
        let mut rows = vec![None; PICTURE_H];
        let mut codes = vec![QUAF_BYTE; PICTURE_W * PICTURE_H];
        let mut quads = [QUAF_BYTE; QUADS];
        let mut last_vpos = None;
        for y in 0..PICTURE_H {
            let fl = y + PICTURE_LINE0;
            let f = fl & 1;
            let vpos = AMIGA_LINE0 + (fl >> 1) as u32;
            let Some(header) = Toaster::header(field, vpos) else {
                continue;
            };
            if last_vpos != Some(vpos) {
                last_vpos = Some(vpos);
                let row = field.row(vpos).unwrap_or(&[]);
                for (c, q) in quads.iter_mut().enumerate() {
                    *q = warp_label(row, x0 + (c as i64) * 4 * scale, scale).unwrap_or(QUAF_BYTE);
                }
            }
            let l = usize::from(((header.i >> 12) << 4) | (header.b >> 12));
            // Picture row = framestore line - 1.
            rows[y] = (2 * l + f)
                .checked_sub(PICTURE_LINE0)
                .filter(|&r| r < SOURCE_ROWS);
            for (x, code) in codes[y * PICTURE_W..(y + 1) * PICTURE_W]
                .iter_mut()
                .enumerate()
            {
                *code = quads[(x + PICTURE_X0) >> 2];
            }
        }
        Self { rows, codes }
    }

    /// Whether any line shows a framestore line below the picture.
    pub fn reaches_below(&self) -> bool {
        self.rows.iter().flatten().any(|&r| r >= PICTURE_H)
    }

    /// The effect's picture from a linear one: the framestore playback as
    /// addressed. `src` holds the picture rows and may go on with the
    /// lines below it; rows it lacks show black.
    pub fn apply(&self, src: &[u32], out: &mut [u32]) {
        for (y, line) in out.chunks_exact_mut(PICTURE_W).enumerate() {
            let codes = &self.codes[y * PICTURE_W..(y + 1) * PICTURE_W];
            let src_row = self.rows[y].and_then(|r| src.get(r * PICTURE_W..(r + 1) * PICTURE_W));
            for (x, (o, &q)) in line.iter_mut().zip(codes).enumerate() {
                *o = match q {
                    0..=191 => {
                        // Card pixel p = x + 12 shows sample 4q + (p & 3).
                        let p = x + PICTURE_X0;
                        let sample = 4 * usize::from(q) + (p & 3);
                        src_row
                            .and_then(|row| {
                                sample.checked_sub(PICTURE_X0).and_then(|sx| row.get(sx))
                            })
                            .copied()
                            .unwrap_or_else(black)
                    }
                    0xE8..=0xEF => colour_quad(q),
                    _ => black(),
                };
            }
        }
    }

    /// The fader's B weight per pixel for the DVE key modes: under QUAF
    /// every quad but `$FF` shows the effect; under TRANSQUAD only the
    /// drop-shadow quads do, a pixel at a time (bit `3 - j` of the code).
    pub fn key(&self, transquad: bool, cd: u8) -> Vec<u8> {
        self.codes
            .iter()
            .enumerate()
            .map(|(i, &q)| {
                let j = (i % PICTURE_W + PICTURE_X0) & 3;
                let on = if transquad {
                    q >= 0xF8 && (q >> (3 - j)) & 1 != 0
                } else {
                    q != QUAF_BYTE
                };
                if on {
                    cd
                } else {
                    0
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with(rows: Vec<Option<usize>>, quad_of: impl Fn(usize, usize) -> u8) -> DveMap {
        let mut codes = vec![QUAF_BYTE; PICTURE_W * PICTURE_H];
        for y in 0..PICTURE_H {
            for x in 0..PICTURE_W {
                codes[y * PICTURE_W + x] = quad_of(y, (x + PICTURE_X0) >> 2);
            }
        }
        DveMap { rows, codes }
    }

    fn ramp() -> Vec<u32> {
        (0..PICTURE_W * PICTURE_H)
            .map(|i| u32::from_le_bytes([(i % 251) as u8, (i / PICTURE_W % 251) as u8, 7, 0xFF]))
            .collect()
    }

    #[test]
    fn the_identity_map_shows_the_framestore_unchanged() {
        // Quad c addresses itself, every row its own line.
        let map = map_with((0..PICTURE_H).map(Some).collect(), |_, c| c as u8);
        let src = ramp();
        let mut out = vec![0; src.len()];
        map.apply(&src, &mut out);
        assert_eq!(out, src);
    }

    #[test]
    fn the_quad_number_rides_on_red_and_green_bit_zero_only() {
        // Q = $A5: I line (green) 1010, B line (red) 0101, under interface
        // colours whose blue low bits are set throughout.
        let px = |r: u8, g: u8| u32::from_le_bytes([0x90 | r, 0x92 | g, 0x9B, 0xFF]);
        let row = [px(0, 1), px(1, 0), px(0, 1), px(1, 0)];
        assert_eq!(warp_label(&row, 0, 1), Some(0xA5));
    }

    #[test]
    fn border_lines_come_from_below_the_picture() {
        // L = 252: framestore line 504/505, past the picture's 480 rows,
        // where the switcher keeps the border colour.
        let mut rows: Vec<Option<usize>> = (0..PICTURE_H).map(Some).collect();
        rows[0] = Some(503);
        let map = map_with(rows, |_, c| c as u8);
        assert!(map.reaches_below());
        let mut src = ramp();
        let border = u32::from_le_bytes([200, 10, 10, 0xFF]);
        src.extend(std::iter::repeat_n(
            border,
            PICTURE_W * (SOURCE_ROWS - PICTURE_H),
        ));
        let mut out = vec![0; PICTURE_W * PICTURE_H];
        map.apply(&src, &mut out);
        assert!(out[..PICTURE_W].iter().all(|&p| p == border));
        // Without those lines the border shows black, not a panic.
        map.apply(&src[..PICTURE_W * PICTURE_H], &mut out);
        assert!(out[..PICTURE_W].iter().all(|&p| p == black()));
    }

    #[test]
    fn a_push_moves_the_picture_by_whole_quads() {
        // Everything one quad to the right, rows two lines down.
        let map = map_with(
            (0..PICTURE_H).map(|y| y.checked_sub(2)).collect(),
            |_, c| c.saturating_sub(1) as u8,
        );
        let src = ramp();
        let mut out = vec![0; src.len()];
        map.apply(&src, &mut out);
        assert_eq!(out[10 * PICTURE_W + 100], src[8 * PICTURE_W + 96]);
        assert_eq!(out[0], black());
    }

    #[test]
    fn transparent_and_colour_quads() {
        let map = map_with((0..PICTURE_H).map(Some).collect(), |_, c| match c {
            0..=50 => QUAF_BYTE,
            51..=60 => 0xE9,
            61..=70 => 0xF9,
            _ => c as u8,
        });
        let src = ramp();
        let mut out = vec![0; src.len()];
        map.apply(&src, &mut out);
        let at = |c: usize| out[4 * c - PICTURE_X0];
        assert_eq!(at(10), black());
        let white = at(55).to_le_bytes();
        assert!(
            white[0] > 240 && white[1] > 240 && white[2] > 240,
            "{white:?}"
        );
        let quaf = map.key(false, 200);
        assert_eq!(quaf[4 * 10 - PICTURE_X0], 0);
        assert_eq!(quaf[4 * 100 - PICTURE_X0], 200);
        // TRANSQUAD: $F9 = %1111_1001: pixels 0 and 3 of the quad.
        let trans = map.key(true, 255);
        let q = 4 * 65 - PICTURE_X0;
        assert_eq!(&trans[q..q + 4], &[255, 0, 0, 255]);
        assert_eq!(trans[4 * 100 - PICTURE_X0], 0);
    }
}
