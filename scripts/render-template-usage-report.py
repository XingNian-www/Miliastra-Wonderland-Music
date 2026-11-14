#!/usr/bin/env python3
"""生成「模板使用报告」配图：缩略图 + 按流程分组的 contact sheet。

输出：
- technical-docs/ui-state-evidence/evidence-gallery/template-usage-thumbs/<模板>.png
- technical-docs/ui-state-evidence/evidence-gallery/template-usage-report-1..4.png
只读 assets，不参与生产。
"""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
OUT = ROOT / "technical-docs" / "ui-state-evidence" / "evidence-gallery"
THUMBS = OUT / "template-usage-thumbs"

FONT = ImageFont.truetype("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 13)
TITLE_FONT = ImageFont.truetype("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 24)
MAX_W = 1780

# (文件, 配置键, 调用点/用途)
GROUPS = [
    (
        "1",
        "聊天标记 / 通用 UI 锚点",
        [
            ("chat-marker-blue.png", "templates.blue_marker", "聊天标记扫描 → 一级/二级"),
            ("chat-marker-yellow.png", "templates.yellow_marker", "聊天标记扫描 → 一级/二级"),
            ("chat-marker-pink.png", "templates.pink_marker", "聊天标记扫描 → 好友私聊"),
            ("ui-secondary-back.png", "templates.secondary_back", "二级界面判据"),
            ("ui-secondary-hall.png", "templates.secondary_hall", "residency 二级当前大厅"),
            ("ui-secondary-chat-submit.png", "templates.secondary_chat_submit", "二级聊天判据"),
            ("ui-main-menu-return.png", "templates.main_menu", "主菜单 / 大厅卡"),
            ("world-wish.png", "templates.world_wish", "未知态回退探针（世界门控）"),
        ],
    ),
    (
        "2",
        "好友面板 / 审核流程",
        [
            ("friend-panel.png", "templates.friend_panel", "refine_panel + moderation"),
            ("friend-search-panel.png", "templates.friend_search_panel", "refine_panel + moderation"),
            ("friend-more-settings.png", "templates.friend_more_settings", "moderation 打开 ⋯ 菜单"),
            ("friend-block-chat.png", "templates.friend_block_chat", "refine_panel + moderation 屏蔽"),
            ("friend-blacklist.png", "templates.friend_blacklist", "moderation 拉黑"),
            ("confirm-button.png", "templates.confirm_button", "refine_panel + moderation 确认 + 启动确认"),
        ],
    ),
    (
        "3",
        "邀请 / 千星卡片",
        [
            ("invite-view-star.png", "templates.invite_view_star", "refine_panel + invite 例程"),
            ("invite-goto-hall.png", "templates.invite_goto_hall", "refine_panel + invite 例程"),
            ("invite-enter-hall.png", "templates.invite_enter_hall", "refine_panel + invite 例程"),
            ("qianxing-exit-menu.png", "templates.qianxing_exit_menu", "refine_panel 千星 Other"),
        ],
    ),
    (
        "4",
        "F2 / 千星地图 / 启动",
        [
            ("qianxing-f2-hall-list.png", "templates.qianxing_f2_hall_list", "F2 左下按钮（双命中）"),
            ("qianxing-f2-invite-player.png", "templates.qianxing_f2_invite_player", "F2 右下按钮（双命中）"),
            ("qianxing-map-label.png", "templates.qianxing_map_label", "地图右下角二值形状锚点"),
            ("startup-wonderland-map-star.png", "startup.wonderland_map_star", "启动定位/确认地图星"),
            ("startup-paimon-menu.png", "startup.paimon_menu", "启动判断已进主界面"),
        ],
    ),
]


def checker(width: int, height: int, size: int = 8) -> Image.Image:
    image = Image.new("RGB", (width, height), (44, 44, 44))
    draw = ImageDraw.Draw(image)
    for y in range(0, height, size):
        for x in range(0, width, size):
            if (x // size + y // size) % 2 == 0:
                draw.rectangle([x, y, x + size - 1, y + size - 1], fill=(88, 88, 88))
    return image


def thumb(path: Path, box_w: int, box_h: int) -> Image.Image:
    image = Image.open(path).convert("RGBA")
    if image.width > box_w or image.height > box_h:
        image.thumbnail((box_w, box_h), Image.LANCZOS)
    else:
        zoom = max(1, min(4, box_w // max(1, image.width), box_h // max(1, image.height)))
        image = image.resize((image.width * zoom, image.height * zoom), Image.NEAREST)
    canvas = checker(max(image.width, 40), max(image.height, 40))
    canvas.paste(image, (0, 0), image)
    return canvas


def render_cell(name: str, key: str, usage: str, keep_thumb: bool = True) -> Image.Image:
    current = thumb(ASSETS / name, 300, 120)
    label_h = 64
    cell = Image.new("RGB", (max(current.width, 300), current.height + label_h), (24, 24, 24))
    cell.paste(current, (0, 0))
    draw = ImageDraw.Draw(cell)
    draw.text((0, current.height + 2), name, font=FONT, fill=(240, 240, 240))
    draw.text((0, current.height + 18), key, font=FONT, fill=(150, 205, 255))
    draw.text((0, current.height + 34), usage, font=FONT, fill=(190, 190, 190))
    if keep_thumb:
        THUMBS.mkdir(parents=True, exist_ok=True)
        small = current.copy()
        small.thumbnail((120, 70), Image.LANCZOS)
        small.save(THUMBS / name, optimize=True)
    return cell


def pack(title: str, cells: list[Image.Image]) -> Image.Image:
    rows: list[list[Image.Image]] = []
    current: list[Image.Image] = []
    width = 0
    for cell in cells:
        if current and width + cell.width + 14 > MAX_W:
            rows.append(current)
            current, width = [], 0
        current.append(cell)
        width += cell.width + 14
    if current:
        rows.append(current)
    height = 66 + sum(max(cell.height for cell in row) + 12 for row in rows)
    sheet = Image.new("RGB", (MAX_W, height), (18, 18, 18))
    draw = ImageDraw.Draw(sheet)
    draw.text((16, 16), title, font=TITLE_FONT, fill=(255, 255, 255))
    y = 66
    for row in rows:
        x = 16
        for cell in row:
            sheet.paste(cell, (x, y))
            x += cell.width + 14
        y += max(cell.height for cell in row) + 12
    return sheet


def main() -> None:
    THUMBS.mkdir(parents=True, exist_ok=True)
    for number, title, entries in GROUPS:
        cells = [render_cell(*entry) for entry in entries]
        sheet = pack(f"模板使用报告 {number}：{title}（缩略图 + 配置键 + 调用点）", cells)
        sheet.save(OUT / f"template-usage-report-{number}.png", optimize=True)
        print(f"template-usage-report-{number}.png {sheet.width}x{sheet.height}")


if __name__ == "__main__":
    main()
