#!/usr/bin/env bash
#
# ブラウザ版を焼き直す (wasm-pack の包み紙)。
#
#     tools/build-web.sh
#
# ## 以前ここにあった「?v= 番号を揃える」仕組みは廃止した
#
# キャッシュ破りの `?v=番号` を全ファイルに付けて手で上げていた。番号が
# ずれると新旧のコードが混ざって `emu.key is not a function` になり、
# 揃える検査までCIに積んだ。
#
# だが**キャッシュ問題は serve.py がとっくに解決していた** — あちらは
# 全応答に `Cache-Control: no-store` を送る。同じ問題を2回別の方法で直し、
# 手作業の方 (?v=) だけが残っていた。番号を消せば、上げ忘れも、ずれも、
# ずれの検査も、仕組みごと消える。**検査で見張るより、事故れない構造にする。**
#
# (将来 GitHub Pages 等のキャッシュする配信に載せるときは、配信側の
#  デプロイでコンテンツハッシュを付ける。手で番号を上げる方式には戻さない)

set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v wasm-pack >/dev/null 2>&1; then
  echo "wasm-pack が無い。  cargo install wasm-pack" >&2
  exit 1
fi
if ! rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown; then
  echo "wasm32 ターゲットが無い。  rustup target add wasm32-unknown-unknown" >&2
  exit 1
fi

echo "==> wasm を作る"
# JIT (F1a call_indirect): 間接呼び出しテーブルを growable にする。生成ブロックの
# wasm関数を JS が table.grow+set でここへ据え、core (Rust) は関数ポインタ経由の
# call_indirect で **JS境界なし** に呼ぶ (ADR-0008)。テーブルは wasm-bindgen の
# function_table() で JS へ渡る。--growable-table = 上限なしで拡張可 (これが無いと
# 既定は min=max=関数数で grow できない)
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=--growable-table"
(cd wasm && wasm-pack build --release --target web --out-dir ../web/pkg)

wasm=$(wc -c < web/pkg/rustx86_wasm_bg.wasm)
glue=$(wc -c < web/pkg/rustx86_wasm.js)
printf '    .wasm %s KB / 糊 %s KB\n' "$((wasm / 1024))" "$((glue / 1024))"
echo "==> できた。python3 web/serve.py 8001 で開く (serve.py がキャッシュを切る)"
