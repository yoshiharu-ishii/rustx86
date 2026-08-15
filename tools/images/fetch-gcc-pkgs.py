"""Alpine v3.24 x86 から gcc 一式を依存ごと取ってきて展開する。

apk コマンドは使えない (ホストが macOS でも動く必要がある) ので、APKINDEX を
自分で読んで依存の閉包を出す。.apk は gzip ストリームの連結で、中身は tar なので
tar で開ける (署名とメタデータの余計なエントリだけ捨てる)。

使い方: fetch-gcc-pkgs.py <展開先dir>
"""

import io
import os
import subprocess
import sys
import tarfile
import urllib.request

MIRROR = "https://dl-cdn.alpinelinux.org/alpine/v3.24"
ARCH = "x86"
REPOS = ["main", "community"]
# この3つの閉包で C のコンパイル→アセンブル→リンクが揃う。
# musl-dev は crt1.o とヘッダ (これが無いと #include <stdio.h> で止まる)
WANT = ["gcc", "musl-dev", "binutils"]
OUT = sys.argv[1] if len(sys.argv) > 1 else "gccroot"


def get(url):
    with urllib.request.urlopen(url, timeout=120) as r:
        return r.read()


def load_index(repo):
    """APKINDEX.tar.gz → レコードの一覧"""
    raw = get(f"{MIRROR}/{repo}/{ARCH}/APKINDEX.tar.gz")
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as t:
        text = t.extractfile("APKINDEX").read().decode("utf-8", "replace")
    recs = []
    for block in text.split("\n\n"):
        if not block.strip():
            continue
        r = {"P": None, "V": None, "D": [], "p": [], "repo": repo}
        for line in block.splitlines():
            k, _, v = line.partition(":")
            if k in ("P", "V"):
                r[k] = v
            elif k in ("D", "p"):
                r[k] = v.split()
        if r["P"]:
            recs.append(r)
    return recs


records, provides = {}, {}
for repo in REPOS:
    for r in load_index(repo):
        records.setdefault(r["P"], r)
        provides.setdefault(r["P"], r["P"])
        # so:libc.musl-x86.so.1 のような「提供するもの」名でも引けるようにする
        for p in r["p"]:
            provides.setdefault(p.split("=")[0], r["P"])

# 依存の閉包 (BFS)。バージョン制約と競合 (!) は読み飛ばす
seen, queue = set(), list(WANT)
while queue:
    name = queue.pop()
    if name in seen:
        continue
    pkg = provides.get(name)
    if pkg is None or pkg in seen:
        continue
    seen.add(pkg)
    for d in records[pkg]["D"]:
        if d.startswith("!"):
            continue
        base = d.split("=")[0].split(">")[0].split("<")[0]
        if base.startswith("pc:"):  # pkg-config の要求は要らない
            continue
        queue.append(base)

print(f"取る package: {len(seen)}")
os.makedirs(OUT, exist_ok=True)
total = 0
for pkg in sorted(seen):
    r = records[pkg]
    url = f"{MIRROR}/{r['repo']}/{ARCH}/{pkg}-{r['V']}.apk"
    try:
        blob = get(url)
    except Exception as e:  # noqa: BLE001
        print(f"  ! {pkg}: {e}")
        continue
    total += len(blob)
    path = os.path.join(OUT, "_apk.tgz")
    with open(path, "wb") as f:
        f.write(blob)
    subprocess.run(
        [
            "tar", "xzf", path, "-C", OUT,
            "--exclude", ".SIGN.*", "--exclude", ".PKGINFO",
            "--exclude", ".pre-install", "--exclude", ".post-install",
            "--exclude", ".trigger",
        ],
        check=False,
        stderr=subprocess.DEVNULL,
    )
    os.unlink(path)
    print(f"  {pkg}-{r['V']} ({len(blob) // 1024} KB)")
print(f"\n合計ダウンロード {total / 1e6:.1f} MB")
