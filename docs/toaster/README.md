# Video Toaster and Flyer: working specs

Notes recovered from NewTek's released sources (switcher, CG, project
editor, Flyer firmware and flyer.library) while emulating the Video
Toaster 4000 and the Video Toaster Flyer in Copperline. Each claim carries
a source reference and a confidence tag. Paths such as `/home/claude/vt/...`
or `/home/claude/fly/...` point at the unpacked source releases
(AmigaVideoToasterComplete, open-video-toaster-src) on the machine the
notes were written on.

- `toaster-protocol.md`: how the card talks to the Amiga (header lines,
  extended headers, the parallel port read-back).
- `video-path-spec.md`: the video path: muxes, fader, keys, framestores,
  which Amiga bits each per-pixel mode reads.
- `dve-spec.md`: the digital effects (framestore addressed per line and
  per quad by the Amiga picture).
- `key-align.md`: why key pages sat 16 samples off (the AA zipper write
  and EH0 LUST).
- `cg-titles.md`: making ToasterCG title pages from outside and keying
  them over Program.
- `flyer-spec.md`: the Flyer board, its command mailbox and the plan for
  high-level emulation.

The Flyer clip format is in `crates/flyerclip/FORMAT.md`. The emulation
itself is described in `docs/internals/video.md` (Toaster section) and
configured as in `docs/guide/configuration.md`.
