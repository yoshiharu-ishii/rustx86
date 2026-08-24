# 起動の長い OS を「ネイティブで進めてブラウザで使う」

DSL 2024 (685MB の CD、glibc、X) はブラウザで**ログインまで 13 分**、X まではさらに数分かかる。
これは待てない。速いネイティブ (PGO ビルド) で目的の状態まで進めて控え、
ブラウザはそこから始める — **控えの受け渡し**という運用でしのぐ。

実測 (2026-08-24、M1):

| | 時間 |
|---|---|
| ブラウザで CD から起動 → ログイン | 約 13 分 |
| ネイティブ (PGO) で起動 → ログイン → X | 約 12 分 |
| **その控えをブラウザで復元 → X の画面** | **約 3 秒** (封筒を開く 1.3s + 復元 1.4s) |

## 1. ネイティブで目的の状態まで進めて控える

```bash
# ログイン画面まで
RAM_MB=256 SNAPSHOT_SAVE=/tmp/dsl-login.snap \
  target/pgo-use/release/examples/boot web/dsl-2024.rc7.iso 60000000000 \
  'boot:' 'text\n' 'login:' ''

# 続きから X まで (控えを読み、ログインして startx。合図が来ない最後の手順は
# 「出ない文字列」を待たせて上限まで回す — 控えは手順の後に必ず書かれる)
RAM_MB=256 SNAPSHOT_LOAD=/tmp/dsl-login.snap SNAPSHOT_SAVE=/tmp/dsl-x.snap \
  target/release/examples/boot web/dsl-2024.rc7.iso 45000000000 \
  '' 'root\n' 'assword:' 'root\n' '~#' 'modprobe bochs-drm; sleep 1; startx\n' 'NEVERMARK' ''
```

- `SNAPSHOT_LOAD` は**第 1 引数の ISO を挿し直す** — CD の像は控えに入っていない
  (685MB を控えに写す意味が無い。素子の状態だけ入り、`cd_wanted()` が印になる)
- **空の合図 `''` は待たずに打つ**。控えから戻した直後は画面が書き換わらないので、
  「画面が変わるまで待つ」では一生合わない
- DSL のプロンプトは `root@dsl:~#` — 合図は `~#` (末尾の空白はテキスト VRAM に残らない)

## 2. ブラウザの封筒に包む

```bash
node tools/webtest/pack-snapshot.mjs /tmp/dsl-x.snap dsl-x L
# → /tmp/dsl-x.rx86snap (380MB の生を gzip して 148MB)
```

## 3. ブラウザで開く

1. Linux の機械を選び、**CD-ROM に同じ ISO を選ぶ** (復元時に挿し直される)
2. `.rx86snap` をページにドロップする (または「イメージを開く…」)

拡張子ではなく**中身の magic** で見分けるので、名前は何でもよい。

## 注意

- 控えには**ゲストのメモリが丸ごと入る** — 配布物のユーザーランドがそのまま含まれる。
  公開リポジトリに置かない (`.gitignore` は `*.snap` / `*.rx86snap` を弾く)
- 版が違う控えは読めない (`snapshot.rs` の `VERSION`)。装置が増えた版で作った控えを
  古い版で開くと「形式の版が違う」で断る — 黙って壊れた機械にはしない
- DSL の `bochs-drm` は udev の coldplug が遅さに負けて (worker が 180 秒で殺される)
  自動では載らない。`udev.event-timeout` を伸ばすと今度は udev が待ち続けて起動が
  終わらない。**手で `modprobe bochs-drm` してから控える**のが今の答え
