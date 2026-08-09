#!/usr/bin/env python3
"""newc形式のcpioを直接書く — mknodなしでデバイスノードを含められる。

macOSの非rootでは /dev/console (キャラクタデバイス 5:1) を作れないが、
cpioはただのバイト列なので、**アーカイブに直接書けばよい**。
カーネルのinitramfs展開はrootで走るから、ノードは正しく生まれる。

使い方: mkcpio.py <出力> <ルートdir> [--console]
"""
import os
import sys
import stat


def entry(out, name, mode, body=b"", rdev=(0, 0), ino=[100]):
    ino[0] += 1
    hdr = (
        b"070701"
        + f"{ino[0]:08X}".encode()
        + f"{mode:08X}".encode()
        + b"00000000" * 2  # uid gid
        + b"00000001"  # nlink
        + b"00000000"  # mtime
        + f"{len(body):08X}".encode()
        + b"00000000" * 2  # devmajor devminor
        + f"{rdev[0]:08X}".encode()
        + f"{rdev[1]:08X}".encode()
        + f"{len(name) + 1:08X}".encode()
        + b"00000000"  # check
    )
    out += hdr + name.encode() + b"\0"
    out += b"\0" * ((4 - len(out) % 4) % 4)
    out += body
    out += b"\0" * ((4 - len(out) % 4) % 4)
    return out


def main():
    dst, root = sys.argv[1], sys.argv[2]
    with_console = "--console" in sys.argv
    out = bytearray()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for d in dirnames:
            rel = os.path.relpath(os.path.join(dirpath, d), root)
            out = entry(out, rel, stat.S_IFDIR | 0o755)
        for f in sorted(filenames):
            full = os.path.join(dirpath, f)
            rel = os.path.relpath(full, root)
            with open(full, "rb") as fh:
                body = fh.read()
            mode = stat.S_IFREG | (0o755 if os.access(full, os.X_OK) else 0o644)
            out = entry(out, rel, mode, body)
    if with_console:
        # /dev/console = キャラクタデバイス major5 minor1
        out = entry(out, "dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
    out = entry(out, "TRAILER!!!", 0)
    with open(dst, "wb") as fh:
        fh.write(bytes(out))
    print(f"{dst}: {len(out)} bytes")


main()
