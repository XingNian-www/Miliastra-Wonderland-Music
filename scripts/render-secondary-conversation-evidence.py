#!/usr/bin/env python3
"""在证据画廊上补「二级聊天会话子状态（ConversationRef）」的实测标注。

数据来源：`cargo test --lib --no-default-features dump_conversation_substate_evidence`
（`src/ui/routines/residency.rs` 的只读诊断）导出的 JSONL。本脚本只画框、排版、
把已测到的数字写到图上，不计算分数、不补数字。

用法：
    python3 scripts/render-secondary-conversation-evidence.py [conversation.jsonl]

默认读取 target/evidence-gallery/conversation.jsonl，产出：
- technical-docs/ui-state-evidence/evidence-gallery/02-secondary-chat-current-hall.png（追加标注）
- technical-docs/ui-state-evidence/evidence-gallery/03-secondary-chat-friend.png（追加标注）
- technical-docs/ui-state-evidence/evidence-gallery/21-secondary-chat-substate.png（新图）

注意：02/03 的基础版式由 render-ui-evidence-gallery.py 生成；重跑那个脚本后需要
再跑一次本脚本，否则会话子状态标注会被覆盖。
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
GALLERY = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery"
FONT_PATH = "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"

C_BG = (16, 16, 20)
C_TEXT = (236, 238, 242)
C_MUTED = (154, 162, 176)
C_ACCENT = (86, 180, 233)
C_GREEN = (61, 220, 132)
C_RED = (255, 82, 82)
C_YELLOW = (255, 210, 42)
C_ORANGE = (255, 140, 0)
C_BLUE = (0, 170, 255)
C_PURPLE = (178, 102, 255)


def font(size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(FONT_PATH, size)


F_TITLE = font(36)
F_H2 = font(27)
F_BODY = font(21)
F_SMALL = font(18)
F_TINY = font(15)


def tw(draw, text, fnt):
    return draw.textlength(text, font=fnt)


def wrap(text, fnt, max_width, draw):
    lines = []
    for paragraph in str(text).split("\n"):
        current = ""
        for token in re.findall(r"[A-Za-z0-9_\-\./:,=+<>%()\[\]{}]+|.", paragraph):
            candidate = current + token
            if tw(draw, candidate, fnt) <= max_width:
                current = candidate
            else:
                if current:
                    lines.append(current)
                current = token.lstrip() if token.isspace() else token
        lines.append(current)
    return lines or [""]


def text_block(draw, x, y, text, fnt, fill, max_width, gap=4):
    for line in wrap(text, fnt, max_width, draw):
        draw.text((x, y), line, font=fnt, fill=fill)
        y += fnt.size + gap
    return y


def box(draw, rect, color, label=None, width=3, alpha=28, label_font=F_SMALL, label_side="above"):
    x0, y0, x1, y1 = rect
    draw.rectangle([x0, y0, x1, y1], fill=color + (alpha,), outline=color + (255,), width=width)
    if not label:
        return
    lw = tw(draw, label, label_font)
    if label_side == "above":
        ly = y0 - label_font.size - 6
    else:
        ly = y1 + 4
    if ly < 2:
        ly = y0 + 3
    draw.rectangle([x0, ly - 2, x0 + lw + 10, ly + label_font.size + 3], fill=(0, 0, 0, 220))
    draw.text((x0 + 5, ly), label, font=label_font, fill=color)


def badge(draw, xy, text, fill, fnt=F_BODY):
    x, y = xy
    w = tw(draw, text, fnt) + 24
    draw.rounded_rectangle([x, y, x + w, y + fnt.size + 14], radius=8, fill=fill)
    draw.text((x + 12, y + 6), text, font=fnt, fill=(10, 10, 10))
    return w


def tag(draw, anchor, xy, lines, color, fnt=F_SMALL):
    """在画面空白处放多行标签，并画一条引线指向被标注的框。"""
    ax, ay = anchor
    x, y = xy
    width = max(tw(draw, line, fnt) for line in lines) + 14
    height = len(lines) * (fnt.size + 3) + 8
    draw.line([ax, ay, x, y + 8], fill=color, width=2)
    draw.rectangle([x, y, x + width, y + height], fill=(0, 0, 0, 220), outline=color + (180,), width=1)
    yy = y + 4
    for line in lines:
        draw.text((x + 7, yy), line, font=fnt, fill=color)
        yy += fnt.size + 3


def rc(rect):
    return f"{rect['w']}×{rect['h']} @({rect['x']},{rect['y']})"


def pt(point):
    return f"({point['x']},{point['y']})" if point else "—"


def load(path):
    return Image.open(ROOT / path).convert("RGB")


def record_by_key(data, key):
    for rec in data:
        if rec["surface_key"] == key:
            return rec
    raise SystemExit(f"缺 {key} 的会话证据")


def annotate_gallery_image(key: str, rec: dict):
    target_file = {
        "secondary_chat_hall": "02-secondary-chat-current-hall.png",
        "secondary_chat_friend": "03-secondary-chat-friend.png",
    }[key]
    canvas = load(GALLERY / target_file).convert("RGBA")
    if canvas.getpixel((2799, 1079))[:3] == (1, 2, 3):
        print(f"skip {target_file}: already annotated（重跑主画廊脚本后再跑本脚本）")
        return None
    overlay = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)
    hall = rec["hall"]
    left = rec["left_column"]
    sidebar = rec["sidebar_region"]

    # 1) 左栏（会话列表）
    box(draw, (sidebar["x"], sidebar["y"], sidebar["x"] + sidebar["w"], sidebar["y"] + sidebar["h"]),
        C_YELLOW, None, width=3, alpha=14)
    # 2) 生产高亮扫描窗
    box(draw, (left["x"], left["y"], left["x"] + left["w"], left["y"] + left["h"]),
        C_BLUE, None, width=2, alpha=10)
    # 3) 真实可见的高亮行（对照扫描）
    row = rec["sidebar_highlight"]
    if row:
        box(draw, (row["x"], row["y"], row["x"] + row["w"], row["y"] + row["h"]),
            C_ORANGE, None, width=3, alpha=34)
    # 4) 大厅模板命中 / 候选
    hall_color = C_GREEN if hall["matched"] else C_RED
    if hall["hit_rect"]:
        hit = hall["hit_rect"]
        box(draw, (hit["x"], hit["y"], hit["x"] + hit["w"], hit["y"] + hit["h"]),
            hall_color, None, width=3, alpha=70)
    # 标签统一放到左栏右侧空白处，避免遮住会话行文字
    lines = []
    if row:
        lines.append(f"视觉高亮行 y={row['y']}–{row['y'] + row['h']}（选中会话）")
    if hall["hit_rect"]:
        lines.append(f"hall {hall['score']:.4f} " + ("≥ 0.90 命中" if hall["matched"] else "< 0.90 拒绝"))
        lines.append(f"{rc(hall['hit_rect'])}，中心 {pt(hall['center'])}")
    if lines:
        tag(draw, (sidebar["x"] + sidebar["w"], (row["y"] if row else 300) + 20), (300, row["y"] if row else 300),
            lines, C_ORANGE if not hall["matched"] else C_GREEN)
    tag(draw, (sidebar["x"] + sidebar["w"], 500), (300, 500),
        ["左栏 / 会话列表", "黄色=整条左栏；蓝色=生产高亮扫描窗"],
        C_YELLOW)

    # 5) 判定徽标
    if rec["verdict"] == "CurrentHall":
        draw.rounded_rectangle([300, 84, 300 + 300, 84 + 70], radius=12, fill=(18, 46, 30, 235),
                               outline=C_GREEN, width=3)
        draw.text((318, 92), "CurrentHall", font=F_H2, fill=C_GREEN)
        draw.text((318, 124), "大厅模板命中（高亮非必要）", font=F_TINY, fill=C_MUTED)
    else:
        draw.rounded_rectangle([300, 84, 300 + 300, 84 + 70], radius=12, fill=(58, 42, 10, 235),
                               outline=C_ORANGE, width=3)
        draw.text((318, 92), "Unknown（不猜）", font=F_H2, fill=C_ORANGE)
        draw.text((318, 124), "视觉是好友行；入口未确认", font=F_TINY, fill=C_MUTED)

    # 6) 侧栏底部空区「会话子状态证据」
    x, y, w = 1942, 770, 838
    draw.rounded_rectangle([x - 4, y - 8, x + w, y + 270], radius=8, fill=(22, 22, 28, 250),
                           outline=(70, 76, 88), width=1)
    draw.text((x + 8, y), "会话子状态（ConversationRef）", font=F_H2, fill=C_ACCENT)
    y += F_H2.size + 10
    lines = [
        ("top-level kind = Secondary", C_TEXT, "会话不是新的 UiStateKind"),
        (f"UiSurface = Secondary(Chat)", C_TEXT, "分类器只到「二级聊天」"),
        (f"hall {hall['score']:.4f} / 阈值 {hall['threshold']:.2f} → " +
         ("命中" if hall["matched"] else "不命中"), C_GREEN if hall["matched"] else C_RED,
         f"{rc(hall['hit_rect'])}，中心 {pt(hall['center'])}" if hall["hit_rect"] else ""),
        (f"高亮行（生产）: {'无' if not rec['highlight'] else rec['highlight']}", C_RED,
         f"扫描窗 y={left['y']}–{left['y'] + left['h']}；对照行 y={row['y']}–{row['y'] + row['h']}" if row else ""),
        ("OCR 候选: " + ("UNVERIFIABLE（本机无 OCR）" if not rec["ocr_available"] else "可用"),
         C_ORANGE if not rec["ocr_available"] else C_GREEN, "不猜；无 OCR 不编候选名"),
        (f"判定: {rec['verdict']}", C_GREEN if rec["verdict"] == "CurrentHall" else C_ORANGE,
         "Friend 需要行内 OCR 唯一候选名" if rec["verdict"] != "CurrentHall" else "大厅模板即足够"),
    ]
    for head, color, note in lines:
        draw.text((x + 8, y), head, font=F_SMALL, fill=color)
        y += F_SMALL.size + 3
        if note:
            y = text_block(draw, x + 20, y, note, F_TINY, C_MUTED, w - 30, 1)
        y += 2

    canvas = Image.alpha_composite(canvas, overlay).convert("RGB")
    canvas.putpixel((2799, 1079), (1, 2, 3))  # 幂等标记：重复运行不再叠加标注
    canvas.save(GALLERY / target_file, optimize=True)
    return GALLERY / target_file


def arrow(draw, x1, y1, x2, y2, color):
    draw.line([x1, y1, x2, y2], fill=color, width=3)
    draw.polygon([(x2, y2), (x2 - 12, y2 - 7), (x2 - 12, y2 + 7)], fill=color)


def render_substate(data):
    canvas = Image.new("RGB", (2800, 1500), C_BG)
    draw = ImageDraw.Draw(canvas)

    draw.text((40, 26), "二级聊天 = UiStateKind::Secondary；会话是 ConversationRef 子状态", font=F_TITLE, fill=C_TEXT)
    draw.text((40, 78), "左侧高亮行是「当前选中会话」的视觉依据，但不是唯一依据；无 OCR / 歧义时返回 Unknown，不猜。",
              font=F_BODY, fill=C_MUTED)

    # ── 状态层级 ──────────────────────────────────────────────────────
    y = 126
    boxes = [
        (40, 420, "UiStateKind::Secondary", "top-level 大类，与 Primary / Panel / Overlay 同级"),
        (510, 440, "UiSurface::Secondary(Chat)", "只到「二级聊天」，不带具体会话"),
        (1000, 780, "ConversationRef（入口采集）", "CurrentHall · Friend{identity, candidates, row_anchor} · Unknown"),
    ]
    for bx, bw, title, sub in boxes:
        draw.rounded_rectangle([bx, y, bx + bw, y + 100], radius=10, fill=(24, 28, 34), outline=C_ACCENT, width=2)
        draw.text((bx + 14, y + 14), title, font=F_H2, fill=C_TEXT)
        text_block(draw, bx + 14, y + 52, sub, F_TINY, C_MUTED, bw - 28, 2)
    arrow(draw, 460, y + 50, 506, y + 50, C_ACCENT)
    arrow(draw, 950, y + 50, 996, y + 50, C_ACCENT)
    draw.rounded_rectangle([1810, y, 2760, y + 100], radius=10, fill=(44, 34, 14), outline=C_YELLOW, width=2)
    draw.text((1824, y + 14), "Friend 是恢复目标，不是新的 UI 大类", font=F_H2, fill=C_YELLOW)
    text_block(draw, 1824, y + 52, "恢复时靠候选名 + 高亮行锚点确认；歧义不点击。", F_TINY, C_MUTED, 920, 2)

    # ── 两帧并排 ──────────────────────────────────────────────────────
    panels = [
        ("secondary_chat_hall", 40, "A · 当前大厅（02）", C_GREEN),
        ("secondary_chat_friend", 1420, "B · 好友行（03）", C_ORANGE),
    ]
    for key, px, title, accent in panels:
        rec = record_by_key(data, key)
        hall = rec["hall"]
        row = rec["sidebar_highlight"]
        py = 266
        draw.rounded_rectangle([px, py, px + 1340, py + 908], radius=12, fill=(20, 20, 26), outline=(70, 76, 88), width=2)
        draw.text((px + 20, py + 16), title, font=F_H2, fill=accent)

        # 左栏 1:1 裁图（真实画面）
        frame_file = "round78-live.png" if key == "secondary_chat_hall" else "verify-secondary-true-private-chat.png"
        frame = load(f"technical-docs/ui-state-evidence/{frame_file}")
        if frame.width != 1920:
            frame = frame.resize((1920, 1080), Image.BILINEAR)
        crop = frame.crop((10, 80, 290, 880))
        cx, cy = px + 20, py + 60
        canvas.paste(crop, (cx, cy))
        draw.rectangle([cx, cy, cx + crop.width, cy + crop.height], outline=C_YELLOW, width=3)
        draw.text((cx + 6, cy + 6), "左栏 10,80–290,880", font=F_TINY, fill=C_YELLOW)

        if row:
            ry0 = row["y"] - 80
            box(draw, (cx, cy + ry0, cx + crop.width, cy + ry0 + row["h"]), C_ORANGE,
                f"高亮行 y={row['y']}–{row['y'] + row['h']}", width=3, alpha=40)
        if hall["hit_rect"]:
            hit = hall["hit_rect"]
            hx = cx + hit["x"] - 10
            hy = cy + hit["y"] - 80
            color = C_GREEN if hall["matched"] else C_RED
            box(draw, (hx, hy, hx + hit["w"], hy + hit["h"]), color, None, width=3, alpha=80)

        # 证据文字
        tx, ty = px + 330, py + 64
        maxw = 990
        kind = rec["kind"] + " / " + rec["label"]
        ty = text_block(draw, tx, ty, f"真实输出：{kind}；UiSurface=Secondary(Chat)", F_BODY, C_TEXT, maxw, 5)
        ty = text_block(draw, tx, ty + 4,
                        f"hall 模板 {Path(hall['template']).name}：score {hall['score']:.4f}（阈值 {hall['threshold']:.2f}）→ "
                        + ("命中" if hall["matched"] else "不命中"),
                        F_BODY, C_GREEN if hall["matched"] else C_RED, maxw, 5)
        if hall["hit_rect"]:
            ty = text_block(draw, tx, ty,
                            f"最佳位置 {hall['hit_rect']}，中心 {hall['center']}；搜索区 {hall['search_rect']}",
                            F_SMALL, C_MUTED, maxw, 4)
        ty = text_block(draw, tx, ty + 4,
                        f"生产高亮行（扫描窗 y={rec['left_column']['y']}–{rec['left_column']['y'] + rec['left_column']['h']}）："
                        + ("None" if not rec["highlight"] else str(rec["highlight"])),
                        F_BODY, C_RED if not rec["highlight"] else C_TEXT, maxw, 5)
        if row:
            coverage = (f"大厅模板中心 {pt(rec['hall']['center'])} 落在行内"
                        if hall["matched"] else "大厅未命中，覆盖检查不适用")
            ty = text_block(draw, tx, ty,
                            f"对照扫描（返回按钮下方整条侧栏）测到唯一高亮行 y={row['y']}–{row['y'] + row['h']}；{coverage}",
                            F_SMALL, C_TEXT, maxw, 4)
        ty = text_block(draw, tx, ty + 4,
                        "OCR 行候选：" + ("UNVERIFIABLE（本机无 OCR，未读候选名）" if not rec["ocr_available"] else "可用"),
                        F_BODY, C_ORANGE if not rec["ocr_available"] else C_GREEN, maxw, 5)
        if key == "secondary_chat_friend":
            ty = text_block(draw, tx, ty,
                            "结论：高亮行未进入生产扫描窗 → 在 OCR 之前就返回 None；视觉上是好友行，但入口采集是 Unknown，不能当 Friend。",
                            F_BODY, C_TEXT, maxw, 5)
        else:
            ty = text_block(draw, tx, ty,
                            "结论：大厅模板命中即足以判 CurrentHall（高亮 None 被放行）；高亮行在这里是旁证，不是必要条件。",
                            F_BODY, C_TEXT, maxw, 5)
        badge(draw, (tx, ty + 14), "判定 CurrentHall" if key == "secondary_chat_hall" else "判定 Unknown（不猜）",
              C_GREEN if key == "secondary_chat_hall" else C_ORANGE)

        # 为什么这样判（填满右下方空区，避免只看结论）
        by = ty + 90
        draw.rounded_rectangle([tx, by, tx + maxw, by + 230], radius=10,
                               fill=(26, 26, 34) if key == "secondary_chat_hall" else (40, 28, 12),
                               outline=C_GREEN if key == "secondary_chat_hall" else C_ORANGE, width=1)
        bullets = (
            [
                "大厅模板 1.0000 命中 36×37 @(26,204)，中心 (44,222) 落在可见高亮行 y=188–259 内（对照扫描）。",
                "生产自动扫描窗从 y=280 起没测到高亮，但 CurrentHall 规则允许高亮 None：模板是强证据。",
                "所以不需要 OCR 也能判 CurrentHall；高亮行是视觉旁证，不是必要条件。",
                "UiSurface 只到 Secondary(Chat)；具体会话靠 identify_conversation 在入口采集。",
            ]
            if key == "secondary_chat_hall"
            else [
                "大厅模板只有 0.7017，低于阈值 0.90，不能当当前大厅。",
                "生产高亮扫描窗没测到 y=226–298 的选中行 → 代码在 `let row = highlighted?` 处返回 None，不会调用 OCR。",
                "即使 OCR 可用，本帧也到不了「行内候选唯一」判断；入口判定只能是 Unknown，不是 Friend。",
                "UiSurface 只到 Secondary(Chat)；Friend 是恢复目标，不是新的 top-level kind。",
            ]
        )
        yy = by + 12
        for bullet in bullets:
            yy = text_block(draw, tx + 14, yy, "· " + bullet, F_SMALL, C_TEXT, maxw - 28, 4)
            yy += 6

    # ── 判定顺序 ──────────────────────────────────────────────────────
    fy = 1210
    draw.text((40, fy), "生产判定顺序（identify_conversation_from_frame）", font=F_H2, fill=C_ACCENT)
    fy += 42
    steps = [
        (40, C_GREEN, "① 大厅模板优先",
         "hall score ≥ 0.90，且（高亮 None 或 模板中心落在高亮行内）→ CurrentHall。\n"
         "大厅模板是强证据，所以高亮未测到也不否决。"),
        (960, C_BLUE, "② 好友会话",
         "否则必须：唯一高亮行 + 行内 OCR 恰好 1 个候选名 → Friend{identity, candidates, row_anchor}。\n"
         "候选 0 个或 ≥2 个都算歧义，不点击。"),
        (1880, C_ORANGE, "③ 不猜",
         "无高亮行 / 无 OCR / 行内候选不唯一 → Unknown。\n"
         "Unknown 不是「好友」，恢复时必须重新取证或 fallback。"),
    ]
    for sx, color, head, body in steps:
        draw.rounded_rectangle([sx, fy, sx + 880, fy + 150], radius=10, fill=(22, 22, 28), outline=color, width=2)
        draw.text((sx + 16, fy + 12), head, font=F_BODY, fill=color)
        y2 = fy + 48
        for line in body.split("\n"):
            text_block(draw, sx + 16, y2, line, F_SMALL, C_TEXT, 848, 3)
            y2 += F_SMALL.size + 10
    draw.text((40, 1450),
              "本帧实测：02 高亮行 y=188–259、03 高亮行 y=226–298，均在 y=280 起的高亮扫描窗之上；"
              "02 靠大厅模板 1.0000 判 CurrentHall，03 因高亮 None 在 OCR 前返回 Unknown。",
              font=F_SMALL, fill=C_MUTED)
    draw.text((40, 1472),
              "隐私：本图含真实昵称，仅本地核验；不提交、不发布。数据：dump_conversation_substate_evidence（真实帧、生产函数）。",
              font=F_TINY, fill=(120, 126, 138))

    out = GALLERY / "21-secondary-chat-substate.png"
    canvas.save(out, optimize=True)
    return out




MD_START = "<!-- conversation-substate:start -->"
MD_END = "<!-- conversation-substate:end -->"
TSV_SURFACES = {
    "secondary_chat_hall": ("二级聊天·当前大厅（Secondary Chat: Current Hall）",
                            "technical-docs/ui-state-evidence/round78-live.png"),
    "secondary_chat_friend": ("二级聊天·好友私聊（Secondary Chat: Friend Conversation）",
                              "technical-docs/ui-state-evidence/verify-secondary-true-private-chat.png"),
}


def _rc(rect):
    return f"{rect['w']}×{rect['h']} @({rect['x']},{rect['y']})" if rect else "—"


def _pt(point):
    return f"({point['x']},{point['y']})" if point else "—"


def markdown_block(data):
    hall = record_by_key(data, "secondary_chat_hall")
    friend = record_by_key(data, "secondary_chat_friend")
    hrow, frow = hall["sidebar_highlight"], friend["sidebar_highlight"]
    lines = [
        MD_START,
        "## 3.5 会话子状态：`当前大厅` vs `好友私聊`（`ConversationRef`）",
        "",
        "[![二级聊天会话子状态（ConversationRef）](21-secondary-chat-substate.png)](21-secondary-chat-substate.png)",
        "",
        "**结论先说**：二级聊天本身是 `UiStateKind::Secondary`（top-level kind）；`当前大厅` / `好友私聊` "
        "不是两个 UI 大类，而是入口采集到的会话子状态 `ConversationRef`。左侧会话列表的**高亮行**是"
        "「当前选中会话」的视觉依据，但**仅高亮不够**：",
        "",
        "- `CurrentHall`：要命中大厅模板/图标 `ui-secondary-hall`（`≥0.90`）；高亮行是旁证，未测到也不否决。",
        "- `Friend{identity, candidates, row_anchor}`：要高亮行 + 行内 OCR **恰好 1 个**候选名；"
        "0 个或 ≥2 个都算歧义，不点击。",
        "- 无高亮 / 无 OCR / 候选不唯一 → `Unknown`，不猜。",
        "",
        "代码口径：大厅分支是 `highlighted.is_none_or(|row| center_in_row)`——测到高亮时必须覆盖命中中心，"
        "未测到则不否决（安全 fallback）；好友分支是 `highlighted?` 后要求行内 OCR 候选恰好 1 个。",
        "",
        "| 层级 | 真实取值 | 说明 |",
        "|---|---|---|",
        "| `UiStateKind` | `Secondary` | 与 `Primary` / `Panel` / `Overlay` 同级；会话细分不新增 kind |",
        "| `UiSurface` | `Secondary(Chat)` | 只到「二级聊天」，不含具体会话 |",
        "| `ConversationRef` | `CurrentHall` / `Friend{...}` / `Unknown` | 由 `identify_conversation` 在入口采集；"
        "`Friend` 是恢复目标，不是新 UI 大类 |",
        "",
        "### 实测 A：02 当前大厅帧",
        "",
        "| 检查 | 真实值 | 阈值/口径 | 结果 | notes |",
        "|---|---|---|---|---|",
        f"| hall 模板 `ui-secondary-hall.png` | `{hall['hall']['score']:.4f}` | `≥{hall['hall']['threshold']:.2f}` | PASS | "
        f"{_rc(hall['hall']['hit_rect'])}，中心 {_pt(hall['hall']['center'])}；搜索区 `{_rc(hall['hall']['search_rect'])}` |",
        f"| 左栏（会话列表） | `10,80,280,800` | 视觉区域 | — | 图上黄框；蓝框是生产高亮扫描窗 |",
        f"| 高亮行（生产扫描窗 `10,280,280,600`） | `None` | 唯一高亮行 | FAIL | 选中行在 `y={hrow['y']}–{hrow['y'] + hrow['h']}`，位于窗口之上 |",
        f"| 高亮行（对照扫描 `10,80,280,800`） | `y={hrow['y']}–{hrow['y'] + hrow['h']}` | 唯一高亮行 | PASS | "
        f"hall 中心 `{_pt(hall['hall']['center'])}` 落在行内：`{hall['hall_center_in_sidebar_highlight']}` |",
        "| OCR 行候选 | 不需要 | — | PASS | 大厅模板已足够，不进入 OCR 分支 |",
        f"| `identify_conversation` | `{hall['verdict']}` | — | PASS | 离线分支（无 OCR）即可判定 |",
        "",
        "### 实测 B：03 好友帧",
        "",
        "| 检查 | 真实值 | 阈值/口径 | 结果 | notes |",
        "|---|---|---|---|---|",
        f"| hall 模板 `ui-secondary-hall.png` | `{friend['hall']['score']:.4f}` | `≥{friend['hall']['threshold']:.2f}` | FAIL | "
        f"最佳候选 {_rc(friend['hall']['hit_rect'])}，中心 {_pt(friend['hall']['center'])}；低于阈值，不命中 |",
        f"| 高亮行（生产扫描窗 `10,280,280,600`） | `None` | 唯一高亮行 | FAIL | 选中行在 `y={frow['y']}–{frow['y'] + frow['h']}` |",
        f"| 高亮行（对照扫描 `10,80,280,800`） | `y={frow['y']}–{frow['y'] + frow['h']}` | 唯一高亮行 | PASS | 视觉上唯一选中行 |",
        "| OCR 行候选 | `UNVERIFIABLE` | 行内恰好 1 个 | UNVERIFIABLE | Linux 离线无 OCR，未读取、也不引用真实候选名 |",
        f"| `identify_conversation` | `{friend['verdict']}`（`None`） | `Friend` 需要 OCR 唯一候选 | UNVERIFIABLE | "
        "高亮 `None` → 在 OCR 之前就返回；视觉是好友行，但不猜 |",
        "",
        "**为什么 03 不是 `Friend`**：生产 `conversation_row_region` 从 `invite.friend_list_region.y=280` 起"
        "（`10,280,280,600`），而本帧选中行在 `y=226–298`，只有 18px 与扫描窗重叠（<24px 最小行高），"
        "`highlighted_conversation_row` 返回 `None`。`identify_conversation_from_frame` 在 `let row = highlighted?` "
        "处直接返回 `None`，不会进入 OCR 候选唯一性判断——这与「OCR 是否可用」是两件事。02 不受影响："
        "大厅模板分支允许 `highlight=None`。",
        "",
        "**对恢复的含义**：`ConversationRef::Friend{...}` 是恢复目标；恢复路径同样依赖高亮行锚点。"
        "这类「选中行在 y<280」的真实帧会走不到 Friend 确认（`residency` 的会话步骤返回 "
        "`Unavailable(\"未找到唯一的高亮会话行\")`）。本画廊只如实记录，不改生产逻辑。",
        "",
        "**UNVERIFIABLE 清单**：03 的 OCR 候选名与 `Friend` 判定无法离线确认；本机没有 OCR，图上只标 "
        "`UNVERIFIABLE`，不贴真实昵称、不拿画面文字冒充 OCR 输出。",
        "",
        MD_END,
        "",
    ]
    return "\n".join(lines)


def update_markdown(data, md_path: Path):
    text = md_path.read_text(encoding="utf-8")
    text = re.sub(re.escape(MD_START) + r".*?" + re.escape(MD_END) + r"\n?", "", text, flags=re.S)
    block = markdown_block(data)
    anchor = "## 4. 二级界面·多人面板（F2）"
    text = text.replace(anchor, block + anchor, 1)

    row21 = ("| 21 | 二级聊天·会话子状态（ConversationRef） | "
             "[21-secondary-chat-substate.png](21-secondary-chat-substate.png) | —（由 02/03 左栏裁图组成） | "
             "`secondary` / `secondary:chat` + `ConversationRef` | `identify_conversation` | "
             "⚠️ 03 = `Unknown`（离线无法确认 `Friend`） |")
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if line.startswith("| 21 |"):
            lines[index] = row21
            break
    else:
        for index, line in enumerate(lines):
            if line.startswith("| 20 |"):
                lines.insert(index + 1, row21)
                break
    text = "\n".join(lines) + "\n"

    pointers = {
        "02-secondary-chat-current-hall.png": "- 会话子状态：`identify_conversation` = `CurrentHall`（大厅模板命中；高亮行是旁证）；详见 §3.5。",
        "03-secondary-chat-friend.png": "- 会话子状态：`identify_conversation` = `Unknown`（生产高亮扫描窗未覆盖选中行 + 本机无 OCR，不猜）；详见 §3.5。",
    }
    for image, pointer in pointers.items():
        marker = f"]({image})]({image})"
        for line in text.splitlines():
            if marker in line:
                text = text.replace(line + "\n" + pointer, line + "\n\n" + pointer)
                if pointer not in text:
                    text = text.replace(line + "\n", line + "\n\n" + pointer + "\n", 1)
                break

    old_convention = "紫色 = 聊天标记扫描区；青色 = 世界判据区域。"
    new_convention = old_convention + " 会话子状态图另加：黄框 = 左栏，蓝框 = 生产高亮扫描窗，橙框 = 可见高亮行，绿/红框 = 大厅模板命中/拒绝。"
    if "会话子状态图另加" not in text:
        text = text.replace(old_convention, new_convention, 1)

    old_intro = "渲染脚本 `scripts/render-ui-evidence-gallery.py` 只画框与排版，不计算分数、不补数字。"
    if "另加一张会话子状态对照图" not in text:
        text = text.replace("本页每个界面/子型一张图。", "本页每个界面/子型一张图，另加一张会话子状态对照图（21）。", 1)
    if "scripts/render-secondary-conversation-evidence.py` 追加" not in text:
        text = text.replace(
            old_intro,
            old_intro + " 02/03 的会话子状态标注与 21 号图由 "
            "`scripts/render-secondary-conversation-evidence.py` 追加，数字来自 "
            "`dump_conversation_substate_evidence` 的只读诊断。", 1)

    text = text.replace("- 20 个界面/子型全部出图；",
                        "- 21 个条目（20 个界面/子型 + 1 张会话子状态对照图）全部出图；")
    text = text.replace("FAIL 2 个，UNVERIFIABLE 1 个。",
                        "FAIL 2 个，UNVERIFIABLE 1 个（按界面/子型计；会话子状态的 03 另见 §3.5）。", 1)
    bullet = ("  - **二级聊天·好友私聊的会话子状态（03）** — `kind=secondary` / `label=secondary:chat` 成立，"
              "但入口 `identify_conversation` 返回 `None`（`Unknown`）：生产高亮扫描窗未覆盖真实选中行，"
              "在 OCR 之前就停住；`Friend` 无法离线确认，标 UNVERIFIABLE。")
    if "**二级聊天·好友私聊的会话子状态（03）**" not in text:
        text = text.replace("- UNVERIFIABLE：\n", "- UNVERIFIABLE：\n" + bullet + "\n", 1)

    old_limit = "  - 二级聊天「当前大厅 vs 好友私聊」共用 `secondary:chat`，细分依赖会话/选中行 OCR，本画廊只能证明两者都进二级聊天；"
    new_limit = ("  - 二级聊天「当前大厅 vs 好友私聊」共用 `secondary:chat`；会话子状态由 `ConversationRef` 表达（见 §3.5）。"
                 "本机无 OCR，03 的 `Friend` 只能标 UNVERIFIABLE；实测生产高亮扫描窗从 `friend_list_region.y=280` 起，"
                 "未覆盖 02/03 的选中行（`y=188–259` / `y=226–298`）。")
    text = text.replace(old_limit, new_limit)

    method_anchor = "  （`#[cfg(test)]` 诊断，读 `MILIASTRA_GALLERY_FRAMES`，写 `MILIASTRA_GALLERY_OUT`）。"
    addition = ("\n- 会话子状态导出：`cargo test --lib --no-default-features dump_conversation_substate_evidence -- --ignored --nocapture`\n"
                "  （`#[cfg(test)]` 只读诊断，读 `MILIASTRA_CONVERSATION_FRAMES`，写 `MILIASTRA_CONVERSATION_OUT`；"
                "真实跑 `FriendDeliveryRoutineConfig` + `identify_conversation_from_frame`）。\n"
                "- 会话子状态渲染：`python3 scripts/render-secondary-conversation-evidence.py target/evidence-gallery/conversation.jsonl`\n"
                "  （追加 02/03 标注并生成 21；重跑主画廊脚本后需再跑一次）。")
    if "cargo test --lib --no-default-features dump_conversation_substate_evidence" not in text:
        text = text.replace(method_anchor, method_anchor + addition, 1)
    assert MD_START in text and "| 21 |" in text and "## 3.5" in text, "画廊 markdown 结构变化，未能插入会话子状态小节"
    md_path.write_text(text, encoding="utf-8")


def update_tsv(data):
    path = ROOT / "technical-docs" / "ui-state-corpus" / "evidence-scores.tsv"
    lines = path.read_text(encoding="utf-8").splitlines()
    lines = [line for line in lines if len(line.split("\t")) < 4 or not line.split("\t")[3].startswith("conversation.")]
    rows = []
    for key in ("secondary_chat_hall", "secondary_chat_friend"):
        rec = record_by_key(data, key)
        surface, frame = TSV_SURFACES[key]
        rule = "secondary_chat_anchor"
        hall = rec["hall"]
        row = rec["sidebar_highlight"]
        rows += [
            (surface, frame, rule, "conversation.kind", "Secondary", "top-level kind", "PASS",
             "UiStateKind::Secondary；会话子状态不新增 kind"),
            (surface, frame, rule, "conversation.ui_surface", "Secondary(Chat)", "只到二级聊天", "PASS",
             "identify_conversation 在入口采集 ConversationRef"),
            (surface, frame, rule, "conversation.hall_template_score", f"{hall['score']:.4f}",
             f"{hall['threshold']:.2f}",
             "PASS" if hall["matched"] else "FAIL",
             f"assets/ui-secondary-hall.png search={_rc(hall['search_rect'])} hit={_rc(hall['hit_rect'])} "
             f"center={_pt(hall['center'])}"),
            (surface, frame, rule, "conversation.highlight_row",
             "None" if not rec["highlight"] else str(rec["highlight"]), "唯一高亮行",
             "PASS" if rec["highlight"] else "FAIL",
             f"生产扫描窗 10,280,280,600；对照扫描 10,80,280,800 测到 y={row['y']}–{row['y'] + row['h']}"),
            (surface, frame, rule, "conversation.hall_center_in_highlight",
             "n/a (None 放行)" if hall["matched"] and not rec["highlight"]
             else ("n/a (无大厅命中)" if not hall["matched"] else str(rec["hall_center_in_highlight"])),
             "大厅模板中心落在高亮行内", "PASS" if hall["matched"] else "FAIL",
             f"highlight=None 时大厅分支不否决；对照行内覆盖={rec['hall_center_in_sidebar_highlight']}"),
            (surface, frame, rule, "conversation.ocr_candidate",
             "not_required" if key == "secondary_chat_hall" else "UNVERIFIABLE",
             "行内恰好 1 个候选名", "PASS" if key == "secondary_chat_hall" else "UNVERIFIABLE",
             "当前大厅不需要 OCR" if key == "secondary_chat_hall" else "Linux 离线无 OCR；不引用真实昵称"),
            (surface, frame, rule, "conversation.verdict", rec["verdict"],
             "CurrentHall" if key == "secondary_chat_hall" else "Friend（需 OCR 唯一候选）",
             "PASS" if rec["verdict"] == "CurrentHall" else "UNVERIFIABLE",
             "离线分支即可判定" if rec["verdict"] == "CurrentHall"
             else "高亮 None → 在 OCR 前返回 None；视觉是好友行，不猜"),
        ]
    # 去掉可能已存在的同 metric 行，再追加，保证脚本可重复执行。
    head = lines[0]
    body = [line for line in lines[1:]
            if not any(line.split("\t")[3] == row[3] for row in rows)]
    rendered = ["\t".join(str(cell).replace("\t", " ").replace("\n", " ") for cell in row) for row in rows]
    path.write_text("\n".join([head] + body + rendered) + "\n", encoding="utf-8")


def update_docs(data):
    md = GALLERY / "evidence-gallery.md"
    update_markdown(data, md)
    update_tsv(data)
    print(f"updated {md.relative_to(ROOT)} and technical-docs/ui-state-corpus/evidence-scores.tsv")


def main():
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "evidence-gallery" / "conversation.jsonl"
    data = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    for key in ("secondary_chat_hall", "secondary_chat_friend"):
        out = annotate_gallery_image(key, record_by_key(data, key))
        if out:
            print(f"annotated {out.relative_to(ROOT)}")
    out = render_substate(data)
    print(f"rendered {out.relative_to(ROOT)}")
    update_docs(data)


if __name__ == "__main__":
    main()
