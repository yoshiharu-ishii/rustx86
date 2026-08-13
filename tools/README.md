# tools/ — 役割別の地図

| ディレクトリ | 役割 | 中身 |
|---|---|---|
| `build/` | 焼く | `build-web.sh` (wasmを焼いてブランチ名をBUILDINFOに刻む)、`pgo-build.sh` (PGO実験用 — 常設運用はしない、ADR-0009) |
| `images/` | OSイメージの入手と組み立て | `fetch-images.sh` (入手経路はこれただ一つ)、`extract-vmlinux.sh`、`make-mini-initramfs.sh`、`make-games-initramfs.sh`、`make-linux-snapshot.sh`、`mkcpio.py` |
| `webtest/` | wasmの検証と定規 | `headless.mjs` (CIの「wasm起動」門番+時間の定規)、`ab.sh` (交互A/B — ネイティブと同じ形式)、`v86-bench.mjs` |
| `perf/` | 計測の解析 | `bootprof-resolve.py` (bootprofサンプル→関数名ヒストグラム) |
| `guest/` | ゲスト側ソース | ゲスト内で動かすプログラム (snake等) |

流儀: 計測の尺度は**時間** (秒)。MIPSは出さない (2026-08-13、命令数は決定的なので変数は時間だけ)。
