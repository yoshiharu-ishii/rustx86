#!/usr/bin/env python3
"""開発用の静的サーバー。**キャッシュを一切させない**。

`python3 -m http.server` を使っていたとき、ブラウザが JS や wasm を
キャッシュし続けて古いコードが動いていた。実装を3回書き換えても結果が
1バイトも変わらず、原因の切り分けを大きく誤った。

`?v=` を付けて回るのは付け忘れが起きる。開発中は毎回取りに来させるのが確実。

    python3 web/serve.py [port]
"""
import sys
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class NoCacheHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, must-revalidate")
        self.send_header("Pragma", "no-cache")
        self.send_header("Expires", "0")
        super().end_headers()

    def log_message(self, *args):
        pass  # 静かにする


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8001
    root = Path(__file__).parent
    handler = partial(NoCacheHandler, directory=str(root))
    print(f"http://localhost:{port}/  (Ctrl-C で停止)")
    ThreadingHTTPServer(("127.0.0.1", port), handler).serve_forever()
