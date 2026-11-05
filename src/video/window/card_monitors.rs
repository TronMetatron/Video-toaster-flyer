// SPDX-License-Identifier: GPL-3.0-or-later

//! Monitor windows for a video-slot card's own outputs: the Video Toaster's
//! Preview and Program, each on a picture monitor of its own.
//!
//! Each output gets a host window showing the monitor artwork
//! (`assets/toaster/monitor.png`, or `monitor.png` in the Toaster files
//! folder) with the output's latest frame on the tube: scaled into the
//! glass, which is found in the artwork by flooding outward from its centre
//! to the dark gap around the tube, and lit by the glass itself so its
//! shading and reflection stay on top of the picture.
//!
//! Composition is on the CPU at the window's own size (the artwork is
//! resampled once per resize), presented through a `pixels` surface of
//! the same size. Each tube pixel's filter taps and glass lighting are
//! worked out once per window and picture size, in fixed point, so a new
//! frame is one pass of integer arithmetic straight into the surface's
//! buffer, split across threads. Each window has a painter thread of its
//! own that waits for the output's next frame, composes it and presents
//! it, so none of this is on the emulator's thread: the machine runs at
//! its own pace whatever the windows' size, and a painter that falls
//! behind skips to the latest frame. The event loop keeps the windows'
//! input and tells the painter about resizes and repaint requests. In the
//! Program window, `R` or the monitor's power button starts and stops a
//! recording of the output; the power lamp shows red while it runs.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use log::{info, warn};
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
use winit::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition},
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowAttributes, WindowId},
};

use crate::videoslot::CardOutput;

const DEFAULT_ART: &[u8] = include_bytes!("../../../assets/toaster/monitor.png");

/// Where the default artwork's power lamp and button are, in its pixels.
/// An artwork supplied in place of it has neither.
const DEFAULT_LAMP: (f32, f32, f32) = (1264.0, 950.0, 6.0);
const DEFAULT_BUTTON: (u32, u32, u32, u32) = (1251, 983, 1282, 1013);

/// How far the picture runs past the glass on each side, as a fraction of
/// the glass: a monitor overscans.
const OVERSCAN: f32 = 0.03;

/// The monitor artwork and the tube found in it.
pub(super) struct MonitorArt {
    w: usize,
    h: usize,
    rgba: Vec<u32>,
    /// 255 on the glass, 0 elsewhere.
    mask: Vec<u8>,
    /// The glass's own brightness, for shading the picture.
    glass: Vec<u8>,
    /// The glass's bounding box, `(x0, y0, x1, y1)`, exclusive.
    screen: (usize, usize, usize, usize),
    lamp: Option<(f32, f32, f32)>,
    button: Option<(u32, u32, u32, u32)>,
}

fn rgba_u32(r: u8, g: u8, b: u8) -> u32 {
    u32::from_le_bytes([r, g, b, 0xFF])
}

impl MonitorArt {
    /// The artwork in `media_root` if there is one, else the built-in one.
    pub(super) fn load(media_root: Option<&std::path::Path>) -> Option<Arc<Self>> {
        if let Some(path) = media_root.map(|r| r.join("monitor.png")) {
            if path.is_file() {
                match std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| Self::decode(&bytes, false))
                {
                    Ok(art) => {
                        info!("toaster monitors: artwork {}", path.display());
                        return Some(Arc::new(art));
                    }
                    Err(e) => warn!("toaster monitors: {}: {e}", path.display()),
                }
            }
        }
        match Self::decode(DEFAULT_ART, true) {
            Ok(art) => Some(Arc::new(art)),
            Err(e) => {
                warn!("toaster monitors: built-in artwork: {e}");
                None
            }
        }
    }

    fn decode(bytes: &[u8], default: bool) -> Result<Self, String> {
        let img = image::load_from_memory(bytes)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let rgba: Vec<u32> = img
            .as_raw()
            .chunks_exact(4)
            .map(|p| rgba_u32(p[0], p[1], p[2]))
            .collect();
        let (mask, screen) = find_glass(&rgba, w, h).ok_or("no tube face found in the artwork")?;
        let glass = rgba
            .iter()
            .zip(&mask)
            .map(|(&p, &m)| {
                if m == 0 {
                    0
                } else {
                    let [r, g, b, _] = p.to_le_bytes();
                    r.max(g).max(b)
                }
            })
            .collect();
        Ok(Self {
            w,
            h,
            rgba,
            mask,
            glass,
            screen,
            lamp: default.then_some(DEFAULT_LAMP),
            button: default.then_some(DEFAULT_BUTTON),
        })
    }

    pub(super) fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }
}

/// A rectangle `(x0, y0, x1, y1)`, exclusive.
type Rect = (usize, usize, usize, usize);

/// The tube face: everything reachable from the artwork's centre without
/// crossing a near-black pixel (the shadow gap between glass and bezel).
fn find_glass(rgba: &[u32], w: usize, h: usize) -> Option<(Vec<u8>, Rect)> {
    let bright = |i: usize| {
        let [r, g, b, _] = rgba[i].to_le_bytes();
        r.max(g).max(b) > 10
    };
    // The tube sits in the upper part of a monitor front; start from the
    // middle of the upper 80 %, stepping down to the first lit pixel.
    let cx = w / 2;
    let start = (h * 2 / 5..h * 3 / 5)
        .map(|y| y * w + cx)
        .find(|&i| bright(i))?;
    let mut mask = vec![0u8; w * h];
    let mut stack = vec![start];
    mask[start] = 255;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x + 1);
        y1 = y1.max(y + 1);
        let mut visit = |j: usize| {
            if mask[j] == 0 && bright(j) {
                mask[j] = 255;
                stack.push(j);
            }
        };
        if x > 0 {
            visit(i - 1);
        }
        if x + 1 < w {
            visit(i + 1);
        }
        if y > 0 {
            visit(i - w);
        }
        if y + 1 < h {
            visit(i + w);
        }
    }
    // A flood that leaked into the whole front found no tube.
    if (x1 - x0) * (y1 - y0) > w * h * 9 / 10 || x1 - x0 < w / 8 {
        return None;
    }
    Some((mask, (x0, y0, x1, y1)))
}

/// Box-filter `src` (`sw` x `sh` RGBA) down, or bilinear-filter it up, to
/// `dw` x `dh`.
fn resample(src: &[u32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u32> {
    let mut out = vec![0u32; dw * dh];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for dy in 0..dh {
        let sy0 = dy as f32 * fy;
        let sy1 = (sy0 + fy.max(1.0)).min(sh as f32);
        for dx in 0..dw {
            let sx0 = dx as f32 * fx;
            let sx1 = (sx0 + fx.max(1.0)).min(sw as f32);
            let mut acc = [0f32; 3];
            let mut weight = 0f32;
            let mut y = sy0.floor() as usize;
            while (y as f32) < sy1 && y < sh {
                let wy = ((y + 1) as f32).min(sy1) - (y as f32).max(sy0);
                let mut x = sx0.floor() as usize;
                while (x as f32) < sx1 && x < sw {
                    let wx = ((x + 1) as f32).min(sx1) - (x as f32).max(sx0);
                    let wgt = wx * wy;
                    let [r, g, b, _] = src[y * sw + x].to_le_bytes();
                    acc[0] += f32::from(r) * wgt;
                    acc[1] += f32::from(g) * wgt;
                    acc[2] += f32::from(b) * wgt;
                    weight += wgt;
                    x += 1;
                }
                y += 1;
            }
            let c = |v: f32| (v / weight.max(1e-6)).round().clamp(0.0, 255.0) as u8;
            out[dy * dw + dx] = rgba_u32(c(acc[0]), c(acc[1]), c(acc[2]));
        }
    }
    out
}

fn resample_u8(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let as_rgba: Vec<u32> = src.iter().map(|&v| rgba_u32(v, v, v)).collect();
    resample(&as_rgba, sw, sh, dw, dh)
        .into_iter()
        .map(|p| p.to_le_bytes()[0])
        .collect()
}

/// One tube pixel's recipe: the 2 x 2 of picture pixels it filters and how
/// the glass lights it, in fixed point.
#[derive(Clone, Copy, Debug)]
struct Tap {
    /// Window pixel.
    i: u32,
    /// Picture pixel at the top left of the 2 x 2.
    base: u32,
    /// The right and lower neighbours are one on (0 at the picture's edge).
    dx: u8,
    dy: u8,
    /// Filter position within the 2 x 2, in 256ths.
    tx: u8,
    ty: u8,
    /// The glass's shading (256 = 1) and sheen.
    shade: u16,
    sheen: u8,
    /// Coverage: 255 inside the glass, less on its antialiased edge.
    a: u8,
}

/// The taps of every tube pixel for one picture size, in window order, and
/// where each window row's run starts.
struct Table {
    vw: usize,
    vh: usize,
    taps: Vec<Tap>,
    /// `rows[r]`: the first tap on window row `screen.1 + r`.
    rows: Vec<usize>,
}

/// Tube pixels below which a frame is composed on one thread.
const PARALLEL_TAPS: usize = 60_000;

/// The artwork laid out for one window size.
pub(super) struct Layer {
    w: usize,
    h: usize,
    bg: Vec<u32>,
    /// `bg` as the surface's bytes.
    bg_bytes: Vec<u8>,
    /// The taps for the last picture size composed.
    table: std::sync::Mutex<Option<Arc<Table>>>,
    mask: Vec<u8>,
    glass: Vec<u8>,
    screen: (usize, usize, usize, usize),
    /// Where the picture lands: the glass grown by the overscan, at 4:3
    /// across the glass's width, `(x, y, w, h)` in window pixels.
    picture: (f32, f32, f32, f32),
    lamp: Option<(f32, f32, f32)>,
}

/// How the artwork fits a `w` x `h` window, centred: `(scale, aw, ah, ox,
/// oy)`, its size and top-left corner in window pixels.
fn fit(art: &MonitorArt, w: usize, h: usize) -> (f32, usize, usize, usize, usize) {
    let w = w.max(1);
    let h = h.max(1);
    let scale = (w as f32 / art.w as f32).min(h as f32 / art.h as f32);
    let aw = ((art.w as f32 * scale).round() as usize).clamp(1, w);
    let ah = ((art.h as f32 * scale).round() as usize).clamp(1, h);
    (scale, aw, ah, (w - aw) / 2, (h - ah) / 2)
}

/// Where the artwork's power button lands in a `w` x `h` window, `(x0, y0,
/// x1, y1)`.
fn button_in(art: &MonitorArt, w: usize, h: usize) -> Option<(f32, f32, f32, f32)> {
    let (scale, _, _, ox, oy) = fit(art, w, h);
    art.button.map(|(x0, y0, x1, y1)| {
        (
            ox as f32 + x0 as f32 * scale,
            oy as f32 + y0 as f32 * scale,
            ox as f32 + x1 as f32 * scale,
            oy as f32 + y1 as f32 * scale,
        )
    })
}

fn hits(button: Option<(f32, f32, f32, f32)>, x: f64, y: f64) -> bool {
    button.is_some_and(|(x0, y0, x1, y1)| {
        (x as f32) >= x0 && (x as f32) <= x1 && (y as f32) >= y0 && (y as f32) <= y1
    })
}

impl Layer {
    pub(super) fn new(art: &MonitorArt, w: usize, h: usize, aspect: f32) -> Self {
        let w = w.max(1);
        let h = h.max(1);
        // Fit the artwork, centred, black around it.
        let (scale, aw, ah, ox, oy) = fit(art, w, h);
        let art_bg = resample(&art.rgba, art.w, art.h, aw, ah);
        let art_mask = resample_u8(&art.mask, art.w, art.h, aw, ah);
        let art_glass = resample_u8(&art.glass, art.w, art.h, aw, ah);
        let mut bg = vec![rgba_u32(0, 0, 0); w * h];
        let mut mask = vec![0u8; w * h];
        let mut glass = vec![0u8; w * h];
        for y in 0..ah {
            let src = y * aw;
            let dst = (y + oy) * w + ox;
            bg[dst..dst + aw].copy_from_slice(&art_bg[src..src + aw]);
            mask[dst..dst + aw].copy_from_slice(&art_mask[src..src + aw]);
            glass[dst..dst + aw].copy_from_slice(&art_glass[src..src + aw]);
        }
        let map = |x: f32, y: f32| (ox as f32 + x * scale, oy as f32 + y * scale);
        let (sx0, sy0) = map(art.screen.0 as f32, art.screen.1 as f32);
        let (sx1, sy1) = map(art.screen.2 as f32, art.screen.3 as f32);
        let screen = (
            (sx0.floor() as usize).min(w),
            (sy0.floor() as usize).min(h),
            (sx1.ceil() as usize).min(w),
            (sy1.ceil() as usize).min(h),
        );
        let gw = (sx1 - sx0) * (1.0 + 2.0 * OVERSCAN);
        let gh = gw / aspect.max(0.1);
        let cx = (sx0 + sx1) / 2.0;
        let cy = (sy0 + sy1) / 2.0;
        let picture = (cx - gw / 2.0, cy - gh / 2.0, gw, gh);
        let lamp = art.lamp.map(|(x, y, r)| {
            let (lx, ly) = map(x, y);
            (lx, ly, r * scale)
        });
        let bg_bytes = bg.iter().flat_map(|p| p.to_le_bytes()).collect();
        Self {
            w,
            h,
            bg,
            bg_bytes,
            table: std::sync::Mutex::new(None),
            mask,
            glass,
            screen,
            picture,
            lamp,
        }
    }

    /// The taps for a `vw` x `vh` picture (made on first use).
    fn table(&self, vw: usize, vh: usize) -> Arc<Table> {
        let mut slot = self.table.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = slot.as_ref().filter(|t| (t.vw, t.vh) == (vw, vh)) {
            return Arc::clone(t);
        }
        let (px, py, pw, ph) = self.picture;
        let (x0, y0, x1, y1) = self.screen;
        let sx = vw as f32 / pw;
        let sy = vh as f32 / ph;
        let mut taps = Vec::new();
        let mut rows = Vec::with_capacity(y1.saturating_sub(y0) + 1);
        for y in y0..y1 {
            rows.push(taps.len());
            let vy = ((y as f32 + 0.5 - py) * sy - 0.5).clamp(0.0, (vh - 1) as f32);
            let ya = vy.floor() as usize;
            let ty = vy - ya as f32;
            for x in x0..x1 {
                let i = y * self.w + x;
                let m = self.mask[i];
                if m == 0 {
                    continue;
                }
                let vx = ((x as f32 + 0.5 - px) * sx - 0.5).clamp(0.0, (vw - 1) as f32);
                let xa = vx.floor() as usize;
                let tx = vx - xa as f32;
                let g = f32::from(self.glass[i]);
                // The tube's own shading darkens toward its edges; whatever
                // is brighter than the bare glass (the reflection) lies on
                // top.
                let shade = 0.72 + 0.28 * (g / 45.0).min(1.0);
                let sheen = (g - 50.0).max(0.0) * 0.9;
                let fixed = |t: f32| (t * 256.0).round().min(255.0) as u8;
                taps.push(Tap {
                    i: i as u32,
                    base: (ya * vw + xa) as u32,
                    dx: u8::from(xa + 1 < vw),
                    dy: u8::from(ya + 1 < vh),
                    tx: fixed(tx),
                    ty: fixed(ty),
                    shade: (shade * 256.0).round() as u16,
                    sheen: sheen.round().min(255.0) as u8,
                    a: m,
                });
            }
        }
        rows.push(taps.len());
        let table = Arc::new(Table { vw, vh, taps, rows });
        *slot = Some(Arc::clone(&table));
        table
    }

    /// The monitor with `video` (`vw` x `vh` RGBA) on the tube, into `out`
    /// (`w * h`). No video leaves the tube dark.
    pub(super) fn compose(
        &self,
        video: Option<(&[u32], usize, usize)>,
        lamp_on: bool,
        out: &mut [u32],
    ) {
        let mut bytes = vec![0u8; self.w * self.h * 4];
        self.compose_rgba8(video, lamp_on, &mut bytes, true);
        for (o, b) in out.iter_mut().zip(bytes.chunks_exact(4)) {
            *o = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        }
    }

    /// [`Self::compose`] into a surface's RGBA bytes (`w * h * 4`). With
    /// `background` false the artwork around the tube is taken to be there
    /// already (the frame before it was composed by this layer, with the
    /// same lamp), and only the tube is drawn.
    pub(super) fn compose_rgba8(
        &self,
        video: Option<(&[u32], usize, usize)>,
        lamp_on: bool,
        frame: &mut [u8],
        background: bool,
    ) {
        let len = self.w * self.h * 4;
        if frame.len() < len {
            return;
        }
        let frame = &mut frame[..len];
        if background || video.is_none() {
            frame.copy_from_slice(&self.bg_bytes);
        }
        if let Some((video, vw, vh)) =
            video.filter(|(v, vw, vh)| *vw > 0 && *vh > 0 && v.len() >= vw * vh)
        {
            let table = self.table(vw, vh);
            let y0 = self.screen.1;
            let rows = table.rows.len().saturating_sub(1);
            let threads = if table.taps.len() < PARALLEL_TAPS {
                1
            } else {
                std::thread::available_parallelism().map_or(1, |n| n.get().clamp(1, 4))
            };
            // Each thread a band of rows: its own run of taps and of bytes.
            let per = rows.div_ceil(threads).max(1);
            let row_bytes = self.w * 4;
            let mut rest = &mut frame[y0 * row_bytes..];
            let mut bands = Vec::new();
            for start in (0..rows).step_by(per) {
                let end = (start + per).min(rows);
                let (band, tail) = rest.split_at_mut((end - start) * row_bytes);
                rest = tail;
                let taps = &table.taps[table.rows[start]..table.rows[end]];
                bands.push((band, taps, (y0 + start) * self.w));
            }
            let run = |(band, taps, first): (&mut [u8], &[Tap], usize)| {
                for t in taps {
                    let at = (t.i as usize - first) * 4;
                    let bg = self.bg[t.i as usize];
                    band[at..at + 4].copy_from_slice(&tap_pixel(t, video, vw, bg));
                }
            };
            if bands.len() <= 1 {
                bands.into_iter().for_each(run);
            } else {
                std::thread::scope(|scope| {
                    let mut bands = bands.into_iter();
                    let first = bands.next();
                    for band in bands {
                        scope.spawn(move || run(band));
                    }
                    if let Some(band) = first {
                        run(band);
                    }
                });
            }
        }
        if lamp_on && (background || video.is_none()) {
            if let Some((lx, ly, r)) = self.lamp {
                let r = r.max(1.5);
                let glow = r * 2.2;
                let ys = ((ly - glow).floor().max(0.0) as usize)
                    ..((ly + glow).ceil() as usize).min(self.h);
                for y in ys {
                    let xs = ((lx - glow).floor().max(0.0) as usize)
                        ..((lx + glow).ceil() as usize).min(self.w);
                    for x in xs {
                        let d =
                            ((x as f32 + 0.5 - lx).powi(2) + (y as f32 + 0.5 - ly).powi(2)).sqrt();
                        let a = if d <= r {
                            1.0
                        } else {
                            ((glow - d) / (glow - r)).clamp(0.0, 1.0) * 0.45
                        };
                        if a <= 0.0 {
                            continue;
                        }
                        let i = (y * self.w + x) * 4;
                        let mix = |base: u8, lit: f32| {
                            (f32::from(base) * (1.0 - a) + lit * a).round() as u8
                        };
                        frame[i] = mix(frame[i], 255.0);
                        frame[i + 1] = mix(frame[i + 1], 40.0);
                        frame[i + 2] = mix(frame[i + 2], 30.0);
                        frame[i + 3] = 0xFF;
                    }
                }
            }
        }
    }
}

/// One tube pixel: the picture filtered at its tap, lit by the glass, over
/// the artwork by its coverage.
#[inline]
fn tap_pixel(t: &Tap, video: &[u32], vw: usize, bg: u32) -> [u8; 4] {
    let b = t.base as usize;
    let dx = usize::from(t.dx);
    let below = b + usize::from(t.dy) * vw;
    let (p00, p01, p10, p11) = (video[b], video[b + dx], video[below], video[below + dx]);
    let (tx, ty) = (u32::from(t.tx), u32::from(t.ty));
    let w00 = (256 - tx) * (256 - ty);
    let w01 = tx * (256 - ty);
    let w10 = (256 - tx) * ty;
    let w11 = tx * ty;
    let a = u32::from(t.a);
    let mut out = [0, 0, 0, 0xFF];
    for (c, o) in out.iter_mut().take(3).enumerate() {
        let s = 8 * c as u32;
        let ch = |p: u32| (p >> s) & 0xFF;
        let v = (ch(p00) * w00 + ch(p01) * w01 + ch(p10) * w10 + ch(p11) * w11 + 32768) >> 16;
        let v = ((v * u32::from(t.shade) + 128) >> 8) + u32::from(t.sheen);
        let v = if a == 255 {
            v
        } else {
            (v * a + ch(bg) * (255 - a) + 127) / 255
        };
        *o = v.min(255) as u8;
    }
    out
}

/// Compose `output`'s latest frame on the artwork at the artwork's own
/// size (for headless captures).
pub(super) fn compose_native(art: &MonitorArt, output: &CardOutput) -> (Vec<u32>, usize, usize) {
    let (w, h) = art.size();
    let layer = Layer::new(art, w, h, output.aspect());
    let mut video = Vec::new();
    let frame = output.frame_since(u64::MAX, &mut video);
    let mut out = vec![0u32; w * h];
    layer.compose(
        frame.map(|(vw, vh, _)| (video.as_slice(), vw, vh)),
        output.is_recording(),
        &mut out,
    );
    (out, w, h)
}

struct MonitorWindow {
    output: Arc<CardOutput>,
    window: Arc<Window>,
    /// The thread that paints the window.
    painter: Painter,
    /// The power button in the window, for the event loop's hit test.
    button: Option<(f32, f32, f32, f32)>,
    shown_recording: bool,
    cursor: (f64, f64),
}

/// What the event loop tells a window's painter.
enum Paint {
    Resized(u32, u32),
    /// Paint the window again (it was exposed, or the lamp changed).
    Redraw,
    Stop,
}

/// The handle on a window's painter thread.
struct Painter {
    tx: mpsc::Sender<Paint>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Painter {
    fn send(&self, msg: Paint) {
        let _ = self.tx.send(msg);
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.send(Paint::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How long a painter waits for a frame before looking at its inbox.
const PAINT_WAIT: Duration = Duration::from_millis(5);

/// The painter's state: the surface and the layer for its size, the last
/// frame taken, and what the surface's buffer holds.
struct Canvas {
    output: Arc<CardOutput>,
    art: Arc<MonitorArt>,
    pixels: Pixels<'static>,
    layer: Option<Layer>,
    size: (u32, u32),
    seen: u64,
    video: Vec<u32>,
    video_size: (usize, usize),
    /// The surface's buffer holds the current layer's artwork, with the
    /// power lamp lit or not.
    painted: Option<bool>,
}

impl Canvas {
    /// Paint until told to stop: every frame the output publishes (or the
    /// latest, when painting is slower than the card), and whatever the
    /// event loop asks for.
    fn run(mut self, rx: mpsc::Receiver<Paint>) {
        loop {
            let mut redraw = false;
            loop {
                match rx.try_recv() {
                    Ok(Paint::Resized(w, h)) => {
                        self.resize(w, h);
                        redraw = true;
                    }
                    Ok(Paint::Redraw) => redraw = true,
                    Ok(Paint::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            if let Some((vw, vh, serial)) =
                self.output
                    .wait_frame_since(self.seen, PAINT_WAIT, &mut self.video)
            {
                self.seen = serial;
                self.video_size = (vw, vh);
                redraw = true;
            }
            let recording = self.output.is_recording();
            if redraw || self.painted != Some(recording) {
                self.paint(recording);
            }
        }
    }

    fn paint(&mut self, recording: bool) {
        let (w, h) = self.size;
        if w == 0 || h == 0 {
            return;
        }
        if self
            .layer
            .as_ref()
            .is_none_or(|l| (l.w, l.h) != (w as usize, h as usize))
        {
            self.layer = Some(Layer::new(
                &self.art,
                w as usize,
                h as usize,
                self.output.aspect(),
            ));
            self.painted = None;
        }
        let Some(layer) = self.layer.as_ref() else {
            return;
        };
        let video = (!self.video.is_empty()).then_some((
            self.video.as_slice(),
            self.video_size.0,
            self.video_size.1,
        ));
        // The artwork around the tube stays in the surface's buffer from
        // frame to frame; it is painted again only when it changes.
        let background = self.painted != Some(recording);
        layer.compose_rgba8(video, recording, self.pixels.frame_mut(), background);
        self.painted = Some(recording);
        if let Err(e) = self.pixels.render() {
            warn!("toaster monitor {}: {e}", self.output.name());
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        self.size = (w, h);
        if let Err(e) = self.pixels.resize_surface(w, h) {
            warn!("toaster monitor resize: {e}");
        }
        if let Err(e) = self.pixels.resize_buffer(w, h) {
            warn!("toaster monitor resize: {e}");
        }
        self.layer = None;
        self.painted = None;
    }
}

impl MonitorWindow {
    fn title(&self) -> String {
        let name = self.output.name();
        if self.output.is_recording() {
            format!("Toaster {name} \u{25CF} REC (R to stop)")
        } else if self.is_program() {
            format!("Toaster {name} (R to record)")
        } else {
            format!("Toaster {name}")
        }
    }

    fn is_program(&self) -> bool {
        self.output.name() == "Program"
    }

    fn resized(&mut self, art: &MonitorArt, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        self.button = button_in(art, w as usize, h as usize);
        self.painter.send(Paint::Resized(w, h));
    }
}

/// The monitor windows of the fitted card.
pub(super) struct CardMonitors {
    /// Loaded on first use: most sessions never fit a card.
    art: std::cell::OnceCell<Option<Arc<MonitorArt>>>,
    windows: Vec<MonitorWindow>,
}

impl CardMonitors {
    pub(super) fn new() -> Self {
        Self {
            art: std::cell::OnceCell::new(),
            windows: Vec::new(),
        }
    }

    fn art(&self, outputs: &[Arc<CardOutput>]) -> Option<Arc<MonitorArt>> {
        self.art
            .get_or_init(|| {
                let root = outputs.iter().find_map(|o| o.media_root());
                MonitorArt::load(root.as_deref())
            })
            .clone()
    }

    /// Make the window set match `outputs`: a window per output, opened
    /// beside `beside` (the main window's outer rectangle).
    pub(super) fn sync(
        &mut self,
        event_loop: &ActiveEventLoop,
        outputs: Vec<Arc<CardOutput>>,
        beside: Option<(PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)>,
    ) {
        let same = outputs.len() == self.windows.len()
            && outputs
                .iter()
                .zip(&self.windows)
                .all(|(o, w)| Arc::ptr_eq(o, &w.output));
        if same {
            return;
        }
        self.windows.clear();
        if outputs.is_empty() {
            return;
        }
        let Some(art) = self.art(&outputs) else {
            return;
        };
        let (aw, ah) = art.size();
        // Half the artwork's size, side by side to the right of the main
        // window where there is room.
        let size = LogicalSize::new(aw as f64 / 2.0, ah as f64 / 2.0);
        for (n, output) in outputs.into_iter().enumerate() {
            let mut attrs = WindowAttributes::default()
                .with_title(format!("Toaster {}", output.name()))
                .with_inner_size(size)
                .with_min_inner_size(LogicalSize::new(aw as f64 / 8.0, ah as f64 / 8.0));
            if let Some((pos, outer)) = beside {
                let x = pos.x + outer.width as i32 + 8;
                let y = pos.y + (n as i32) * (ah as i32 / 2 + 40);
                attrs = attrs.with_position(PhysicalPosition::new(x, y));
            } else {
                attrs = attrs.with_position(LogicalPosition::new(
                    40.0 + n as f64 * (aw as f64 / 2.0 + 16.0),
                    40.0,
                ));
            }
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    warn!("toaster monitor {}: {e}", output.name());
                    continue;
                }
            };
            let inner = window.inner_size();
            let (w, h) = (inner.width.max(1), inner.height.max(1));
            let surface = SurfaceTexture::new(w, h, Arc::clone(&window));
            let mut builder = PixelsBuilder::new(w, h, surface)
                .present_mode(super::present::window_present_mode(false));
            if cfg!(target_os = "linux") {
                builder = builder.wgpu_backend(
                    pixels::wgpu::Backends::from_env().unwrap_or(pixels::wgpu::Backends::VULKAN),
                );
            }
            let pixels = match builder.build() {
                Ok(p) => p,
                Err(e) => {
                    warn!("toaster monitor {}: {e}", output.name());
                    continue;
                }
            };
            info!("toaster monitor window: {}", output.name());
            let canvas = Canvas {
                output: Arc::clone(&output),
                art: Arc::clone(&art),
                pixels,
                layer: None,
                size: (w, h),
                seen: 0,
                video: Vec::new(),
                video_size: (0, 0),
                painted: None,
            };
            let (tx, rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name(format!(
                    "toaster-monitor-{}",
                    output.name().to_ascii_lowercase()
                ))
                .spawn(move || canvas.run(rx));
            let thread = match thread {
                Ok(t) => Some(t),
                Err(e) => {
                    warn!("toaster monitor {}: no painter thread: {e}", output.name());
                    None
                }
            };
            let mw = MonitorWindow {
                output,
                window,
                painter: Painter { tx, thread },
                button: button_in(&art, w as usize, h as usize),
                shown_recording: false,
                cursor: (0.0, 0.0),
            };
            mw.window.set_title(&mw.title());
            mw.painter.send(Paint::Redraw);
            self.windows.push(mw);
        }
    }

    pub(super) fn owns(&self, id: WindowId) -> bool {
        self.windows.iter().any(|w| w.window.id() == id)
    }

    /// Keep each window's title with its recording (the painters follow
    /// the frames and the lamp by themselves).
    pub(super) fn poll(&mut self) {
        for w in &mut self.windows {
            let recording = w.output.is_recording();
            if recording != w.shown_recording {
                w.shown_recording = recording;
                w.window.set_title(&w.title());
            }
        }
    }

    /// Handle an event for one of the monitor windows. `Some(message)` is
    /// something to tell the user (a recording started or stopped).
    pub(super) fn window_event(&mut self, id: WindowId, event: &WindowEvent) -> Option<String> {
        let art = self.art.get().cloned().flatten()?;
        let w = self.windows.iter_mut().find(|w| w.window.id() == id)?;
        match event {
            WindowEvent::RedrawRequested => w.painter.send(Paint::Redraw),
            WindowEvent::Resized(size) => w.resized(&art, size.width, size.height),
            WindowEvent::CloseRequested => {
                // Closing a monitor turns it away rather than unplugging
                // it: the menu brings it back.
                w.window.set_visible(false);
            }
            WindowEvent::CursorMoved { position, .. } => w.cursor = (position.x, position.y),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if w.is_program() && hits(w.button, w.cursor.0, w.cursor.1) => {
                return Some(toggle_recording(w));
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && event.physical_key == PhysicalKey::Code(KeyCode::KeyR)
                    && w.is_program() =>
            {
                return Some(toggle_recording(w));
            }
            _ => {}
        }
        None
    }

    /// Bring every monitor window back into view.
    pub(super) fn show_all(&self) {
        for w in &self.windows {
            w.window.set_visible(true);
            w.window.focus_window();
        }
    }

    /// Start or stop recording the Program output.
    pub(super) fn toggle_program_recording(&mut self) -> Option<String> {
        let w = self.windows.iter_mut().find(|w| w.is_program())?;
        Some(toggle_recording(w))
    }

    pub(super) fn program_recording(&self) -> bool {
        self.windows
            .iter()
            .any(|w| w.is_program() && w.output.is_recording())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Headless: save every output, on its monitor, beside `path`
    /// (`shot.png` gives `shot-program.png`, `shot-preview.png`).
    pub(super) fn save_shots(&self, outputs: &[Arc<CardOutput>], path: &std::path::Path) {
        if outputs.is_empty() {
            return;
        }
        let Some(art) = self.art(outputs) else {
            return;
        };
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("shot");
        for output in outputs {
            let (pixels, w, h) = compose_native(&art, output);
            let name = format!("{stem}-{}.png", output.name().to_ascii_lowercase());
            let target = path.with_file_name(name);
            match crate::screenshot::save(&target, &pixels, w as u32, h as u32) {
                Ok(()) => info!("monitor screenshot saved: {}", target.display()),
                Err(e) => warn!("monitor screenshot {}: {e:#}", target.display()),
            }
        }
    }
}

fn toggle_recording(w: &mut MonitorWindow) -> String {
    let name = w.output.name();
    let message = if w.output.is_recording() {
        match w.output.stop_recording() {
            Ok(Some(path)) => {
                info!("toaster {name} recording saved: {}", path.display());
                format!("{name} recording saved: {}", path.display())
            }
            Ok(None) => format!("{name} was not recording"),
            Err(e) => {
                warn!("toaster {name} recording: {e:#}");
                format!("{name} recording failed: {e}")
            }
        }
    } else {
        match w.output.start_recording_auto() {
            Ok(path) => {
                info!("toaster {name} recording to {}", path.display());
                format!("Recording {name} to {}", path.display())
            }
            Err(e) => {
                warn!("toaster {name} recording: {e:#}");
                format!("{name} recording failed: {e}")
            }
        }
    };
    w.shown_recording = w.output.is_recording();
    w.window.set_title(&w.title());
    w.painter.send(Paint::Redraw);
    message
}

impl super::App {
    /// Open, close and repaint the card's monitor windows. Headless runs
    /// open none (their screenshots carry the monitors instead), nor does
    /// `[video_slot] monitors = false`.
    pub(super) fn service_card_monitors(&mut self, event_loop: &ActiveEventLoop) {
        let wanted = !self.headless_capture_active()
            && self.machine_config.video_slot.monitors.unwrap_or(true);
        let outputs = if wanted {
            self.emu.bus().video_slot_outputs()
        } else {
            Vec::new()
        };
        let beside = self.render.as_ref().and_then(|r| {
            r.window
                .outer_position()
                .ok()
                .map(|p| (p, r.window.outer_size()))
        });
        self.card_monitors.sync(event_loop, outputs, beside);
        self.card_monitors.poll();
    }

    /// Start or stop recording the card's Program output (menu).
    pub(super) fn toggle_card_recording(&mut self) {
        if let Some(message) = self.card_monitors.toggle_program_recording() {
            self.show_osd(message);
        }
    }

    pub(super) fn show_card_monitors(&mut self) {
        self.card_monitors.show_all();
    }

    /// Headless screenshots also save the card's outputs on their
    /// monitors, beside the main capture.
    pub(super) fn save_card_monitor_shots(&self, path: &std::path::Path) {
        let outputs = self.emu.bus().video_slot_outputs();
        self.card_monitors.save_shots(&outputs, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_artwork_has_a_tube() {
        let art = MonitorArt::decode(DEFAULT_ART, true).expect("artwork");
        let (x0, y0, x1, y1) = art.screen;
        // The glass measured off the artwork: 925 x 681 at (265, 119).
        assert!(
            (260..=270).contains(&x0) && (114..=124).contains(&y0),
            "{:?}",
            art.screen
        );
        assert!(
            (1185..=1195).contains(&x1) && (795..=805).contains(&y1),
            "{:?}",
            art.screen
        );
    }

    #[test]
    fn the_fixed_point_tube_follows_the_float_model() {
        let art = MonitorArt::decode(DEFAULT_ART, true).expect("artwork");
        let layer = Layer::new(&art, 1100, 817, 4.0 / 3.0);
        // A picture with detail in every channel.
        let (vw, vh) = (752, 480);
        let video: Vec<u32> = (0..vw * vh)
            .map(|k| {
                let (x, y) = (k % vw, k / vw);
                rgba_u32((x * 3) as u8, (y * 5) as u8, ((x + y) * 7) as u8)
            })
            .collect();
        let mut out = vec![0u32; 1100 * 817];
        layer.compose(Some((&video, vw, vh)), false, &mut out);
        // The float model this replaced, at a spread of tube pixels.
        let (px, py, pw, ph) = layer.picture;
        let (sx, sy) = (vw as f32 / pw, vh as f32 / ph);
        let (x0, y0, x1, y1) = layer.screen;
        let mut checked = 0;
        for y in (y0..y1).step_by(37) {
            for x in (x0..x1).step_by(41) {
                let i = y * 1100 + x;
                let m = layer.mask[i];
                if m == 0 {
                    continue;
                }
                let vy = ((y as f32 + 0.5 - py) * sy - 0.5).clamp(0.0, (vh - 1) as f32);
                let vx = ((x as f32 + 0.5 - px) * sx - 0.5).clamp(0.0, (vw - 1) as f32);
                let (ya, xa) = (vy.floor() as usize, vx.floor() as usize);
                let (yb, xb) = ((ya + 1).min(vh - 1), (xa + 1).min(vw - 1));
                let (ty, tx) = (vy - ya as f32, vx - xa as f32);
                let g = f32::from(layer.glass[i]);
                let shade = 0.72 + 0.28 * (g / 45.0).min(1.0);
                let sheen = (g - 50.0).max(0.0) * 0.9;
                let a = f32::from(m) / 255.0;
                let got = out[i].to_le_bytes();
                let bg = layer.bg[i].to_le_bytes();
                for c in 0..3 {
                    let p = |k: usize| f32::from(video[k].to_le_bytes()[c]);
                    let top = p(ya * vw + xa) * (1.0 - tx) + p(ya * vw + xb) * tx;
                    let bottom = p(yb * vw + xa) * (1.0 - tx) + p(yb * vw + xb) * tx;
                    let v = (top * (1.0 - ty) + bottom * ty) * shade + sheen;
                    let v = (v * a + f32::from(bg[c]) * (1.0 - a)).clamp(0.0, 255.0);
                    assert!(
                        (f32::from(got[c]) - v).abs() <= 3.0,
                        "({x}, {y}) channel {c}: {} vs {v}",
                        got[c]
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 200, "{checked}");
    }

    #[test]
    fn a_frame_lands_on_the_glass_only() {
        let art = MonitorArt::decode(DEFAULT_ART, true).expect("artwork");
        let layer = Layer::new(&art, 728, 540, 4.0 / 3.0);
        let red = vec![rgba_u32(255, 0, 0); 16 * 12];
        let mut out = vec![0u32; 728 * 540];
        layer.compose(Some((&red, 16, 12)), false, &mut out);
        let at = |x: usize, y: usize| out[y * 728 + x].to_le_bytes();
        // The middle of the tube is red, the bezel below it is not.
        let mid = at(364, 230);
        assert!(mid[0] > 150 && mid[1] < 60, "{mid:?}");
        let bezel = at(364, 500);
        assert!(bezel[0] < 120, "{bezel:?}");
    }
}
