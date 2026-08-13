#!/usr/bin/env bash
# bzImage から非圧縮の vmlinux (ELF) を取り出す。
#
#   tools/images/extract-vmlinux.sh                # images/vmlinuz-lts → images/vmlinux-lts
#   tools/images/extract-vmlinux.sh in.bz out.elf
#
# ## なぜ取り出すのか
#
# bzImage は「カーネルを圧縮したもの + 自己解凍ステブ」で、実行すると
# ゲストの中で解凍が走る。実測でこれが**起動全体の55% (540M命令)** を占め、
# しかもシリアルに何も出せない「無言の黒画面」になる。
# 非圧縮の vmlinux を直接ロードすれば、この区間は丸ごと消える —
# Firecracker が vmlinux を要求するのはまさにこのためである。
#
# 仕組みはカーネル付属 scripts/extract-vmlinux と同じ: 圧縮データの
# マジックを探して展開し、ELF が出てくるまで試す。
set -euo pipefail
cd "$(dirname "$0")/../.."

IN="${1:-images/vmlinuz-lts}"
OUT="${2:-images/vmlinux-lts}"

python3 - "$IN" "$OUT" <<'EOF'
import lzma, subprocess, sys, zlib

src, dst = sys.argv[1], sys.argv[2]
data = open(src, 'rb').read()

# 1MBずつ食わせ、途中で文句が出ても手前までの出力を採る
# (圧縮ストリームの後ろはbzImageのゴミなので、末尾のエラーは無視してよい)
def feed(decomp, b):
    out = bytearray()
    for i in range(0, len(b), 1 << 20):
        try:
            out += decomp(b[i:i + (1 << 20)])
        except Exception:
            break
    return bytes(out)

def run(cmd, b):  # stdlibに無い形式だけ外部コマンドに頼る
    try:
        return subprocess.run(cmd, input=b, capture_output=True).stdout
    except FileNotFoundError:
        return b''

# 形式ごとのマジックと展開器。gzip/xz は標準ライブラリで展開する —
# 外部コマンドはWindowsに無い (Git Bashのgunzipはシェルスクリプトで
# WindowsのPythonから起動できない)。gzipのマジックは短く誤検知しうるので、
# **候補を順に試して ELF が出たものを採る**
magics = [
    (b'\x1f\x8b\x08', 'gzip', lambda b: feed(zlib.decompressobj(31).decompress, b)),
    (b'\xfd7zXZ\x00', 'xz', lambda b: feed(lzma.LZMADecompressor().decompress, b)),
    (b'\x02!L\x18', 'lz4', lambda b: run(['lz4', '-dc'], b)),
    (b'\x28\xb5\x2f\xfd', 'zstd', lambda b: run(['zstd', '-dc'], b)),
]
for magic, name, expand in magics:
    pos = data.find(magic)
    while pos >= 0:
        out = expand(data[pos:])
        if out.startswith(b'\x7fELF'):
            open(dst, 'wb').write(out)
            print(f'{dst}: {len(out)/1e6:.1f}MB (ELF, {name} @ {pos})')
            sys.exit(0)
        pos = data.find(magic, pos + 1)
print('ELF が出てこなかった (未対応の圧縮形式?)', file=sys.stderr)
sys.exit(1)
EOF
