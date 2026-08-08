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
import base64
from pathlib import Path


class NoCacheHandler(SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store, must-revalidate")
        self.send_header("Pragma", "no-cache")
        self.send_header("Expires", "0")
        super().end_headers()

    def log_message(self, *args):
        pass  # 静かにする

    def do_POST(self):
        """画面の写しを受け取って `docs/images/` へ置く。

        エミュレータの画面は canvas なので、`toDataURL()` を投げてもらえば
        **ページの装飾が混ざらない、画素そのままの絵**が手に入る。
        画面写真を撮り直したくなるたびに手で撮るのは続かないので、
        開発サーバーに受け口を付けておく。

            POST /shot/elks-tetris   本文は data:image/png;base64,...

        開発用サーバーなので 127.0.0.1 でしか待ち受けていない。
        """
        if not self.path.startswith("/shot/"):
            self.send_error(404)
            return
        name = Path(self.path[len("/shot/"):]).name
        if not name or not all(c.isalnum() or c in "-_" for c in name):
            self.send_error(400, "名前が不正")
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", 0))).decode()
        _, _, b64 = body.partition("base64,")
        out = Path(__file__).resolve().parent.parent / "docs" / "images" / f"{name}.png"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(base64.b64decode(b64))
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.end_headers()
        self.wfile.write(f"{out} ({out.stat().st_size} bytes)\n".encode())


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8001
    root = Path(__file__).parent
    handler = partial(NoCacheHandler, directory=str(root))
    print(f"http://localhost:{port}/  (Ctrl-C で停止)")
    ThreadingHTTPServer(("127.0.0.1", port), handler).serve_forever()
