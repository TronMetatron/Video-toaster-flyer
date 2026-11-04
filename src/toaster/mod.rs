// SPDX-License-Identifier: GPL-3.0-or-later

//! NewTek Video Toaster 4000: the video-slot card's interface to the Amiga.
//!
//! The Toaster has no registers on the Amiga's bus. Its switcher software
//! talks to it through the picture: every scan line the Amiga displays can
//! carry a line header in sprites 0 and 1 (attached, at comparator position
//! `$5B`) and, in the bitplanes, a line of data for the card's framestore or
//! a request for data from it. The card answers requests on the parallel
//! port data pins (`$BFE101`), which the switcher reads at beam-timed
//! positions. The protocol here was recovered from NewTek's released
//! switcher source (`cop.a`, `autocal.a`, `vthand.a`, `elh.i`) and their
//! 4.2 binaries; see the project notes for the derivation.
//!
//! The card samples four digital lines per hi-res pixel, each wired to two
//! RGB bits of the video slot (NewTek's names; AGA low-nibble bits):
//!
//! | line | bits      | sprite word (header) | bitplane (data) |
//! |------|-----------|----------------------|-----------------|
//! | R    | R2 or R1  | sprite 0 A           | plane 0         |
//! | G    | B2 or G1  | sprite 0 B           | plane 1         |
//! | B    | R0 or B0  | sprite 1 A           | plane 2         |
//! | I    | G0 or B1  | sprite 1 B           | plane 3         |
//!
//! A data pixel's value is `R | G<<1 | B<<2 | I<<3`. The header's sixteen
//! lo-res sprite pixels give four 16-bit words (bit 15 first); NewTek uses
//! bits 15-8:
//!
//! - `R` bits 15-12: extended-header select (`%1111` = none, else one of
//!   eight codes), or on a read request the pixel-in-quad select.
//! - `G` bit 15: framestore write; bit 14: high nibble; bit 13: the
//!   colour-subcarrier-inverted ("complemented") copy; bit 12: field II/IV.
//! - `B`/`I` bits 15-12: line number (low/high nibble). A line that carries
//!   nothing has `R = %1111` with the write bit clear (the switcher gives it
//!   line number 255).
//!
//! A framestore line is written as four scan lines (normal high and low
//! nibbles, then the complemented pair); interlaced line `2L + 1` for
//! fields I/III, `2L` for II/IV. A read request draws a bitmap in which
//! every quad (four pixels, one colour clock) is labelled with the quad
//! number to return -- the `I` line carries the label's high nibble and `B`
//! its low nibble across the quad's four pixels -- and the card puts
//! framestore byte `4Q + pixel` of the requested line on the port a fixed
//! pipeline after quad `Q` passed its input. The extended line headers
//! (EH0-EH7, EH60-EH6F) hold the card's control state: the input muxes,
//! mattes, keyer, pedestal, gain, hue and the flag bits. EH0 is also the
//! header of every line of the switcher's normal display, so its bits
//! change within a field; the write bank comes from its `OBR` bit (reads
//! use it inverted), taken in beam order.
//!
//! Pinned against the 4.2 switcher's own tests: its bitmaps' first pixel
//! is the card's pixel 12 (quad 3), which puts pixel 0 at lo-res `$5A`,
//! two lo-res pixels before the header sprite; a quad label is the plain
//! quad number on the wire; and ten quads of read pipeline put each of
//! the `HS.default` read positions inside its label run.

pub mod composite;
pub mod dve;
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub mod media;
pub mod regs;
pub mod video;

use crate::videoslot::{BeamPos, FieldPixels, LiveField, VideoSlotDevice};

/// Pixels per framestore line (the AGA switcher's 768-wide bitmap).
pub const LINE_PIXELS: usize = 768;
/// Interlaced framestore lines: the header's eight-bit line number covers
/// 256 lines per field (0-480 carry the picture; the switcher's read
/// self-test uses line 504, and 510/511 are the "dummy" line number it
/// puts on lines that carry nothing).
pub const LINES: usize = 512;

/// How far right the card stores a write line sent with EH0 `LUST` clear:
/// the switcher scrolls the second pass of its zipper write 16 hi-res
/// pixels left (cop.a, "shift left 16 HIRES pixels").
const ZIPPER_SHIFT: usize = 16;
/// DV1 and DV2.
pub const BANKS: usize = 2;
/// Normal and complemented copies of each line.
const SIGNALS: usize = 2;

/// Comparator position of the line header sprite.
const HEADER_LORES: i32 = 0x5B;
/// Comparator position of the card's pixel 0. Quad 0 is the two lo-res
/// pixels before the header sprite, the sprite's four used lo-res pixels
/// are quads 1-2, and the switcher's bitmaps -- whose first pixel
/// `TestReadScanLine` expects to read back as quad 3, pixel 0 -- start at
/// pixel 12. Measured against the 4.2 switcher: its written ramp's first
/// byte lands at this window's pixel 12.
const PIXEL0_LORES: i32 = 0x5A;
/// Hi-res pixels between a quad label passing the card's input and the
/// labelled byte being on the parallel port. Fitted to the switcher's
/// `HS.default` read positions: each of its ten reads per line must land
/// inside an 18-quad run of one label, and the CIA access (E-clock
/// synchronised) happens up to five clocks after the beam position it
/// waited for; ten quads keeps at least four quads of margin either way.
const READ_PIPELINE_PIXELS: i64 = 40;

/// The extended headers as the switcher sets them up (`VTSetUp`), so the
/// card shows something sensible before it has been told otherwise.
const ELH_DEFAULTS: [(usize, Header); 10] = [
    (
        0,
        Header {
            r: 0x8E00,
            g: 0x0500,
            b: 0x0100,
            i: 0x0F00,
        },
    ),
    (
        1,
        Header {
            r: 0x4800,
            g: 0x0000,
            b: 0x0F00,
            i: 0x0F00,
        },
    ),
    (
        2,
        Header {
            r: 0xA000,
            g: 0x0000,
            b: 0x0000,
            i: 0x0F00,
        },
    ),
    (
        3,
        Header {
            r: 0x6000,
            g: 0x0000,
            b: 0x0000,
            i: 0x0F00,
        },
    ),
    (
        4,
        Header {
            r: 0x9F00,
            g: 0x0F00,
            b: 0x0700,
            i: 0x0F00,
        },
    ),
    (
        5,
        Header {
            r: 0x5800,
            g: 0x0800,
            b: 0x0000,
            i: 0x0F00,
        },
    ),
    (
        ELH_EH6_BASE + 8,
        Header {
            r: 0xB000,
            g: 0x0800,
            b: 0x0800,
            i: 0x0F00,
        },
    ),
    (
        ELH_EH6_BASE + 9,
        Header {
            r: 0xB000,
            g: 0x0800,
            b: 0x0900,
            i: 0x0F00,
        },
    ),
    (
        ELH_EH6_BASE + 0xA,
        Header {
            r: 0xB000,
            g: 0x0800,
            b: 0x0A00,
            i: 0x0F00,
        },
    ),
    (
        ELH_EH6_BASE + 0xB,
        Header {
            r: 0xB000,
            g: 0x0800,
            b: 0x0B00,
            i: 0x0F00,
        },
    ),
];

/// Number of extended line headers: EH0-EH5, EH60-EH6F, EH7.
const ELH_COUNT: usize = 23;
const ELH_EH0: usize = 0;
const ELH_EH6_BASE: usize = 6;
const ELH_EH7: usize = 22;

/// EH0 `R` word bit 8: output/write bank select.
const EH0R_OBR: u16 = 1 << 8;

/// One decoded line header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// An extended header: `index` into the ELH file.
    Elh { index: usize },
    /// A line carrying nothing (`I` nibble `$F`).
    Dummy,
    /// A framestore write.
    Write {
        line: usize,
        high_nibble: bool,
        complemented: bool,
    },
    /// A read request.
    Read {
        line: usize,
        pixel: usize,
        complemented: bool,
    },
}

/// The four header words of a line, bit 15 = the sprite's first pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub r: u16,
    pub g: u16,
    pub b: u16,
    pub i: u16,
}

impl Header {
    /// Extended-header select codes (`elh.i` `VTI_E*`), indexed as the ELH
    /// file is.
    fn elh_index(self) -> Option<usize> {
        let base = match self.r >> 12 {
            0b1000 => 0,
            0b0100 => 1,
            0b1010 => 2,
            0b0110 => 3,
            0b1001 => 4,
            0b0101 => 5,
            // EH6's page is in the B word (`VTI_E6x` = x << 8 there).
            0b1011 => ELH_EH6_BASE + usize::from((self.b >> 8) & 0xF),
            0b0111 => ELH_EH7,
            _ => return None,
        };
        // EH6's sixteen pages occupy the file between EH5 and EH7.
        Some(base)
    }

    pub fn kind(self) -> LineKind {
        if let Some(index) = self.elh_index() {
            return LineKind::Elh { index };
        }
        let l = usize::from(((self.i >> 12) << 4) | (self.b >> 12));
        let field_ii = self.g & 0x1000 != 0;
        let complemented = self.g & 0x2000 != 0;
        let line = 2 * l + usize::from(!field_ii);
        if self.g & 0x8000 != 0 {
            return LineKind::Write {
                line,
                high_nibble: self.g & 0x4000 != 0,
                complemented,
            };
        }
        // Pixel-in-quad codes %0000 %0010 %1100 %1110; anything else with
        // no extended header and no write bit carries nothing.
        let pixel = match self.r >> 12 {
            0b0000 => 0,
            0b0010 => 1,
            0b1100 => 2,
            0b1110 => 3,
            _ => return LineKind::Dummy,
        };
        LineKind::Read {
            line,
            pixel,
            complemented,
        }
    }
}

/// The four-bit value the card's input lines read from one RGBA8 pixel.
#[inline]
pub fn lines_of(pixel: u32) -> u8 {
    let [r, g, b, _] = pixel.to_le_bytes();
    let line_r = (r & 0x06) != 0;
    let line_g = (b & 0x04) != 0 || (g & 0x02) != 0;
    let line_b = (r & 0x01) != 0 || (b & 0x01) != 0;
    let line_i = (g & 0x01) != 0 || (b & 0x02) != 0;
    u8::from(line_r) | (u8::from(line_g) << 1) | (u8::from(line_b) << 2) | (u8::from(line_i) << 3)
}

/// Per-field activity, for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub fields: u64,
    pub elh_lines: u64,
    pub write_lines: u64,
    pub read_requests: u64,
    pub port_bytes: u64,
}

/// The card's two framestore banks: `[bank][copy][line][pixel]` composite
/// samples, the normal and the complemented copy of every line.
pub struct FrameStore {
    data: Vec<u8>,
}

impl FrameStore {
    fn new() -> Self {
        Self {
            data: vec![0; BANKS * SIGNALS * LINES * LINE_PIXELS],
        }
    }

    fn index(bank: usize, complemented: bool, line: usize, pixel: usize) -> usize {
        ((bank * SIGNALS + usize::from(complemented)) * LINES + line) * LINE_PIXELS + pixel
    }

    /// One line of one copy of one bank.
    pub fn line(&self, bank: usize, complemented: bool, line: usize) -> &[u8] {
        let i = Self::index(bank & 1, complemented, line % LINES, 0);
        &self.data[i..i + LINE_PIXELS]
    }

    pub fn line_mut(&mut self, bank: usize, complemented: bool, line: usize) -> &mut [u8] {
        let i = Self::index(bank & 1, complemented, line % LINES, 0);
        &mut self.data[i..i + LINE_PIXELS]
    }
}

pub struct Toaster {
    frame_store: FrameStore,
    video: video::Video,
    /// Lines this field that asked the card to record its input (`CERB`
    /// outside a framestore write).
    record_lines: u32,
    /// Raw extended line headers, last value written.
    elh: [Header; ELH_COUNT],
    /// Seen a valid extended header: the switcher is driving the card.
    active: bool,
    /// Lines of the field in progress already taken in through the live
    /// scan: `(field number, next line to take)`.
    scanned: Option<(u64, u32)>,
    stats: Stats,
    diag: bool,
    diag_lines_logged: u32,
    diag_fields: bool,
    diag_outputs: String,
    /// Show the switcher's interface over Preview when it asks to
    /// (`USERON`, the Toaster's "preview overlay" for one-monitor setups).
    preview_overlay: bool,
    /// The digital effect's addressing for the frame being composed.
    dve: Option<dve::DveMap>,
}

impl Default for Toaster {
    fn default() -> Self {
        Self::new()
    }
}

impl Toaster {
    /// A card with no host folder: no inputs, outputs not recorded to a
    /// folder of their own.
    pub fn new() -> Self {
        Self::with_media(None)
    }

    /// A card whose connectors are the folders under `media_root`
    /// (created if missing): see [`media`].
    pub fn with_media(media_root: Option<std::path::PathBuf>) -> Self {
        let mut elh = [Header::default(); ELH_COUNT];
        for (index, header) in ELH_DEFAULTS {
            elh[index] = header;
        }
        Self {
            frame_store: FrameStore::new(),
            video: video::Video::new(media_root),
            record_lines: 0,
            elh,
            active: false,
            scanned: None,
            stats: Stats::default(),
            diag: crate::envcfg::flag("COPPERLINE_DIAG_TOASTER"),
            diag_lines_logged: 0,
            diag_fields: crate::envcfg::var_os("COPPERLINE_DIAG_TOASTER")
                .is_some_and(|v| v == "fields"),
            diag_outputs: String::new(),
            preview_overlay: false,
            dve: None,
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn elh(&self, index: usize) -> Header {
        self.elh[index]
    }

    /// Show the switcher's interface over the Preview output when it asks
    /// for that (its "preview overlay", meant for a single composite
    /// monitor). Off by default: the interface has a window of its own.
    pub fn set_preview_overlay(&mut self, on: bool) {
        self.preview_overlay = on;
    }

    /// The latched control registers.
    pub fn regs(&self) -> regs::Regs<'_> {
        regs::Regs { elh: &self.elh }
    }

    /// One framestore byte.
    pub fn frame_store_byte(
        &self,
        bank: usize,
        complemented: bool,
        line: usize,
        pixel: usize,
    ) -> u8 {
        self.frame_store.line(bank, complemented, line)[pixel]
    }

    /// Bank a write goes to: EH0 `OBR`.
    fn write_bank(&self) -> usize {
        usize::from(self.elh[ELH_EH0].r & EH0R_OBR != 0)
    }

    /// Bank a read comes from: the switcher sets `OBR` for bank 0.
    fn read_bank(&self) -> usize {
        usize::from(self.elh[ELH_EH0].r & EH0R_OBR == 0)
    }

    /// Sample the line header sprite of row `vpos`.
    fn header(field: &FieldPixels<'_>, vpos: u32) -> Option<Header> {
        let row = field.row(vpos)?;
        // The serializer's first pixel lands one lo-res pixel after the
        // comparator match.
        let x0 = field.column_for_lores(HEADER_LORES + crate::bus::SPRITE_OUTPUT_DELAY_LORES);
        let step = 2 * field.canvas_scale as i64;
        let mut h = Header::default();
        for k in 0..16u32 {
            let col = usize::try_from(x0 + i64::from(k) * step).ok()?;
            let v = lines_of(*row.get(col)?);
            let bit = 15 - k;
            h.r |= u16::from(v & 1) << bit;
            h.g |= u16::from((v >> 1) & 1) << bit;
            h.b |= u16::from((v >> 2) & 1) << bit;
            h.i |= u16::from((v >> 3) & 1) << bit;
        }
        Some(h)
    }

    /// Canvas column of the card's pixel 0.
    fn pixel0_column(field: &FieldPixels<'_>) -> i64 {
        field.column_for_lores(PIXEL0_LORES)
    }

    fn store_write_line(
        &mut self,
        field: &FieldPixels<'_>,
        vpos: u32,
        line: usize,
        high_nibble: bool,
        complemented: bool,
    ) {
        let Some(row) = field.row(vpos) else {
            return;
        };
        if line >= LINES {
            return;
        }
        let bank = self.write_bank();
        let x0 = Self::pixel0_column(field);
        let step = field.canvas_scale as i64;
        // The second pass of a zipper write (LUST clear) lands 16 samples
        // right of where its pixels are on the Amiga line; the header
        // pixels left of the bitmap stay out of the picture.
        let shift = if self.regs().lust() { 0 } else { ZIPPER_SHIFT };
        let store = self.frame_store.line_mut(bank, complemented, line);
        for (p, byte) in store.iter_mut().enumerate() {
            let Some(src) = p.checked_sub(shift) else {
                continue;
            };
            if shift != 0 && src < video::PICTURE_X0 {
                continue;
            }
            let Ok(col) = usize::try_from(x0 + src as i64 * step) else {
                continue;
            };
            let Some(&pixel) = row.get(col) else {
                break;
            };
            let v = lines_of(pixel);
            *byte = if high_nibble {
                (*byte & 0x0F) | (v << 4)
            } else {
                (*byte & 0xF0) | v
            };
        }
        self.stats.write_lines += 1;
        if self.diag && self.diag_lines_logged < 400 {
            self.diag_lines_logged += 1;
            let bytes: Vec<u8> = self.frame_store.line(bank, complemented, line)[8..56].to_vec();
            log::info!(
                "toaster: write vpos={vpos} line={line} hi={high_nibble} comp={complemented} bank={bank} pixels 8.. {bytes:02X?}"
            );
        }
    }

    /// Act on one line's header: latch an extended header, store a written
    /// line, count a read request.
    fn take_line(&mut self, field: &FieldPixels<'_>, vpos: u32, header: Header) {
        match header.kind() {
            LineKind::Elh { index } => {
                if !self.active {
                    log::info!(
                        "toaster: first extended header on line {vpos}: the switcher is driving the card"
                    );
                }
                self.active = true;
                if self.diag && self.elh[index] != header {
                    log::info!(
                        "toaster: ELH {index} <- R={:04X} G={:04X} B={:04X} I={:04X} (vpos {vpos})",
                        header.r,
                        header.g,
                        header.b,
                        header.i
                    );
                }
                self.elh[index] = header;
                self.stats.elh_lines += 1;
            }
            // `G` bit 15 is CERB, "record this line". In write mode (EH0
            // WERB set, ABSE clear) what is recorded is the Amiga's data on
            // the line; otherwise it is the input the IS mux selects: live
            // video, or a grab.
            LineKind::Write {
                line,
                high_nibble,
                complemented,
            } if self.active && self.regs().write_mode() => {
                self.store_write_line(field, vpos, line, high_nibble, complemented);
            }
            LineKind::Write { .. } if self.active => {
                self.record_lines += 1;
            }
            LineKind::Read { .. } if self.active => {
                self.stats.read_requests += 1;
            }
            _ => {}
        }
    }

    /// Take in the lines of the field in progress that the beam has
    /// passed since the last scan, so a read on line N sees the headers
    /// (bank select, above all) of lines 0..N of its own field, as the
    /// card does.
    fn scan_to_beam(&mut self, live: &mut LiveField<'_>) {
        let beam = live.beam();
        let mut next = match self.scanned {
            Some((frame, next)) if frame == beam.frame => next,
            _ => live.first_vpos(),
        };
        while next < beam.vpos {
            if let Some(field) = live.line(next) {
                if let Some(header) = Self::header(&field, next) {
                    self.take_line(&field, next, header);
                }
            }
            next += 1;
        }
        self.scanned = Some((beam.frame, next));
    }

    /// The quad label drawn at canvas column `col` on `row`: the `I` line
    /// across the quad's four pixels is the label's high nibble, the `B`
    /// line its low nibble, first pixel = bit 3. (The switcher's source
    /// talks of complementing the label data, but that cancels against its
    /// inverted read palette: on the wire the label is the quad number.)
    fn quad_label(row: &[u32], quad_col: i64, scale: i64) -> Option<u8> {
        let mut hi = 0u8;
        let mut lo = 0u8;
        for j in 0..4i64 {
            let col = usize::try_from(quad_col + j * scale).ok()?;
            let v = lines_of(*row.get(col)?);
            hi |= ((v >> 3) & 1) << (3 - j);
            lo |= ((v >> 2) & 1) << (3 - j);
        }
        Some((hi << 4) | lo)
    }
}

impl VideoSlotDevice for Toaster {
    fn name(&self) -> &'static str {
        "Video Toaster 4000"
    }

    fn supplies_sync(&self) -> bool {
        true
    }

    fn field_complete(&mut self, field: &FieldPixels<'_>, at: BeamPos) {
        self.stats.fields += 1;
        // Lines the live scan already took in this field are done.
        let first_unscanned = match self.scanned.take() {
            Some((frame, next)) if frame == at.frame => next,
            _ => 0,
        };
        // `COPPERLINE_DIAG_TOASTER=fields`: dump every line header of one
        // field in forty.
        let dump = self.diag_fields && self.active && self.stats.fields.is_multiple_of(40);
        for row in 0..field.rows {
            let vpos = field.visible_start_vpos + row as u32;
            if vpos < first_unscanned {
                continue;
            }
            let Some(header) = Self::header(field, vpos) else {
                continue;
            };
            if dump {
                log::info!(
                    "toaster: field {} line {vpos}: R={:04X} G={:04X} B={:04X} I={:04X} {:?}",
                    self.stats.fields,
                    header.r,
                    header.g,
                    header.b,
                    header.i,
                    header.kind()
                );
            }
            self.take_line(field, vpos, header);
        }
        // A field with CERB lines outside a framestore write digitised the
        // IS input into the recording bank: live video, or a grab.
        let record = std::mem::take(&mut self.record_lines) > 0;
        self.video.set_live(record && self.regs().abse());
        let frame_done = self.video.begin_field();
        if frame_done {
            self.produce_outputs(field);
        }
        if record {
            self.record_input(field);
        }
    }

    fn video_outputs(&self) -> Vec<std::sync::Arc<crate::videoslot::CardOutput>> {
        self.video.outputs()
    }

    fn control_status(&self) -> serde_json::Value {
        self.status_value()
    }

    fn take_audio(&mut self, out: &mut Vec<(f32, f32)>) {
        self.video.take_audio(out);
    }

    fn control(
        &mut self,
        command: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match command {
            "input" => {
                let n = params
                    .get("input")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|n| (1..=media_inputs()).contains(&(*n as usize)))
                    .ok_or_else(|| format!("input must be 1-{}", media_inputs()))?
                    as usize;
                let file = match params.get("file") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(f)) if f.is_empty() => None,
                    Some(serde_json::Value::String(f)) => {
                        let path = std::path::PathBuf::from(f);
                        if !path.is_file() {
                            return Err(format!("{f}: no such file"));
                        }
                        Some(path)
                    }
                    Some(_) => return Err("file must be a path string or null".into()),
                };
                self.video.pin_input(n - 1, file)?;
                Ok(serde_json::json!({ "inputs": self.video.input_status() }))
            }
            other => Err(format!("the toaster has no {other} control")),
        }
    }

    fn parallel_read(&mut self, live: &mut LiveField<'_>, _ddrb: u8) -> Option<u8> {
        self.scan_to_beam(live);
        if !self.active {
            return None;
        }
        let beam = live.beam();
        let field = live.pixels();
        self.port_byte(&field, beam)
    }
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn media_inputs() -> usize {
    media::INPUTS
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn media_inputs() -> usize {
    4
}

/// How much of each video input an output shows (0-1 each): the sound
/// follows the picture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Inputs([f32; 4]);

impl Inputs {
    fn add(&mut self, other: &Inputs, weight: f32) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a += b * weight;
        }
    }
}

/// The folder, under the card's host folder, through which the host hands
/// ARexx jobs to the Amiga (S:Copperline-Link.rexx runs them).
pub const LINK_FOLDER: &str = ".copperline";

impl Toaster {
    /// The card's state as the control protocol reports it.
    fn status_value(&self) -> serde_json::Value {
        use regs::{Cds, Mux};
        let r = self.regs();
        let mux = |m: Mux, seven: &str| -> String {
            match m {
                Mux::Vid(n) => format!("Input {}", n + 1),
                Mux::Dac(b) => format!("DV{}", b + 1),
                Mux::Encoder if r.matte() => "Matte".to_string(),
                Mux::Encoder => "Amiga".to_string(),
                Mux::Seven => seven.to_string(),
            }
        };
        let cds = match r.cds() {
            Cds::Quaf => "digital effect (QUAF)",
            Cds::PixelSw => "Amiga transparency (PIXELSW)",
            Cds::TransQuad => "digital effect, drop shadow (TRANSQUAD)",
            Cds::LumKey => "luminance key (LUMKEY)",
            Cds::Dibgr => "Amiga 4-bit key (DIBGR)",
            Cds::Dib => "Amiga 2-bit wipe (DIB)",
            Cds::RgQuad => "RGQUAD",
            Cds::Cd => "mix (CD)",
        };
        let (ma, mb, mp) = r.matte_code();
        let outputs: Vec<serde_json::Value> = self
            .video
            .outputs()
            .iter()
            .map(|o| {
                serde_json::json!({
                    "name": o.name(),
                    "recording": o.recording_path().map(|p| p.display().to_string()),
                })
            })
            .collect();
        let root = self.video.media_root();
        serde_json::json!({
            "card": "NewTek Video Toaster 4000",
            "switcher_active": self.active,
            "program": {
                "a": if r.amwipe() && r.am() == Mux::Seven {
                    "per pixel (latch effect)".to_string()
                } else {
                    mux(r.am(), "Mono")
                },
                "b": if r.bmwipe() && r.bm() == Mux::Seven {
                    "per pixel (latch effect)".to_string()
                } else {
                    mux(r.bm(), "Mono")
                },
                "fader": r.cd(),
                "fader_note": "0 = all A, 255 = all B; the T-bar and AUTO move it",
                "mode": cds,
            },
            "preview": mux(r.pv(), "Program"),
            "framestore_input": mux(r.is(), "Program"),
            "key_source": mux(r.lk(), "External"),
            "luma_key_level": r.lka(),
            "matte": {"a": ma, "b": mb, "hue": mp},
            "live_framestore": r.abse(),
            "outputs": outputs,
            "inputs": self.video.input_status(),
            "media_root": root.map(|p| p.display().to_string()),
            "link_folder": root.map(|p| p.join(LINK_FOLDER).display().to_string()),
        })
    }

    /// What mux position `mux` carries, as a picture, into `out`.
    /// `role` says which mux it is: position 7 means a different thing on
    /// each.
    fn source_picture(
        &mut self,
        mux: regs::Mux,
        role: video::Role,
        field: &FieldPixels<'_>,
        out: &mut [u32],
    ) {
        use regs::Mux;
        match (mux, role) {
            (Mux::Vid(n), _) => match self.video.input_picture(n) {
                Some(p) => out.copy_from_slice(&p[..out.len()]),
                None => out.fill(video::black()),
            },
            // On the IS mux, 4 is the Program output fed back and 5 the
            // external input (DAC0).
            (Mux::Dac(0), video::Role::Is) => self.video.last_program(out),
            (Mux::Dac(1), video::Role::Is) => self.dac_picture(0, false, out),
            (Mux::Dac(bank), _) => self.dac_picture(bank, true, out),
            (Mux::Encoder, _) => {
                if self.regs().matte() {
                    let (ma, mb, mp) = self.regs().matte_code();
                    if self.regs().snow() {
                        self.video.snow(out);
                    } else {
                        out.fill(video::matte_colour(ma, mb, mp));
                    }
                } else {
                    video::amiga_picture(field, Self::pixel0_column(field), out);
                }
            }
            // MONO: the luminance of the LK source.
            (Mux::Seven, video::Role::Fader | video::Role::Is) => {
                let lk = self.regs().lk();
                if lk == Mux::Seven {
                    out.fill(video::black());
                } else {
                    self.source_picture(lk, video::Role::Key, field, out);
                    video::to_mono(out);
                }
            }
            (Mux::Seven, _) => out.fill(video::black()),
        }
    }

    /// Framestore bank playback; bank 0 shows the live input while the
    /// card is digitising it.
    /// With `mapped`, a digital effect in progress readdresses the playback
    /// (see [`dve`]).
    fn dac_picture(&mut self, bank: usize, mapped: bool, out: &mut [u32]) {
        let map = if mapped { self.dve.take() } else { None };
        let linear = |this: &mut Self, out: &mut [u32]| {
            if bank == 0 && this.video.live() {
                if let regs::Mux::Vid(n) = this.regs().is() {
                    match this.video.input_picture(n) {
                        Some(p) => out.copy_from_slice(&p[..out.len()]),
                        None => out.fill(video::black()),
                    }
                    return;
                }
            }
            let pic = this.video.framestore_picture(&this.frame_store, bank);
            out.copy_from_slice(&pic[..out.len()]);
        };
        match map {
            Some(map) => {
                let mut src = vec![video::black(); out.len()];
                linear(self, &mut src);
                let live = bank == 0 && self.video.live();
                if map.reaches_below() && !live {
                    src.extend(video::Video::framestore_below(&self.frame_store, bank));
                }
                map.apply(&src, out);
                self.dve = Some(map);
            }
            None => linear(self, out),
        }
    }

    /// What a fader bus carries: its mux position, or with its WIPE bit
    /// and position 7, a source per pixel chosen by the Amiga picture.
    /// Returns how much of each video input the bus shows, for the sound.
    fn bus_picture(
        &mut self,
        mux: regs::Mux,
        latched: bool,
        field: &FieldPixels<'_>,
        out: &mut [u32],
    ) -> Inputs {
        if !(latched && mux == regs::Mux::Seven) {
            self.source_picture(mux, video::Role::Fader, field, out);
            return self.source_inputs(mux, video::Role::Fader);
        }
        let codes = video::latch_codes(field, Self::pixel0_column(field));
        let mut count = [0usize; 8];
        for c in codes.iter() {
            count[usize::from(c.unwrap_or(7))] += 1;
        }
        let mut shares = Inputs::default();
        for code in 0..8u8 {
            let part = count[usize::from(code)] as f32 / codes.len().max(1) as f32;
            if part > 0.0 {
                shares.add(
                    &self.source_inputs(regs::Mux::from_code(code), video::Role::Fader),
                    part,
                );
            }
        }
        let mut src = vec![video::black(); out.len()];
        for code in (0..8u8).filter(|&c| count[usize::from(c)] > 0) {
            self.source_picture(
                regs::Mux::from_code(code),
                video::Role::Fader,
                field,
                &mut src,
            );
            for ((o, s), c) in out.iter_mut().zip(&src).zip(&codes) {
                if c.unwrap_or(7) == code {
                    *o = *s;
                }
            }
        }
        shares
    }

    /// Which video inputs a mux position carries (the sound follows them):
    /// an input itself, the live framestore passing its input through, or
    /// for MONO the key source. Framestores, mattes and the Amiga are
    /// silent.
    fn source_inputs(&self, mux: regs::Mux, role: video::Role) -> Inputs {
        use regs::Mux;
        let mut shares = Inputs::default();
        match (mux, role) {
            (Mux::Vid(n), _) => shares.0[n] = 1.0,
            (Mux::Dac(0), video::Role::Fader | video::Role::Preview) if self.video.live() => {
                if let Mux::Vid(n) = self.regs().is() {
                    shares.0[n] = 1.0;
                }
            }
            (Mux::Seven, video::Role::Fader) => {
                let lk = self.regs().lk();
                if lk != Mux::Seven {
                    return self.source_inputs(lk, video::Role::Key);
                }
            }
            _ => {}
        }
        shares
    }

    /// The sound of `shares` for this frame: each input's frame of sound,
    /// scaled by how much of it is on the output.
    fn mix_sound(&self, shares: &Inputs, len: usize) -> Vec<(f32, f32)> {
        let mut out = vec![(0.0f32, 0.0f32); len];
        for (n, &w) in shares.0.iter().enumerate() {
            if w < 0.001 {
                continue;
            }
            let Some(sound) = self.video.input_sound(n) else {
                continue;
            };
            // A source frame's sound (1001/30 ms) fitted to the Amiga's.
            for (o, (l, r)) in out.iter_mut().zip(crate::videoslot::resample(sound, len)) {
                o.0 += l * w;
                o.1 += r * w;
            }
        }
        out
    }

    /// Compose and publish one frame of the Program and Preview outputs.
    fn produce_outputs(&mut self, field: &FieldPixels<'_>) {
        let (am, bm, pv) = (self.regs().am(), self.regs().bm(), self.regs().pv());
        let (amwipe, bmwipe) = (self.regs().amwipe(), self.regs().bmwipe());
        // A digital effect: the framestore playback is addressed line by
        // line and quad by quad from the Amiga's picture.
        self.dve = (!self.regs().linp() && !self.regs().lut())
            .then(|| dve::DveMap::from_field(field, Self::pixel0_column(field)));
        let n = video::PICTURE_W * video::PICTURE_H;
        let mut a = self.video.scratch();
        let a_inputs = self.bus_picture(am, amwipe, field, &mut a);
        let mut program = vec![video::black(); n];
        let program_inputs = if bm == am && bmwipe == amwipe {
            program.copy_from_slice(&a);
            a_inputs
        } else {
            let mut b = vec![video::black(); n];
            let b_inputs = self.bus_picture(bm, bmwipe, field, &mut b);
            let key = self.fader_key(field);
            video::mix(&a, &b, &key, &mut program);
            let k = key.mean() / 255.0;
            let mut shares = Inputs::default();
            shares.add(&a_inputs, 1.0 - k);
            shares.add(&b_inputs, k);
            shares
        };
        let mut preview = vec![video::black(); n];
        let preview_inputs = if pv == regs::Mux::Seven {
            preview.copy_from_slice(&program);
            program_inputs
        } else {
            self.source_picture(pv, video::Role::Preview, field, &mut preview);
            self.source_inputs(pv, video::Role::Preview)
        };
        let samples = self.video.frame_samples();
        let program_sound = self.mix_sound(&program_inputs, samples);
        let preview_sound = self.mix_sound(&preview_inputs, samples);
        if self.preview_overlay && self.regs().useron() {
            video::overlay_interface(field, Self::pixel0_column(field), &mut preview);
        }
        if self.diag {
            let r = self.regs();
            let state = format!(
                "AM={:?}{} BM={:?}{} PV={:?} IS={:?} LK={:?} CDS={:?} CD={} matte={} {:?} live={} OBR={}",
                r.am(),
                if r.amwipe() { "+wipe" } else { "" },
                r.bm(),
                if r.bmwipe() { "+wipe" } else { "" },
                r.pv(),
                r.is(),
                r.lk(),
                r.cds(),
                r.cd(),
                r.matte(),
                r.matte_code(),
                self.video.live(),
                r.obr()
            );
            if state != self.diag_outputs {
                log::info!("toaster: outputs {state}");
                self.diag_outputs = state;
            }
        }
        self.video
            .publish(&program, &preview, &program_sound, &preview_sound);
        self.dve = None;
        self.video.give_back(a);
    }

    /// The fader's mix, per picture pixel (0 = A, 255 = B).
    fn fader_key(&mut self, field: &FieldPixels<'_>) -> video::Key {
        let cd = self.regs().cd();
        match self.regs().cds() {
            // During a digital effect the quad codes say where the DVE
            // picture (B) shows over the background (A).
            mode @ (regs::Cds::Quaf | regs::Cds::TransQuad) if self.dve.is_some() => {
                let map = self.dve.as_ref().expect("checked");
                video::Key::Map(map.key(mode == regs::Cds::TransQuad, cd))
            }
            regs::Cds::Cd | regs::Cds::Quaf | regs::Cds::TransQuad | regs::Cds::RgQuad => {
                video::Key::Flat(cd)
            }
            regs::Cds::LumKey => {
                let lk = self.regs().lk();
                let mut src = vec![video::black(); video::PICTURE_W * video::PICTURE_H];
                self.source_picture(lk, video::Role::Key, field, &mut src);
                video::Key::Map(video::luma_key(
                    &src,
                    self.regs().lka(),
                    self.regs().no_key_invert(),
                ))
            }
            mode => video::Key::Map(video::amiga_key(
                field,
                Self::pixel0_column(field),
                mode,
                cd,
            )),
        }
    }

    /// Digitise the IS input into the recording bank (OBR).
    fn record_input(&mut self, field: &FieldPixels<'_>) {
        let is = self.regs().is();
        let bank = usize::from(self.regs().obr());
        let mut pic = vec![video::black(); video::PICTURE_W * video::PICTURE_H];
        self.source_picture(is, video::Role::Is, field, &mut pic);
        video::Video::grab(&mut self.frame_store, bank, &pic);
        self.video.invalidate(bank);
    }

    /// The byte on the parallel port while the beam is at `beam`, from the
    /// line's rendered pixels: on a read-request line, the framestore byte
    /// the quad label under the beam (less the read pipeline) asks for.
    fn port_byte(&mut self, field: &FieldPixels<'_>, beam: BeamPos) -> Option<u8> {
        let header = Self::header(field, beam.vpos)?;
        let LineKind::Read {
            line,
            pixel,
            complemented,
        } = header.kind()
        else {
            return None;
        };
        if line >= LINES {
            return None;
        }
        let row = field.row(beam.vpos)?;
        let scale = field.canvas_scale as i64;
        let x0 = Self::pixel0_column(field);
        let beam_col = field.column_for_hpos(beam.hpos);
        let card_pixel = (beam_col - x0) / scale - READ_PIPELINE_PIXELS;
        if card_pixel < 0 {
            return None;
        }
        let quad = card_pixel / 4;
        let label = Self::quad_label(row, x0 + quad * 4 * scale, scale)?;
        let p = usize::from(label) * 4 + pixel;
        let byte = if p < LINE_PIXELS {
            self.frame_store_byte(self.read_bank(), complemented, line, p)
        } else {
            0
        };
        self.stats.port_bytes += 1;
        if self.diag && self.stats.port_bytes <= 600 {
            log::info!(
                "toaster: read vpos={} hpos={} quad {quad} label {label} line {line} px {pixel} comp={complemented} -> {byte:02X}",
                beam.vpos,
                beam.hpos
            );
        }
        Some(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videoslot::{SLOT_FIRST_CCK, SLOT_LINE_CCK};

    const WIDTH: usize = SLOT_LINE_CCK * 4;

    /// A one-line slot canvas for beam line `vpos`.
    struct Line {
        pixels: Vec<u32>,
        vpos: u32,
    }

    #[test]
    fn the_sound_follows_how_much_of_each_input_program_shows() {
        let mut shares = Inputs::default();
        let mut a = Inputs::default();
        a.0[0] = 1.0;
        let mut b = Inputs::default();
        b.0[1] = 1.0;
        let k = video::Key::Flat(64).mean() / 255.0;
        shares.add(&a, 1.0 - k);
        shares.add(&b, k);
        assert!((shares.0[0] - 0.749).abs() < 0.01 && (shares.0[1] - 0.251).abs() < 0.01);
        let map = video::Key::Map(vec![0, 255, 255, 255]);
        assert!((map.mean() - 191.25).abs() < 0.01);
        // An input pinned to nothing is silent: no sound without frames.
        let card = Toaster::new();
        assert!(card
            .mix_sound(&shares, 1471)
            .iter()
            .all(|&(l, r)| l == 0.0 && r == 0.0));
    }

    #[test]
    fn a_latch_effect_picks_each_pixel_source_from_the_amiga_colour() {
        // Picture row 1 is Amiga line AMIGA_LINE0 + 1.
        let mut line = Line::new(video::AMIGA_LINE0 + 1);
        let x0 = Toaster::pixel0_column(&line.field());
        // Picture column c is card pixel PICTURE_X0 + c.
        let at = |c: usize| x0 as usize + video::PICTURE_X0 + c;
        // Transparent (T) pixels latch: DV1 (code 4, r1) from picture
        // column 10, input 3 (code 2, r2) from 100; opaque pixels keep it.
        let latch = |r: u8, b: u8| u32::from_le_bytes([r, 0, b, 0]);
        line.pixels[at(10)] = latch(0x02, 0);
        line.pixels[at(100)] = latch(0x04, 0);
        line.pixels[at(200)] = latch(0x00, 0x04);
        let codes = video::latch_codes(&line.field(), x0);
        let row = &codes[video::PICTURE_W..2 * video::PICTURE_W];
        assert_eq!(row[5], None);
        assert_eq!(row[10], Some(4));
        assert_eq!(row[99], Some(4));
        assert_eq!(row[100], Some(2));
        assert_eq!(row[200], Some(1));
        assert_eq!(row[700], Some(1));
    }

    impl Line {
        fn new(vpos: u32) -> Self {
            Self {
                pixels: vec![u32::from_le_bytes([0, 0, 0, 0xFF]); WIDTH],
                vpos,
            }
        }

        fn field(&self) -> FieldPixels<'_> {
            FieldPixels {
                pixels: &self.pixels,
                width: WIDTH,
                rows: 1,
                visible_start_vpos: self.vpos,
                first_cck: SLOT_FIRST_CCK,
                canvas_scale: 1,
                long_frame: true,
                interlaced: false,
                valid_rows: 1,
            }
        }

        /// A pixel the card's four lines read as `v` (R, G<<1, B<<2, I<<3).
        fn wire(v: u8) -> u32 {
            let r = if v & 1 != 0 { 0x02 } else { 0 } | if v & 4 != 0 { 0x01 } else { 0 };
            let g = if v & 2 != 0 { 0x02 } else { 0 } | if v & 8 != 0 { 0x01 } else { 0 };
            u32::from_le_bytes([r, g, 0, 0xFF])
        }

        /// Draw the header sprite: sixteen lo-res pixels from `$5C`.
        fn header(&mut self, h: Header) {
            let x0 = self
                .field()
                .column_for_lores(HEADER_LORES + crate::bus::SPRITE_OUTPUT_DELAY_LORES);
            for k in 0..16 {
                let bit = 15 - k;
                let v = ((h.r >> bit) & 1)
                    | ((h.g >> bit) & 1) << 1
                    | ((h.b >> bit) & 1) << 2
                    | ((h.i >> bit) & 1) << 3;
                let col = (x0 + 2 * k) as usize;
                self.pixels[col] = Self::wire(v as u8);
                self.pixels[col + 1] = Self::wire(v as u8);
            }
        }

        /// Put nibble `v` on card pixel `p`.
        fn card_pixel(&mut self, p: usize, v: u8) {
            let col = (Toaster::pixel0_column(&self.field()) + p as i64) as usize;
            self.pixels[col] = Self::wire(v);
        }

        /// The read bitmap: quads from 3 labelled `label`, 18 quads each.
        fn labels(&mut self, first_label: u8) {
            for quad in 3..192usize {
                let label = first_label + ((quad - 3) / 18) as u8;
                for j in 0..4 {
                    let bit = 3 - j;
                    let v = ((label >> 4 >> bit) & 1) << 3 | ((label >> bit) & 1) << 2;
                    self.card_pixel(quad * 4 + j, v);
                }
            }
        }
    }

    /// Framestore line `line` written with `bytes` from card pixel 12 by
    /// the four passes the switcher makes, with `OBR` as given.
    fn write_line(card: &mut Toaster, obr: bool, line: usize, bytes: &[u8]) {
        write_line_pass(card, obr, line, bytes, true, 0);
    }

    /// One write of `line`, with EH0's `LUST` as given and the bitmap
    /// drawn `left` pixels left of card pixel 12 (a zipper's second pass).
    fn write_line_pass(
        card: &mut Toaster,
        obr: bool,
        line: usize,
        bytes: &[u8],
        lust: bool,
        left: usize,
    ) {
        let mut eh0 = Line::new(23);
        eh0.header(Header {
            r: 0x8000 | if obr { EH0R_OBR } else { 0 },
            g: 0x0100,
            b: 0xF000,
            i: if lust { 0x0F00 } else { 0x0E00 },
        });
        card.field_complete(&eh0.field(), BeamPos::default());
        let l = (line / 2) as u16;
        let field_ii = line.is_multiple_of(2);
        for (vpos, g) in [(24, 0xC000u16), (25, 0x8000), (26, 0xE000), (27, 0xA000)] {
            let mut data = Line::new(vpos);
            data.header(Header {
                r: 0xF000,
                g: g | if field_ii { 0x1000 } else { 0 },
                b: (l & 0xF) << 12,
                i: (l >> 4) << 12,
            });
            for (k, &byte) in bytes.iter().enumerate().skip(left) {
                let nibble = if g & 0x4000 != 0 {
                    byte >> 4
                } else {
                    byte & 0xF
                };
                data.card_pixel(12 + k - left, nibble);
            }
            card.field_complete(&data.field(), BeamPos::default());
        }
    }

    fn read_request(line: usize, pixel: u8, complemented: bool) -> Header {
        let l = (line / 2) as u16;
        let field_ii = line.is_multiple_of(2);
        Header {
            r: [0x0000, 0x2000, 0xC000, 0xE000][usize::from(pixel)],
            g: if complemented { 0x2000 } else { 0 } | if field_ii { 0x1000 } else { 0 },
            b: (l & 0xF) << 12,
            i: (l >> 4) << 12,
        }
    }

    #[test]
    fn a_zipper_writes_second_pass_lands_where_the_first_did() {
        // The switcher's AA framestore load: the whole line with LUST set,
        // then again with LUST clear and the bitmap scrolled 16 pixels
        // left. Both passes must agree on every sample.
        let mut card = Toaster::new();
        let bytes: Vec<u8> = (0..756).map(|k| 71 + (k % 129) as u8).collect();
        write_line_pass(&mut card, false, 100, &bytes, true, 0);
        write_line_pass(&mut card, false, 100, &bytes, false, 16);
        let stored = card.frame_store.line(0, false, 100);
        for k in [0usize, 4, 15, 16, 300, 731, 739] {
            assert_eq!(stored[12 + k], bytes[k], "sample {}", 12 + k);
        }
    }

    #[test]
    fn a_written_line_reads_back_by_quad_label() {
        let mut card = Toaster::new();
        let ramp: Vec<u8> = (71..=200).collect();
        write_line(&mut card, false, 504, &ramp);
        assert!(card.active);
        assert_eq!(card.frame_store_byte(0, false, 504, 12), 71);
        assert_eq!(card.frame_store_byte(0, true, 504, 13), 72);

        // The switcher reads bank 0 with OBR set.
        let mut eh0 = Line::new(23);
        eh0.header(Header {
            r: 0x8000 | EH0R_OBR,
            g: 0x0100,
            b: 0xF000,
            i: 0x0F00,
        });
        card.field_complete(&eh0.field(), BeamPos::default());
        let mut read = Line::new(24);
        read.header(read_request(504, 0, false));
        read.labels(3);
        let at = |hpos| BeamPos {
            vpos: 24,
            hpos,
            ..BeamPos::default()
        };
        // HS.default's ten read positions land in labels 3..=12.
        let expected: Vec<u8> = (0..10).map(|i| 71 + 4 * i).collect();
        let got: Vec<u8> = [58, 75, 93, 111, 129, 147, 166, 183, 201, 216]
            .into_iter()
            .map(|h| card.port_byte(&read.field(), at(h)).unwrap())
            .collect();
        assert_eq!(got, expected);
        // Five clocks later (the CIA access's E-clock slip) still reads
        // the same labels.
        let late: Vec<u8> = [63, 80, 98, 116, 134, 152, 171, 188, 206, 221]
            .into_iter()
            .map(|h| card.port_byte(&read.field(), at(h)).unwrap())
            .collect();
        assert_eq!(late, expected);
        // Pixel 1 of each quad, from the complemented copy.
        let mut read = Line::new(25);
        read.header(read_request(504, 1, true));
        read.labels(3);
        assert_eq!(
            card.port_byte(
                &read.field(),
                BeamPos {
                    vpos: 25,
                    hpos: 75,
                    ..BeamPos::default()
                }
            ),
            Some(72 + 4)
        );
    }

    #[test]
    fn banks_follow_the_obr_bit() {
        let mut card = Toaster::new();
        write_line(&mut card, true, 100, &[1, 2, 3]);
        assert_eq!(card.frame_store_byte(1, false, 100, 12), 1);
        assert_eq!(card.frame_store_byte(0, false, 100, 12), 0);
        write_line(&mut card, false, 100, &[9]);
        assert_eq!(card.frame_store_byte(0, false, 100, 12), 9);
        // Reads come from the other bank than OBR names: the last EH0 above
        // cleared OBR, so bank 1 is read.
        let mut read = Line::new(24);
        read.header(read_request(100, 0, false));
        read.labels(3);
        assert_eq!(
            card.port_byte(
                &read.field(),
                BeamPos {
                    vpos: 24,
                    hpos: 58,
                    ..BeamPos::default()
                }
            ),
            Some(1)
        );
    }

    #[test]
    fn input_lines_follow_the_wired_rgb_bits() {
        let px = |r: u8, g: u8, b: u8| u32::from_le_bytes([r, g, b, 0xFF]);
        // The switcher's write palette: one line per bitplane.
        assert_eq!(lines_of(px(0x04, 0, 0)), 1);
        assert_eq!(lines_of(px(0, 0, 0x04)), 2);
        assert_eq!(lines_of(px(0x01, 0, 0)), 4);
        assert_eq!(lines_of(px(0, 0x01, 0)), 8);
        // The alternate bits of each pair, and the sprite palette's pairs.
        assert_eq!(lines_of(px(0x02, 0, 0)), 1);
        assert_eq!(lines_of(px(0, 0x02, 0)), 2);
        assert_eq!(lines_of(px(0, 0, 0x01)), 4);
        assert_eq!(lines_of(px(0, 0, 0x02)), 8);
        assert_eq!(lines_of(px(0x66, 0, 0)), 1);
        assert_eq!(lines_of(px(0, 0x22, 0x44)), 2);
        // High bits of the guns carry nothing.
        assert_eq!(lines_of(px(0xF8, 0xFC, 0xF8)), 0);
    }

    #[test]
    fn header_kinds_decode() {
        let h = |r, g, b, i| Header { r, g, b, i };
        // DoSafeWrite's filler: no ELH, no write bit, line number 255.
        assert_eq!(h(0xF000, 0x0000, 0xF000, 0xF000).kind(), LineKind::Dummy);
        // The read self-test's line 504 (L = 252) decodes like any other.
        assert_eq!(
            h(0xF000, 0xD000, 0xC000, 0xF000).kind(),
            LineKind::Write {
                line: 504,
                high_nibble: true,
                complemented: false
            }
        );
        // tblI/tblII write codes for toaster line L = $21: field I line 67.
        assert_eq!(
            h(0xF000, 0xC000, 0x1000, 0x2000).kind(),
            LineKind::Write {
                line: 67,
                high_nibble: true,
                complemented: false
            }
        );
        assert_eq!(
            h(0xF000, 0xB000, 0x1000, 0x2000).kind(),
            LineKind::Write {
                line: 66,
                high_nibble: false,
                complemented: true
            }
        );
        // Read request, pixel 2 of each quad, complemented, field II.
        assert_eq!(
            h(0xC000, 0x3000, 0x1000, 0x2000).kind(),
            LineKind::Read {
                line: 66,
                pixel: 2,
                complemented: true
            }
        );
        assert_eq!(
            h(0x8100, 0x0500, 0xFF00, 0xFF00).kind(),
            LineKind::Elh { index: 0 }
        );
        // EH6's page is in the B word: EH63.
        assert_eq!(h(0xB000, 0, 0x0300, 0).kind(), LineKind::Elh { index: 9 });
        assert_eq!(h(0x7000, 0, 0, 0).kind(), LineKind::Elh { index: ELH_EH7 });
    }
}
