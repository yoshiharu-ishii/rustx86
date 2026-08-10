#!/usr/bin/env bash
# bzImage から非圧縮の vmlinux (ELF) を取り出す。
#
#   tools/extract-vmlinux.sh                # images/vmlinuz-lts → images/vmlinux-lts
#   tools/extract-vmlinux.sh in.bz out.elf
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
cd "$(dirname "$0")/.."

IN="${1:-images/vmlinuz-lts}"
OUT="${2:-images/vmlinux-lts}"

python3 - "$IN" "$OUT" <<'EOF'
import subprocess, sys

src, dst = sys.argv[1], sys.argv[2]
data = open(src, 'rb').read()

# 形式ごとのマジックと展開コマンド。gzipのマジックは短く誤検知しうるので、
# **候補を順に試して ELF が出たものを採る**
magics = [
    (b'\x1f\x8b\x08', ['gunzip', '-c']),
    (b'\xfd7zXZ\x00', ['xz', '-dc']),
    (b'\x02!L\x18', ['lz4', '-dc']),
    (b'\x28\xb5\x2f\xfd', ['zstd', '-dc']),
]
start = 0
for magic, cmd in magics:
    pos = data.find(magic)
    while pos >= 0:
        p = subprocess.run(cmd, input=data[pos:], capture_output=True)
        out = p.stdout
        # 途中で切れても手前までは出る (解凍器は末尾のゴミで文句を言うが無視)
        if out.startswith(b'\x7fELF'):
            open(dst, 'wb').write(out)
            print(f'{dst}: {len(out)/1e6:.1f}MB (ELF, {cmd[0]} @ {pos})')
            sys.exit(0)
        pos = data.find(magic, pos + 1)
print('ELF が出てこなかった (未対応の圧縮形式?)', file=sys.stderr)
sys.exit(1)
EOF
