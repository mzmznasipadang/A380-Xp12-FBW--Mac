"""Pad a thin, unsigned 64-bit Mach-O so LC_SYMTAB's string pool is 8-byte aligned.

The macOS 27 linker (ld-27106.1) can leave the string pool 4 bytes off an 8-byte
boundary, in either slice, and dyld then refuses the library ("mis-aligned
LINKEDIT string pool"). Inserts the missing zero bytes before the pool and bumps
`stroff` and `__LINKEDIT.filesize`. Only handles the layout seen here: the pool is
the last thing in the file, true once the code signature is removed.

Usage: align_strtab.py file   (rewrites it in place; a no-op when aligned)
"""
import struct
import sys

LC_SEGMENT_64, LC_SYMTAB = 0x19, 0x2

path = sys.argv[1]
data = bytearray(open(path, "rb").read())
magic, _, _, _, ncmds, _, _, _ = struct.unpack_from("<IiiIIIII", data, 0)
assert magic == 0xFEEDFACF, "not a thin 64-bit Mach-O"

off, symtab, linkedit = 32, None, None
for _ in range(ncmds):
    cmd, size = struct.unpack_from("<II", data, off)
    if cmd == LC_SYMTAB:
        symtab = off
    elif cmd == LC_SEGMENT_64 and data[off + 8:off + 24].rstrip(b"\0") == b"__LINKEDIT":
        linkedit = off
    off += size
assert symtab and linkedit, "no LC_SYMTAB or __LINKEDIT"

stroff, strsize = struct.unpack_from("<II", data, symtab + 16)
pad = (-stroff) % 8
if pad == 0:
    print(f"{path}: string pool already aligned")
    sys.exit(0)
assert stroff + strsize == len(data), "string pool is not last; remove the signature first"
le_filesize = struct.unpack_from("<Q", data, linkedit + 48)[0]
le_vmsize = struct.unpack_from("<Q", data, linkedit + 32)[0]
assert le_filesize + pad <= le_vmsize, "no room in __LINKEDIT vmsize"

data[stroff:stroff] = b"\0" * pad
struct.pack_into("<I", data, symtab + 16, stroff + pad)
struct.pack_into("<Q", data, linkedit + 48, le_filesize + pad)
open(path, "wb").write(data)
print(f"{path}: stroff {stroff} -> {stroff + pad}, __LINKEDIT filesize +{pad}")
