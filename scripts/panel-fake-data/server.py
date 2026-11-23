#!/usr/bin/env python3
"""面板假数据服务：只实现视觉核验需要的 GET 端点。

存在的理由：`scripts/panel-layout-audit.py` 默认连真实面板（需要 token 与远端 base）。
前端改动的视觉核验不该依赖一台正在跑游戏的机器——那样每改一次样式都要找人开服。
这里用同一份 dist，只把 fetch 指向本地假响应。

形状来源（不猜）：src/interfaces/http/routes/*.rs 的实际 JSON 构造。
"""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

NUMBER_BOMB_LIVE = {
    "enabled": True, "phase": "playing", "players": [{"display": "甲"}, {"display": "乙"}, {"display": "丙"}],
    "lower": 1, "upper": 64, "guessed": [12, 33, 47], "turn": 1,
    "elapsed_seconds": 17, "turn_timeout_seconds": 60,
    "last_result": {"loser": 2, "bomb": 58},
}
IDIOM_LIVE = {
    "enabled": True, "phase": "active", "mode": "成语接龙", "starter": "甲",
    "expected_first": "足", "total": 7,
    "history": ["画蛇添足", "足智多谋", "谋定后动", "动人心弦"],
    "lexicon_len": 30892,
}
CARD_GAME_LIVE = {
    "enabled": True, "phase": "playing", "variant": "斗地主",
    "players": [
        {"name": "甲", "hand_count": 17, "trustee": False, "timeouts": 0},
        {"name": "乙", "hand_count": 17, "trustee": False, "timeouts": 1},
        {"name": "丙", "hand_count": 16, "trustee": True, "timeouts": 0},
    ],
    "current": 1, "landlord": 0, "elapsed_seconds": 42,
    "lobby_timeout_seconds": 120, "turn_timeout_seconds": 45,
}
TURTLE = {"enabled": True, "phase": "asking", "phaseLabel": "提问中", "elapsedSeconds": 128,
          "participantCount": 4, "participants": ["甲", "乙", "丙", "丁"], "questionCount": 3}
UNDERCOVER = {"enabled": True, "phase": "voting", "round": 2,
              "players": [{"name": "甲", "alive": True}, {"name": "乙", "alive": False}],
              "remainingSeconds": 55}

ROUTES = {
    "/number-bomb": NUMBER_BOMB_LIVE,
    "/idiom-chain": IDIOM_LIVE,
    "/card-game": CARD_GAME_LIVE,
    "/turtle-soup": TURTLE,
    "/undercover": UNDERCOVER,
    "/monitor": {"turtleSoup": TURTLE, "undercover": UNDERCOVER, "logs": ["数字炸弹 开局", "成语接龙 已接 7 条"]},
    "/health/services": {"ok": True},
    "/config": {"revision": 42, "sections": [], "loadMode": "lenient", "quarantinedCount": 0, "quarantined": []},
}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802
        path = self.path.split("?")[0]
        body = ROUTES.get(path)
        if body is None:
            # 未知端点一律回空对象而不是 404：面板很多页面会并行拉一堆端点，
            # 404 会把整页拖进错误态，截图就看不到布局了。
            body = {}
        raw = json.dumps(body).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(length)
        raw = json.dumps({"ok": True}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    import sys
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18899
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
