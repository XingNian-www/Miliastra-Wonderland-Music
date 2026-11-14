#!/usr/bin/env python3
"""把文字类 UI 模板的按钮底/胶囊底透明化，只保留字形（软 matte）。

与 `recrop-templates.py` 的分工：
- 本脚本只处理**以文字内容为判据**的模板；图标/色块模板仍由 `recrop-templates.py`
  负责（该脚本会跳过本脚本的清单，避免互相覆盖）。
- 只改 alpha + 按字形 bbox 裁剪 canvas；不缩放、不合成、不手绘。
- 裁剪会改变模板原点：新 canvas 从原图的 `crop=(x0,y0,x1,y1)` 开始，默认四周留
  `CROP_PADDING=2px` 透明 padding。**生产搜索区必须同步平移**：旧命中的模板左上角
  `P` 在新坐标里是 `P + (x0, y0)`，区域要围绕该位置留给定位余量，不能只把旧区域
  宽高改小。纯文字清单与匹配器 `TEXT_TEMPLATE_FILE_NAMES` 不变。

为什么不再用“阈值 + 腐蚀”：
- 硬阈值会在笔画内部留下 0/255 交替的孔洞和锯齿；1px 腐蚀又会把细笔画啃掉，
  深色字/浅色字的残边表现还不一致。

现在的做法（软 matte）：
1. 局部背景：形态学闭/开运算取先验，再用**归一化卷积**（只用非字形像素）估出
   每像素底色；小高斯半径能跟住按钮底上的阴影/渐变，不会把阴影误当笔画。
2. 覆盖率：alpha = 像素相对背景、沿“字色→背景”方向的投影（0–1 连续）。彩色
   图标/蓝色描边偏离这个方向，按残差衰减（gate），不会留下彩色残边。
3. 连通域：先按“高度接近字高”挑锚点，再按行聚类出文字行框；行框内的所有连通域
   （偏旁、点、被 JPEG 噪点切开的笔画）全部保留，行框外的图标/胶囊描边/分隔线
   全部清掉。
4. 残影抬升：alpha < 0.45 的投影阴影/辉光压到 0（连续映射，不是二值化），
   0.45–0.92 保持连续，只把笔画核心抬到 1。
5. 针孔闭合：3×3 闭运算只补 <2px 的针孔，真正的字腔（回/口/日）保持透明。
6. RGB 原样保留：生产匹配器的 SAD 直接比较 RGB 灰度，2×/4× 降采样又用同一个
   Triangle 滤波器缩 RGB 与 alpha；把半透明像素改成纯字色会让模板灰度偏暗，
   正样本分数掉 0.02–0.12。字形外观只由 alpha 决定，RGB 不参与透明合成。

用法：
    python3 scripts/pure-text-templates.py                     # 写 assets/ 与 JSON
    python3 scripts/pure-text-templates.py --dry-run           # 只打印 bbox/像素统计
    python3 scripts/pure-text-templates.py --review DIR        # 另外渲染绿底复核图

复核图：每个模板一行，BEFORE=原始截图裁剪，AFTER=新模板合成在纯绿 (0,190,80) 上，
放大 6–8 倍（最近邻），标注模板名/canvas/bbox/非透明像素。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import cv2
import numpy as np
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
ORIGINALS = (
    ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery" / "template-originals"
)
REPORT = (
    ROOT
    / "technical-docs"
    / "ui-state-evidence"
    / "evidence-gallery"
    / "pure-text-templates.json"
)

DARK = "dark"
LIGHT = "light"

# 每个模板的参数：
# - mode   : 深色字（暗前景/亮底）或浅色字（亮前景/暗底），决定背景估计的形态学方向。
# - clear  : 原图坐标矩形 (x, y, w, h)，排除按钮左侧图标/热键等非文字区域。
# - roi    : 可选，原图坐标文字区 (x0, y0, x1, y1)；默认由连通域自动聚类。
# - kernel : 背景估计的先验形态学核（应大于最粗笔画，默认 15）。
# - sigma  : 背景归一化卷积的高斯半径（默认 4.0，小半径能跟住底部的阴影/渐变）。
CONFIG: dict[str, dict] = {
    "ui-main-menu-return": {"mode": DARK},
    # pad=(x,y)：在字形 bbox 外保留的透明像素数。默认 (2,2)；
    # 降采样模板按 `pad ≡ 裁剪原点 (mod 下采样倍数)` 选择，保证模板与画面
    # 用同一采样相位（否则 2×/4× 下正样本会掉 0.03–0.10）。见文件末尾「裁剪与搜索区」。
    "invite-enter-hall": {"mode": LIGHT, "clear": [(0, 0, 120, 68)], "pad": (3, 3)},
    "invite-goto-hall": {"mode": DARK},
    "invite-view-star": {"mode": DARK, "pad": (3, 3)},
    "friend-block-chat": {"mode": DARK, "pad": (3, 3)},
    "friend-blacklist": {"mode": DARK},
    "confirm-button": {"mode": LIGHT, "clear": [(0, 0, 80, 82)]},
    "friend-search-panel": {"mode": DARK},
    # 确认弹窗文案锚点：只裁提示文案里的关键词（进入大厅 / 离开大厅 / 屏蔽玩家），
    # 不含整句、标点与弹窗边框；仍用默认 2px 透明 padding 贴 bbox。
    # 2× 下采样模板：canvas 的绝对原点与宽高都取偶数，保证与生产搜索区
    # （同样偶数原点）使用同一采样相位；否则自匹配会掉到 0.90–0.96。
    "confirm-message-random-hall": {"mode": DARK, "crop": (0, 1, 170, 33)},
    "confirm-message-return-teyvat": {"mode": DARK, "crop": (1, 1, 143, 33)},
    "confirm-message-block": {"mode": DARK, "crop": (0, 1, 96, 37)},
    "qianxing-f2-hall-list": {"mode": DARK, "clear": [(0, 0, 78, 36)], "pad": (3, 3)},
    "qianxing-f2-invite-player": {"mode": DARK, "clear": [(0, 0, 120, 50)], "pad": (3, 3)},
    # 退出菜单只认第一行「退出至登录界面」：roi 限定在第一行胶囊内，
    # 第二行「退出至桌面」不再参与；canvas 由字形 bbox + 2px 自动取得。
    "qianxing-exit-menu": {
        "mode": DARK,
        "clear": [(0, 0, 190, 230)],
        "roi": (196, 44, 396, 80),
    },
    # 19-52-12 档案卡只有「查看资料」，与「前往其大厅」同 canvas 尺寸/中心。
    "invite-view-profile": {"mode": DARK},
    # 提瓦特资料卡右下角深色「添加好友」按钮：只服务「非好友」判据。
    "teyvat-profile-add-friend": {"mode": LIGHT},
    # ── 资料卡四分类（2026-11-13 用户提供的 1920×1080 实拍帧）──────────────
    # 提瓦特资料卡：左页上部「冒险等阶 / 世界等级」白色文字（绿色/米色底）。
    # 两者同时命中才算提瓦特资料卡；搜索区分别限定在左页对应行。
    "teyvat-profile-adventure-rank": {"mode": LIGHT, "pad": (2, 2), "core_ratio": 0.45},
    "teyvat-profile-world-level": {"mode": LIGHT, "pad": (2, 2)},
    # 千星资料卡：上部「奇域等级」白色文字（蓝紫渐变带），命中即千星资料卡。
    "qianxing-profile-domain-level": {"mode": LIGHT, "pad": (2, 2)},
    # 卡片下部白色胶囊：非好友「添加好友」/好友「查看资料」，都是深色字。
    "qianxing-profile-add-friend": {"mode": DARK, "pad": (2, 2)},
    # 千星「查看资料」直接复用既有 `invite-view-profile`（同版式、同分数）。
    # 个人资料页两个按钮：从真实帧按钮内裁字形（深色字 + 白色胶囊底），
    # 透明化后 canvas = 字形 bbox + 2px；模板原点按实测命中平移。
    "qianxing-profile-page-wishlist": {"mode": DARK},
    "qianxing-profile-page-wardrobe": {"mode": DARK},
}

# 残影抬升与饱和度上界：alpha < LEVELS_LO 的阴影/辉光压到 0，
# alpha >= LEVELS_HI 的笔画核心抬到 1；中间保持连续（抗锯齿不被二值化）。
# 0.45 是逐张实测的折中：再低会把游戏字体的投影阴影（覆盖率 0.2–0.4）留在
# 绿底上形成“重影”，再高会把细笔画的抗锯齿边缘啃掉。
LEVELS_LO = 0.45
LEVELS_HI = 0.92
# 裁剪时在字形 bbox 外保留的透明 padding（原始像素，不缩放）。
# 2px 足够让 2× 降采样时模板边缘不把笔画颜色钳到边界外，又不会留下可被误触的
# 大片空白；个别模板可在 CONFIG 里用 `pad` 覆盖。
CROP_PADDING = 2
# RGB 保持原图像素（与 `recrop-templates.py` 同一理由）：
# 生产匹配器直接比较「模板 RGB 灰度」与「画面灰度」，4×/2× 降采样又用同一个 Triangle
# 滤波器把 RGB 和 alpha 分开缩小。把半透明像素改成纯字色会让模板灰度系统性偏暗，
# 正样本分数掉 0.02–0.12（实测 `invite-goto-hall` 0.976→0.860），并让「近似按钮」
# 更容易压过真锚点。字形外观由 alpha 负责，RGB 不参与透明合成。
# 残影门限：与“字色-背景”方向偏离过大的像素（彩色图标/描边）按残差衰减。
GATE_LO_RATIO = 0.10
GATE_HI_RATIO = 0.26


def luma(rgb: np.ndarray) -> np.ndarray:
    return rgb @ np.array([0.299, 0.587, 0.114], dtype=np.float32)


def morph_prior(rgb: np.ndarray, mode: str, kernel: int) -> np.ndarray:
    """形态学先验背景：暗字用闭运算、亮字用开运算，抹掉笔画留底色。"""
    shape = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (kernel, kernel))
    op = cv2.MORPH_CLOSE if mode == DARK else cv2.MORPH_OPEN
    return cv2.morphologyEx(rgb.astype(np.uint8), op, shape).astype(np.float32)


def smooth_background(
    rgb: np.ndarray, glyph: np.ndarray, prior: np.ndarray, sigma: float
) -> np.ndarray:
    """归一化卷积：只用非字形像素估局部底色，字形位置由邻域插值。

    相比大核中值/闭运算，小 sigma 能跟住按钮底上的阴影和渐变，避免把阴影当成
    半透明的“残影”；字形像素被 mask 掉，不会把笔画颜色带进背景。
    """
    mask = (~glyph).astype(np.float32)
    num = cv2.GaussianBlur(rgb * mask[..., None], (0, 0), sigma)
    den = cv2.GaussianBlur(mask, (0, 0), sigma)
    bg = num / np.maximum(den[..., None], 1e-3)
    weak = den < 0.06
    if weak.any():
        bg[weak] = prior[weak]
    return bg


def tone(rgb: np.ndarray, bg: np.ndarray, mode: str) -> np.ndarray:
    delta = luma(rgb - bg)
    return -delta if mode == DARK else delta


def local_background(
    rgb: np.ndarray, mode: str, kernel: int, valid: np.ndarray, sigma: float
) -> tuple[np.ndarray, np.ndarray, float]:
    """迭代两次：先粗估字形 mask，再用更完整的 mask 重估背景。"""
    prior = morph_prior(rgb, mode, kernel)
    glyph = np.zeros(rgb.shape[:2], bool)
    bg = prior
    for _ in range(2):
        score = tone(rgb, bg, mode)
        full = float(np.percentile(score[valid], 99.0))
        mask = ((score > 0.45 * full) & valid).astype(np.uint8)
        glyph = cv2.dilate(mask, np.ones((3, 3), np.uint8)) > 0
        bg = smooth_background(rgb, glyph, prior, sigma)
    score = tone(rgb, bg, mode)
    return bg, score, float(np.percentile(score[valid], 99.0))


def text_components(
    core: np.ndarray, width: int, height: int
) -> tuple[np.ndarray, float, list[tuple[int, int, int, int]], list[tuple]]:
    """保留文字字形（含被拆开的偏旁/点），丢掉图标、描边、分隔线。

    1. 高度参考 H：按面积加权中位，排除横贯的长条；
    2. 锚点：高度接近 H、不是长条、不贴边成细条（胶囊描边/场景边界）；
    3. 按行聚类锚点得到文字行框；
    4. 行框内的连通域全部保留——偏旁、点、被 JPEG 噪点切开的笔画都能回来。
    """
    count, labels, stats, _ = cv2.connectedComponentsWithStats(core.astype(np.uint8), 8)
    comps = []
    for index in range(1, count):
        x, y, w, h, area = (int(v) for v in stats[index])
        if area >= 4:
            comps.append({"id": index, "x": x, "y": y, "w": w, "h": h, "area": area})
    if not comps:
        return np.zeros((height, width), np.uint8), 0.0, [], []

    heights: list[int] = []
    for comp in comps:
        if comp["w"] <= 3.0 * comp["h"] and comp["area"] >= 20:
            heights.extend([comp["h"]] * comp["area"])
    ref = float(np.median(heights)) if heights else float(np.median([c["h"] for c in comps]))

    anchors = []
    for comp in comps:
        if (
            comp["w"] >= 0.30 * width
            or comp["h"] >= 1.7 * ref
            or comp["area"] >= 0.25 * width * height
            or comp["h"] < 4
        ):
            continue
        if (comp["x"] == 0 or comp["x"] + comp["w"] == width) and comp["w"] < 0.5 * comp["h"]:
            continue
        if not (0.55 * ref <= comp["h"] <= 1.35 * ref):
            continue
        anchors.append(comp)
    if not anchors:
        return np.zeros((height, width), np.uint8), ref, [], []

    rows: list[dict] = []
    for comp in sorted(anchors, key=lambda c: c["y"] + c["h"] / 2):
        center = comp["y"] + comp["h"] / 2
        for row in rows:
            if abs(row["center"] - center) <= 0.6 * ref:
                row["items"].append(comp)
                break
        else:
            rows.append({"center": center, "items": [comp]})

    margin = max(3, int(round(0.12 * ref)))
    boxes = []
    for row in rows:
        x0 = min(c["x"] for c in row["items"])
        x1 = max(c["x"] + c["w"] for c in row["items"])
        y0 = min(c["y"] for c in row["items"])
        y1 = max(c["y"] + c["h"] for c in row["items"])
        boxes.append(
            (max(0, x0 - margin), max(0, y0 - margin), min(width, x1 + margin), min(height, y1 + margin))
        )

    keep = np.zeros((height, width), np.uint8)
    for comp in comps:
        cx = comp["x"] + comp["w"] / 2
        cy = comp["y"] + comp["h"] / 2
        if any(x0 <= cx <= x1 and y0 <= cy <= y1 for x0, y0, x1, y1 in boxes):
            keep[labels == comp["id"]] = 1
    return keep, ref, boxes, anchors


def glyph_alpha(name: str, rgb: np.ndarray) -> tuple[np.ndarray, dict]:
    cfg = CONFIG[name]
    mode = cfg["mode"]
    kernel = int(cfg.get("kernel", 15))
    sigma = float(cfg.get("sigma", 4.0))
    height, width = rgb.shape[:2]

    valid = np.ones((height, width), bool)
    for x, y, w, h in cfg.get("clear", []):
        valid[y : y + h, x : x + w] = False
    roi = cfg.get("roi")
    if roi is not None:
        x0, y0, x1, y1 = roi
        inside = np.zeros((height, width), bool)
        inside[y0:y1, x0:x1] = True
        valid &= inside

    bg, score, full = local_background(rgb, mode, kernel, valid, sigma)
    # 字形核心阈值默认 0.5×满分。个别白字（如「冒险等阶」的“冒”）笔画中间有 1px
    # 暗缝，0.5 会把字拆成上下两块、连行框都选不进去；该模板在 CONFIG 里单独降到
    # 0.45 即可把整字并回同一连通域，其余模板保持原语义。
    core = (score > float(cfg.get("core_ratio", 0.5)) * full) & valid
    keep, ref, boxes, anchors = text_components(core, width, height)
    if keep.sum() == 0:
        raise SystemExit(f"{name}: 没有找到字形连通域")
    support = cv2.dilate(keep, cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (7, 7))) > 0
    support &= valid

    # 字色：字形核心内的高覆盖像素中位数（深/浅字都适用）。
    ink_mask = keep.astype(bool) & (score > 0.85 * full)
    ink = np.median(rgb[ink_mask], axis=0)

    # 软 matte：alpha = 像素相对背景沿“字色-背景”方向的投影，残差大的彩色装饰衰减。
    delta = rgb - bg
    fg = ink - bg
    den = np.maximum((fg * fg).sum(axis=2), 1.0)
    alpha = np.clip((delta * fg).sum(axis=2) / den, 0.0, 1.0)
    residual = np.sqrt(((delta - alpha[..., None] * fg) ** 2).sum(axis=2))
    norm = np.sqrt(den)
    gate_lo = np.maximum(12.0, GATE_LO_RATIO * norm)
    gate_hi = np.maximum(32.0, GATE_HI_RATIO * norm)
    gate = np.clip((gate_hi - residual) / np.maximum(gate_hi - gate_lo, 1e-3), 0.0, 1.0)
    alpha = alpha * gate

    # 残影抬升（连续映射，非二值化）：扣掉字形外圈阴影/辉光。
    alpha = np.clip((alpha - LEVELS_LO) / (LEVELS_HI - LEVELS_LO), 0.0, 1.0)
    alpha = np.where(support, alpha, 0.0)
    alpha[alpha < 0.02] = 0.0
    # 针孔闭合：只补 <2px 的洞，字腔（回/口/日）保持透明。
    closed = cv2.morphologyEx(alpha, cv2.MORPH_CLOSE, np.ones((3, 3), np.uint8))
    alpha = np.maximum(alpha, closed * (alpha > 0.35))
    alpha = np.where(support, alpha, 0.0)

    info = {
        "mode": mode,
        "kernel": kernel,
        "sigma": sigma,
        "ink_rgb": [int(round(v)) for v in ink],
        "components_kept": len(anchors),
        "rows": [list(box) for box in boxes],
        "glyph_height": round(ref, 1),
        "levels": [LEVELS_LO, LEVELS_HI],
    }
    return alpha, info


def run(name: str, dry_run: bool) -> dict:
    source = ORIGINALS / f"{name}.png"
    rgb = np.asarray(Image.open(source).convert("RGB")).astype(np.float32)
    alpha, info = glyph_alpha(name, rgb)
    alpha_bytes = np.round(alpha * 255).astype(np.uint8)
    ys, xs = np.where(alpha_bytes > 0)
    if len(xs) == 0:
        raise SystemExit(f"{name}: 透明化后没有剩余像素")
    bbox = [int(xs.min()), int(ys.min()), int(xs.max()) + 1, int(ys.max()) + 1]
    height, width = alpha_bytes.shape
    # 裁到字形 bbox + 少量透明 padding：原按钮/面板 canvas 的大片透明边不再参与匹配。
    # padding 必须是原始像素（RGB=原背景色、alpha=0），这样 2×/4× 降采样时模板边缘
    # 与画面边缘的 Triangle 采样口径一致，不会把笔画颜色夹到边界外。
    pad_x, pad_y = CONFIG[name].get("pad", (CROP_PADDING, CROP_PADDING))
    # 少数 4× 模板需要显式裁剪框：让 canvas 宽高都是下采样倍数的整数倍，
    # 否则 `resize_exact(w/s)` 向下取整会引入非整数缩放，自匹配分数从 1.0 掉到 0.95。
    if "crop" in CONFIG[name]:
        x0, y0, x1, y1 = CONFIG[name]["crop"]
    else:
        x0 = max(0, bbox[0] - pad_x)
        y0 = max(0, bbox[1] - pad_y)
        x1 = min(width, bbox[2] + pad_x)
        y1 = min(height, bbox[3] + pad_y)
    rgba = np.dstack([rgb.astype(np.uint8), alpha_bytes])[y0:y1, x0:x1]
    # bbox 记录在**裁剪后 canvas** 里的位置，方便复核图与生产搜索区对齐。
    cropped_bbox = [bbox[0] - x0, bbox[1] - y0, bbox[2] - x0, bbox[3] - y0]
    new_height, new_width = rgba.shape[:2]
    output = ASSETS / f"{name}.png"
    # RGB 原样保留：匹配器按 RGB 灰度算 SAD，改色会改变 2×/4× 降采样模板的灰度。
    # alpha=0 的像素不参与评分也不可见，同样保留原值，避免额外分支。
    opaque = rgba[..., 3] > 0
    if not dry_run:
        Image.fromarray(rgba, "RGBA").save(output, optimize=True)
    return {
        "template": name,
        "source": str(source.relative_to(ROOT)),
        "width": new_width,
        "height": new_height,
        "old_width": width,
        "old_height": height,
        "crop": [x0, y0, x1, y1],
        "padding": [bbox[0] - x0, bbox[1] - y0, x1 - bbox[2], y1 - bbox[3]],
        "bbox": cropped_bbox,
        "nontransparent": int(opaque.sum()),
        "opaque": int((rgba[..., 3] == 255).sum()),
        "partial": int(((rgba[..., 3] > 0) & (rgba[..., 3] < 255)).sum()),
        "alpha_sum": int(rgba[..., 3].astype(np.uint64).sum()),
        **info,
    }


# ---------------------------------------------------------------------------
# 绿底复核图（BEFORE=原图裁剪，AFTER=新模板合成在纯绿上）
# ---------------------------------------------------------------------------

GREEN = (0, 190, 80)
SHEET_TEMPLATES = 4


CJK_FONT_CANDIDATES = (
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simhei.ttf",
    "/System/Library/Fonts/PingFang.ttc",
)


def review_font(size: int) -> tuple[object, bool]:
    """复核图字体：优先能显示中文的字体；找不到时退回英文标注（避免豆腐块）。"""
    for candidate in CJK_FONT_CANDIDATES:
        if Path(candidate).exists():
            try:
                return ImageFont.truetype(candidate, size), True
            except OSError:
                continue
    try:
        return ImageFont.load_default(size=size), False
    except TypeError:
        return ImageFont.load_default(), False


def zoom_for(width: int, height: int) -> int:
    scale = 8
    while scale > 6 and max(width, height) * scale > 1800:
        scale -= 1
    return scale


def compose_after(rgb: np.ndarray, alpha_bytes: np.ndarray) -> np.ndarray:
    alpha = alpha_bytes.astype(np.float32)[..., None] / 255.0
    ink = np.zeros_like(rgb)
    ink[..., 0], ink[..., 1], ink[..., 2] = GREEN
    return (rgb * alpha + ink * (1.0 - alpha) + 0.5).astype(np.uint8)


def render_review(report: list[dict], out_dir: Path) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    paths = []
    for sheet_index in range(0, len(report), SHEET_TEMPLATES):
        chunk = report[sheet_index : sheet_index + SHEET_TEMPLATES]
        rows = []
        for entry in chunk:
            name = entry["template"]
            source = np.asarray(Image.open(ORIGINALS / f"{name}.png").convert("RGB"))
            rgba = np.asarray(Image.open(ASSETS / f"{name}.png").convert("RGBA"))
            after = compose_after(rgba[..., :3].astype(np.float32), rgba[..., 3])
            scale = zoom_for(entry["width"], entry["height"])
            # BEFORE 显示与 AFTER 同一个裁剪窗口，两边才能逐像素对照。
            x0, y0, x1, y1 = entry["crop"]
            before_img = Image.fromarray(source[y0:y1, x0:x1]).resize(
                (entry["width"] * scale, entry["height"] * scale), Image.NEAREST
            )
            after_img = Image.fromarray(after).resize(
                (entry["width"] * scale, entry["height"] * scale), Image.NEAREST
            )
            rows.append((entry, scale, before_img, after_img))

        pad = 24
        caption_h = 40
        header_font, cjk = review_font(30)
        caption_font, _ = review_font(24)
        label_font, _ = review_font(22)
        cell_w = max(max(before.width for _, _, before, _ in rows), max(after.width for _, _, _, after in rows))
        sheet_w = pad * 2 + cell_w * 2 + pad
        header_h = 52
        sheet_h = pad + header_h
        for _, _, before, _ in rows:
            sheet_h += caption_h + before.height + pad
        sheet = Image.new("RGB", (sheet_w, sheet_h), (18, 18, 20))
        draw = ImageDraw.Draw(sheet)
        sheet_no = sheet_index // SHEET_TEMPLATES + 1
        sheet_total = max(1, (len(report) + SHEET_TEMPLATES - 1) // SHEET_TEMPLATES)
        header = (
            f"纯文字模板 绿底复核 {sheet_no}/{sheet_total}"
            "（绿底=透明区域；BEFORE=原始截图裁剪，AFTER=新模板）"
            if cjk
            else f"Pure-text templates on green {sheet_no}/{sheet_total} "
            "(green = transparent; BEFORE = original crop, AFTER = new template)"
        )
        draw.text((pad, 10), header, fill=(255, 255, 255), font=header_font)
        y = pad + header_h
        for entry, scale, before, after in rows:
            bbox = entry["bbox"]
            canvas = (
                f"{entry['old_width']}x{entry['old_height']} → "
                f"{entry['width']}x{entry['height']}"
            )
            if cjk:
                caption = (
                    f"{entry['template']}   canvas {canvas}   "
                    f"crop ({entry['crop'][0]}, {entry['crop'][1]})   "
                    f"bbox ({bbox[0]}, {bbox[1]}, {bbox[2]}, {bbox[3]})   "
                    f"非透明 {entry['nontransparent']} px / 不透明 {entry['opaque']} px   "
                    f"x{scale}   {entry['mode']}"
                )
            else:
                caption = (
                    f"{entry['template']}   canvas {canvas}   "
                    f"crop ({entry['crop'][0]}, {entry['crop'][1]})   "
                    f"bbox ({bbox[0]}, {bbox[1]}, {bbox[2]}, {bbox[3]})   "
                    f"{entry['nontransparent']} px / {entry['opaque']} opaque   "
                    f"x{scale}   {entry['mode']}"
                )
            draw.text((pad, y), caption, fill=(255, 230, 140), font=caption_font)
            y += caption_h - 14
            draw.text((pad, y), "BEFORE", fill=(150, 150, 160), font=label_font)
            draw.text((pad + cell_w + pad, y), "AFTER", fill=(150, 230, 160), font=label_font)
            y += 22
            sheet.paste(before, (pad, y))
            sheet.paste(after, (pad + cell_w + pad, y))
            y += max(before.height, after.height) + pad
        path = out_dir / f"pure-text-green-{sheet_index // SHEET_TEMPLATES + 1}.png"
        sheet.save(path)
        paths.append(path)
    return paths


def main() -> int:
    dry_run = "--dry-run" in sys.argv
    review_dir = None
    if "--review" in sys.argv:
        review_dir = Path(sys.argv[sys.argv.index("--review") + 1]).resolve()
    report = [run(name, dry_run) for name in CONFIG]
    if not dry_run:
        REPORT.write_text(
            json.dumps(report, ensure_ascii=False, indent=1) + "\n", encoding="utf-8"
        )
        if review_dir is not None:
            for path in render_review(report, review_dir):
                print(f"review: {path.relative_to(ROOT)}")
    print(json.dumps(report, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
