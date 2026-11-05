// SPDX-License-Identifier: GPL-3.0-or-later

//! The ribbon between the Flyer and the Toaster 4000.
//!
//! On the real cards the Toaster's inputs 3 and 4 pass through the Flyer,
//! which can put its playback channel A on input 3 and channel B on input
//! 4 instead (`ToasterMux`), and the Flyer's audio output is the system's
//! sound (the Toaster has none). Here the two cards are separate devices
//! (the Flyer a Zorro board, the Toaster on the video slot), so they share
//! a [`FlyerLink`] made when both are fitted:
//!
//! - the Flyer sets the routing and, every field, says what each channel
//!   shows (a clip field, black, a matte) and adds a field of its sound;
//! - the Toaster, when it composes a frame, asks for a routed input's
//!   picture and takes the sound since the last frame.
//!
//! Clip fields are decoded only when the Toaster asks for a channel, so a
//! channel nobody looks at costs nothing.

use super::clip::ClipFile;
use std::sync::{Arc, Mutex};

/// Picture size: the Toaster's inputs.
pub const PICTURE_W: usize = 752;
pub const PICTURE_H: usize = 480;

/// What a Flyer channel puts out for a field.
#[derive(Clone, Debug)]
pub enum FieldSource {
    /// Field `n` of a clip.
    Clip(Arc<ClipFile>, u32),
    /// A flat colour (RGBA): black, or the clip's end matte.
    Matte(u32),
}

/// Black, as the Toaster's pictures hold it.
pub const BLACK: u32 = u32::from_le_bytes([0, 0, 0, 0xFF]);

/// One playback channel's output as a picture: its two fields woven.
struct Channel {
    picture: Vec<u32>,
    /// Fields shown since the picture was last decoded, by line parity.
    pending: [Option<(Arc<ClipFile>, u32)>; 2],
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            picture: vec![BLACK; PICTURE_W * PICTURE_H],
            pending: [None, None],
        }
    }
}

/// Most of the Flyer's sound kept when nobody takes it: one second.
const SOUND_CAP: usize = 44_100;

#[derive(Default)]
struct Shared {
    /// TOASTMUX: input 3 shows channel A, input 4 channel B.
    routed: [bool; 2],
    channels: [Channel; 2],
    /// Stereo output at the Flyer's rate (735 samples a field), not yet
    /// taken by the Toaster.
    sound: Vec<(f32, f32)>,
    /// Peak of the Toaster's Program sound in its last frame (0-1), the
    /// level the Flyer's record meters show.
    line_peak: (f32, f32),
}

/// The shared state; see the module documentation.
#[derive(Default)]
pub struct FlyerLink {
    shared: Mutex<Shared>,
}

impl std::fmt::Debug for FlyerLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlyerLink").finish_non_exhaustive()
    }
}

impl FlyerLink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- The Flyer's side ----

    /// TOASTMUX: whether inputs 3 and 4 show the Flyer's channels A and B.
    pub fn set_routing(&self, input3: bool, input4: bool) {
        self.lock().routed = [input3, input4];
    }

    /// Channel `ch` (0 = A, 1 = B) shows `source` this field.
    pub fn show(&self, ch: usize, source: FieldSource) {
        let mut s = self.lock();
        let Some(channel) = s.channels.get_mut(ch) else {
            return;
        };
        match source {
            FieldSource::Clip(clip, n) => channel.pending[(n & 1) as usize] = Some((clip, n)),
            FieldSource::Matte(rgba) => {
                channel.pending = [None, None];
                channel.picture.fill(rgba);
            }
        }
    }

    /// A field of the Flyer's sound, at its own rate.
    pub fn push_sound(&self, sound: &[(f32, f32)]) {
        let mut s = self.lock();
        s.sound.extend_from_slice(sound);
        if s.sound.len() > SOUND_CAP {
            let excess = s.sound.len() - SOUND_CAP;
            s.sound.drain(..excess);
        }
    }

    /// The peak level the Toaster last put out (for the record meters).
    pub fn line_peak(&self) -> (f32, f32) {
        self.lock().line_peak
    }

    // ---- The Toaster's side ----

    /// The Flyer channel on Toaster input `n` (0-based), if it is routed
    /// there: channel A on input 3, B on input 4.
    pub fn channel_on_input(&self, n: usize) -> Option<usize> {
        let ch = n.checked_sub(2).filter(|&c| c < 2)?;
        self.lock().routed[ch].then_some(ch)
    }

    /// Channel `ch`'s current picture into `out` (752 x 480), decoding the
    /// fields it showed since the last call.
    pub fn picture(&self, ch: usize, out: &mut [u32]) {
        let mut s = self.lock();
        let Some(channel) = s.channels.get_mut(ch) else {
            out.fill(BLACK);
            return;
        };
        let jobs: Vec<(Arc<ClipFile>, u32)> = channel
            .pending
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        for (n, rows) in decode_all(&jobs) {
            weave(&rows, n, &mut channel.picture);
        }
        let len = out.len().min(channel.picture.len());
        out[..len].copy_from_slice(&channel.picture[..len]);
    }

    /// The Flyer's sound since the last call, at its own rate (735
    /// samples a field).
    pub fn take_sound(&self) -> Vec<(f32, f32)> {
        std::mem::take(&mut self.lock().sound)
    }

    /// Note the peak of the sound the Toaster just put out.
    pub fn set_line_peak(&self, peak: (f32, f32)) {
        self.lock().line_peak = peak;
    }
}

/// Decode the fields in `jobs`: the two of a frame side by side where
/// threads are available.
fn decode_all(jobs: &[(Arc<ClipFile>, u32)]) -> Vec<(u32, Vec<Vec<u8>>)> {
    let one = |(clip, n): &(Arc<ClipFile>, u32)| clip.decode_field(*n).map(|rows| (*n, rows));
    #[cfg(not(target_arch = "wasm32"))]
    if jobs.len() > 1 {
        return std::thread::scope(|scope| {
            let handles: Vec<_> = jobs[1..]
                .iter()
                .map(|job| scope.spawn(move || one(job)))
                .collect();
            let mut out: Vec<_> = one(&jobs[0]).into_iter().collect();
            for h in handles {
                if let Ok(Some(r)) = h.join() {
                    out.push(r);
                }
            }
            out
        });
    }
    jobs.iter().filter_map(one).collect()
}

/// Put a decoded field (`n % 4` of its colour frame) on its lines of the
/// picture: field I/III on the even lines, II/IV on the odd.
fn weave(rows: &[Vec<u8>], n: u32, picture: &mut [u32]) {
    let f = n % 4;
    for (y, samples) in rows.iter().enumerate() {
        let line = 2 * y + (f & 1) as usize;
        if line >= PICTURE_H {
            break;
        }
        let positive = flyerclip::composite::phase_positive(f, y);
        crate::toaster::composite::decode_single(
            samples,
            positive,
            &mut picture[line * PICTURE_W..(line + 1) * PICTURE_W],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_3_and_4_carry_channels_a_and_b_when_routed() {
        let link = FlyerLink::new();
        assert_eq!(link.channel_on_input(2), None);
        link.set_routing(true, false);
        assert_eq!(link.channel_on_input(2), Some(0));
        assert_eq!(link.channel_on_input(3), None);
        assert_eq!(link.channel_on_input(0), None);
        link.set_routing(false, true);
        assert_eq!(link.channel_on_input(3), Some(1));
    }

    #[test]
    fn a_clip_field_is_decoded_onto_its_lines_when_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Clip");
        super::super::clip::tests::write_test_clip(&path, 2);
        let clip = Arc::new(ClipFile::open(&path).unwrap());
        let link = FlyerLink::new();
        let white = u32::from_le_bytes([255, 255, 255, 255]);
        link.show(0, FieldSource::Matte(white));
        let mut out = vec![0u32; PICTURE_W * PICTURE_H];
        link.picture(0, &mut out);
        assert!(out.iter().all(|&p| p == white));
        // Field 4 (field I) replaces the even lines only.
        link.show(0, FieldSource::Clip(Arc::clone(&clip), 4));
        link.picture(0, &mut out);
        let grey = |p: u32| p.to_le_bytes()[1];
        assert!(grey(out[100 * PICTURE_W + 300]) < 200);
        assert_eq!(out[101 * PICTURE_W + 300], white);
        // Then field 5 the odd ones; channel B is untouched.
        link.show(0, FieldSource::Clip(clip, 5));
        link.picture(0, &mut out);
        assert!(grey(out[101 * PICTURE_W + 300]) < 200);
        link.picture(1, &mut out);
        assert!(out.iter().all(|&p| p == BLACK));
    }

    #[test]
    fn sound_is_kept_until_taken_and_bounded() {
        let link = FlyerLink::new();
        link.push_sound(&[(0.5, -0.5); 735]);
        link.push_sound(&[(0.25, 0.0); 735]);
        let s = link.take_sound();
        assert_eq!(s.len(), 1470);
        assert_eq!(s[0], (0.5, -0.5));
        assert!(link.take_sound().is_empty());
        for _ in 0..100 {
            link.push_sound(&[(0.1, 0.1); 735]);
        }
        assert_eq!(link.take_sound().len(), SOUND_CAP);
    }
}
