#!/usr/bin/env python3
"""Let the Toaster switcher play effect sounds with a Flyer fitted.
animfxhandler.a (switcher 4.x): `tst.l TB_FlyerBase(a5) / bne 18$  ;dont do
sound if flyer!` skips loading an effect's sounds whenever flyer.library is
open. This turns the bne.w into two NOPs. Usage: patch.py FILE... (undo:
--undo FILE...)."""
import sys, re
undo = '--undo' in sys.argv
pat = re.compile(rb'\x4a\xad\x03\x72(\x66\x00\x00\x90|\x4e\x71\x4e\x71)\x4a\x2a\x00\x7d\x66\x0e\x4a\x2a\x00\x7e', re.S)
for f in [a for a in sys.argv[1:] if a != '--undo']:
    d = bytearray(open(f, 'rb').read())
    hits = list(pat.finditer(d))
    if len(hits) != 1:
        print(f, 'pattern found', len(hits), 'times; not changed'); continue
    at = hits[0].start(1)
    new = b'\x66\x00\x00\x90' if undo else b'\x4e\x71\x4e\x71'
    if d[at:at+4] == new:
        print(f, 'already', 'original' if undo else 'patched'); continue
    d[at:at+4] = new
    open(f, 'wb').write(d)
    print(f, 'restored' if undo else 'patched', hex(at))
