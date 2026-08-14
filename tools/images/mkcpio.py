#!/usr/bin/env python3
"""newc形式のcpioを直接書く — mknodなしでデバイスノードを含められる。

macOSの非rootでは /dev/console (キャラクタデバイス 5:1) を作れないが、
cpioはただのバイト列なので、**アーカイブに直接書けばよい**。
カーネルのinitramfs展開はrootで走るから、ノードは正しく生まれる。

使い方: mkcpio.py <出力> <ルートdir> [--console]
        mkcpio.py --extract <アーカイブ(.gz可)> <展開先dir>

--extract は cpio コマンドの無いホスト (Windows) 用の読み側。
シンボリックリンクは実体のコピーとして展開する — Windowsでリンクを
作るには権限が要るし、この後の cp も実体を運ぶので同じことになる。
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


def extract(src, root):
    data = open(src, "rb").read()
    if data[:3] == b"\x1f\x8b\x08":
        import gzip

        data = gzip.decompress(data)
    links = []  # シンボリックリンクは全実体を出し終えてから解決する
    pos, count = 0, 0
    while True:
        pos = data.find(b"070701", pos)  # 連結アーカイブ (マイクロコード等) も歩ける
        if pos < 0:
            break

        def field(i):
            return int(data[pos + 6 + 8 * i : pos + 14 + 8 * i], 16)

        mode, size, namesize = field(1), field(6), field(11)
        name = data[pos + 110 : pos + 110 + namesize - 1].decode()
        body_off = (pos + 110 + namesize + 3) // 4 * 4
        body = data[body_off : body_off + size]
        pos = (body_off + size + 3) // 4 * 4
        if name == "TRAILER!!!":
            continue
        rel = os.path.normpath(name)
        if rel.startswith("..") or os.path.isabs(rel):
            continue  # アーカイブに外へ書かせない
        dest = os.path.join(root, rel)
        kind = stat.S_IFMT(mode)
        if kind == stat.S_IFDIR:
            os.makedirs(dest, exist_ok=True)
        elif kind == stat.S_IFREG:
            os.makedirs(os.path.dirname(dest) or ".", exist_ok=True)
            with open(dest, "wb") as fh:
                fh.write(body)
            count += 1
        elif kind == stat.S_IFLNK:
            links.append((dest, body.decode()))
        # デバイスノード等は展開しない (要らないし、非rootでは作れない)
    for dest, target in links:
        base = root if target.startswith("/") else os.path.dirname(dest)
        full = os.path.normpath(os.path.join(base, target.lstrip("/")))
        if os.path.isfile(full):
            os.makedirs(os.path.dirname(dest) or ".", exist_ok=True)
            with open(full, "rb") as s, open(dest, "wb") as d:
                d.write(s.read())
            count += 1
    print(f"{root}: {count} ファイルを展開")


def main():
    if sys.argv[1] == "--extract":
        extract(sys.argv[2], sys.argv[3])
        return
    dst, root = sys.argv[1], sys.argv[2]
    with_console = "--console" in sys.argv
    out = bytearray()

    def rel_posix(path):  # cpioの区切りは常に / (Windowsのrelpathは \ を返す)
        return os.path.relpath(path, root).replace(os.sep, "/")

    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for d in dirnames:
            rel = rel_posix(os.path.join(dirpath, d))
            out = entry(out, rel, stat.S_IFDIR | 0o755)
        for f in sorted(filenames):
            full = os.path.join(dirpath, f)
            rel = rel_posix(full)
            if os.path.islink(full):
                # symlinkはリンクのまま運ぶ (本文=リンク先)。実体を複製すると
                # /etc/ssl/cert.pem のようなCA束の別名で179KBが二重になる
                out = entry(out, rel, stat.S_IFLNK | 0o777, os.readlink(full).encode())
                continue
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
