"""番犬 — コマンドを走らせ、出力が止まったらプロセスグループごと殺す。

    python3 tools/watchdog.py --idle 300 --log bench.log -- cargo run --release --example guestcmd
    python3 tools/watchdog.py --idle 300 --total 3600 --log a.log -- sh tools/…

無限ループ・永久待ちの抑止。設計は2連敗の反省から:

- **殺すのはプロセスグループ** (start_new_session で自分のグループに隔離してから
  killpg)。pkill -f のパターン照合は使わない — 見張り役自身のコマンドラインに
  パターンが載っていて**自分を撃った**実績がある
- **「反応」はログの伸びで見る**。guestcmd はシリアルを流し見せ + 5G命令ごとの
  心拍を打つので、ログが --idle 秒伸びない = 本当に止まっている。
  沈黙が正常な区間 (コンパイル中など) は心拍が埋める

出口コード: 対象の終了コードそのまま / 124 = idleで殺した / 125 = totalで殺した
"""

import argparse
import os
import signal
import subprocess
import sys
import time


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--idle", type=int, default=300, help="ログがこの秒数伸びなければ殺す")
    ap.add_argument("--total", type=int, default=0, help="全体の上限秒 (0=無し)")
    ap.add_argument("--log", required=True, help="stdout/stderrの書き先 (伸びの監視対象)")
    ap.add_argument("cmd", nargs=argparse.REMAINDER, help="-- の後に対象コマンド")
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    if not cmd:
        ap.error("-- の後にコマンドを書く")

    log = open(a.log, "ab", buffering=0)
    # start_new_session: 対象を自分のプロセスグループへ。killpgの範囲が
    # 「この子とその子孫」だけになり、見張り役や無関係の同名プロセスに流れ弾が出ない
    child = subprocess.Popen(cmd, stdout=log, stderr=log, start_new_session=True)

    t0 = time.monotonic()
    last_size = -1
    last_grow = t0
    verdict = 0
    while True:
        rc = child.poll()
        if rc is not None:
            return rc  # 自分で終わった — 番犬の出番なし
        now = time.monotonic()
        size = os.path.getsize(a.log)
        if size != last_size:
            last_size = size
            last_grow = now
        if now - last_grow > a.idle:
            print(f"[watchdog] {a.idle}秒 出力が伸びない — 殺す (log {size}B)", file=sys.stderr)
            verdict = 124
            break
        if a.total and now - t0 > a.total:
            print(f"[watchdog] 全体上限 {a.total}秒 — 殺す", file=sys.stderr)
            verdict = 125
            break
        time.sleep(5)

    # TERM → 5秒待って残っていれば KILL (行儀よく死ぬ猶予を与える)
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        return child.wait()
    for _ in range(10):
        if child.poll() is not None:
            break
        time.sleep(0.5)
    if child.poll() is None:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
    return verdict


if __name__ == "__main__":
    sys.exit(main())
