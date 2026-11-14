#!/usr/bin/env python3
"""把 UI 状态分类器的真实证据渲染成画廊（PNG + markdown + TSV）。

数据来源只有一个：`dump_ui_state_evidence_for_gallery` 导出的 JSONL
（生产分类器 + 生产结构判据的真实测量值）。本脚本不计算任何分数，
不写死任何“看起来对”的数字；它只负责画框、排版、汇总和链接校验。

用法：
    python3 scripts/render-ui-evidence-gallery.py target/evidence-gallery/records.jsonl
"""

from __future__ import annotations

import csv
import json
import os
import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
GALLERY_DIR = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery"
CORPUS_DIR = ROOT / "technical-docs" / "ui-state-corpus"

FONT_PATH = "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"
FRAME_W, FRAME_H = 1920, 1080
PANEL_W = 880
CANVAS_W = FRAME_W + PANEL_W

SURFACES = [
    dict(key="primary", file="01-primary-hall.png", title="千星大厅（Primary Hall）",
         group="基础界面", frame_note="聊天框关/开都属大厅；判据 = 聊天标记 或 底栏 7–8 白键结构"),
    dict(key="secondary_chat_hall", file="02-secondary-chat-current-hall.png", title="二级聊天·当前大厅（Secondary Chat: Current Hall）",
         group="基础界面", frame_note="左侧选中「大厅」行；kind/label 与好友私聊同为 secondary:chat，会话细分靠 OCR/选中行"),
    dict(key="secondary_chat_friend", file="03-secondary-chat-friend.png", title="二级聊天·好友私聊（Secondary Chat: Friend Conversation）",
         group="基础界面", frame_note="左侧选中好友行；分类器只给 secondary:chat，好友/大厅由 ConversationRef 证据再分"),
    dict(key="secondary_panel", file="04-secondary-multiplayer-panel.png", title="二级界面·多人面板（F2）",
         group="基础界面", frame_note="当前生产代码没有 secondary:*panel label；F2 帧实际落 panel:hall_card"),
    dict(key="secondary_map", file="05-secondary-hall-map.png", title="二级界面·千星地图（Hall Map）",
         group="基础界面", frame_note="整屏深青虚空色调；亮底变体走 is_bright_qianxing_map"),
    dict(key="main_menu", file="06-main-menu-wonderland.png", title="主菜单千星（Main Menu: Miliastra）",
         group="基础界面", frame_note="左上返回 +「返回提瓦特」锚点"),
    dict(key="paimon", file="07-main-menu-paimon.png", title="主菜单大世界（Main Menu: Overworld）",
         group="基础界面", frame_note="左上返回 + 无返回提瓦特 + 无发送锚点 + 左侧多瓦片宫格"),
    dict(key="loading_dark", file="08-loading-dark.png", title="加载界面·黑底徽记（Loading: Dark）",
         group="基础界面", frame_note="整屏暗占比 + 主色一致度双达标"),
    dict(key="loading_white", file="09-loading-white.png", title="加载界面·白底元素（Loading: White）",
         group="基础界面", frame_note="整屏近白占比 + 主色一致度双达标"),
    dict(key="game_gate", file="10-game-gate.png", title="登录入口 GameGate（OCR）",
         group="基础界面", frame_note="判据是 OCR「点击进入」；Linux 离线无 OCR，无法验证"),
    dict(key="overworld", file="11-overworld.png", title="提瓦特大世界 Overworld（HUD + 底栏 1–4 键）",
         group="基础界面", frame_note="世界判据 HUD 在场 + 判 teyvat + 底栏白键 1–4"),
    dict(key="overlay_friend_panel", file="12-overlay-friend-panel.png", title="覆盖层·好友面板（Overlay: Friend Panel）",
         group="覆盖层", frame_note="friend_panel 49×35 小图标，全分辨率匹配"),
    dict(key="overlay_search", file="13-overlay-search.png", title="覆盖层·搜索页（Overlay: Search Page）",
         group="覆盖层", frame_note="生产 label 为 panel:add_friend；与好友面板同级，不是父子"),
    dict(key="overlay_more_settings", file="14-overlay-more-settings.png", title="覆盖层·更多设置（Overlay: More Settings）",
         group="覆盖层", frame_note="用菜单展开才有的「屏蔽聊天」，不用共用 ⋯ chrome"),
    dict(key="overlay_blocklist", file="15-overlay-blocklist.png", title="覆盖层·拉黑（Overlay: Blocklist）",
         group="覆盖层", frame_note="代码里没有 panel:blocklist 判据；friend_blacklist 模板未接线"),
    dict(key="overlay_confirm", file="16-overlay-confirm.png", title="覆盖层·确认弹窗·屏蔽玩家（Overlay: Confirm Block Player）",
         group="覆盖层", frame_note="确认按钮 65×34 + 「屏蔽吗」文案双锚点"),
    dict(key="overlay_profile_card", file="17-overlay-profile-card.png", title="覆盖层·档案卡（Overlay: Profile Card）",
         group="覆盖层", frame_note="与更多设置共用 ⋯ chrome；靠「前往其大厅」专属按钮 + 排序区分"),
    dict(key="overlay_hall_card", file="18-overlay-hall-card.png", title="覆盖层·大厅卡（Overlay: Hall Card）",
         group="覆盖层", frame_note="进入该大厅(0.994) 排在 前往其大厅(0.962) 之前"),
    dict(key="overlay_avatar_popup", file="19-overlay-avatar-popup.png", title="覆盖层·头像弹窗（Overlay: Avatar Popup）",
         group="覆盖层", frame_note="invite_view_star 393×58，叠在二级聊天之上时由 refine_panel 认出"),
    dict(key="overlay_other", file="20-overlay-other-qianxing-page.png", title="覆盖层·Other 千星覆盖页（panel:qianxing_page）",
         group="覆盖层", frame_note="右上白色圆形关闭按钮结构判据；无专属 label 的千星全屏页"),
]
SURFACE_BY_KEY = {s["key"]: s for s in SURFACES}


def load_manifest():
    path = CORPUS_DIR / "manifest.tsv"
    if not path.exists():
        return {}
    rows = {}
    with path.open(encoding="utf-8") as fh:
        reader = csv.DictReader(fh, delimiter="\t")
        for row in reader:
            rows[row["path"]] = row
    return rows


MANIFEST = load_manifest()


def manifest_note(path):
    row = MANIFEST.get(path)
    if not row:
        return "manifest=not_listed"
    return f"manifest={row['expected_label']} / {row['label_status']}"
ORDER = [s["key"] for s in SURFACES]

C_BG = (16, 16, 20)
C_TEXT = (236, 238, 242)
C_MUTED = (154, 162, 176)
C_ACCENT = (86, 180, 233)
C_PASS = (61, 220, 132)
C_FAIL = (255, 82, 82)
C_WARN = (255, 193, 7)
C_BOX_SEARCH = (0, 170, 255)
C_BOX_HIT = (61, 220, 132)
C_BOX_STRUCT = (255, 210, 0)
C_BOX_MARKER = (178, 102, 255)
C_BOX_PANEL = (255, 140, 0)
C_BOX_WORLD = (0, 230, 230)
C_BOX_CLOSE = (255, 255, 255)


def font(size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(FONT_PATH, size)


F_TITLE = font(30)
F_SUB = font(21)
F_BODY = font(19)
F_SMALL = font(16)
F_TINY = font(14)
F_BOLD = font(22)


def text_width(draw, text, fnt):
    return draw.textlength(text, font=fnt)


def wrap(text, fnt, max_width, draw):
    """按像素宽度折行（中文逐字，英文按空格优先）。整行放得下就不折。"""
    text = str(text)
    if text_width(draw, text, fnt) <= max_width:
        return [text]
    lines = []
    for paragraph in str(text).split("\n"):
        current = ""
        for token in re.findall(r"[A-Za-z0-9_\-\./:,=+<>%()\[\]{}]+|.", paragraph):
            candidate = current + token
            if text_width(draw, candidate, fnt) <= max_width:
                current = candidate
            else:
                if current:
                    lines.append(current)
                current = token.lstrip() if token.isspace() else token
        lines.append(current)
    return lines or [""]


def draw_text_block(draw, x, y, text, fnt, fill, max_width, line_gap=3):
    for line in wrap(text, fnt, max_width, draw):
        draw.text((x, y), line, font=fnt, fill=fill)
        y += fnt.size + line_gap
    return y


def rect_xy(rect, scale_x=1.0, scale_y=1.0):
    return (
        int(rect["x"] * scale_x),
        int(rect["y"] * scale_y),
        int((rect["x"] + rect["w"]) * scale_x),
        int((rect["y"] + rect["h"]) * scale_y),
    )


def box(draw, rect, color, label=None, width=3, alpha=26, label_font=F_SMALL):
    x0, y0, x1, y1 = rect_xy(rect)
    draw.rectangle([x0, y0, x1, y1], fill=color + (alpha,), outline=color + (255,), width=width)
    if not label:
        return
    tw = text_width(draw, label, label_font)
    ty = y0 - label_font.size - 6
    if ty < 2:
        ty = y0 + 3
    draw.rectangle([x0, ty - 2, x0 + tw + 8, ty + label_font.size + 3], fill=(0, 0, 0, 205))
    draw.text((x0 + 4, ty), label, font=label_font, fill=color)


def probe_score(rec, name_part):
    for probe in rec.get("probes", []):
        if name_part in os.path.basename(probe["template"]):
            return probe
    return None


def panel_score(rec, label):
    for score in rec.get("panel_scores", []):
        if score["label"] == label:
            return score
    return None


def fmt(value, digits=3):
    if value is None:
        return "—"
    if isinstance(value, float):
        return f"{value:.{digits}f}"
    return str(value)


def expectation_ok(rec, expected):
    if expected.startswith("!"):
        return not match_expected(rec, expected[1:])
    return match_expected(rec, expected)


def match_expected(rec, expected):
    kind, label, rule = rec["kind"], rec["label"], rec["final_rule"]
    if expected == "primary_hall":
        return kind == "primary"
    if expected == "secondary_chat":
        return kind == "secondary" and label == "secondary:chat"
    if expected == "secondary_panel":
        return kind == "secondary" and label.rsplit(":", 1)[-1].endswith("panel")
    if expected == "secondary_map":
        return kind == "secondary" and label.rsplit(":", 1)[-1].endswith("map")
    if expected == "main_menu":
        return kind == "main_menu" and rule == "main_menu_template"
    if expected == "paimon":
        return kind == "main_menu" and rule == "paimon_menu_grid"
    if expected == "loading_dark":
        return kind == "loading" and rule == "loading_dark_screen"
    if expected == "loading_white":
        return kind == "loading" and rule == "loading_white_screen"
    if expected == "game_gate":
        return kind == "game_gate"
    if expected == "overworld":
        return kind == "overworld"
    if expected.startswith("panel:"):
        return kind == "panel" and label == expected
    return False


def rule_consistent(rec):
    """final_rule 是否描述了真正参与判定的那条规则。

    已知诊断缺陷：`classify_at` 在 `Unknown` 之后用结构判据改判时复用了
    `detect_ui_state_with_evidence` 留下的证据串，于是 Overworld / 千星地图 /
    panel:qianxing_page 的 `final_rule` 仍是 `no_reliable_anchor` 或更早的
    `primary_chat_markers`。kind/label 是对的，但规则名会误导定位，标 UNSTABLE。
    """
    kind, label, rule = rec["kind"], rec["label"], rec["final_rule"]
    if kind in ("overworld", "secondary") and rule == "no_reliable_anchor":
        return False
    if kind == "panel" and rule in ("primary_chat_markers", "no_reliable_anchor"):
        return False
    return True


def status_of(rec, expected):
    if "!" in expected:
        ok = expectation_ok(rec, expected)
        return ("NEG", ok)  # NEG means "rejected as expected"
    if rec["surface_key"] == "game_gate" and rec["kind"] != "game_gate":
        return ("UNVERIFIABLE", False)
    if not expectation_ok(rec, expected):
        return ("FAIL", False)
    if not rule_consistent(rec):
        return ("UNSTABLE", True)
    return ("PASS", True)


def negative_reason(rec, surface_key):
    """用负样本的真实输出/测量值解释它为什么不命中。"""
    kind, label, rule = rec["kind"], rec["label"], rec["final_rule"]
    m = rec["metrics"]
    if surface_key == "primary":
        hb = m["hall_bar_exact"]
        nb = m["hall_bar_near_white"]
        return (f"真实输出 {kind}/{label}（{rule}）。底栏纯白键 {hb['tiles']} 块"
                f"（需 7–8，高 {hb['min_height_px']}–{hb['max_height_px']}，顶差 {hb['top_spread_px']}）；"
                f"近白键 {nb['tiles']} 块 → 不是大厅底栏。")
    if surface_key in ("secondary_chat_hall", "secondary_chat_friend"):
        back = probe_score(rec, "ui-secondary-back")
        submit = probe_score(rec, "ui-secondary-chat-submit")
        return (f"真实输出 {kind}/{label}（{rule}）。返回按钮 {fmt(back and back['score'])}"
                f"（阈值 0.90），发送锚点 {fmt(submit and submit['score'])} —— "
                f"{'命中但被更早的主菜单规则拦下' if back and submit and submit['score'] and submit['score'] >= 0.9 else '无聊天发送锚点'}，所以不是二级聊天。")
    if surface_key == "secondary_panel":
        dist = m["qianxing_panel_distance"]
        return (f"真实输出 {kind}/{label}（{rule}）。F2 面板参考距离 {fmt(dist, 4)}"
                f"（阈值 {m['qianxing_panel_max']:.2f}）—— 普通大厅/其它页面距离超阈值，没有 F2 底栏描述子。")
    if surface_key == "secondary_map":
        tone = m.get("qianxing_map_tone")
        if tone:
            return (f"真实输出 {kind}/{label}（{rule}）。暗占比 {fmt(tone['dark'])}"
                    f"（需 ≥{tone['dark_min']:.2f}）、暗青占比 {fmt(tone['dark_teal'])}"
                    f"（需 ≥{tone['dark_teal_min']:.2f}）→ 不是千星地图色调。")
        return f"真实输出 {kind}/{label}（{rule}）。画面太小/量不出千星地图色调。"
    if surface_key == "main_menu":
        mm = probe_score(rec, "ui-main-menu-return")
        return (f"真实输出 {kind}/{label}（{rule}）。「返回提瓦特」分数 {fmt(mm and mm['score'])}"
                f"（阈值 0.90）→ 没有主菜单千星锚点。")
    if surface_key == "paimon":
        grid = m.get("paimon_grid")
        return (f"真实输出 {kind}/{label}（{rule}）。派蒙宫格 "
                f"{'测到 ' + str(grid['tiles']) + ' 块/' + str(grid['rows']) + ' 行/' + str(grid['cols']) + ' 列' if grid else '未测到（需 ≥8 块 / ≥3 列 / ≥2 行）'} → 不是主菜单大世界。")
    if surface_key == "loading_dark":
        t = m["screen_tone"]
        return (f"真实输出 {kind}/{label}（{rule}）。暗占比 {t['dark']:.2%}（阈值 62.23%）、"
                f"主色一致度 {t['dominant']:.2%}（阈值 90%）→ 达不到加载界面的极端整屏色调。")
    if surface_key == "loading_white":
        t = m["screen_tone"]
        return (f"真实输出 {kind}/{label}（{rule}）。近白占比 {t['white']:.2%}（阈值 71.54%）、"
                f"主色一致度 {t['dominant']:.2%}（阈值 90%）→ 不是白底加载。")
    if surface_key == "game_gate":
        return (f"真实输出 {kind}/{label}（{rule}）。无 OCR「点击进入」→ 不是 GameGate；"
                f"GameGate 在 Linux 离线不可验证，故只标 UNVERIFIABLE。")
    if surface_key == "overworld":
        w = rec.get("world") or {}
        return (f"真实输出 {kind}/{label}（{rule}）。世界判据 teyvat={m['world_is_teyvat']}、"
                f"青色条纹带 {fmt(w.get('count'))}（teyvat≤{w.get('teyvat_max')} / 千星≥{w.get('qianxing_min')}）→ 不是提瓦特大世界。")
    if surface_key == "overlay_blocklist":
        probes = rec.get("probes") or []
        detail = ""
        if probes:
            first = probes[0]
            detail = (f"本帧由 {os.path.basename(first['template'])} 以 {fmt(first['score'])} "
                      f"判成 {label}。")
        return (f"真实输出 {kind}/{label}（{rule}）。代码没有 panel:blocklist 判据：refine_panel 的固定表里"
                f"没有 friend_blacklist；{detail}拉黑/屏蔽聊天模板在真实帧上抓的是同一视觉元素，"
                f"不能拆成两个专属锚点。")
    if surface_key == "overlay_other":
        return (f"真实输出 {kind}/{label}（{rule}）。覆盖页关闭按钮判据需白占比 ≥0.35；本帧是普通大厅"
                f"（关闭按钮白占比 {fmt((m.get('overlay_close') or {}).get('white_ratio'), 4)}），没有整屏覆盖页。")
    if surface_key.startswith("overlay_"):
        expected = {"overlay_friend_panel": "panel:friend_list",
                    "overlay_search": "panel:add_friend",
                    "overlay_more_settings": "panel:more_settings",
                    "overlay_blocklist": "panel:blocklist",
                    "overlay_confirm": "panel:confirm_block_player",
                    "overlay_profile_card": "panel:archive_card",
                    "overlay_hall_card": "panel:hall_card",
                    "overlay_avatar_popup": "panel:avatar_popup",
                    "overlay_other": "panel:qianxing_page"}[surface_key]
        sc = panel_score(rec, expected)
        if sc and sc["pass"]:
            return (f"真实输出 {kind}/{label}（{rule}）。{expected} 模板分数 {fmt(sc['score'])} 已过阈值，"
                    f"但 refine_panel 命中即停，被更专属的 {label} 先占，所以不判 {expected}。")
        if sc:
            return (f"真实输出 {kind}/{label}（{rule}）。{expected} 模板分数 {fmt(sc['score'])}"
                    f" < 阈值 {sc['threshold']:.2f} → 不命中；该帧版面属于 {label}。")
        return (f"真实输出 {kind}/{label}（{rule}）。代码没有 {expected} 的专属模板分数可量"
                f"（该子型判据不存在或未接线），因此不可能命中。")
    return f"真实输出 {kind}/{label}（{rule}）。"


def metric_relevant(metric, key):
    """markdown 里只展示与该界面判据相关的指标；TSV 保留全量。"""
    if metric.startswith("template:"):
        return True
    if metric.startswith("panel_score:"):
        return key.startswith("overlay_") or key == "game_gate"
    if metric.startswith("hall_bar."):
        return key in ("primary", "overworld")
    if metric == "chat_markers":
        return key in ("primary", "secondary_chat_hall", "secondary_chat_friend")
    if metric.startswith("screen.") or metric.startswith("loading."):
        return key in ("loading_dark", "loading_white")
    if metric == "paimon_grid":
        return key in ("paimon", "main_menu")
    if metric.startswith("map."):
        return key == "secondary_map"
    if metric == "panel.reference_distance":
        return key == "secondary_panel"
    if metric.startswith("overlay_close."):
        return key.startswith("overlay_")
    if metric.startswith("world."):
        return key in ("primary", "overworld", "secondary_map")
    return True


def stat_rows(rec, positive=True):
    """把真实证据拆成 (metric, value, threshold, result, notes) 行。"""
    rows = []
    m = rec["metrics"]
    for probe in rec.get("probes", []):
        name = os.path.basename(probe["template"])
        result = "PASS" if probe["outcome"] == "matched" else "FAIL"
        hit = probe.get("hit_rect")
        rows.append((f"template:{name}", fmt(probe["score"], 4), fmt(probe["threshold"], 2), result,
                     f"search={probe['search_rect']} hit={hit} outcome={probe['outcome']}"))
    hb, nb = m["hall_bar_exact"], m["hall_bar_near_white"]
    rows.append(("hall_bar.tiles_255", hb["tiles"], "7–8 (Overworld 1–4)",
                 "PASS" if hb["looks_like_hall_keys"] else "FAIL",
                 f"height {hb['min_height_px']}–{hb['max_height_px']}px (22–36), top_spread {hb['top_spread_px']}px (≤4)"))
    rows.append(("hall_bar.tiles_near_white", nb["tiles"], "7–9 (lossy fallback)",
                 "PASS" if nb["looks_like_hall_keys"] else "FAIL",
                 f"height {nb['min_height_px']}–{nb['max_height_px']}px, top_spread {nb['top_spread_px']}px"))
    t = m["screen_tone"]
    rows.append(("screen.dark_ratio", f"{t['dark']:.4f}", "0.6223 (loading dark)",
                 "PASS" if t["dark"] >= 0.6223 else "FAIL", f"white={t['white']:.4f} dominant={t['dominant']:.4f}"))
    rows.append(("screen.white_ratio", f"{t['white']:.4f}", "0.7154 (loading white)",
                 "PASS" if t["white"] >= 0.7154 else "FAIL", f"dominant={t['dominant']:.4f} (需≥0.90)"))
    rows.append(("screen.dominant_ratio", f"{t['dominant']:.4f}", "0.90 (loading consistency)",
                 "PASS" if t["dominant"] >= 0.90 else "FAIL", "加载界面的色彩一致度门槛"))
    lc = m["loading_center"]
    rows.append(("loading.center160_dark", f"{lc['center160_dark']:.4f}", "0.35",
                 "PASS" if lc["center160_dark"] >= 0.35 else "FAIL", f"center480={lc['center480_dark']:.4f} (<0.10 才算亮底加载)"))
    grid = m["paimon_grid"]
    rows.append(("paimon_grid", f"{grid['tiles']}块/{grid['rows']}行/{grid['cols']}列" if grid else "未测到",
                 "≥8块 / ≥3列 / ≥2行", "PASS" if grid else "FAIL",
                 "downscale 1/8 颜色掩码 + 4 连通域，布局无关" if grid else "未形成宫格"))
    tone = m["qianxing_map_tone"]
    rows.append(("map.dark_ratio", fmt(tone and tone["dark"], 4), f"{tone['dark_min']:.2f}" if tone else "—",
                 "PASS" if tone and tone["dark"] >= tone["dark_min"] else "FAIL", "千星地图暗虚空占比"))
    rows.append(("map.dark_teal_ratio", fmt(tone and tone["dark_teal"], 4), f"{tone['dark_teal_min']:.2f}" if tone else "—",
                 "PASS" if tone and tone["dark_teal"] >= tone["dark_teal_min"] else "FAIL", "暗青占比"))
    dist = m["qianxing_panel_distance"]
    rows.append(("panel.reference_distance", fmt(dist, 4), f"{m['qianxing_panel_max']:.2f}",
                 "PASS" if dist is not None and dist <= m["qianxing_panel_max"] else "FAIL",
                 "F2 多人面板冻结参考描述子（±2px 搜索，取最小距离）"))
    close = m.get("overlay_close")
    if close:
        rows.append(("overlay_close.white_ratio", f"{close['white_ratio']:.4f}", "0.35",
                     "PASS" if close["hit"] else "FAIL",
                     f"blue_ratio={close['blue_ratio']:.4f}（需≥0.20）；右上 100×60 白色圆形关闭按钮"))
    world = rec.get("world") or {}
    count = world.get("count")
    rows.append(("world.cyan_band_count", fmt(count), f"teyvat≤{world.get('teyvat_max')} / 千星≥{world.get('qianxing_min')}",
                 "PASS" if count is not None else "FAIL", f"guess={world.get('guess')} region={world.get('region')}"))
    marker = rec.get("marker")
    if marker:
        rows.append(("chat_markers", f"blue={marker['blue']} yellow={marker['yellow']} pink={marker['pink']}",
                     "blue>0 或 yellow>0 → 千星", "PASS" if marker["blue"] or marker["yellow"] else "FAIL",
                     f"scan={marker['search_rect']}；pink 不作判据"))
    for score in rec.get("panel_scores", []):
        if not score["pass"]:
            continue
        rows.append((f"panel_score:{score['label']}", fmt(score["score"], 4), fmt(score["threshold"], 2), "PASS",
                     f"旁证（未参与最终判定）；region={score['region']} downscale={score['downscale']} "
                     f"template={os.path.basename(score['template'])}"))
    return rows


def status_note(rec, key, status):
    """把 FAIL / UNSTABLE / UNVERIFIABLE 的原因写在图上，不让读者猜。"""
    if key == "game_gate":
        sc = panel_score(rec, "panel:archive_card") or {}
        return (f"UNVERIFIABLE：GameGate 判据是 OCR「点击进入」，Linux 离线没有 OCR。"
                f"离线路径先跑 refine_panel，这帧被 invite-goto-hall 以 {fmt(sc.get('score'))} 抢成 panel:archive_card"
                f"（{sc.get('region')}），所以只标不可验证，不冒充 PASS/FAIL。")
    if key == "secondary_panel":
        return ("当前生产代码没有 secondary:*panel 分支：F2 帧被 is_qianxing_player_panel 认出后"
                "写入 panel:hall_card（与覆盖层·大厅卡同 label）。F2 参考距离本身见下方结构行。")
    if key == "overlay_blocklist":
        return (f"代码里没有 panel:blocklist 判据：refine_panel 固定表不含 friend_blacklist。"
                f"本帧被 {rec['label']} 的专属锚点判成 {rec['label']}；"
                f"拉黑与屏蔽聊天在真实帧上抓同一视觉元素，不能拆成两个专属 label。")
    if status == "UNSTABLE":
        return (f"kind/label 判对了，但 final_rule=`{rec['final_rule']}` 是更早一层（被否决）的证据串："
                f"`classify_at` 用结构判据改判时复用了旧 UiStateEvidence。定位日志时不能只看 final_rule。")
    return None


def relevant_structure_lines(rec, key):
    """只展示与该界面判据相关的结构量，避免把无关指标堆成噪声。"""
    m = rec["metrics"]
    hb, nb = m["hall_bar_exact"], m["hall_bar_near_white"]
    t = m["screen_tone"]
    grid = m.get("paimon_grid")
    tone = m.get("qianxing_map_tone") or {}
    close = m.get("overlay_close") or {}
    lines = []
    if key == "primary":
        lines.append(f"底栏白键(255): {hb['tiles']} 块，高 {hb['min_height_px']}–{hb['max_height_px']} px，"
                     f"顶差 {hb['top_spread_px']} px → {'PASS' if hb['looks_like_hall_keys'] else 'FAIL'}"
                     f"（大厅需 7–8 块 / 22–36 px / ≤4 px）")
        lines.append(f"底栏近白(225): {nb['tiles']} 块，高 {nb['min_height_px']}–{nb['max_height_px']} px，"
                     f"顶差 {nb['top_spread_px']} px → {'PASS' if nb['looks_like_hall_keys'] else 'FAIL'}")
        if rec["final_rule"].startswith("primary_"):
            lines.append(f"本条由 `{rec['final_rule']}` 判定；底栏数字是本帧旁证，不是本次判据。")
    if key == "overworld":
        ow = 1 <= nb["tiles"] <= 4
        lines.append(f"底栏白键(255): {hb['tiles']} 块；近白(225): {nb['tiles']} 块，高 {nb['min_height_px']}–{nb['max_height_px']} px，"
                     f"顶差 {nb['top_spread_px']} px → {'PASS' if ow else 'FAIL'}（Overworld 需 1–4 块；大厅 7–8 必须排除）")
        world = rec.get("world") or {}
        lines.append(f"世界判据: teyvat={m['world_is_teyvat']}，HUD/青色条纹带 count={fmt(world.get('count'))}"
                     f"（teyvat≤{world.get('teyvat_max')} / 千星≥{world.get('qianxing_min')}）")
    if key in ("paimon", "main_menu"):
        lines.append(f"派蒙宫格: {grid['tiles']} 块 / {grid['rows']} 行 / {grid['cols']} 列 → PASS"
                     f"（需 ≥8 块 / ≥3 列 / ≥2 行）" if grid else "派蒙宫格: 未测到 → FAIL（需 ≥8 块 / ≥3 列 / ≥2 行）")
    if key == "secondary_map":
        lines.append(f"千星地图色调: 暗 {fmt(tone.get('dark'), 4)}（需 ≥{tone.get('dark_min'):.2f}）、"
                     f"暗青 {fmt(tone.get('dark_teal'), 4)}（需 ≥{tone.get('dark_teal_min'):.2f}）")
    if key.startswith("loading"):
        lines.append(f"loading_rule = {m.get('loading_rule')}")
        lines.append(f"整屏色调: 暗 {t['dark']:.4f}（阈值 0.6223）/ 白 {t['white']:.4f}（阈值 0.7154），"
                     f"主色一致度 {t['dominant']:.4f}（阈值 0.90）")
        lines.append(f"中心 spinner: center160 暗 {m['loading_center']['center160_dark']:.4f}（阈值 0.35），"
                     f"center480 {m['loading_center']['center480_dark']:.4f}（<0.10）")
    if key == "secondary_panel":
        lines.append(f"F2 参考距离: {fmt(m['qianxing_panel_distance'], 4)}（阈值 {m['qianxing_panel_max']:.2f}）")
    if key.startswith("overlay_"):
        for probe in rec.get("probes", []):
            lines.append(f"命中锚点 {os.path.basename(probe['template'])} = {fmt(probe['score'], 4)}"
                         f"（阈值 {probe['threshold']:.2f}），搜索区 {probe['search_rect']}")
        beige = m.get("beige_panel_fraction")
        if beige is not None:
            lines.append(f"千星门控: 米黄面板占比 {beige:.4f}（大厅≥0.24 / 档案<0.34 / 退出菜单<0.12）、"
                         f"整屏暗占比 {t['dark']:.4f}（退出菜单≥0.40）、右上关闭按钮 "
                         f"{'PASS' if (close or {}).get('hit') else 'FAIL'}")
    if close and key.startswith("overlay_"):
        lines.append(f"右上关闭按钮: 白占比 {close['white_ratio']:.4f}（阈值 0.35），"
                     f"蓝调 {close['blue_ratio']:.4f}（门槛 0.20）")
    if not lines:
        lines.append("本界面无额外结构量；判据见上方探针。")
    return lines


def render_surface(key, positive, negative):
    spec = SURFACE_BY_KEY[key]
    source = Image.open(ROOT / positive["frame"]).convert("RGB").resize((FRAME_W, FRAME_H), Image.LANCZOS)
    canvas = Image.new("RGB", (CANVAS_W, FRAME_H), C_BG)
    canvas.paste(source, (0, 0))
    overlay = Image.new("RGBA", (CANVAS_W, FRAME_H), (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)
    draw.rectangle([0, 0, FRAME_W - 1, FRAME_H - 1], outline=(255, 255, 255, 40), width=2)

    rects = positive["rects"]
    metrics = positive["metrics"]
    status, ok = status_of(positive, positive["expected"])

    # 1) 证据探针的搜索区 / 命中区
    for probe in positive.get("probes", []):
        hit = probe["outcome"] == "matched"
        color = C_BOX_HIT if hit else C_BOX_SEARCH
        label = f"{os.path.basename(probe['template'])} {fmt(probe['score'])}"
        box(draw, probe["search_rect"], color, label, width=2, alpha=12)
        if probe.get("hit_rect"):
            box(draw, probe["hit_rect"], C_BOX_HIT, None, width=4, alpha=60)

    # 2) 聊天标记扫描区
    marker = positive.get("marker")
    if marker:
        box(draw, marker["search_rect"], C_BOX_MARKER,
            f"chat markers b={marker['blue']} y={marker['yellow']} p={marker['pink']}", width=2, alpha=10)

    # 3) 世界判据区域
    world = positive.get("world")
    if world:
        box(draw, world["region"], C_BOX_WORLD,
            f"world cyan={world['count']} ({world['guess']})", width=2, alpha=30)

    # 4) 结构判据区域
    if key in ("primary", "overworld"):
        hb = metrics["hall_bar_exact"]
        nb = metrics["hall_bar_near_white"]
        if key == "primary":
            label = (f"hall bar: 255={hb['tiles']} / near={nb['tiles']} keys "
                     f"h={nb['min_height_px']}-{nb['max_height_px']} spread={nb['top_spread_px']}")
        else:
            label = (f"bottom bar: 255={hb['tiles']} / near={nb['tiles']} keys (Overworld 1–4, hall 7–8)"
                     f" h={nb['min_height_px']}-{nb['max_height_px']}")
        box(draw, rects["hall_bar"], C_BOX_STRUCT, label, width=3, alpha=12)
    if key in ("main_menu", "paimon", "secondary_chat_hall", "secondary_chat_friend"):
        box(draw, rects["secondary_back"], C_BOX_SEARCH, "back anchor", width=2, alpha=10)
    if key == "main_menu":
        box(draw, rects["main_menu"], C_BOX_HIT, "返回提瓦特", width=3, alpha=12)
    if key in ("secondary_chat_hall", "secondary_chat_friend"):
        box(draw, rects["secondary_chat_submit"], C_BOX_HIT, "send anchor", width=3, alpha=12)
    if key == "paimon":
        grid = metrics.get("paimon_grid")
        left = {"x": 0, "y": 0, "w": int(FRAME_W * 45 / 100), "h": FRAME_H}
        box(draw, left, C_BOX_STRUCT,
            f"paimon grid {grid['tiles']}块/{grid['rows']}行/{grid['cols']}列" if grid else "paimon grid: none",
            width=3, alpha=8)
    if key in ("loading_dark", "loading_white", "secondary_map"):
        label = {
            "loading_dark": f"loading_rule={metrics['loading_rule']} dark={metrics['screen_tone']['dark']:.4f}",
            "loading_white": f"loading_rule={metrics['loading_rule']} white={metrics['screen_tone']['white']:.4f}",
            "secondary_map": f"map dark={fmt((metrics.get('qianxing_map_tone') or {}).get('dark'), 4)} teal={fmt((metrics.get('qianxing_map_tone') or {}).get('dark_teal'), 4)}",
        }[key]
        box(draw, {"x": 0, "y": 0, "w": FRAME_W, "h": FRAME_H}, C_BOX_STRUCT, label, width=4, alpha=0)
    if key.startswith("overlay_"):
        chosen = positive["label"]
        expected = positive["expected"]
        for score in positive.get("panel_scores", []):
            if score["label"] == chosen:
                box(draw, score["region"], C_BOX_HIT,
                    f"{score['label']} {fmt(score['score'])}", width=3, alpha=10)
            elif score["label"] == expected:
                box(draw, score["region"], C_BOX_PANEL,
                    f"{score['label']} {fmt(score['score'])}", width=2, alpha=6)
            else:
                # 其余子型只在侧栏列出分数；不画大框，避免遮住画面。
                pass
        close = metrics.get("overlay_close")
        if close:
            box(draw, {"x": FRAME_W - 100, "y": 10, "w": 80, "h": 60}, C_BOX_CLOSE,
                f"close {close['white_ratio']:.3f}", width=2, alpha=16)

    canvas = Image.alpha_composite(canvas.convert("RGBA"), overlay).convert("RGB")
    draw = ImageDraw.Draw(canvas)

    # ── 侧栏 ───────────────────────────────────────────────────────────
    x = FRAME_W + 22
    max_w = PANEL_W - 44
    y = 14

    badge_color = C_PASS if status in ("PASS", "NEG") and ok else (C_WARN if status in ("UNVERIFIABLE", "UNSTABLE") else C_FAIL)
    badge_text = {"PASS": "PASS", "FAIL": "FAIL", "NEG": "NEG·不命中", "UNVERIFIABLE": "UNVERIFIABLE", "UNSTABLE": "UNSTABLE"}[status]
    badge_w = int(text_width(draw, badge_text, F_BOLD)) + 24
    draw.rounded_rectangle([CANVAS_W - 22 - badge_w, y - 1, CANVAS_W - 22, y + 29], radius=8, fill=badge_color)
    draw.text((CANVAS_W - 22 - badge_w + 12, y + 2), badge_text, font=F_BOLD, fill=(10, 10, 10))

    title_font = font(20)
    for size in (30, 28, 26, 24, 22, 20):
        candidate = font(size)
        if text_width(draw, spec["title"], candidate) <= max_w - badge_w - 14:
            title_font = candidate
            break
    title_lines = wrap(spec["title"], title_font, max_w - badge_w - 14, draw)
    for line in title_lines:
        draw.text((x, y), line, font=title_font, fill=C_TEXT)
        y += title_font.size + 4
    y += 2
    draw.text((x, y), f"surface_key = {key}", font=F_SMALL, fill=C_MUTED)
    y += F_SMALL.size + 2
    y = draw_text_block(draw, x, y, spec["frame_note"], F_SMALL, C_MUTED, max_w, 1)
    y += 8

    draw.line([x, y, x + max_w, y], fill=(70, 76, 88), width=1)
    y += 8

    classification = [
        ("代表帧", positive["frame"]),
        ("kind / label", f"{positive['kind']} / {positive['label']}"),
        ("final_rule", positive["final_rule"]),
        ("UiSurface", f"{positive['surface_debug']} · {positive['surface_chinese']}"),
        ("预期", positive["expected"]),
    ]
    for name, value in classification:
        draw.text((x, y), name, font=F_SMALL, fill=C_MUTED)
        yy = y - 1
        for line in wrap(value, F_BODY, max_w - 138, draw):
            draw.text((x + 138, yy), line, font=F_BODY, fill=C_TEXT)
            yy += F_BODY.size + 1
        y = max(y + F_BODY.size + 3, yy + 1)

    note = status_note(positive, key, status)
    if note:
        y += 4
        lines = wrap(note, F_SMALL, max_w - 12, draw)
        box_h = len(lines) * (F_SMALL.size + 2) + 10
        draw.rounded_rectangle([x - 2, y - 3, x + max_w, y + box_h - 3], radius=6,
                               fill=(60, 46, 14, 255) if status != "UNSTABLE" else (44, 40, 24, 255),
                               outline=C_WARN, width=1)
        yy = y + 3
        for line in lines:
            draw.text((x + 4, yy), line, font=F_SMALL, fill=(255, 226, 170))
            yy += F_SMALL.size + 2
        y += box_h + 4

    y += 4
    draw.text((x, y), "真实探针（分类器证据）", font=F_BOLD, fill=C_ACCENT)
    y += F_BOLD.size + 6
    probes = positive.get("probes", [])
    if not probes:
        if status == "UNSTABLE":
            text = (f"本帧无模板探针：label 由 `is_qianxing_overlay_close` 结构判据改写，"
                    f"但 evidence 里 final_rule 仍是更早一层的 `{positive['final_rule']}`。")
        elif key == "secondary_map":
            text = (f"本帧无模板探针：label 由整屏暗/暗青色调判据给出，"
                    f"evidence 里 final_rule 仍是更早一层的 `{positive['final_rule']}`。")
        elif key == "overworld":
            text = (f"本帧无模板探针：label 由世界判据（HUD + teyvat）给出，"
                    f"evidence 里 final_rule 仍是更早一层的 `{positive['final_rule']}`。")
        else:
            text = f"本帧由 `{positive['final_rule']}` 直接判定，无模板探针（聊天标记计数见框上标注）。"
        y = draw_text_block(draw, x, y, text, F_SMALL, C_MUTED, max_w, 2)
        y += 4
    for probe in probes[:5]:
        name = os.path.basename(probe["template"])
        mark = "PASS" if probe["outcome"] == "matched" else "FAIL"
        color = C_PASS if probe["outcome"] == "matched" else C_FAIL
        text = f"[{mark}] {name} = {fmt(probe['score'], 4)}  (≥{probe['threshold']:.2f})"
        for line in wrap(text, F_SMALL, max_w, draw):
            draw.text((x, y), line, font=F_SMALL, fill=color)
            y += F_SMALL.size + 1

    y += 4
    draw.text((x, y), "结构判据（真实测量值）", font=F_BOLD, fill=C_ACCENT)
    y += F_BOLD.size + 6
    for line in relevant_structure_lines(positive, key):
        color = C_TEXT
        if "→ PASS" in line:
            color = C_PASS
        elif "→ FAIL" in line:
            color = C_FAIL
        y = draw_text_block(draw, x, y, line, F_SMALL, color, max_w, 1)
        y += 2

    # 模板裁图
    templates = []
    seen = set()
    for probe in probes:
        path = ROOT / probe["template"]
        if path.exists() and path not in seen:
            seen.add(path)
            templates.append((path, probe))
    if templates:
        y += 2
        draw.text((x, y), "模板裁图（按实际尺寸缩放，非重绘）", font=F_BOLD, fill=C_ACCENT)
        y += F_BOLD.size + 6
        cx, cy, row_h = x, y, 0
        for path, probe in templates[:4]:
            try:
                tpl = Image.open(path).convert("RGBA")
            except Exception:
                continue
            scale = min(190 / tpl.width, 46 / tpl.height, 1.0)
            tpl = tpl.resize((max(1, int(tpl.width * scale)), max(1, int(tpl.height * scale))), Image.LANCZOS)
            if cx + tpl.width > x + max_w:
                cx = x
                cy += row_h + 24
                row_h = 0
            bg = Image.new("RGBA", (tpl.width + 6, tpl.height + 6), (44, 44, 52, 255))
            bg.alpha_composite(tpl, (3, 3))
            canvas.paste(bg.convert("RGB"), (int(cx), int(cy)))
            short = path.stem.replace("ui-secondary-", "").replace("ui-main-menu-", "return-")
            short = short.replace("invite-", "").replace("friend-", "")
            draw.text((cx, cy + tpl.height + 6), f"{short[:16]} {fmt(probe['score'])}", font=F_TINY, fill=C_MUTED)
            cx += tpl.width + 16
            row_h = max(row_h, tpl.height + 20)
        y = cy + row_h + 22

    # 负样本缩略图 + 原因（全宽，避免挤压）
    y += 2
    draw.text((x, y), "近似负样本（同一分类器真实跑过）", font=F_BOLD, fill=C_WARN)
    y += F_BOLD.size + 5
    neg_thumb = None
    if negative:
        neg_img = Image.open(ROOT / negative["frame"]).convert("RGB").resize((FRAME_W, FRAME_H), Image.LANCZOS)
        tw = 300
        th = int(FRAME_H * tw / FRAME_W)
        neg_thumb = neg_img.resize((tw, th), Image.LANCZOS)
    if neg_thumb:
        canvas.paste(neg_thumb, (x, y))
        draw.rectangle([x, y, x + neg_thumb.width, y + neg_thumb.height], outline=C_FAIL, width=3)
        draw.text((x, y + neg_thumb.height + 3), Path(negative["frame"]).name, font=F_TINY, fill=C_MUTED)
        nx = x + neg_thumb.width + 14
        draw.text((nx, y), f"真实输出: {negative['kind']} / {negative['label']}", font=F_SMALL, fill=C_FAIL)
        draw.text((nx, y + F_SMALL.size + 2), f"final_rule: {negative['final_rule']}", font=F_SMALL, fill=C_MUTED)
        y += neg_thumb.height + 20
        reason = negative_reason(negative, key)
        y = draw_text_block(draw, x, y, reason, F_SMALL, C_TEXT, max_w, 2)

    if y > FRAME_H - 46:
        print(f"WARN {key}: sidebar content reaches y={y} (> {FRAME_H - 46})", file=sys.stderr)
    draw.text((x, FRAME_H - 22), "框：搜索区 / 命中区 / 结构区；分数与阈值来自生产分类器", font=F_TINY, fill=(190, 190, 196))

    out = GALLERY_DIR / spec["file"]
    out.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(out, optimize=True)
    return out


def tsv_rows(records):
    rows = [("surface", "frame", "rule", "metric", "value", "threshold", "result", "notes")]
    for rec in records:
        spec = SURFACE_BY_KEY[rec["surface_key"]]
        role = "负样本" if rec["expected"].startswith("!") else "正样本"
        status, ok = status_of(rec, rec["expected"])
        rule = rec["final_rule"]
        extra = ""
        if rec["surface_key"] in ("secondary_chat_hall", "secondary_chat_friend"):
            extra = "; conversation_split=not_in_classifier(OCR/selected-row)"
        elif rec["surface_key"] == "secondary_panel":
            extra = "; F2 与覆盖层·大厅卡同帧，无独立 secondary:*panel label"
        elif rec["surface_key"] == "game_gate":
            extra = "; OCR-dependent (Windows only), Linux offline"
        rows.append((spec["title"], rec["frame"], rule, "final_rule", rule, "expected=" + rec["expected"],
                     status if not rec["expected"].startswith("!") else ("NEG_OK" if ok else "NEG_FAIL"),
                     f"kind={rec['kind']} label={rec['label']} surface={rec['surface_debug']} role={role}; "
                     f"{manifest_note(rec['frame'])}{extra}"))
        for metric, value, threshold, result, notes in stat_rows(rec, not rec["expected"].startswith("!")):
            rows.append((spec["title"], rec["frame"], rule, metric, str(value), str(threshold), result, notes))
    return rows


def write_tsv(records):
    path = CORPUS_DIR / "evidence-scores.tsv"
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = tsv_rows(records)
    with path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write("\t".join(str(cell).replace("\t", " ").replace("\n", " ") for cell in row) + "\n")
    return path, len(rows) - 1


def rel_link(path: Path, from_dir: Path):
    return os.path.relpath(path, from_dir)


def write_markdown(records):
    md_path = GALLERY_DIR / "evidence-gallery.md"
    by_key = {}
    for rec in records:
        by_key.setdefault(rec["surface_key"], {})["neg" if rec["expected"].startswith("!") else "pos"] = rec

    lines = []
    lines.append("# UI 状态证据画廊（真实分类器输出）")
    lines.append("")
    lines.append("> 本页每个界面/子型一张图。图上的分数、阈值、结构测量值全部来自**当前生产分类器**"
                 "（`src/ui/state.rs`）在真实代表帧上的输出；渲染脚本 `scripts/render-ui-evidence-gallery.py`"
                 " 只画框与排版，不计算分数、不补数字。可追溯明细见 "
                 f"[evidence-scores.tsv]({rel_link(CORPUS_DIR / 'evidence-scores.tsv', GALLERY_DIR)})。")
    lines.append("")
    lines.append("**读图约定**：绿色粗框 = 模板真实命中位置；蓝色/橙色细框 = 证据搜索区；"
                 "黄色框 = 结构判据区域（底栏/宫格/整屏色调）；紫色 = 聊天标记扫描区；青色 = 世界判据区域。"
                 "右侧栏给出 `kind / label / final_rule`、逐模板真实分数与阈值、结构测量值、"
                 "以及一张**真实跑过分类器的近似负样本**及其不命中原因。")
    lines.append("")
    lines.append("**隐私**：代表帧取自本地 `technical-docs/` 与 `tests/fixtures/ui/`，"
                 "部分截图含真实 UID/昵称，仅作本地核验，不提交、不发布。")
    lines.append("")
    lines.append("## 总览")
    lines.append("")
    lines.append("| # | 界面 / 子型 | 画廊 | 代表帧 | 真实输出 | `final_rule` | 结果 |")
    lines.append("|---|---|---|---|---|---|---|")
    for index, key in enumerate(ORDER, 1):
        spec = SURFACE_BY_KEY[key]
        pair = by_key.get(key, {})
        pos = pair.get("pos")
        if not pos:
            lines.append(f"| {index} | {spec['title']} | — | — | — | — | 缺数据 |")
            continue
        status, ok = status_of(pos, pos["expected"])
        frame_link = rel_link(ROOT / pos["frame"], GALLERY_DIR)
        result = {"PASS": "✅ PASS", "FAIL": "❌ FAIL", "UNVERIFIABLE": "⚠️ UNVERIFIABLE（Linux 离线）", "UNSTABLE": "⚠️ UNSTABLE", "NEG": "➖"}[status]
        lines.append(
            f"| {index} | {spec['title']} | [{spec['file']}]({spec['file']}) | "
            f"[{Path(pos['frame']).name}]({frame_link}) | `{pos['kind']}` / `{pos['label']}` | "
            f"`{pos['final_rule']}` | {result} |")
    lines.append("")

    for index, key in enumerate(ORDER, 1):
        spec = SURFACE_BY_KEY[key]
        pair = by_key.get(key, {})
        pos = pair.get("pos")
        neg = pair.get("neg")
        lines.append(f"## {index}. {spec['title']}")
        lines.append("")
        lines.append(f"[![{spec['title']}]({spec['file']})]({spec['file']})")
        lines.append("")
        lines.append(f"- 代表帧：[{pos['frame']}]({rel_link(ROOT / pos['frame'], GALLERY_DIR)})")
        lines.append(f"- 真实输出：`{pos['kind']}` / `{pos['label']}`；`final_rule = {pos['final_rule']}`；"
                     f"`UiSurface = {pos['surface_debug']}`（{pos['surface_chinese']}）")
        lines.append(f"- 语料标注（**未改动**）：{manifest_note(pos['frame'])}")
        status, ok = status_of(pos, pos["expected"])
        result = {"PASS": "PASS", "FAIL": "FAIL", "UNVERIFIABLE": "UNVERIFIABLE（离线无法验证）",
                  "UNSTABLE": "UNSTABLE（label 正确，final_rule 证据串与判定不一致）", "NEG": "NEG"}[status]
        lines.append(f"- 判定：**{result}**（预期 `{pos['expected']}`）")
        if key in ("secondary_chat_hall", "secondary_chat_friend"):
            lines.append("- 注意：分类器只给 `secondary:chat`，**不区分**当前大厅/好友私聊；"
                         "选中行与会话名由 `ConversationRef` 的会话证据再分，本图只证明两者都进二级聊天。")
        if key == "secondary_panel":
            lines.append("- 注意：语料里没有独立的 F2 实机帧；本帧 `qxf2-from-hall.jpg` 是"
                         "「大厅按 F2 打开的当前大厅页」，manifest 也把它标成覆盖层·大厅卡。"
                         "所以这是**表面重叠**，不是分类器把 F2 判对了。")
        if key == "game_gate":
            lines.append("- 注意：GameGate 判据依赖 Windows OpenVINO OCR（「点击进入」）；"
                         "Linux 离线无法复现，本图只记录离线时的真实误判路径。")
        lines.append("")
        lines.append("| metric | value | threshold | result | notes |")
        lines.append("|---|---|---|---|---|")
        shown = 0
        for metric, value, threshold, row_result, notes in stat_rows(pos):
            if not metric_relevant(metric, key):
                continue
            lines.append(f"| `{metric}` | `{value}` | `{threshold}` | {row_result} | {notes} |")
            shown += 1
        lines.append("")
        lines.append(f"> 表中只列与**该界面判据相关**的项（{shown} 行）；其余判据（如大厅帧的加载色调）"
                     "不适用于本界面，全量测量值仍逐行保存在 TSV 里。")
        lines.append("")
        if neg:
            neg_status, neg_ok = status_of(neg, neg["expected"])
            lines.append(f"**近似负样本**：[{Path(neg['frame']).name}]({rel_link(ROOT / neg['frame'], GALLERY_DIR)}) "
                         f"（真实输出 `{neg['kind']}` / `{neg['label']}`，`{neg['final_rule']}`）")
            lines.append("")
            lines.append(f"> {negative_reason(neg, key)}")
            lines.append("")
    statuses = {}
    for key in ORDER:
        pos = by_key.get(key, {}).get("pos")
        if not pos:
            continue
        st, _ = status_of(pos, pos["expected"])
        statuses.setdefault(st, []).append(key)
    lines.append("## 结论：当前分类器真实输出")
    lines.append("")
    lines.append(f"- 20 个界面/子型全部出图；PASS {len(statuses.get('PASS', []))} 个，"
                 f"UNSTABLE {len(statuses.get('UNSTABLE', []))} 个，FAIL {len(statuses.get('FAIL', []))} 个，"
                 f"UNVERIFIABLE {len(statuses.get('UNVERIFIABLE', []))} 个。")
    for st, label in (("FAIL", "FAIL"), ("UNSTABLE", "UNSTABLE"), ("UNVERIFIABLE", "UNVERIFIABLE")):
        if statuses.get(st):
            lines.append(f"- {label}：")
            for key in statuses[st]:
                spec = SURFACE_BY_KEY[key]
                pos = by_key[key]["pos"]
                note = status_note(pos, key, st) or negative_reason(by_key[key]["neg"], key)
                lines.append(f"  - **{spec['title']}** — 真实输出 `{pos['kind']}` / `{pos['label']}`"
                             f"（`{pos['final_rule']}`）。{note}")
    lines.append("")
    lines.append("## 方法、可追溯性与局限")
    lines.append("")
    lines.append("- 数据导出（可复跑）：")
    lines.append("  ```bash")
    lines.append("  MILIASTRA_GALLERY_FRAMES=technical-docs/ui-state-corpus/evidence-gallery-frames.tsv \\")
    lines.append("  MILIASTRA_GALLERY_OUT=target/evidence-gallery/records.jsonl \\")
    lines.append("  cargo test --lib --no-default-features dump_ui_state_evidence_for_gallery -- --ignored --nocapture")
    lines.append("  python3 scripts/render-ui-evidence-gallery.py target/evidence-gallery/records.jsonl")
    lines.append("  ```")
    lines.append(f"- 帧清单：[evidence-gallery-frames.tsv]({rel_link(CORPUS_DIR / 'evidence-gallery-frames.tsv', GALLERY_DIR)})；"
                 f"原始导出（每帧 kind/label/final_rule/探针/结构量）："
                 f"[evidence-gallery-records.jsonl]({rel_link(CORPUS_DIR / 'evidence-gallery-records.jsonl', GALLERY_DIR)})。")
    lines.append("- 每帧独立分类器实例，避免 `GameFallbackState` 的跨帧状态污染单帧证据。")
    lines.append("- 结构量直接调用生产函数：`hall_bar_signature` / `paimon_menu_grid_structure` / `screen_tone` / "
                 "`qianxing_map_tone` / `qianxing_player_panel_distance` / `is_qianxing_overlay_close` / "
                 "`beige_panel_fraction`。")
    lines.append("- 覆盖层那一栏的 `panel_scores` 是把 `refine_panel` 的固定表（含新版千星专属锚点）"
                 "按同模板/同区域/同降采样倍数再量一遍，**不参与最终判定**，只用于展示各子型互串时的真实分数。")
    lines.append("- 快照说明：本轮生成时 `src/ui/state.rs` 正被另一个任务重构（新增千星专属锚点与门控）。"
                 "数据来自 **2026-11-11 01:16:57 的稳定快照**；并发任务在此期间继续编辑该文件时，"
                 "本页数字应对照该快照，而不是之后的工作区。")
    lines.append("- 已知判不了的：")
    lines.append("  - 登录入口 GameGate 依赖 Windows OpenVINO OCR（「点击进入」），Linux 离线只能如实标 UNVERIFIABLE；")
    lines.append("  - 二级聊天「当前大厅 vs 好友私聊」共用 `secondary:chat`，细分依赖会话/选中行 OCR，本画廊只能证明两者都进二级聊天；")
    lines.append("  - 二级界面·多人面板（F2）当前没有 `secondary:*panel` label，真实输出落 `panel:hall_card`，标 FAIL；")
    lines.append("  - 覆盖层·拉黑当前没有 `panel:blocklist` 判据（`friend_blacklist` 模板未接线），标 FAIL。")
    lines.append("")
    md_path.write_text("\n".join(lines), encoding="utf-8")
    return md_path


def check_links(md_path: Path):
    """校验 markdown 内所有相对链接都存在。"""
    text = md_path.read_text(encoding="utf-8")
    missing = []
    links = re.findall(r"\[[^\]]*\]\(([^)]+)\)", text)
    for link in links:
        if link.startswith(("http://", "https://", "#", "mailto:")):
            continue
        target = (md_path.parent / link).resolve()
        if not target.exists():
            missing.append(link)
    return missing, len(links)


def main():
    if len(sys.argv) < 2:
        print("usage: render-ui-evidence-gallery.py <records.jsonl>", file=sys.stderr)
        return 2
    records = [json.loads(line) for line in Path(sys.argv[1]).read_text(encoding="utf-8").splitlines() if line.strip()]
    by_key = {}
    for rec in records:
        by_key.setdefault(rec["surface_key"], {})["neg" if rec["expected"].startswith("!") else "pos"] = rec
    for key in ORDER:
        pair = by_key.get(key)
        if not pair or "pos" not in pair or "neg" not in pair:
            raise SystemExit(f"surface {key} 缺正/负样本: {sorted(pair or {})}")
        out = render_surface(key, pair["pos"], pair["neg"])
        print(f"rendered {out.relative_to(ROOT)}")
    tsv_path, rows = write_tsv(records)
    print(f"wrote {tsv_path.relative_to(ROOT)} ({rows} rows)")
    md_path = write_markdown(records)
    print(f"wrote {md_path.relative_to(ROOT)}")
    missing, total = check_links(md_path)
    if missing:
        print(f"BROKEN LINKS ({len(missing)}/{total}):", file=sys.stderr)
        for link in missing:
            print("  " + link, file=sys.stderr)
        return 1
    print(f"links ok: {total} relative links, 0 missing")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
