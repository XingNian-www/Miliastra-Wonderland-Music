#!/usr/bin/env python3
"""Linux 离线 OCR 辅助：RapidOCR(ONNX) 优先，Tesseract 兜底。

只用于检测报告展示，不参与分类判定。

用法:
    python3 scripts/ui_ocr_linux.py <image> --rect x,y,w,h [--scale 2] [--json]
    python3 scripts/ui_ocr_linux.py <image> --rect x,y,w,h --backend tesseract
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

from PIL import Image, ImageOps

FIXES = [
    ("干星", "千星"),
    ("千星奇城", "千星奇域"),
    ("奇城", "奇域"),
    ("大戌", "大厅"),
    ("大汀", "大厅"),
]

# 已知 UI 文案：OCR 噪声时收敛到规范文案（仅报告展示）。
_CANON = [
    ("好友 / 我的好友", ("好友", "我的好友")),
    ("当前大厅", ("当前大厅",)),
    ("千星奇域·大厅", ("千星奇域", "大厅")),
    ("公共大厅", ("公共大厅",)),
    ("大厅人数", ("大厅人数",)),
    ("大厅列表", ("大厅列表",)),
    ("新建大厅", ("新建大厅",)),
    ("邀请玩家", ("邀请玩家",)),
    ("奇域等级", ("奇域等级",)),
    ("缤纷评分", ("缤纷评分",)),
    ("特典活动", ("特典活动",)),
    ("申请加入", ("申请加入",)),
    ("前往其大厅", ("前往其大厅",)),
    ("进入该大厅", ("进入该大厅",)),
    ("发送", ("发送",)),
]


def _strip_noise(text: str) -> str:
    text = re.sub(r"\s+", " ", text).strip()
    text = re.sub(r"(?<=[\u4e00-\u9fff])\s+(?=[\u4e00-\u9fff])", "", text)
    text = re.sub(r"\s*[.·。]\s*", "·", text)
    for a, b in FIXES:
        text = text.replace(a, b)
    return text.strip()


def canonicalise(text: str) -> str:
    for canon, keys in _CANON:
        if all(k in text for k in keys):
            return canon
    return text


def normalize(text: str) -> str:
    return canonicalise(_strip_noise(text))


def _load_crop(image_path, rect, scale):
    x, y, w, h = rect
    im = Image.open(image_path).convert("RGB").crop((x, y, x + w, y + h))
    if scale > 0:
        im = im.resize((im.width * scale, im.height * scale), Image.LANCZOS)
    return im


def _rapidocr_crop(im) -> dict | None:
    try:
        import numpy as np
        from rapidocr_onnxruntime import RapidOCR
    except Exception:
        return None
    engine = _rapidocr_crop.engine if hasattr(_rapidocr_crop, "engine") else None
    if engine is None:
        engine = RapidOCR()
        _rapidocr_crop.engine = engine
    result, _ = engine(np.array(im))
    lines = []
    for item in result or []:
        text, score = item[1], float(item[2])
        lines.append({"text": _strip_noise(text), "score": round(score, 4)})
    joined = " ".join(l["text"] for l in lines)
    return {
        "lines": lines,
        "text": normalize(joined),
        "raw": " | ".join(l["text"] for l in lines),
        "confidence": round(sum(l["score"] for l in lines) / len(lines), 4) if lines else None,
        "backend": "rapidocr",
        "available": True,
    }


def _tesseract_crop(im) -> dict | None:
    if shutil.which("tesseract") is None:
        return None
    im = ImageOps.autocontrast(ImageOps.grayscale(im))
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "crop.png"
        im.save(p)
        cp = subprocess.run(
            ["tesseract", str(p), "stdout", "-l", "chi_sim", "--psm", "6", "tsv"],
            capture_output=True, text=True, timeout=30,
        )
    lines, confs = [], []
    for line in cp.stdout.splitlines()[1:]:
        cols = line.split("\t")
        if len(cols) < 12:
            continue
        txt = cols[11].strip()
        if not txt:
            continue
        try:
            conf = float(cols[10]) / 100.0
        except ValueError:
            conf = 0.0
        lines.append({"text": _strip_noise(txt), "score": round(conf, 4)})
        confs.append(conf)
    if not lines:
        return None
    raw = " ".join(l["text"] for l in lines)
    return {
        "lines": lines,
        "text": normalize(raw),
        "raw": raw,
        "confidence": round(sum(confs) / len(confs), 4),
        "backend": "tesseract",
        "available": True,
    }


def available() -> list[str]:
    out = []
    try:
        import rapidocr_onnxruntime  # noqa: F401
        out.append("rapidocr")
    except Exception:
        pass
    if shutil.which("tesseract"):
        out.append("tesseract")
    return out


def ocr_rect(image_path, rect, scale: int = 2, backend: str = "auto") -> dict:
    im = _load_crop(image_path, rect, scale)
    order = [backend] if backend != "auto" else ["rapidocr", "tesseract"]
    for name in order:
        if name == "rapidocr":
            r = _rapidocr_crop(im)
        elif name == "tesseract":
            r = _tesseract_crop(im)
        else:
            r = None
        if r:
            r["rect"] = list(rect)
            return r
    return {"text": "", "raw": "", "lines": [], "confidence": None,
            "backend": backend, "available": False, "rect": list(rect)}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--rect", required=True, help="x,y,w,h")
    ap.add_argument("--scale", type=int, default=2)
    ap.add_argument("--backend", default="auto", choices=["auto", "rapidocr", "tesseract"])
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    x, y, w, h = (int(v) for v in a.rect.split(","))
    r = ocr_rect(a.image, (x, y, w, h), a.scale, a.backend)
    print(json.dumps(r, ensure_ascii=False, indent=2) if a.json else r["text"])


if __name__ == "__main__":
    main()
