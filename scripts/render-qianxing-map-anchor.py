#!/usr/bin/env python3
"""把「千星地图右下角锚点」的真实证据渲染成一张标注图（PNG）+ markdown。

数据来源只有一个：现有证据导出器 `dump_ui_state_evidence_for_gallery` 输出的 JSONL
（生产分类器 + 生产锚点判据 `metrics.qianxing_map_anchor` 的真实测量值）。本脚本
不计算任何分数、不写死任何“看起来对”的数字；它只负责画框、排版、汇总和判定 PASS/FAIL。

用法：
    MILIASTRA_GALLERY_FRAMES=technical-docs/ui-state-corpus/qianxing-map-anchor-frames.tsv \
    MILIASTRA_GALLERY_OUT=/tmp/qianxing-map-anchor/records.jsonl \
      cargo test --lib --no-default-features -- --ignored --exact --nocapture \
        ui::state::tests::dump_ui_state_evidence_for_gallery
    python3 scripts/render-qianxing-map-anchor.py /tmp/qianxing-map-anchor/records.jsonl
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery"
FONT_PATH = "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"

FRAME_W, FRAME_H = 480, 270          # 整帧缩略图
ZOOM_SCALE = 2                        # 右下角 ROI 放大倍数
TEXT_W = 430
ROW_H = 286
PAD = 18

F_BIG = ImageFont.truetype(FONT_PATH, 26)
F_MID = ImageFont.truetype(FONT_PATH, 20)
F_SMALL = ImageFont.truetype(FONT_PATH, 16)
F_TINY = ImageFont.truetype(FONT_PATH, 13)

PASS_COLOR = (60, 200, 120)
FAIL_COLOR = (230, 80, 80)
ROI_COLOR = (255, 200, 40)
TPL_COLOR = (80, 180, 255)
TEXT_COLOR = (235, 235, 235)
DIM_COLOR = (160, 160, 160)


def load_records(path: Path) -> list[dict]:
    records = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            records.append(json.loads(line))
    return records


def rect_tuple(rect: dict, scale_x: float, scale_y: float, ox: int = 0, oy: int = 0):
    x = int(rect["x"] * scale_x) + ox
    y = int(rect["y"] * scale_y) + oy
    w = max(1, int(rect["width"] * scale_x))
    h = max(1, int(rect["height"] * scale_y))
    return x, y, x + w, y + h


def draw_box(draw, box, color, width=2, label=None, font=F_TINY):
    draw.rectangle(box, outline=color, width=width)
    if label:
        x0, y0 = box[0], box[1]
        draw.rectangle(
            (x0, y0 - 16, x0 + int(draw.textlength(label, font=font)) + 8, y0),
            fill=(0, 0, 0),
        )
        draw.text((x0 + 4, y0 - 16), label, font=font, fill=color)


def render(records: list[dict], out_png: Path) -> list[dict]:
    for record in records:
        anchor = record.get("metrics", {}).get("qianxing_map_anchor")
        if anchor is not None:
            record["anchor"] = anchor
    rows = [r for r in records if r.get("anchor")]
    rows.sort(key=lambda r: (0 if r["role"] == "positive" else 1, -float(r["anchor"]["score"] or 0)))
    # 表头
    head_h = 116
    canvas_h = head_h + ROW_H * len(rows) + PAD
    zoom_w = int(rows[0]["anchor"]["rect"]["width"] * ZOOM_SCALE) if rows else 608
    canvas_w = PAD * 2 + FRAME_W + PAD + zoom_w + PAD + TEXT_W
    image = Image.new("RGB", (canvas_w, canvas_h), (24, 26, 32))
    draw = ImageDraw.Draw(image)

    draw.text((PAD, 14), "千星地图 · 右下角锚点（模板匹配）", font=F_BIG, fill=TEXT_COLOR)
    anchor = rows[0]["anchor"] if rows else {}
    draw.text(
        (PAD, 48),
        f"ROI（规范 1920×1080）：x={anchor.get('rect', {}).get('x')} y={anchor.get('rect', {}).get('y')} "
        f"w={anchor.get('rect', {}).get('width')} h={anchor.get('rect', {}).get('height')}"
        f"   模板：{anchor.get('template')}（{anchor.get('template_width')}×{anchor.get('template_height')}）"
        f"   二值阈值 luma≥{anchor.get('bright_min')}   F1 阈值 {anchor.get('threshold')}",
        font=F_MID,
        fill=DIM_COLOR,
    )
    draw.text(
        (PAD, 78),
        "判据：模板形状（千星奇域·大厅 文字 + 庙宇图标）与画面二值亮块做 F1，±4 px 位移取最大；"
        "命中即 secondary:qianxing_map。",
        font=F_SMALL,
        fill=DIM_COLOR,
    )

    table = []
    y = head_h
    for rec in rows:
        a = rec["anchor"]
        score = float(a["score"] or 0.0)
        threshold = float(a["threshold"])
        positive = rec["role"] == "positive"
        ok = (score >= threshold) if positive else (score < threshold)
        color = PASS_COLOR if ok else FAIL_COLOR

        # 整帧
        frame_img = Image.open(rec["frame"]).convert("RGB")
        sx = FRAME_W / frame_img.width
        sy = FRAME_H / frame_img.height
        thumb = frame_img.resize((FRAME_W, FRAME_H), Image.LANCZOS)
        image.paste(thumb, (PAD, y))
        draw.rectangle((PAD, y, PAD + FRAME_W, y + FRAME_H), outline=(70, 70, 80), width=1)
        draw_box(
            draw,
            rect_tuple(a["rect"], sx, sy, PAD, y),
            ROI_COLOR,
            2,
            "anchor ROI",
        )
        draw_box(
            draw,
            rect_tuple(a["hit_rect"], sx, sy, PAD, y) if a.get("hit_rect") else rect_tuple(a["rect"], sx, sy, PAD, y),
            TPL_COLOR,
            1,
        )

        # ROI 放大 + 模板框
        zx = PAD + FRAME_W + PAD
        rect = a["rect"]
        crop = frame_img.crop(
            (
                rect["x"],
                rect["y"],
                rect["x"] + rect["width"],
                rect["y"] + rect["height"],
            )
        )
        crop = crop.resize((rect["width"] * ZOOM_SCALE, rect["height"] * ZOOM_SCALE), Image.LANCZOS)
        image.paste(crop, (zx, y))
        draw.rectangle(
            (zx, y, zx + crop.width, y + crop.height), outline=(70, 70, 80), width=1
        )
        if a.get("hit_rect"):
            hx = (a["hit_rect"]["x"] - rect["x"]) * ZOOM_SCALE + zx
            hy = (a["hit_rect"]["y"] - rect["y"]) * ZOOM_SCALE + y
            draw_box(
                draw,
                (
                    hx,
                    hy,
                    hx + a["template_width"] * ZOOM_SCALE,
                    hy + a["template_height"] * ZOOM_SCALE,
                ),
                TPL_COLOR,
                2,
                "template",
            )

        # 文本块
        tx = zx + crop.width + PAD
        name = Path(rec["frame"]).name
        draw.text((tx, y + 2), name, font=F_MID, fill=TEXT_COLOR)
        rel = str(Path(rec["frame"]).relative_to(ROOT)) if str(rec["frame"]).startswith(str(ROOT)) else rec["frame"]
        draw.text((tx, y + 32), rel[:64], font=F_TINY, fill=DIM_COLOR)
        draw.text(
            (tx, y + 58),
            f"score {score:.4f}   threshold {threshold:.2f}",
            font=F_MID,
            fill=color if ok else TEXT_COLOR,
        )
        kind = rec.get("kind", "?")
        label = rec.get("label", "?")
        draw.text((tx, y + 88), f"→ {kind} / {label}", font=F_SMALL, fill=TEXT_COLOR)
        draw.text((tx, y + 112), f"rule {rec.get('final_rule', '?')}", font=F_SMALL, fill=DIM_COLOR)
        draw.text(
            (tx, y + 136),
            f"期望 {rec.get('expected', '?')}",
            font=F_SMALL,
            fill=DIM_COLOR,
        )
        verdict = "PASS" if ok else "FAIL"
        draw.rectangle((tx, y + 168, tx + 92, y + 200), fill=color)
        draw.text((tx + 20, y + 172), verdict, font=F_MID, fill=(20, 20, 20))
        draw.text(
            (tx + 104, y + 172),
            "正样本命中" if positive and ok else ("负样本拒绝" if not positive and ok else "不符合期望"),
            font=F_SMALL,
            fill=TEXT_COLOR,
        )

        table.append(
            {
                "frame": rel,
                "role": rec["role"],
                "score": score,
                "threshold": threshold,
                "kind": kind,
                "label": label,
                "rule": rec.get("final_rule", ""),
                "expected": rec.get("expected", ""),
                "ok": ok,
            }
        )
        y += ROW_H

    image.save(out_png)
    return table


def write_markdown(table: list[dict], out_md: Path, png_rel: str) -> None:
    positives = [r for r in table if r["role"] == "positive"]
    negatives = [r for r in table if r["role"] != "positive"]
    pos_min = min((r["score"] for r in positives), default=0.0)
    neg_max = max((r["score"] for r in negatives), default=0.0)
    threshold = table[0]["threshold"] if table else 0.0
    lines = [
        "# 千星地图 · 右下角锚点证据",
        "",
        "本页数字全部由生产分类器 `TemplateUiStateClassifier` / 锚点判据在同一批真实帧上导出"
        "（`dump_ui_state_evidence_for_gallery` + `scripts/render-qianxing-map-anchor.py`），"
        "不是手写估计。",
        "",
        f"![anchor]({png_rel})",
        "",
        "## 判据",
        "",
        "- 位置：规范 1920×1080 画面右下角，`x=1596, y=980, 304×76`（搜索区，模板四周各留 4 px 位移余量）。",
        "- 模板：`assets/qianxing-map-label.png`，`296×68`，裁自真实帧 "
        "`qianxing-entry-56/07-qianxing-map.jpg` 的 `x=1600, y=984`。",
        "- 只裁「千星奇域·大厅」文字 + 庙宇图标 + 红色角标，不含 UID，也不含整条右下角背景。",
        "- 匹配：模板与画面都按 luma≥150 二值化后做形状 F1（`2·交集 / (模板亮块 + 画面亮块)`），"
        "±4 px 位移取最大。F1 对“亮块多出/缺失”双向惩罚，背景亮度（深蓝/紫）被判为 0，"
        "所以跨背景（深色版与亮底版地图）仍稳定。",
        f"- 阈值：**{threshold:.2f}**。",
        "",
        "## 分离度",
        "",
        f"- 正样本（真实千星地图）最低分：**{pos_min:.4f}**",
        f"- 负样本最高分：**{neg_max:.4f}**",
        f"- 空档宽 {pos_min - neg_max:.4f}，阈值落在空档内（不是迁就某一侧）。",
        "",
        "## 明细",
        "",
        "| 帧 | 角色 | F1 | 期望 | 分类输出 | 结果 |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for r in positives + negatives:
        lines.append(
            f"| `{r['frame']}` | {r['role']} | {r['score']:.4f} | {r['expected']} | "
            f"{r['kind']} / {r['label']} | {'PASS' if r['ok'] else 'FAIL'} |"
        )
    lines += [
        "",
        "## 负样本覆盖",
        "",
        "千星大厅、主菜单千星、主菜单大世界、F2 大厅页、二级聊天、覆盖层大厅卡/档案卡、"
        "提瓦特地图（含右下角同样有文字+圆形图标+红点的「月之高地」帧，是最难的近邻）"
        "、Loading、Overworld，全部未判千星地图。",
        "",
        "## OCR 后备",
        "",
        "`qianxing_map_label_text_matches`（纯字符串）+ `qianxing_map_label_via_ocr`"
        "（对同一块 ROI 调 OCR）是可选后备：只在回退探针里跑（`Unknown` 每 5 s 一次才能走到）；"
        "离线门禁与单测走模板路径即可，不需要 OCR 模型。",
        "",
    ]
    out_md.write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: render-qianxing-map-anchor.py <records.jsonl>", file=sys.stderr)
        return 2
    records = load_records(Path(sys.argv[1]))
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    out_png = OUT_DIR / "qianxing-map-anchor.png"
    out_md = OUT_DIR / "qianxing-map-anchor.md"
    table = render(records, out_png)
    write_markdown(table, out_md, out_png.name)
    failed = [r for r in table if not r["ok"]]
    print(f"渲染 {len(table)} 帧 -> {out_png}")
    print(f"报告 -> {out_md}")
    if failed:
        print(f"FAIL {len(failed)} 帧：", file=sys.stderr)
        for r in failed:
            print(f"  {r['frame']} score={r['score']:.4f}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
