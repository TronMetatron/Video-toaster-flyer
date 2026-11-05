// SPDX-License-Identifier: GPL-3.0-or-later

//! NewTek Video Toaster Flyer: a high-level emulation of the Zorro II card.
//!
//! The Flyer is a nonlinear video recorder and player for the Toaster
//! 4000: its own 68000, three SCSI buses, VTASC compression FPGAs, a DSP
//! for audio and a ribbon to the Toaster that puts its two playback
//! channels on Toaster inputs 3 and 4. To the Amiga it is nothing but a
//! 256 KB window onto its shared SRAM (manufacturer 2191, product 0).
//! flyer.library downloads the 68000 firmware (Flyer.bin) into that SRAM
//! and from then on talks to it through a mailbox of eight command slots
//! (see [`mailbox`]); no Amiga code touches a Flyer chip directly.
//!
//! So the Flyer's 68000 is not emulated. The window is plain memory, and
//! a write to a slot's opcode word runs the command in Rust ([`ops`]),
//! answering as the firmware would. The design and the evidence for it
//! are written up in NewTek's released sources as cited in the code
//! (`flyerlib.asm` for the library, the firmware's `amishar.c`, `ops.c`,
//! `hard.c`).
//!
//! Fitted with `[video_slot] flyer = true` next to the Toaster.

pub mod clip;
pub mod drive;
pub mod fs;
pub mod link;
pub mod mailbox;
pub mod ops;
pub mod play;

use crate::zorro::{BoardBacking, BoardSpec, ZorroVersion};
use crate::zorro_device::{DeviceHost, ZorroDevice};
use link::FlyerLink;
use mailbox::SRAM_SIZE;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// NewTek's Zorro manufacturer number (`NEWTEK_ID`).
pub const MANUFACTURER: u16 = 2191;
/// The Flyer's product number (`FLYER_ID`).
pub const PRODUCT: u8 = 0;

/// The Flyer's autoconfig identity: a Zorro II I/O board whose 256 KB
/// window is the shared SRAM (flyer.library uses `cd_BoardAddr` as the
/// SRAM base). `slot` is the index of the matching [`Flyer`] device in
/// `Bus::devices`.
pub fn board_spec(slot: usize) -> BoardSpec {
    BoardSpec {
        name: "Video Flyer".into(),
        version: ZorroVersion::II,
        manufacturer: MANUFACTURER,
        product: PRODUCT,
        serial: 0,
        size_bytes: SRAM_SIZE,
        backing: BoardBacking::Device(slot),
        memlist: false,
        memory_space: false,
        chained: false,
        no_shutup: false,
        window: 0,
        diag_vec: None,
    }
}

/// The Flyer card: its shared SRAM and the state of its firmware.
#[derive(Serialize, Deserialize)]
pub struct Flyer {
    sram: Vec<u8>,
    firmware: ops::Firmware,
    /// The Toaster's host folder; the Flyer's drives are its `Flyer`
    /// subfolders.
    media: Option<PathBuf>,
}

impl Flyer {
    /// A Flyer at power-on, its drives in `media` (the Toaster's host
    /// folder): drive FA0 is the folder `Flyer/FA0` there, created if
    /// missing, and FB0, FC0, ... are there when their folders are.
    pub fn new(media: Option<PathBuf>) -> Self {
        let drives = media.as_ref().map(|m| m.join("Flyer"));
        if let Some(flyer) = &drives {
            let fa0 = flyer.join(drive::drive_name(0));
            if let Err(e) = std::fs::create_dir_all(&fa0) {
                log::warn!("flyer: cannot create {}: {e}", fa0.display());
            }
        }
        Self {
            sram: vec![0; SRAM_SIZE],
            firmware: ops::Firmware::new().with_drives(drives),
            media,
        }
    }

    /// Connect the card to the Toaster beside it.
    pub fn with_link(mut self, link: Option<Arc<FlyerLink>>) -> Self {
        self.firmware = std::mem::take(&mut self.firmware).with_link(link);
        self
    }

    /// Take over the ribbon to the Toaster from the card this one replaces
    /// (a loaded state replaces the board; the Toaster stays).
    pub fn adopt_link_from(&mut self, live: &Flyer) {
        self.firmware.adopt_link(live.firmware.link().cloned());
    }

    /// The firmware state, for tests and the debugger.
    pub fn firmware(&self) -> &ops::Firmware {
        &self.firmware
    }

    /// The shared SRAM as the Amiga sees it.
    pub fn sram(&self) -> &[u8] {
        &self.sram
    }

    /// The Toaster's host folder the drives live in.
    pub fn media(&self) -> Option<&std::path::Path> {
        self.media.as_deref()
    }
}

impl ZorroDevice for Flyer {
    fn read(&mut self, off: u32, size: usize, _host: &mut DeviceHost) -> u32 {
        let off = off as usize;
        (0..size).fold(0u32, |value, i| {
            (value << 8) | u32::from(mailbox::rd8(&self.sram, off + i))
        })
    }

    fn write(&mut self, off: u32, size: usize, value: u32, _host: &mut DeviceHost) {
        let off = off as usize;
        for i in 0..size {
            let shift = 8 * (size - 1 - i);
            mailbox::wr8(&mut self.sram, off + i, (value >> shift) as u8);
        }
        self.firmware.after_write(&mut self.sram, off, size);
    }

    fn peek_word(&self, off: u32) -> Option<u16> {
        // The window is plain memory: reading it changes nothing.
        Some(mailbox::rd16(&self.sram, off as usize))
    }

    fn tick(&mut self, _cck: u32, host: &mut DeviceHost) {
        if let Some(beam) = host.beam() {
            self.firmware.advance_beam(beam);
            self.firmware.run_fields(&mut self.sram);
        }
    }

    fn reset(&mut self) {
        self.firmware.reset();
    }

    fn kind(&self) -> &'static str {
        "flyer"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flyer::mailbox::{err, op, Slot, STAT_DONE, STAT_INPROG, STAT_NEW};
    use crate::memory::Memory;
    use crate::zorro_device::HostBeam;

    fn memory() -> Memory {
        Memory {
            chip_ram: vec![0; 0x100],
            slow_ram: Vec::new(),
            mb_ram: Vec::new(),
            accel_ram: Vec::new(),
            rom: Vec::new(),
            overlay: false,
            zorro: crate::zorro::ZorroChain::default(),
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs: Vec::new(),
            wcs_write_protected: false,
        }
    }

    #[test]
    fn the_board_autoconfigs_as_newteks_flyer() {
        let spec = board_spec(3);
        assert_eq!(spec.manufacturer, 2191);
        assert_eq!(spec.product, 0);
        assert_eq!(spec.version, ZorroVersion::II);
        assert_eq!(spec.size_bytes, 256 * 1024);
        assert_eq!(spec.backing, BoardBacking::Device(3));
        assert!(!spec.memlist);
        assert!(crate::zorro::zorro_ii_size_code(spec.size_bytes).is_some());
        let mut chain = crate::zorro::ZorroChain::default();
        chain.add_board(spec).unwrap();
    }

    #[test]
    fn the_window_is_the_sram_at_every_access_size() {
        let mut board = Flyer::new(None);
        let mut mem = memory();
        let mut host = DeviceHost::new(&mut mem);
        board.write(0x1_8000, 4, 0x4BF9_00C2, &mut host);
        board.write(0x1_8004, 2, 0xDB64, &mut host);
        board.write(0x1_8006, 1, 0x46, &mut host);
        assert_eq!(board.read(0x1_8000, 2, &mut host), 0x4BF9);
        assert_eq!(board.read(0x1_8002, 4, &mut host), 0x00C2_DB64);
        assert_eq!(board.read(0x1_8006, 1, &mut host), 0x46);
        assert_eq!(board.peek_word(0x1_8000), Some(0x4BF9));
        assert_eq!(board.sram()[0x3_FFFF], 0);
    }

    /// Drive the board through its window, as flyer.library does.
    fn command(
        board: &mut Flyer,
        host: &mut DeviceHost,
        slot: Slot,
        opcode: u16,
        args: &[(u32, u32)],
    ) {
        let base = slot.base() as u32;
        board.write(base, 2, 0x0001, host);
        for off in 2..32 {
            board.write(base + off, 1, 0, host);
        }
        for &(off, value) in args {
            board.write(base + off, 4, value, host);
        }
        board.write(base, 2, u32::from(opcode | STAT_NEW), host);
    }

    #[test]
    fn a_command_written_through_the_window_completes_before_the_next_read() {
        let mut board = Flyer::new(None);
        let mut mem = memory();
        let mut host = DeviceHost::new(&mut mem);
        // Flyer.bin downloaded, then FIRMWARE.
        board.write(0x1_8000, 4, 0x4BF9_00C2, &mut host);
        command(
            &mut board,
            &mut host,
            Slot(0),
            op::FIRMWARE,
            &[(4, 0x1_8000), (8, 96092)],
        );
        let word = board.read(0x500, 2, &mut host) as u16;
        assert_eq!(word, op::FIRMWARE | STAT_DONE);
        assert_eq!(board.read(0x503, 1, &mut host) as u8, err::OKAY);
        assert_eq!(board.firmware().cpu, ops::Cpu::Running);
        board.write(0x500, 2, 0, &mut host);
        // EEREAD of the set-up magic, the long write covering +4..+7.
        command(&mut board, &mut host, Slot(0), op::EEREAD, &[(4, 0)]);
        assert_eq!(
            board.read(0x500, 2, &mut host) as u16,
            op::EEREAD | STAT_DONE
        );
        assert_eq!(board.read(0x506, 2, &mut host), 0xBEEF);
    }

    #[test]
    fn an_amiga_reset_returns_the_card_to_its_boot_rom() {
        let mut board = Flyer::new(None);
        let mut mem = memory();
        let mut host = DeviceHost::new(&mut mem);
        command(&mut board, &mut host, Slot(0), op::FIRMWARE, &[]);
        assert_eq!(board.firmware().cpu, ops::Cpu::Running);
        board.reset();
        assert_eq!(board.firmware().cpu, ops::Cpu::Rom);
    }

    #[test]
    fn ticks_follow_the_beam_the_host_reports() {
        let mut board = Flyer::new(None);
        let mut mem = memory();
        // A host without a beam (a CPU access) changes nothing.
        board.tick(100, &mut DeviceHost::new(&mut mem));
        for (frame, vpos) in [(0, 0), (0, 200), (1, 0), (1, 200)] {
            let beam = HostBeam {
                vpos,
                long_frame: true,
                frame,
            };
            board.tick(100, &mut DeviceHost::new(&mut mem).with_beam(beam));
        }
        assert_eq!(board.firmware().field_clock, 2);
    }

    #[test]
    fn a_clip_played_through_the_mailbox_reaches_the_toaster_link() {
        let dir = tempfile::tempdir().unwrap();
        let fa0 = dir.path().join("Flyer/FA0");
        std::fs::create_dir_all(&fa0).unwrap();
        clip::tests::write_test_clip(&fa0.join("Clip"), 3);
        let link = link::FlyerLink::new();
        let mut board =
            Flyer::new(Some(dir.path().to_path_buf())).with_link(Some(Arc::clone(&link)));
        let mut mem = memory();
        let mut host = DeviceHost::new(&mut mem);
        command(&mut board, &mut host, Slot(0), op::FIRMWARE, &[]);
        board.write(0x500, 2, 0, &mut host);
        command(&mut board, &mut host, Slot(0), op::PLAYMODE, &[]);
        board.write(0x500, 2, 0, &mut host);
        // ToasterMux(1, 0, 0): channel A on input 3.
        command(
            &mut board,
            &mut host,
            Slot(0),
            op::TOASTMUX,
            &[(4, 0x0100_0000)],
        );
        board.write(0x500, 2, 0, &mut host);
        assert_eq!(link.channel_on_input(2), Some(0));
        // The library's PassClipAction: path, FlyerVolume, ClipAction.
        let (path, volume, action) = (0x1000u32, 0x1040u32, 0x1080u32);
        for (i, b) in b"FlyerA0:Clip\0".iter().enumerate() {
            board.write(path + i as u32, 1, u32::from(*b), &mut host);
        }
        board.write(volume, 4, path, &mut host);
        board.write(action, 4, volume, &mut host);
        board.write(action + 8, 4, 0x0300_0700, &mut host); // RT_STOPPED, A, video+stereo
        board.write(action + 38, 4, 0xFFFF_FFFF, &mut host); // full volume
        board.write(action + 44, 4, 0x8000_7FFF, &mut host); // hard left, right
        command(&mut board, &mut host, Slot(1), op::FLYPLAY, &[(4, action)]);
        assert_eq!(Slot(1).status(board.sram()), STAT_INPROG);
        assert_eq!(Slot(1).cont(board.sram()), 2);
        // Fields go by: the clip starts 20 fields on, at a colour frame.
        let mut sound = Vec::new();
        for frame in 0..40u64 {
            for vpos in [0, 200] {
                let beam = HostBeam {
                    vpos,
                    long_frame: true,
                    frame,
                };
                board.tick(100, &mut DeviceHost::new(&mut mem).with_beam(beam));
            }
            sound.extend(link.take_sound());
            if frame == 25 {
                let mut out = vec![0u32; link::PICTURE_W * link::PICTURE_H];
                link.picture(0, &mut out);
                assert!(out.iter().any(|&p| p != link::BLACK));
            }
        }
        assert_eq!(Slot(1).opcode_word(board.sram()), op::FLYPLAY | STAT_DONE);
        assert_eq!(Slot(1).error(board.sram()), mailbox::err::OKAY);
        assert_eq!(mailbox::rd32(board.sram(), action as usize + 126), 12);
        // A field of sound for every field, the clip's twelve not silent.
        assert_eq!(sound.len() % clip::SAMPLES_PER_FIELD, 0);
        let loud = sound
            .chunks(clip::SAMPLES_PER_FIELD)
            .filter(|f| f.iter().any(|&(l, _)| l != 0.0))
            .count();
        assert_eq!(loud, 11, "field 0 of the test clip is silent");
    }

    #[test]
    fn the_board_survives_a_save_state_round_trip() {
        let mut board = Flyer::new(Some(PathBuf::from("/tmp/tf")));
        let mut mem = memory();
        let mut host = DeviceHost::new(&mut mem);
        command(&mut board, &mut host, Slot(0), op::FIRMWARE, &[]);
        let bytes =
            bincode::serialize(&crate::zorro_device::BoardDevice::Flyer(Box::new(board))).unwrap();
        let back: crate::zorro_device::BoardDevice = bincode::deserialize(&bytes).unwrap();
        let crate::zorro_device::BoardDevice::Flyer(back) = back else {
            panic!("not a Flyer");
        };
        assert_eq!(back.firmware().cpu, ops::Cpu::Running);
        assert_eq!(back.sram().len(), SRAM_SIZE);
        assert_eq!(back.media(), Some(std::path::Path::new("/tmp/tf")));
    }
}
