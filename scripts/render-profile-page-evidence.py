#!/usr/bin/env python3
"""个人资料页（panel:profile_page）判据标注图。

数据来源只有两个：
  1. 生产分类器导出的 JSON（scripts/ui-report.py 生成的报告，含两个模板探针的
     真实分数 / 阈值 / 命中框 / 搜索区）；
  2. 两张模板 PNG（与运行时同一文件，1:1 原样贴出）。
本脚本不计算任何分数，也不改判定逻辑，只负责画框、排版和汇总。

用法：
    python3 scripts/render-profile-page-evidence.py <frame.png> <report.json> \
        --out technical-docs/ui-state-evidence/evidence-gallery/22-overlay-profile-page.png
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
FONT_PATH = "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"
PANEL_W = 1000

C_BG = (16, 16, 20)
C_TEXT = (236, 238, 242)
C_MUTED = (154, 162, 176)
C_ACCENT = (86, 180, 233)
C_PASS = (61, 220, 132)
C_FAIL = (255, 82, 82)
C_SEARCH = (255, 210, 0)
C_HIT = (61, 220, 132)


def font(size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(FONT_PATH, size)


F_TITLE = font(30)
F_SUB = font(20)
F_BODY = font(18)
F_SMALL = font(15)
F_BADGE = font(26)


def dashed_rect(draw: ImageDraw.ImageDraw, rect, color, width=2, dash=14):
    x0, y0, x1, y1 = rect
    for x in range(x0, x1, dash * 2):
        draw.line([(x, y0), (min(x + dash, x1), y0)], fill=color, width=width)
        draw.line([(x, y1), (min(x + dash, x1), y1)], fill=color, width=width)
    for y in range(y0, y1, dash * 2):
        draw.line([(x0, y), (x0, min(y + dash, y1))], fill=color, width=width)
        draw.line([(x1, y), (x1, min(y + dash, y1))], fill=color, width=width)


def label_chip(draw, xy, text, color, bg=(10, 10, 14), text_font=F_SMALL, pad=6):
    x, y = xy
    left, top, right, bottom = draw.textbbox((0, 0), text, font=text_font)
    w, h = right - left, bottom - top
    draw.rectangle([x, y, x + w + pad * 2, y + h + pad * 2], fill=bg, outline=color, width=2)
    draw.text((x + pad, y + pad), text, fill=color, font=text_font)
    return x + w + pad * 2, y + h + pad * 2


def draw_frame(frame: Image.Image, probes, verdict: str, scale: int = 2):
    """主画面：搜索区虚线框 + 两个按钮实线框（带分数/阈值/PASS）。"""
    canvas = frame.copy()
    draw = ImageDraw.Draw(canvas)
    search = probes[0]["search_rect"]
    dashed_rect(
        draw,
        (
            search["x"],
            search["y"],
            search["x"] + search["w"] - 1,
            search["y"] + search["h"] - 1,
        ),
        C_SEARCH,
    )
    draw.text(
        (search["x"] + 2, search["y"] - 26),
        f"双按钮搜索区 {search['w']}x{search['h']} @({search['x']},{search['y']})",
        fill=C_SEARCH,
        font=F_SMALL,
    )
    for probe in probes:
        hit = probe["hit_rect"]
        color = C_HIT if probe["pass"] else C_FAIL
        box = (hit["x"], hit["y"], hit["x"] + hit["w"] - 1, hit["y"] + hit["h"] - 1)
        draw.rectangle(box, outline=color, width=3)
        name = Path(probe["template"]).stem.replace("qianxing-profile-page-", "")
        text = f"{name} {probe['score']:.4f} ≥ {probe['threshold']:.3f} PASS"
        # 两个标签都放在各自按钮框上方：间距 216 px，横向不会互相压住，也不会被画布下沿裁掉。
        label_chip(draw, (hit["x"], hit["y"] - 34), text, color)
    # 左上角总结论
    label_chip(
        draw,
        (24, 24),
        f"覆盖层·个人资料页  panel:profile_page  ·  双按钮双命中  ·  {verdict}",
        C_PASS if verdict == "PASS" else C_FAIL,
        bg=(6, 20, 12) if verdict == "PASS" else (20, 6, 6),
        text_font=F_SUB,
        pad=10,
    )
    return canvas


def draw_panel(panel: Image.Image, frame: Image.Image, probes, report, scale: int = 2):
    draw = ImageDraw.Draw(panel)
    width = panel.width
    y = 28
    draw.text((28, y), "个人资料页（Overlay: ProfilePage）", fill=C_TEXT, font=F_TITLE)
    y += 46
    draw.text(
        (28, y),
        f"kind={report['kind']}  label={report['label']}  final_rule={report['final_rule']}",
        fill=C_ACCENT,
        font=F_BODY,
    )
    y += 30
    draw.text(
        (28, y),
        "判据：右下角「心愿单」+「衣橱」两个模板同时命中（分数 ≥ 阈值且各自落在自己的位置）",
        fill=C_MUTED,
        font=F_SMALL,
    )
    y += 30
    surface = report["surface"]
    draw.text(
        (28, y),
        f"surface = {surface['debug']}（{surface['chinese']}）",
        fill=C_MUTED,
        font=F_SMALL,
    )
    y += 40

    # 右下角 2x 放大图（只截两个按钮附近，1:1 像素放大）
    search = probes[0]["search_rect"]
    # 下沿停在 1046：按钮底部抗锯齿行之后、UID 行之前，放大图不携带 UID。
    crop_box = (1440, 980, 1900, 1046)
    crop = frame.crop(crop_box).resize(
        ((crop_box[2] - crop_box[0]) * scale, (crop_box[3] - crop_box[1]) * scale),
        Image.NEAREST,
    )
    zoom_draw = ImageDraw.Draw(crop)
    for probe in probes:
        hit = probe["hit_rect"]
        color = C_HIT if probe["pass"] else C_FAIL
        box = (
            (hit["x"] - crop_box[0]) * scale,
            (hit["y"] - crop_box[1]) * scale,
            (hit["x"] - crop_box[0] + hit["w"]) * scale - 1,
            (hit["y"] - crop_box[1] + hit["h"]) * scale - 1,
        )
        zoom_draw.rectangle(box, outline=color, width=3)
    draw.text((28, y), f"右下角 {scale}x 放大（绿框 = 命中框）", fill=C_TEXT, font=F_BODY)
    y += 28
    panel.paste(crop, (28, y))
    y += crop.height + 12
    draw.text(
        (28, y),
        f"搜索区 {search['w']}x{search['h']} @({search['x']},{search['y']})，1/2 下采样匹配",
        fill=C_MUTED,
        font=F_SMALL,
    )
    y += 40

    # 两张模板 1:1 + 分数
    draw.text((28, y), "模板（1:1 原样贴出，无 alpha / 无缩放）", fill=C_TEXT, font=F_BODY)
    y += 32
    for probe in probes:
        name = Path(probe["template"]).stem
        color = C_HIT if probe["pass"] else C_FAIL
        thumb = Image.open(ROOT / probe["template"]).convert("RGB")
        draw.text(
            (28, y),
            f"{name}  {thumb.width}x{thumb.height}",
            fill=C_TEXT,
            font=F_SMALL,
        )
        y += 22
        panel.paste(thumb, (28, y))
        draw.rectangle(
            [28, y, 28 + thumb.width - 1, y + thumb.height - 1], outline=color, width=2
        )
        draw.text(
            (28 + thumb.width + 20, y + 4),
            f"score {probe['score']:.4f}\nthreshold {probe['threshold']:.3f}\n"
            f"{'PASS' if probe['pass'] else 'FAIL'}",
            fill=color,
            font=F_SMALL,
        )
        y += thumb.height + 26
    # 结论
    verdict = "PASS" if all(p["pass"] for p in probes) else "FAIL"
    draw.rectangle([24, y + 4, width - 24, y + 78], outline=C_PASS if verdict == "PASS" else C_FAIL, width=3)
    draw.text(
        (40, y + 18),
        f"{verdict}  {len(probes)}/2 模板命中，判定 panel:profile_page",
        fill=C_PASS if verdict == "PASS" else C_FAIL,
        font=F_BADGE,
    )
    return panel


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("frame", type=Path)
    parser.add_argument("report", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    report = json.loads(args.report.read_text(encoding="utf-8"))
    probes = [p for p in report["template_probes"] if p["source"] == "production"]
    if len(probes) != 2:
        raise SystemExit(f"需要两个生产模板探针，实际 {len(probes)}")
    frame = Image.open(args.frame).convert("RGB")
    if frame.size != (1920, 1080):
        raise SystemExit(f"帧尺寸必须是 1920x1080，实际 {frame.size}")
    verdict = "PASS" if report["label"] == "panel:profile_page" and all(
        p["pass"] for p in probes
    ) else "FAIL"

    canvas = Image.new("RGB", (frame.width + PANEL_W, frame.height), C_BG)
    canvas.paste(draw_frame(frame, probes, verdict), (0, 0))
    panel = Image.new("RGB", (PANEL_W, frame.height), C_BG)
    canvas.paste(draw_panel(panel, frame, probes, report), (frame.width, 0))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(args.out)
    print(f"{verdict} -> {args.out} ({canvas.width}x{canvas.height})")


if __name__ == "__main__":
    main()
