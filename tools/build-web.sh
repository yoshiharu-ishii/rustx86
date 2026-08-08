#!/usr/bin/env bash
#
# ブラウザ版を焼き直す。wasm を作り、キャッシュ破りの番号を上げる。
#
# ## なぜ要るのか
#
# `docs/build.md` に「出力が変わらないときは、まず**そのコードが本当に
# 走っているか**を疑う」と書いてある。ブラウザはキャッシュを効かせるので、
# wasm を作り直しても古いものが読まれる。だから `?v=` を付けている。
#
# ところが**その番号を手で上げていた**。1日で20回近く、4ファイルにわたって。
# 上げ忘れれば古いコードが走り、片方だけ上げれば
#
#     TypeError: emu.key is not a function        (糊が新しく .wasm が古い)
#     wasm.emulator_cursor_row is not a function  (逆)
#
# になる。**手順書に「気をつけて」と書いてある作業は、いずれ必ず失敗する。**
#
# ## 番号を1つにする
#
# 以前はファイルごとにばらばらだった (11 / 21 / 6)。**揃える。**
# `web/` の中の `?v=` を全部同じ番号にすれば、
# 「糊だけ新しい」という状態が**構造的に作れなくなる**。
#
# 番号は今ある最大値+1。別のファイルに持たないので、置き忘れも起きない。
#
# ## 使い方
#
#     tools/build-web.sh              # 焼き直して番号を上げる
#     tools/build-web.sh --bump-only  # 作り直さず番号だけ (JS/HTMLだけ直したとき)
#     tools/build-web.sh --check      # 番号が揃っているかだけ見る (CI向け)

set -euo pipefail

cd "$(dirname "$0")/.."
WEB=web
# `?v=` を持つファイル。生成物 (pkg/) は対象外 — あちらは参照される側である
files() { find "$WEB" -maxdepth 1 -type f \( -name '*.js' -o -name '*.html' \); }

mode=build
case "${1:-}" in
  --bump-only) mode=bump ;;
  --check) mode=check ;;
  -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  '') ;;
  *) echo "知らない引数: $1  (--bump-only / --check / --help)" >&2; exit 2 ;;
esac

# --- いま使われている番号を集める ---
#
# 配列も mapfile も使わない。**macOS の bash は 3.2** で、`mapfile` が無い
# (実際に `mapfile: command not found` で落ちた)。CI の ubuntu と手元の Mac の
# 両方で同じに動く書き方だけを使う
versions=$(grep -ho '?v=[0-9][0-9]*' $(files) | sed 's/?v=//' | sort -un)
if [ -z "$versions" ]; then
  echo "web/ に ?v= が見つからない。付け先が無いので何もしない" >&2
  exit 1
fi
count=$(echo "$versions" | wc -l | tr -d ' ')
max=$(echo "$versions" | tail -1)

if [ "$mode" = check ]; then
  if [ "$count" -eq 1 ]; then
    echo "OK: ?v= は全部 $max で揃っている"
    exit 0
  fi
  echo "NG: ?v= が揃っていない: $(echo $versions)" >&2
  echo "    片方だけ新しいと「その関数は無い」と言われる。tools/build-web.sh で揃える" >&2
  grep -Hno '?v=[0-9][0-9]*' $(files) >&2
  exit 1
fi

# --- wasm を作る ---
#
# **番号を上げる前に作る。** 失敗したのに番号だけ進むと、
# 「新しいはずなのに古い挙動」という一番たちの悪い状態になる
if [ "$mode" = build ]; then
  if ! command -v wasm-pack >/dev/null 2>&1; then
    echo "wasm-pack が無い。  cargo install wasm-pack" >&2
    exit 1
  fi
  if ! rustup target list --installed 2>/dev/null | grep -q wasm32-unknown-unknown; then
    echo "wasm32 ターゲットが無い。  rustup target add wasm32-unknown-unknown" >&2
    exit 1
  fi
  echo "==> wasm を作る"
  (cd wasm && wasm-pack build --release --target web --out-dir ../"$WEB"/pkg)
fi

# --- 番号を揃えて上げる ---
next=$((max + 1))
echo "==> ?v= を $next に揃える (これまで: $(echo $versions))"
# sed の -i は GNU と BSD で書き方が違う。perl なら両方で同じに書ける
perl -pi -e "s/\?v=\d+/?v=$next/g" $(files)

# --- 確かめる ---
after=$(grep -ho '?v=[0-9][0-9]*' $(files) | sed 's/?v=//' | sort -un)
if [ "$after" != "$next" ]; then
  echo "揃わなかった: $(echo $after)" >&2
  exit 1
fi

if [ "$mode" = build ]; then
  wasm=$(ls -l "$WEB"/pkg/rustx86_wasm_bg.wasm | awk '{print $5}')
  glue=$(ls -l "$WEB"/pkg/rustx86_wasm.js | awk '{print $5}')
  printf '    .wasm %s KB / 糊 %s KB\n' "$((wasm / 1024))" "$((glue / 1024))"
fi
echo "==> できた。python3 web/serve.py 8001 で開く"
