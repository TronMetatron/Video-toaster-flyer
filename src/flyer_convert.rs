// SPDX-License-Identifier: GPL-3.0-or-later

//! Video files to NewTek Video Flyer clips and back: the `flyerclip`
//! program and the MCP bridge's `flyer_import` / `flyer_export` tools.
//!
//! A Flyer clip holds NTSC composite video (the Toaster framestore's 4fsc
//! samples, 752 per line) coded field by field with VTASC, and 16-bit
//! audio at 44.1 kHz x 1000/1001, all in colour frames of four fields
//! (`flyerclip`, from NewTek's Flyer sources). Importing decodes the source
//! with ffmpeg to 752 x 480 at 29.97 frames a second (4:3, letterboxed or
//! pillarboxed like a Toaster input), encodes each frame's two fields as
//! composite samples (fields I/II from one frame, III/IV, with the
//! subcarrier inverted, from the next) and codes them. Exporting decodes
//! the fields, separates luma and chroma per field, weaves each pair back
//! into a frame and has ffmpeg write the file.

use crate::toaster::composite;
use anyhow::{anyhow, bail, Context, Result};
use flyerclip::vtasc::TOASTER_LINES;
use flyerclip::{Clip, ClipWriter, FrameVideo, Grade};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Samples per line, and the picture's lines.
const W: usize = 752;
const H: usize = 480;
/// Video frames per second, and the Flyer's audio rate as ffmpeg takes it.
const FPS: &str = "30000/1001";
const AUDIO_RATE: u32 = 44056;
/// Audio samples per channel per field, and fields per colour frame.
const PER_FIELD: usize = flyerclip::audio::SAMPLES_PER_FIELD;

/// How to import.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Per-field size budget: std (default), HQ5, HQ6.
    pub grade: Grade,
    /// Keep the sound (stereo); off writes a silent, video-only clip.
    pub audio: bool,
    /// Stop after this many seconds of the source.
    pub max_seconds: Option<f64>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            grade: Grade::Std,
            audio: true,
            max_seconds: None,
        }
    }
}

/// What a conversion did.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub frames: usize,
    pub fields: u32,
    pub seconds: f64,
    pub audio_channels: u8,
    pub bytes: u64,
    /// Fields that had to be coded sub-Nyquist or with loss to fit.
    pub degraded_fields: usize,
    /// Fields cut at the size budget (their bottom lines are garbage).
    pub truncated_fields: usize,
}

/// ffmpeg: `COPPERLINE_FFMPEG`, then `search` (e.g. the Toaster files
/// folder), beside this program, the `PATH`.
pub fn find_ffmpeg(search: Option<&Path>) -> Option<PathBuf> {
    let root = search
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::paths::toaster_files_dir);
    crate::toaster::media::find_ffmpeg(&root)
}

fn command(ffmpeg: &Path) -> Command {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashing up.
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// The source's sound as interleaved stereo at the Flyer's rate; empty
/// when it has none.
fn read_audio(ffmpeg: &Path, input: &Path, max_seconds: Option<f64>) -> Result<Vec<i16>> {
    let mut cmd = command(ffmpeg);
    cmd.arg("-i").arg(input);
    if let Some(t) = max_seconds {
        cmd.args(["-t", &format!("{t}")]);
    }
    cmd.args([
        "-vn",
        "-ac",
        "2",
        "-ar",
        &AUDIO_RATE.to_string(),
        "-f",
        "s16le",
        "-",
    ])
    .stdin(Stdio::null())
    .stderr(Stdio::null());
    let out = cmd.output().context("running ffmpeg for the sound")?;
    // No audio stream is an error exit with nothing written: silence.
    Ok(out
        .stdout
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect())
}

/// The four fields of a colour frame from two pictures (`b` the later):
/// I/II the even and odd lines of `a`, III/IV those of `b` with the
/// subcarrier inverted (the framestore's complemented copy).
fn colour_frame(a: &[u32], b: &[u32]) -> FrameVideo {
    let mut normal = vec![0u8; W];
    let mut complemented = vec![0u8; W];
    let black = vec![u32::from_le_bytes([0, 0, 0, 0xFF]); W];
    let mut fields: FrameVideo = Default::default();
    for (f, field) in fields.iter_mut().enumerate() {
        let picture = if f < 2 { a } else { b };
        *field = (0..TOASTER_LINES)
            .map(|y| {
                let line = 2 * y + (f & 1);
                let row = if line < H {
                    &picture[line * W..(line + 1) * W]
                } else {
                    &black[..]
                };
                composite::encode_line(row, line, &mut normal, &mut complemented);
                if f < 2 {
                    normal.clone()
                } else {
                    complemented.clone()
                }
            })
            .collect();
    }
    fields
}

/// Convert a video file (anything ffmpeg reads) into a Flyer clip.
pub fn import(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    options: &ImportOptions,
    mut progress: impl FnMut(usize),
) -> Result<Report> {
    let audio = if options.audio {
        read_audio(ffmpeg, input, options.max_seconds)?
    } else {
        Vec::new()
    };
    let channels: u8 = if audio.is_empty() { 0 } else { 2 };

    let filter = format!(
        "scale=640:480:force_original_aspect_ratio=decrease,\
         pad=640:480:(ow-iw)/2:(oh-ih)/2:black,scale={W}:{H},fps={FPS}"
    );
    let mut cmd = command(ffmpeg);
    cmd.arg("-i").arg(input);
    if let Some(t) = options.max_seconds {
        cmd.args(["-t", &format!("{t}")]);
    }
    cmd.args([
        "-an", "-vf", &filter, "-pix_fmt", "rgba", "-f", "rawvideo", "-",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting ffmpeg")?;
    let mut stdout = child.stdout.take().context("ffmpeg's output")?;

    let mut writer = ClipWriter::new(options.grade, channels);
    let mut report = Report {
        audio_channels: channels,
        ..Report::default()
    };
    let mut buf = vec![0u8; W * H * 4];
    let read_picture = |stdout: &mut dyn Read, buf: &mut Vec<u8>| -> Option<Vec<u32>> {
        stdout.read_exact(buf).ok()?;
        Some(
            buf.chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], 0xFF]))
                .collect(),
        )
    };
    let per_frame = 4 * PER_FIELD * usize::from(channels);
    while let Some(a) = read_picture(&mut stdout, &mut buf) {
        // An odd last picture fills its colour frame on its own.
        let (b, last) = match read_picture(&mut stdout, &mut buf) {
            Some(b) => (b, false),
            None => (a.clone(), true),
        };
        let video = colour_frame(&a, &b);
        let start = report.frames * per_frame;
        let sound = (channels > 0).then(|| {
            let end = (start + per_frame).min(audio.len());
            &audio[start.min(end)..end]
        });
        let stats = writer.push_frame(Some(&video), sound);
        for s in stats.iter().flatten() {
            if s.truncated {
                report.truncated_fields += 1;
            } else if s.tolerance > 0 || s.algo == flyerclip::Algo::SubNyquist {
                report.degraded_fields += 1;
            }
        }
        report.frames += 1;
        progress(report.frames);
        if last {
            break;
        }
    }
    drop(stdout);
    let status = child.wait()?;
    if report.frames == 0 {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        bail!(
            "ffmpeg read no video from {} ({status}): {}",
            input.display(),
            err.trim()
        );
    }
    let bytes = writer.finish();
    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(output, &bytes).with_context(|| format!("writing {}", output.display()))?;
    report.fields = (report.frames * 4) as u32;
    report.seconds = report.fields as f64 / 60.0 * 1.001;
    report.bytes = bytes.len() as u64;
    Ok(report)
}

/// One field's 242 lines as RGBA rows of the picture they belong to.
fn decode_field_rows(lines: &[Vec<u8>], field: usize, picture: &mut [u32]) {
    let mut row = vec![0u32; W];
    for (y, samples) in lines.iter().enumerate() {
        let line = 2 * y + (field & 1);
        if line >= H {
            break;
        }
        let positive = flyerclip::composite::phase_positive(field as u32, y);
        composite::decode_single(samples, positive, &mut row);
        picture[line * W..(line + 1) * W].copy_from_slice(&row);
    }
}

/// Convert a Flyer clip into a video file (the container and codec come
/// from the extension: MP4 gets H.264 and AAC).
pub fn export(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    mut progress: impl FnMut(usize),
) -> Result<Report> {
    let bytes = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let clip =
        Clip::read(&bytes).map_err(|e| anyhow!("{}: not a Flyer clip: {e:?}", input.display()))?;
    let fields = clip.field_count();
    if fields == 0 {
        bail!("{} has no video", input.display());
    }
    let (channels, audio) = clip.audio();

    // The sound goes through a temporary file: ffmpeg reads two inputs.
    let audio_file = if channels > 0 && !audio.is_empty() {
        let path = std::env::temp_dir().join(format!(
            "flyerclip-{}-{}.pcm",
            std::process::id(),
            crate::timestamp::compact_now()
        ));
        let mut raw = Vec::with_capacity(audio.len() * 2);
        for s in &audio {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, raw)?;
        Some(path)
    } else {
        None
    };

    let mut cmd = command(ffmpeg);
    cmd.arg("-y")
        .args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{W}x{H}"),
        ])
        .args(["-r", FPS, "-i", "-"]);
    if let Some(path) = &audio_file {
        cmd.args(["-f", "s16le", "-ar", &AUDIO_RATE.to_string()])
            .args(["-ac", &channels.to_string(), "-i"])
            .arg(path);
    }
    cmd.args(["-vf", "scale=640:480,setsar=1"]);
    let is_mp4 = output
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "m4v" | "mov"));
    if is_mp4 {
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "16"]);
        if audio_file.is_some() {
            cmd.args(["-c:a", "aac", "-b:a", "192k"]);
        }
    }
    cmd.arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("starting ffmpeg")?;
    let mut stdin = child.stdin.take().context("ffmpeg's input")?;

    let mut report = Report {
        audio_channels: channels,
        fields,
        ..Report::default()
    };
    let mut picture = vec![u32::from_le_bytes([0, 0, 0, 0xFF]); W * H];
    let mut bytes_out = vec![0u8; W * H * 4];
    let mut write_err = None;
    for n in 0..fields {
        let lines = clip
            .decode_field(n)
            .map_err(|e| anyhow!("field {n}: {e:?}"))?;
        let f = (n % 4) as usize;
        decode_field_rows(&lines, f, &mut picture);
        // Fields II and IV complete a picture.
        if f & 1 == 1 || n + 1 == fields {
            for (o, p) in bytes_out.chunks_exact_mut(4).zip(&picture) {
                o.copy_from_slice(&p.to_le_bytes());
            }
            if let Err(e) = stdin.write_all(&bytes_out) {
                write_err = Some(e);
                break;
            }
            report.frames += 1;
            progress(report.frames);
        }
    }
    drop(stdin);
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait()?;
    if let Some(path) = audio_file {
        let _ = std::fs::remove_file(path);
    }
    if !status.success() || write_err.is_some() {
        bail!(
            "ffmpeg could not write {} ({status}): {}",
            output.display(),
            err.trim()
        );
    }
    report.seconds = f64::from(fields) / 60.0 * 1.001;
    report.bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    Ok(report)
}

/// `path`, or when that exists `name (2).ext`, `name (3).ext`...: a
/// default output never replaces a file.
pub fn unused_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let ext = path.extension().and_then(|e| e.to_str());
    (2..)
        .map(|n| {
            path.with_file_name(match ext {
                Some(ext) => format!("{stem} ({n}).{ext}"),
                None => format!("{stem} ({n})"),
            })
        })
        .find(|p| !p.exists())
        .expect("an unused name")
}

/// A one-paragraph description of a clip.
pub fn describe(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let clip =
        Clip::read(&bytes).map_err(|e| anyhow!("{}: not a Flyer clip: {e:?}", path.display()))?;
    let h = &clip.header;
    let fields = clip.field_count();
    Ok(format!(
        "{}: Flyer clip, {} fields ({:.2} s), {} audio channel(s), video grade {}, \
         Flyer version {}, VTASC chips {}, {} bytes",
        path.display(),
        fields,
        f64::from(fields) / 60.0 * 1.001,
        h.audio_chans,
        match h.vid_grade {
            0 => "std",
            1 => "HQ5",
            2 => "HQ6",
            _ => "?",
        },
        h.version,
        h.vid_cmp_ver,
        bytes.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_frame_round_trips_through_vtasc() {
        // Colour bars on picture a, a grey ramp on picture b.
        let bars = [
            [191u8, 191, 191],
            [191, 191, 0],
            [0, 191, 191],
            [0, 191, 0],
            [191, 0, 191],
            [191, 0, 0],
            [0, 0, 191],
        ];
        let a: Vec<u32> = (0..W * H)
            .map(|i| {
                let [r, g, b] = bars[(i % W) * 7 / W];
                u32::from_le_bytes([r, g, b, 0xFF])
            })
            .collect();
        let b: Vec<u32> = (0..W * H)
            .map(|i| {
                let v = ((i % W) * 255 / W) as u8;
                u32::from_le_bytes([v, v, v, 0xFF])
            })
            .collect();
        let mut writer = ClipWriter::new(Grade::Hq6, 0);
        writer.push_frame(Some(&colour_frame(&a, &b)), None);
        let bytes = writer.finish();
        let clip = Clip::read(&bytes).unwrap();
        assert_eq!(clip.field_count(), 4);
        let mut pa = vec![0u32; W * H];
        let mut pb = vec![0u32; W * H];
        for n in 0..4 {
            let lines = clip.decode_field(n).unwrap();
            decode_field_rows(&lines, n as usize, if n < 2 { &mut pa } else { &mut pb });
        }
        let close = |got: u32, want: u32, slack: i32| {
            let (g, w) = (got.to_le_bytes(), want.to_le_bytes());
            (0..3).all(|k| (i32::from(g[k]) - i32::from(w[k])).abs() <= slack)
        };
        // Inside each bar (away from the edges the filters smear) and on
        // the ramp, on both fields' lines. The Flyer's FIR comes back
        // with chroma a little hot, so colour gets more slack.
        for row in [10, 11, 200, 301] {
            for bar in 0..7 {
                let x = bar * W / 7 + W / 14;
                assert!(
                    close(pa[row * W + x], a[row * W + x], 40),
                    "bar {bar} row {row}: {:?}",
                    pa[row * W + x].to_le_bytes()
                );
            }
            for x in [100, 376, 650] {
                assert!(
                    close(pb[row * W + x], b[row * W + x], 12),
                    "ramp {x} row {row}: {:?}",
                    pb[row * W + x].to_le_bytes()
                );
            }
        }
    }
}
