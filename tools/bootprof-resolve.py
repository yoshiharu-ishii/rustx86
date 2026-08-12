# bootprof のサンプルをカーネルのSystem.mapで関数名に解決してヒストグラムを出す。
# (Alpineの配布vmlinuxはシンボルが剥がされているのでELF symtabは使えない —
#  netbootディレクトリの System.map を使う。tools/fetch-images.sh 参照)
#   python3 tools/bootprof-resolve.py /tmp/bootprof.txt images/System.map-lts
import sys
from bisect import bisect_right

samples_path, sysmap_path = sys.argv[1], sys.argv[2]

# System.map: "アドレス 種別 名前"。コード (T/t) だけ拾う
syms = []
for line in open(sysmap_path):
    parts = line.split()
    if len(parts) == 3 and parts[1] in ("T", "t"):
        syms.append((int(parts[0], 16), parts[2]))
syms.sort()
addrs = [a for a, _ in syms]

hist = {}
total = 0
for line in open(samples_path):
    line = line.strip()
    if not line:
        continue
    ip = int(line, 16)
    total += 1
    i = bisect_right(addrs, ip) - 1
    # 直近シンボルから1MB以上離れていたら不明扱い (リアルモード・トランポリン等)
    if i < 0 or ip - addrs[i] > 0x100000:
        name = f"<不明 {ip >> 24:#x}xx帯>"
    else:
        name = syms[i][1]
    hist[name] = hist.get(name, 0) + 1

print(f"総サンプル {total} (1点 ≒ 4096命令 ≒ {total * 4096 // 1_000_000}M命令)\n")
cum = 0.0
for name, c in sorted(hist.items(), key=lambda kv: -kv[1])[:40]:
    pct = c * 100.0 / total
    cum += pct
    print(f"{c * 4096 // 1_000_000:>5}M  {pct:5.1f}%  累積{cum:5.1f}%  {name}")
