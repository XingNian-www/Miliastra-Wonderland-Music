#!/usr/bin/env python3
"""把 UI 模板的“多余背景”透明化（只改 alpha，不改尺寸/RGB）。

为什么只改 alpha：
- 生产匹配器 `src/ui/template.rs` 的 `GrayMatcher` 按 alpha 加权 SAD，
  分母是 Σalpha；alpha=0 的像素完全不参与比较。
- 保留原 RGB 是为了让 4× 降采样模板的灰度边缘仍接近真实画面（灰度与 alpha
  是用同一 Triangle 滤波器分开缩小的），避免透明填充色在边缘引入偏色。
- 尺寸不变，因此所有按模板宽高写死的搜索区/偏移都不需要改。

用法：
    python3 scripts/recrop-templates.py            # 生成到 assets/
    python3 scripts/recrop-templates.py --dry-run  # 只打印统计

原图备份在 technical-docs/ui-state-evidence/evidence-gallery/template-originals/。
"""

from __future__ import annotations

import json
import shutil
import sys
from collections import deque
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
ORIGINALS = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery" / "template-originals"


# ---------------------------------------------------------------------------
# 基础图像操作
# ---------------------------------------------------------------------------

def load_rgb(name: str) -> np.ndarray:
    return np.asarray(Image.open(ASSETS / name).convert("RGB")).astype(np.int16)


def distance_to(rgb: np.ndarray, ref: tuple[int, int, int]) -> np.ndarray:
    ref_array = np.array(ref, dtype=np.int32)
    return np.sqrt(((rgb.astype(np.int32) - ref_array) ** 2).sum(axis=2))


def min_distance(rgb: np.ndarray, refs) -> np.ndarray:
    return np.min(np.stack([distance_to(rgb, ref) for ref in refs], axis=0), axis=0)


def luma(rgb: np.ndarray) -> np.ndarray:
    return rgb @ np.array([0.299, 0.587, 0.114])


def ramp(values: np.ndarray, low: float, high: float) -> np.ndarray:
    return np.clip((values - low) / max(high - low, 1e-6), 0.0, 1.0)


def remove_by_color(rgb: np.ndarray, refs, low: float, high: float) -> np.ndarray:
    """颜色接近 refs 的像素 alpha 低；距离 low→0、high→1 线性过渡。"""
    return ramp(min_distance(rgb, refs), low, high)


def label_components(mask: np.ndarray) -> list[np.ndarray]:
    """4 连通分量，返回每个分量的布尔掩码（只保留面积 ≥ 3 的）。"""
    h, w = mask.shape
    seen = np.zeros_like(mask)
    components = []
    for y in range(h):
        for x in range(w):
            if not mask[y, x] or seen[y, x]:
                continue
            queue = deque([(y, x)])
            seen[y, x] = True
            points = []
            while queue:
                cy, cx = queue.popleft()
                points.append((cy, cx))
                for ny, nx in ((cy - 1, cx), (cy + 1, cx), (cy, cx - 1), (cy, cx + 1)):
                    if 0 <= ny < h and 0 <= nx < w and mask[ny, nx] and not seen[ny, nx]:
                        seen[ny, nx] = True
                        queue.append((ny, nx))
            if len(points) >= 3:
                component = np.zeros_like(mask)
                ys = np.array([point[0] for point in points])
                xs = np.array([point[1] for point in points])
                component[ys, xs] = True
                components.append(component)
    return components


def fill_holes(mask: np.ndarray) -> np.ndarray:
    """填掉被 mask 完全包围的洞（文字镂空），保留外部背景为 False。"""
    h, w = mask.shape
    outside = np.zeros_like(mask)
    queue = deque()
    for y in range(h):
        for x in (0, w - 1):
            if not mask[y, x] and not outside[y, x]:
                outside[y, x] = True
                queue.append((y, x))
    for x in range(w):
        for y in (0, h - 1):
            if not mask[y, x] and not outside[y, x]:
                outside[y, x] = True
                queue.append((y, x))
    while queue:
        cy, cx = queue.popleft()
        for ny, nx in ((cy - 1, cx), (cy + 1, cx), (cy, cx - 1), (cy, cx + 1)):
            if 0 <= ny < h and 0 <= nx < w and not mask[ny, nx] and not outside[ny, nx]:
                outside[ny, nx] = True
                queue.append((ny, nx))
    return ~outside


def regions_from_color(rgb: np.ndarray, refs, tol: float, min_area: int = 200) -> np.ndarray:
    """把接近 refs 的颜色块填成实心区域（按钮/圆钮的本体），用于圈定可保留内容。"""
    mask = min_distance(rgb, refs) < tol
    region = np.zeros_like(mask)
    for component in label_components(mask):
        if component.sum() < min_area:
            continue
        region |= fill_holes(component)
    return region


def border_flood_bright(rgb: np.ndarray, refs, tol: float = 30.0, step: float = 10.0) -> np.ndarray:
    """只从“亮色边界像素”向内泛洪，返回背景布尔掩码。

    角色图（paimon）会顶到边界；只用亮色边界作种子，避免把角色当成背景吃掉。
    """
    h, w, _ = rgb.shape
    mask = np.zeros((h, w), dtype=bool)
    queue: deque[tuple[int, int]] = deque()
    ref_array = np.array(refs, dtype=np.int16)
    for y in range(h):
        for x in range(w):
            if y not in (0, h - 1) and x not in (0, w - 1):
                continue
            pixel = rgb[y, x]
            if np.abs(ref_array - pixel).sum(axis=1).min() <= tol * 3:
                mask[y, x] = True
                queue.append((y, x))
    while queue:
        y, x = queue.popleft()
        current = rgb[y, x]
        for ny, nx in ((y - 1, x), (y + 1, x), (y, x - 1), (y, x + 1)):
            if ny < 0 or ny >= h or nx < 0 or nx >= w or mask[ny, nx]:
                continue
            pixel = rgb[ny, nx]
            local = int(np.abs(pixel - current).sum())
            ref_dist = int(np.abs(ref_array - pixel).sum(axis=1).min())
            if local <= step * 3 or ref_dist <= tol * 3:
                mask[ny, nx] = True
                queue.append((ny, nx))
    return mask


def border_flood(rgb: np.ndarray, tol: float = 26.0, step: float = 12.0) -> np.ndarray:
    """从四边向内泛洪，返回“与边界连通的背景”布尔掩码。"""
    h, w, _ = rgb.shape
    border = np.concatenate(
        [rgb[0].reshape(-1, 3), rgb[-1].reshape(-1, 3), rgb[:, 0].reshape(-1, 3), rgb[:, -1].reshape(-1, 3)]
    )
    quantized = (border // 24) * 24
    values, counts = np.unique(quantized, axis=0, return_counts=True)
    order = np.argsort(-counts)[:8]
    refs = [tuple(int(v) for v in values[i]) for i in order]
    refs_array = np.array(refs, dtype=np.int16)

    mask = np.zeros((h, w), dtype=bool)
    queue: deque[tuple[int, int]] = deque()
    for x in range(w):
        for y in (0, h - 1):
            mask[y, x] = True
            queue.append((y, x))
    for y in range(h):
        for x in (0, w - 1):
            mask[y, x] = True
            queue.append((y, x))
    while queue:
        y, x = queue.popleft()
        current = rgb[y, x]
        for ny, nx in ((y - 1, x), (y + 1, x), (y, x - 1), (y, x + 1)):
            if ny < 0 or ny >= h or nx < 0 or nx >= w or mask[ny, nx]:
                continue
            pixel = rgb[ny, nx]
            local = int(np.abs(pixel - current).sum())
            ref_dist = int(np.abs(refs_array - pixel).sum(axis=1).min())
            if local <= step * 3 or ref_dist <= tol * 3:
                mask[ny, nx] = True
                queue.append((ny, nx))
    return mask


def erode(mask: np.ndarray, iterations: int = 1) -> np.ndarray:
    result = mask.copy()
    for _ in range(iterations):
        padded = np.pad(result, 1, constant_values=False)
        result = (
            padded[:-2, 1:-1]
            & padded[2:, 1:-1]
            & padded[1:-1, :-2]
            & padded[1:-1, 2:]
            & padded[1:-1, 1:-1]
        )
    return result


def clear_rect(alpha: np.ndarray, x: int, y: int, w: int, h: int) -> None:
    alpha[max(y, 0) : y + h, max(x, 0) : x + w] = 0.0


def radial_keep(alpha: np.ndarray, cx: float, cy: float, radius: float, feather: float = 1.0) -> np.ndarray:
    h, w = alpha.shape
    yy, xx = np.mgrid[0:h, 0:w]
    dist = np.sqrt((xx - cx) ** 2 + (yy - cy) ** 2)
    return alpha * np.clip((radius - dist) / max(feather, 1e-6) + 0.5, 0.0, 1.0)


def rounded_chip(alpha: np.ndarray, width: int, height: int, inset: float = 1.5) -> np.ndarray:
    """保留圆角矩形键帽本体，去掉四角场景/底色。"""
    if width < 2 * inset + 2 or height < 2 * inset + 2:
        return alpha
    x0, y0 = inset, inset
    x1, y1 = width - 1 - inset, height - 1 - inset
    radius = min(x1 - x0, y1 - y0) / 2
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    return radial_keep(alpha, cx, cy, radius + 1.0)


def keep_bright(rgb: np.ndarray, low: float, high: float) -> np.ndarray:
    return ramp(luma(rgb), low, high)


def keep_dark(rgb: np.ndarray, low: float, high: float) -> np.ndarray:
    return 1.0 - ramp(luma(rgb), low, high)


def text_on_bright_plate(rgb, plate_refs, plate_tol=26, low=8, high=30):
    """亮色底/胶囊 + 深色文字：去掉底色，保留文字与彩色图标。"""
    return remove_by_color(rgb, plate_refs, low, high)


def bright_content_inside_dark_button(rgb, dark_refs, dark_tol=40, bright_low=110, bright_high=170):
    """深色按钮 + 亮色文字/圆环：用深色填色块圈出按钮本体，只保留本体里的亮色内容。"""
    region = regions_from_color(rgb, dark_refs, dark_tol, min_area=500)
    return region.astype(np.float32) * keep_bright(rgb, bright_low, bright_high)


def dark_content_inside_plate(rgb, plate_refs, plate_tol=26, dark_low=100, dark_high=150):
    """亮色圆钮/胶囊 + 深色图标：只保留圆钮本体里的深色图标。"""
    region = regions_from_color(rgb, plate_refs, plate_tol, min_area=300)
    return region.astype(np.float32) * keep_dark(rgb, dark_low, dark_high)


def cream_content_inside_plate(rgb, plate_refs, plate_tol=26, low=8, high=34):
    """亮色胶囊里只保非底色内容（深色字、彩色图标）。

    区域先向内收 1px：胶囊自身有一圈抗锯齿描边，不收掉会留下按钮轮廓。
    """
    region = regions_from_color(rgb, plate_refs, plate_tol, min_area=300)
    region = erode(region, 1)
    alpha = remove_by_color(rgb, plate_refs, low, high)
    return region.astype(np.float32) * alpha


# ---------------------------------------------------------------------------
# 逐模板策略
# ---------------------------------------------------------------------------

# 文字内容模板由 `scripts/pure-text-templates.py` 统一生成（只保留字形，并裁到
# 字形 bbox + 小 padding）。本脚本跳过它们，避免把纯文字资产覆盖回“按钮面/描边”
# 版本；下面同名的例外分支是历史入口，已不可达，保留仅作记录。
PURE_TEXT = {
    "ui-main-menu-return.png",
    "invite-enter-hall.png",
    "invite-goto-hall.png",
    "invite-view-star.png",
    "invite-view-profile.png",
    "teyvat-profile-add-friend.png",
    "friend-block-chat.png",
    "friend-blacklist.png",
    "confirm-button.png",
    "friend-search-panel.png",
    "confirm-message-random-hall.png",
    "confirm-message-return-teyvat.png",
    "confirm-message-block.png",
    "qianxing-f2-hall-list.png",
    "qianxing-f2-invite-player.png",
    "qianxing-exit-menu.png",
    "teyvat-profile-adventure-rank.png",
    "teyvat-profile-world-level.png",
    "qianxing-profile-domain-level.png",
    "qianxing-profile-add-friend.png",
}

PLATE_WHITE = [(248, 248, 248), (240, 240, 240), (232, 232, 232), (236, 229, 216)]
PLATE_CREAM = [(240, 240, 240), (232, 224, 216), (224, 224, 208), (216, 216, 200)]

# 完全不改字节的模板：色块/内容信号、透明图标、以及保留原始按钮面的例外。
UNTOUCHED = {
    "chat-marker-blue.png",
    "chat-marker-pink.png",
    "chat-marker-yellow.png",
    "world-wish.png",
    "startup-wonderland-map-star.png",
    "startup-paimon-menu.png",
}


def build_alpha(name: str, rgb: np.ndarray) -> np.ndarray:
    h, w, _ = rgb.shape

    # 这些模板本身就是色块/内容信号，不能透明化（否则判据消失）。
    if name in {
        "chat-marker-blue.png",
        "chat-marker-pink.png",
        "chat-marker-yellow.png",
        "world-wish.png",
        "startup-wonderland-map-star.png",
    }:
        return np.ones((h, w), dtype=np.float32)

    # ── 好友覆盖层 ──────────────────────────────────────────────────────
    if name in {"friend-blacklist.png", "friend-search-panel.png"}:
        return text_on_bright_plate(rgb, PLATE_WHITE)

    if name == "friend-block-chat.png":
        # 例外：只留文字会把过渡帧 loading-screens/after-x.jpg 从 0.8987 抬到 0.9036
        # （阈值 0.90），凭空多出 panel:more_settings；奶白按钮面是该锚点必要的区分度。
        return np.ones((h, w), dtype=np.float32)

    if name == "friend-panel.png":
        # 奶白页面底 + 深色圆角图标；图标内部的人形与背景交换，靠连通性保留。
        kept = ~border_flood(rgb, tol=26, step=10)
        return erode(kept, 1).astype(np.float32)

    if name == "friend-more-settings.png":
        # 旧审计实测：整块白色圆钮与底色自匹配 0.906/0.929，区分度太低；
        # 只留三个深色点（圆钮/白底都透明化），自匹配降到 0.555/0.596。
        return radial_keep(np.ones((h, w), dtype=np.float32), (w - 1) / 2, (h - 1) / 2, min(w, h) / 2 - 0.5) * keep_dark(
            rgb, 60, 110
        )

    if name == "confirm-button.png":
        return bright_content_inside_dark_button(
            rgb, [(64, 80, 96), (56, 72, 88), (72, 88, 104)], dark_tol=40
        )

    # ── 邀请 / 大厅按钮 ────────────────────────────────────────────────
    if name == "invite-enter-hall.png":
        return bright_content_inside_dark_button(
            rgb, [(48, 64, 80), (56, 72, 88)], dark_tol=40
        )

    if name == "invite-goto-hall.png":
        # 例外：去掉胶囊底后「前往其大厅」在 Teyvat 档案卡 modI-2/recov-3 上
        # 从 0.930 掉到 0.753（阈值 0.90），档案卡会落 unknown；奶白胶囊底是该锚点
        # 的必要区分度，保留原始按钮面（主菜单负样本仍只有 0.792）。
        return np.ones((h, w), dtype=np.float32)

    if name == "invite-view-star.png":
        # 只留文字会与过渡帧/大厅卡 0.908–0.909 误命中（阈值 0.90）；
        # 保留胶囊的 2px 描边作为结构锚点，填充仍然透明。
        refs = [(208, 208, 192), (216, 216, 200)]
        alpha = cream_content_inside_plate(rgb, refs, plate_tol=28, low=10, high=34)
        region = regions_from_color(rgb, refs, 34, min_area=300)
        outline = region & ~erode(region, 2)
        return np.maximum(alpha, outline.astype(np.float32))

    # ── 千星页面 ───────────────────────────────────────────────────────
    if name == "qianxing-exit-menu.png":
        # 深色场景 + 两颗奶白胶囊：只留胶囊内部的深色文字/图标。
        return cream_content_inside_plate(rgb, [(232, 224, 216)], plate_tol=30, low=10, high=34)

    if name in {"qianxing-f2-hall-list.png", "qianxing-f2-invite-player.png"}:
        alpha = text_on_bright_plate(rgb, PLATE_WHITE, low=12, high=34)
        if name == "qianxing-f2-invite-player.png":
            # 左下角是 F 热键（含白色键帽），按钮锚点不需要它。
            clear_rect(alpha, 0, 31, 16, 19)
        return alpha

    if name == "qianxing-map-label.png":
        # 只留亮色文字 + 庙宇图标；深色地图底与红色未读角标全部透明。
        # 地图锚点走二值形状（亮度≥150），这里保留原 RGB，因此形状分不变。
        alpha = keep_bright(rgb, 150, 185)
        red = (rgb[:, :, 0].astype(np.int32) - np.maximum(rgb[:, :, 1], rgb[:, :, 2]) > 40) & (rgb[:, :, 0] > 150)
        alpha[red] = 0.0
        clear_rect(alpha, 264, 0, 32, 18)
        return alpha

    # ── 启动流程 ───────────────────────────────────────────────────────
    if name == "startup-paimon-menu.png":
        # 例外：这是角色碎片，白色不是可分离的页面底；去掉后启动阶段正样本
        # 从 >0.9 掉到 0.81，会漏掉“游戏已进主界面”。保留原样。
        return np.ones((h, w), dtype=np.float32)

    # ── 通用 UI 锚点 ──────────────────────────────────────────────────
    if name == "ui-main-menu-return.png":
        return text_on_bright_plate(rgb, PLATE_CREAM, low=10, high=32)

    if name == "ui-secondary-back.png":
        # 例外：奶白圆钮是按钮面，去掉后主菜单同款返回箭头会被 0.999 命中，
        # 把主菜单翻成二级聊天。只去掉圆外的深色场景四角。
        return radial_keep(np.ones((h, w), dtype=np.float32), (w - 1) / 2, (h - 1) / 2, min(w, h) / 2 - 0.5)

    if name == "ui-secondary-hall.png":
        # 深色方块底 → 白色庙宇图标。
        return keep_bright(rgb, 140, 190)

    if name == "ui-secondary-chat-submit.png":
        # 例外：奶白圆钮是按钮面。只留深色内圆+黄环会与提瓦特地图的黄圈路点
        # 0.9013 误命中（阈值 0.90），二级聊天判据被地图抢走。只去掉圆外四角。
        return radial_keep(np.ones((h, w), dtype=np.float32), (w - 1) / 2, (h - 1) / 2, min(w, h) / 2 - 0.5)

    raise SystemExit(f"没有为 {name} 定义裁剪策略")


# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------

def main() -> int:
    dry_run = "--dry-run" in sys.argv
    originals = sorted(ORIGINALS.glob("*.png"))
    if not originals:
        raise SystemExit(f"找不到原图备份目录: {ORIGINALS}")
    report = []
    for original in originals:
        name = original.name
        if name in PURE_TEXT:
            continue
        if name in UNTOUCHED:
            # 这些模板不能改：直接按原字节覆盖，连编码都不变。
            if not dry_run:
                shutil.copyfile(original, ASSETS / name)
            report.append(
                {
                    "template": name,
                    "width": Image.open(original).width,
                    "height": Image.open(original).height,
                    "opaque_fraction": None,
                    "alpha_sum": None,
                    "untouched": True,
                }
            )
            continue
        rgb = load_rgb(name)
        alpha = np.clip(build_alpha(name, rgb), 0.0, 1.0)
        alpha[alpha < 0.02] = 0.0
        opaque = float((alpha >= 0.5).mean())
        alpha_bytes = np.round(alpha * 255).astype(np.uint8)
        rgba = np.dstack([rgb.astype(np.uint8), alpha_bytes])
        report.append(
            {
                "template": name,
                "width": int(rgb.shape[1]),
                "height": int(rgb.shape[0]),
                "opaque_fraction": round(opaque, 4),
                "alpha_sum": int(alpha_bytes.astype(np.uint64).sum()),
            }
        )
        if not dry_run:
            Image.fromarray(rgba, "RGBA").save(ASSETS / name, optimize=True)
    print(json.dumps(report, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
