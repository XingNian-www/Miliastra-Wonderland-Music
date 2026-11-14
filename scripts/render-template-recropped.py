#!/usr/bin/env python3
"""把模板“原图 / 透明化后”并排渲染成审计对比图。

输出：technical-docs/ui-state-evidence/evidence-gallery/template-recropped-1..4.png
只读 assets 与 template-originals，不参与生产匹配。
"""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
ORIGINALS = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery" / "template-originals"
OUT = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery"

FONT = ImageFont.truetype("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 15)
TITLE_FONT = ImageFont.truetype("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 24)

GROUPS = [
    (
        "1",
        "聊天标记 / 通用 UI 锚点",
        [
            "chat-marker-blue.png",
            "chat-marker-pink.png",
            "chat-marker-yellow.png",
            "world-wish.png",
            "startup-wonderland-map-star.png",
            "ui-secondary-back.png",
            "ui-secondary-hall.png",
            "ui-secondary-chat-submit.png",
            "friend-panel.png",
            "friend-search-panel.png",
            "friend-more-settings.png",
        ],
    ),
    (
        "2",
        "好友覆盖层 / 邀请按钮",
        [
            "friend-block-chat.png",
            "friend-blacklist.png",
            "confirm-button.png",
            "invite-enter-hall.png",
            "invite-goto-hall.png",
            "invite-view-star.png",
        ],
    ),
    (
        "3",
        "千星卡片 / 主菜单 / 启动",
        [
            "qianxing-exit-menu.png",
            "ui-main-menu-return.png",
            "startup-paimon-menu.png",
        ],
    ),
    (
        "4",
        "F2 大厅页 / 千星地图",
        [
            "qianxing-f2-hall-list.png",
            "qianxing-f2-invite-player.png",
            "qianxing-map-label.png",
        ],
    ),
]

# 不能透明化的模板：在对比图上直接标注原因。
EXCEPTIONS = {
    "chat-marker-blue.png": "色块即判据，保持原样",
    "chat-marker-pink.png": "色块即判据，保持原样",
    "chat-marker-yellow.png": "色块即判据，保持原样",
    "world-wish.png": "透明图标，保持原样",
    "startup-wonderland-map-star.png": "已是透明图标",
    "invite-goto-hall.png": "去掉按钮面后 Teyvat 档案卡正样本掉到 0.75，保留原样",
    "friend-block-chat.png": "去掉按钮面后过渡帧 0.9036>0.90，保留原样",
    "startup-paimon-menu.png": "角色碎片，去底后启动正样本掉到 0.81，保留原样",
    "ui-secondary-back.png": "圆钮是按钮面；只清四角",
    "ui-secondary-chat-submit.png": "圆钮是按钮面；只清四角",
}

MAX_W = 1780


def checker(width: int, height: int, size: int = 10) -> Image.Image:
    image = Image.new("RGB", (width, height), (44, 44, 44))
    draw = ImageDraw.Draw(image)
    for y in range(0, height, size):
        for x in range(0, width, size):
            if (x // size + y // size) % 2 == 0:
                draw.rectangle([x, y, x + size - 1, y + size - 1], fill=(88, 88, 88))
    return image


def transparent_fraction(path: Path) -> float:
    image = Image.open(path).convert("RGBA")
    alpha = image.getchannel("A")
    total = image.width * image.height
    return sum(1 for value in alpha.getdata() if value == 0) / total


def render_cell(name: str) -> Image.Image:
    original = Image.open(ORIGINALS / name).convert("RGB")
    current = Image.open(ASSETS / name).convert("RGBA")
    zoom = max(1, min(4, 460 // original.width, 230 // original.height))
    left = original.resize((original.width * zoom, original.height * zoom), Image.NEAREST)
    right = current.resize((current.width * zoom, current.height * zoom), Image.NEAREST)
    backdrop = checker(right.width, right.height)
    backdrop.paste(right, (0, 0), right)
    gap, label_h = 12, 40
    cell = Image.new("RGB", (left.width + gap + backdrop.width, max(left.height, backdrop.height) + label_h), (24, 24, 24))
    cell.paste(left, (0, 0))
    cell.paste(backdrop, (left.width + gap, 0))
    draw = ImageDraw.Draw(cell)
    transparency = transparent_fraction(ASSETS / name)
    note = EXCEPTIONS.get(name, "")
    title = f"{name}  {original.width}x{original.height}  透明 {transparency:.0%}"
    if note:
        title += f"  [{note}]"
    draw.text((0, cell.height - label_h + 6), title, font=FONT, fill=(235, 235, 235))
    return cell


def pack(title: str, cells: list[Image.Image]) -> Image.Image:
    rows: list[list[Image.Image]] = []
    current: list[Image.Image] = []
    width = 0
    for cell in cells:
        if current and width + cell.width + 16 > MAX_W:
            rows.append(current)
            current, width = [], 0
        current.append(cell)
        width += cell.width + 16
    if current:
        rows.append(current)
    height = 70 + sum(max(cell.height for cell in row) + 14 for row in rows)
    sheet = Image.new("RGB", (MAX_W, height), (18, 18, 18))
    draw = ImageDraw.Draw(sheet)
    draw.text((18, 18), title, font=TITLE_FONT, fill=(255, 255, 255))
    y = 70
    for row in rows:
        x = 18
        for cell in row:
            sheet.paste(cell, (x, y))
            x += cell.width + 16
        y += max(cell.height for cell in row) + 14
    return sheet


def main() -> None:
    for number, title, names in GROUPS:
        cells = [render_cell(name) for name in names]
        sheet = pack(f"模板透明化复核 {number}：{title}（左：原图 / 右：透明化后）", cells)
        sheet.save(OUT / f"template-recropped-{number}.png", optimize=True)
        print(f"template-recropped-{number}.png {sheet.width}x{sheet.height}")


if __name__ == "__main__":
    main()
