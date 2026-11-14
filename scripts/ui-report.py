#!/usr/bin/env python3
"""把界面检测证据导出成 annotated PNG + JSON + Markdown 报告。

数据来源只有一个：生产分类器 + 生产匹配函数。脚本本身不实现任何模板匹配，
也不改判定逻辑；它调用 ignored 测试 `dump_ui_detect_report`（见
`src/ui/state.rs`）拿原始证据，然后只负责画框、排版、汇总。

用法：
    # 单张
    python3 scripts/ui-report.py <image> [--expect "<expected label>"] [--out target/ui-reports]

    # 批量（manifest 为 TSV，列含 path / expected_label / label_status / ...）
    python3 scripts/ui-report.py --manifest technical-docs/ui-state-corpus/manifest.tsv --status confirmed
    python3 scripts/ui-report.py --label "千星大厅（Primary Hall）"

输出（单张）：
    <out>/<stem>.annotated.png   原图 + 彩框 + 分数/阈值/结果
    <out>/<stem>.json            完整原始证据（生产分类器导出）
    <out>/<stem>.md              人读报告

批量时额外生成 `<out>/index.md`（FAIL 在前）。

无 --expect 时 PASS 只表示“分类器给出了确定界面”（kind != unknown）；给了
--expect 才做 expected/predicted 对照。OCR 区域照画；本机有 OCR 时自动填充文本，
不可用或大区域跳过时标 `UNVERIFIABLE (offline)` / `SKIPPED`。
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

try:
    from ui_ocr_linux import ocr_rect as _ocr_rect, available as _ocr_backends
except Exception:  # pragma: no cover
    _ocr_rect = None
    _ocr_backends = lambda: []
from typing import Any

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
FONT_PATH = "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"
DEFAULT_MANIFEST = ROOT / "technical-docs" / "ui-state-corpus" / "manifest.tsv"
OCR_ENABLED = True
DRAW_DIAGNOSTICS = True
MAX_OCR_AREA = 200_000

DEFAULT_CONFIG = ROOT / "tests" / "fixtures" / "config.full.yaml"
TEST_NAME = "ui::state::tests::dump_ui_detect_report"

# 颜色契约（与任务描述一致）。
C_HIT = (0, 220, 90)  # 绿色：模板命中
C_MISS = (255, 140, 0)  # 橙色：模板未命中
C_OCR = (0, 170, 255)  # 蓝色：OCR 区域
C_STRUCT = (255, 210, 0)  # 黄色：结构判据区域
C_TEXT = (240, 242, 246)
C_PASS = (61, 220, 132)
C_FAIL = (255, 82, 82)
C_WARN = (255, 193, 7)
C_PANEL = (10, 10, 14)


# ── 期望标签归一化 ────────────────────────────────────────────────────────
# manifest 的 expected_label 是给人看的中文名；分类器输出的是稳定英文
# kind/label/surface。这里只做**对照映射**，不参与分类。
EXPECT_ALIASES: dict[str, dict[str, str]] = {
    "千星大厅（Primary Hall）": {"kind": "primary", "surface": "PrimaryHall"},
    "二级聊天·当前大厅（Secondary Chat: Current Hall）": {
        "kind": "secondary",
        "surface": "Secondary(Chat)",
    },
    "二级聊天·好友私聊（Secondary Chat: Friend Conversation）": {
        "kind": "secondary",
        "surface": "Secondary(Chat)",
    },
    "二级界面·千星多人面板（Secondary Interface: Multiplayer Panel）": {
        "kind": "secondary",
        "surface": "Secondary(MultiplayerPanel)",
        "label": "secondary:qianxing_panel",
    },
    "二级界面·千星地图（Secondary Interface: Hall Map）": {
        "kind": "secondary",
        "surface": "Secondary(HallMap)",
    },
    "主菜单千星（Main Menu: Miliastra）": {
        "kind": "main_menu",
        "surface": "WonderlandMainMenu",
    },
    "主菜单大世界（Main Menu: Overworld）": {
        "kind": "main_menu",
        "surface": "PaimonMenu",
    },
    "加载界面（Loading Screen）": {"kind": "loading", "surface": "Loading"},
    "登录入口（Game Gate）": {"kind": "game_gate", "surface": "GameGate"},
    "提瓦特大世界（Overworld）": {"kind": "overworld", "surface": "Overworld"},
    "覆盖层·好友面板（Overlay: Friend Panel）": {
        "kind": "panel",
        "label": "panel:friend_list",
        "surface": "Overlay(FriendPanel)",
    },
    "覆盖层·搜索页（Overlay: Search Page）": {
        "kind": "panel",
        "label": "panel:add_friend",
        "surface": "Overlay(SearchPage)",
    },
    "覆盖层·更多设置（Overlay: More Settings）": {
        "kind": "panel",
        "label": "panel:more_settings",
        "surface": "Overlay(MoreSettings)",
    },
    "覆盖层·确认弹窗·进入随机大厅（Overlay: Confirm Enter Random Hall）": {
        "kind": "panel",
        "label": "panel:confirm_enter_hall",
        "surface": "Overlay(ConfirmEnterHall)",
    },
    "覆盖层·确认弹窗·离开大厅（Overlay: Confirm Leave Hall）": {
        "kind": "panel",
        "label": "panel:confirm_leave_hall",
        "surface": "Overlay(ConfirmLeaveHall)",
    },
    "覆盖层·确认弹窗·屏蔽玩家（Overlay: Confirm Block Player）": {
        "kind": "panel",
        "label": "panel:confirm_block_player",
        "surface": "Overlay(ConfirmBlockPlayer)",
    },
    "覆盖层·档案卡（Overlay: Profile Card）": {
        "kind": "panel",
        "label": "panel:archive_card",
        "surface": "Overlay(ProfileCard)",
    },
    "覆盖层·个人资料页（Overlay: Profile Page）": {
        "kind": "panel",
        "label": "panel:profile_page",
        "surface": "Overlay(ProfilePage)",
    },
    "覆盖层·大厅卡（Overlay: Hall Card）": {
        "kind": "panel",
        "label": "panel:hall_card",
        "surface": "Overlay(HallCard)",
    },
    "覆盖层·头像弹窗（Overlay: Avatar Popup）": {
        "kind": "panel",
        "label": "panel:avatar_popup",
        "surface": "Overlay(AvatarPopup)",
    },
}

# 生产分类器没有语义级 label 的千星覆盖页，统一落 panel:qianxing_page。
for _name in [
    "覆盖层·颂愿（Overlay: Wish）",
    "覆盖层·装扮搭配（Overlay: Dress Up）",
    "覆盖层·特典活动（Overlay: Special Event）",
    "覆盖层·更换角色（Overlay: Change Character）",
    "覆盖层·退出菜单（Overlay: Exit Menu）",
    "覆盖层·我的奇域（Overlay: My Wonderland）",
    "覆盖层·奇域等级（Overlay: Wonderland Level）",
    "覆盖层·人气奇域（Overlay: Popular Wonderland）",
    "覆盖层·纪游（Overlay: Wonderland Journey）",
    "覆盖层·寻找队伍（Overlay: Team Finder）",
    "覆盖层·千星商城（Overlay: Wonderland Shop）",
]:
    EXPECT_ALIASES[_name] = {
        "kind": "panel",
        "label": "panel:qianxing_page",
        "surface": "Overlay(Other)",
    }

KIND_ALIASES = {
    "primary": "primary",
    "一级界面": "primary",
    "secondary": "secondary",
    "二级聊天": "secondary",
    "main_menu": "main_menu",
    "主菜单": "main_menu",
    "loading": "loading",
    "加载界面": "loading",
    "game_gate": "game_gate",
    "登录入口": "game_gate",
    "overworld": "overworld",
    "大世界": "overworld",
    "panel": "panel",
    "覆盖层": "panel",
    "unknown": "unknown",
}

STRUCT_KEY_LABELS = {
    "screen.hall_bar_rect",
    "paimon_grid",
    "world_probe",
    "screen.qianxing_f2_invite_rect",
    "screen.main_menu_rect",
    "screen.secondary_back_rect",
    "screen.secondary_chat_submit_rect",
    "qianxing_map_anchor",
}


def eprint(*args: Any) -> None:
    print(*args, file=sys.stderr)


def font(size: int) -> ImageFont.FreeTypeFont:
    try:
        return ImageFont.truetype(FONT_PATH, size)
    except OSError:
        # 仅用于开发机兜底；验收机有 wqy-microhei。
        return ImageFont.load_default()


F_BANNER = font(26)
F_BANNER_SMALL = font(18)
F_LABEL = font(16)
F_LABEL_SMALL = font(14)
F_TINY = font(13)


def text_width(draw: ImageDraw.ImageDraw, text: str, fnt: ImageFont.FreeTypeFont) -> int:
    return int(draw.textlength(text, font=fnt))


def wrap_px(draw: ImageDraw.ImageDraw, text: str, fnt: ImageFont.FreeTypeFont, max_width: int):
    if text_width(draw, text, fnt) <= max_width:
        return [text]
    lines: list[str] = []
    current = ""
    for token in re.findall(r"[A-Za-z0-9_\-\./:,=+<>%()\[\]{}]+|.", str(text)):
        candidate = current + token
        if text_width(draw, candidate, fnt) <= max_width:
            current = candidate
        else:
            if current:
                lines.append(current)
            current = token
    if current:
        lines.append(current)
    return lines or [""]


def esc_md(value: Any) -> str:
    return str(value).replace("|", "\\|").replace("\n", " ")


def rect_str(rect: dict[str, Any] | None) -> str:
    if not rect:
        return "—"
    return (
        f"{rect.get('w', rect.get('width'))}×{rect.get('h', rect.get('height'))} "
        f"@({rect.get('x')},{rect.get('y')})"
    )


def fmt_score(value: Any) -> str:
    return "—" if value is None else f"{float(value):.4f}"


def fmt_threshold(value: Any) -> str:
    return "—" if value is None else f"{float(value):.4f}"


def normalize_expectation(text: str) -> dict[str, str]:
    raw = text.strip()
    if raw in EXPECT_ALIASES:
        return dict(EXPECT_ALIASES[raw])
    if raw in KIND_ALIASES:
        return {"kind": KIND_ALIASES[raw]}
    if raw in {"primary_hall", "PrimaryHall"}:
        return {"kind": "primary", "surface": "PrimaryHall"}
    if ":" in raw:
        return {"label": raw}
    if raw in {
        "PrimaryHall",
        "Secondary(Chat)",
        "Secondary(MultiplayerPanel)",
        "Secondary(HallMap)",
        "WonderlandMainMenu",
        "PaimonMenu",
        "Overlay(FriendPanel)",
    }:
        return {"surface": raw}
    return {"label": raw}


def compare(data: dict[str, Any], expected: dict[str, str]) -> tuple[bool, list[str], str]:
    differences: list[str] = []
    actual = {
        "kind": data.get("kind", "?"),
        "label": data.get("label", "?"),
        "surface": (data.get("surface") or {}).get("debug", "?"),
    }
    for key in ("kind", "label", "surface"):
        if key in expected and expected[key] != actual[key]:
            differences.append(
                f"{key}: 期望 `{expected[key]}`，实际 `{actual[key]}`"
            )
    if differences:
        return False, differences, "；".join(differences)
    return True, [], ""


def evaluate(data: dict[str, Any], expected_text: str | None) -> dict[str, Any]:
    if data.get("error"):
        return {
            "result": "FAIL",
            "pass": False,
            "expected": normalize_expectation(expected_text) if expected_text else None,
            "differences": [],
            "failure_reason": str(data["error"]),
        }
    if expected_text:
        expected = normalize_expectation(expected_text)
        passed, differences, reason = compare(data, expected)
        return {
            "result": "PASS" if passed else "FAIL",
            "pass": passed,
            "expected": expected,
            "expected_raw": expected_text,
            "differences": differences,
            "failure_reason": "" if passed else reason,
        }
    kind = str(data.get("kind", "unknown"))
    label = str(data.get("label", "unknown"))
    passed = kind != "unknown" and not label.startswith("unknown")
    reason = ""
    if not passed:
        reason = "分类器拒判（unknown）"
        if (data.get("structural") or {}).get("marker_threshold") is None:
            pass
    return {
        "result": "PASS" if passed else "FAIL",
        "pass": passed,
        "expected": None,
        "differences": [],
        "failure_reason": reason,
    }


# ── 调用生产导出测试 ──────────────────────────────────────────────────────


class ExportError(RuntimeError):
    pass


def cargo_available() -> str:
    cargo = shutil.which("cargo")
    if cargo:
        return cargo
    for candidate in (Path.home() / ".cargo" / "bin" / "cargo", Path("/root/.cargo/bin/cargo")):
        if candidate.exists():
            return str(candidate)
    raise ExportError("找不到 cargo（请先安装 Rust 工具链）")


def run_export(
    image: Path,
    out_json: Path,
    config: Path,
    expect: str | None,
    *,
    full_probes: bool = False,
) -> dict[str, Any]:
    cargo = cargo_available()
    out_json.parent.mkdir(parents=True, exist_ok=True)
    if out_json.exists():
        out_json.unlink()
    env = os.environ.copy()
    cargo_bin = str(Path(cargo).parent)
    env["PATH"] = cargo_bin + os.pathsep + env.get("PATH", "")
    env["MILIASTRA_REPORT_IMAGE"] = str(image)
    env["MILIASTRA_REPORT_OUT"] = str(out_json)
    env["MILIASTRA_REPORT_CONFIG"] = str(config)
    if expect:
        env["MILIASTRA_REPORT_EXPECT"] = expect
    else:
        env.pop("MILIASTRA_REPORT_EXPECT", None)
    if full_probes:
        env["MILIASTRA_REPORT_FULL_PROBES"] = "1"
    else:
        env.pop("MILIASTRA_REPORT_FULL_PROBES", None)
    command = [
        cargo,
        "test",
        "--lib",
        "--no-default-features",
        "--",
        "--ignored",
        "--exact",
        "--nocapture",
        "--test-threads=1",
        TEST_NAME,
    ]
    proc = subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if not out_json.exists():
        tail = "\n".join((proc.stderr or proc.stdout).strip().splitlines()[-25:])
        raise ExportError(
            f"生产导出未生成 JSON（cargo 退出码 {proc.returncode}）：\n{tail}"
        )
    try:
        return json.loads(out_json.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise ExportError(f"导出 JSON 无法解析：{error}") from error


# ── 画图 ──────────────────────────────────────────────────────────────────


class LabelPlacer:
    """在框附近放标签，自动避让已放标签与画面边界。"""

    def __init__(self, width: int, height: int, top: int) -> None:
        self.width = width
        self.height = height
        self.top = top
        self.placed: list[tuple[int, int, int, int]] = []

    def _overlaps(self, box: tuple[int, int, int, int]) -> bool:
        x0, y0, x1, y1 = box
        for px0, py0, px1, py1 in self.placed:
            if x0 < px1 and x1 > px0 and y0 < py1 and y1 > py0:
                return True
        return False

    def add(
        self,
        draw: ImageDraw.ImageDraw,
        anchor: tuple[int, int, int, int],
        text: str,
        color: tuple[int, int, int],
        fnt: ImageFont.FreeTypeFont,
        *,
        inside: bool = False,
    ) -> None:
        x0, y0, x1, y1 = anchor
        tw = text_width(draw, text, fnt)
        th = fnt.size + 4
        bw = tw + 10
        x = max(2, min(x0, self.width - bw - 2))
        candidates: list[int] = []
        if not inside:
            candidates.append(y0 - th - 4)
        candidates.extend([y0 + 3, y1 + 4])
        if not inside:
            candidates.extend(range(max(self.top, y0 + 3), self.height - th - 2, th + 2))
        chosen: tuple[int, int, int, int] | None = None
        for y in candidates:
            if y < self.top or y + th > self.height - 2:
                continue
            box = (x, y, x + bw, y + th)
            if not self._overlaps(box):
                chosen = box
                break
        if chosen is None:
            # 最后兜底：从 banner 下沿开始找第一块空地；找不到就贴左下角。
            y = self.top + 2
            while y + th <= self.height - 2:
                box = (x, y, x + bw, y + th)
                if not self._overlaps(box):
                    chosen = box
                    break
                y += th + 2
            if chosen is None:
                chosen = (2, self.height - th - 2, 2 + bw, self.height - 2)
        bx0, by0, bx1, by1 = chosen
        draw.rectangle([bx0, by0, bx1, by1], fill=(0, 0, 0, 205))
        draw.text((bx0 + 5, by0 + 1), text, font=fnt, fill=color)
        self.placed.append(chosen)


def clip_rect(rect: dict[str, Any], width: int, height: int) -> tuple[int, int, int, int] | None:
    try:
        rw = float(rect.get("w", rect.get("width")))
        rh = float(rect.get("h", rect.get("height")))
        x0 = int(round(float(rect["x"])))
        y0 = int(round(float(rect["y"])))
        x1 = int(round(float(rect["x"]) + rw))
        y1 = int(round(float(rect["y"]) + rh))
    except (KeyError, TypeError, ValueError):
        return None
    x0 = max(0, min(x0, width - 1))
    y0 = max(0, min(y0, height - 1))
    x1 = max(0, min(x1, width - 1))
    y1 = max(0, min(y1, height - 1))
    if x1 <= x0 or y1 <= y0:
        return None
    return x0, y0, x1, y1


def draw_box(
    draw: ImageDraw.ImageDraw,
    rect: dict[str, Any],
    color: tuple[int, int, int],
    width: int,
    *,
    width_px: int,
    height_px: int,
    fill_alpha: int = 0,
) -> tuple[int, int, int, int] | None:
    box = clip_rect(rect, width_px, height_px)
    if box is None:
        return None
    if fill_alpha:
        draw.rectangle(list(box), fill=color + (fill_alpha,))
    draw.rectangle(list(box), outline=color + (255,), width=width)
    return box


def normalize_image(source_path: Path, data: dict[str, Any]) -> tuple[Image.Image, dict[str, Any]]:
    image = Image.open(source_path).convert("RGB")
    original = image.size
    canvas = data.get("image") or {}
    target = (int(canvas.get("canvas_width", image.width)), int(canvas.get("canvas_height", image.height)))
    resized = bool(canvas.get("resized")) or image.size != target
    if resized and image.size != target:
        # 生产 resize_to_canvas 用 image crate 的 Triangle（双线性）滤波；
        # PIL 的 BILINEAR 是同样的 2×2 三角形核。
        image = image.resize(target, Image.Resampling.BILINEAR)
    scale_x = target[0] / original[0] if original[0] else 1.0
    scale_y = target[1] / original[1] if original[1] else 1.0
    return image, {
        "original": original,
        "canvas": target,
        "resized": resized,
        "scale_x": scale_x,
        "scale_y": scale_y,
    }


def ocr_summary(data: dict[str, Any]) -> str:
    regions = data.get("ocr_regions") or []
    with_text = [region for region in regions if region.get("text")]
    if not with_text:
        return "OCR: UNVERIFIABLE (offline)"
    backends = sorted({str(region.get("backend")) for region in with_text if region.get("backend")})
    backend = ", ".join(backends) if backends else "offline"
    return f"OCR: {len(with_text)}/{len(regions)} 区域有文本 ({backend})"


def annotation_label(probe: dict[str, Any], name: str) -> str:
    score = probe.get("score")
    threshold = probe.get("threshold")
    verdict = "命中" if probe.get("pass") else "未命中"
    score_text = "无候选" if score is None else f"{float(score):.3f}"
    return f"{name} {score_text}/{fmt_threshold(threshold)} {verdict}"


def is_full_frame(rect: dict[str, Any], width: int, height: int) -> bool:
    return (
        rect.get("x") == 0
        and rect.get("y") == 0
        and rect.get("w", rect.get("width")) == width
        and rect.get("h", rect.get("height")) == height
    )


def annotate(
    source_path: Path,
    out_png: Path,
    data: dict[str, Any],
    verdict: dict[str, Any],
    template_names: dict[str, str],
) -> dict[str, Any]:
    image, size_info = normalize_image(source_path, data)
    width, height = image.size
    base = image.convert("RGBA")
    overlay = Image.new("RGBA", base.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)

    # 1) 顶部标题条：kind / label / final_rule / PASS|FAIL
    result = verdict["result"]
    banner_line = (
        f"{data.get('kind', '?')} / {data.get('label', '?')} / "
        f"{data.get('final_rule', '?')} / {result}"
    )
    banner_lines = wrap_px(draw, banner_line, F_BANNER, width - 32)
    surface = (data.get("surface") or {}).get("chinese") or ""
    detail_bits = [surface] if surface else []
    detail_bits.append(f"原图 {size_info['original'][0]}×{size_info['original'][1]}")
    detail_bits.append(
        f"画布 {width}×{height}（缩放 {size_info['scale_x']:.3f}×{size_info['scale_y']:.3f}）"
    )
    detail_bits.append(ocr_summary(data))
    detail_line = " | ".join(detail_bits)
    detail_lines = wrap_px(draw, detail_line, F_BANNER_SMALL, width - 32)
    banner_h = 12 + len(banner_lines) * (F_BANNER.size + 4) + len(detail_lines) * (
        F_BANNER_SMALL.size + 3
    )
    banner_h += 6
    draw.rectangle([0, 0, width, banner_h], fill=(8, 8, 12, 215))
    banner_color = C_PASS if result == "PASS" else C_FAIL
    draw.rectangle([0, 0, 6, banner_h], fill=banner_color + (255,))
    y = 8
    for line in banner_lines:
        draw.text((16, y), line, font=F_BANNER, fill=C_TEXT)
        y += F_BANNER.size + 4
    for line in detail_lines:
        draw.text((16, y), line, font=F_BANNER_SMALL, fill=(176, 184, 198))
        y += F_BANNER_SMALL.size + 3
    placer = LabelPlacer(width, height, banner_h + 3)

    # 5) 左下角图例的位置先占住，避免后续标签压在图例上。
    legend = [
        ("绿：模板命中", C_HIT),
        ("橙：模板未命中", C_MISS),
        ("蓝：OCR 区域", C_OCR),
        ("黄：结构判据区域", C_STRUCT),
    ]
    lw = max(text_width(draw, text, F_LABEL_SMALL) for text, _ in legend) + 26
    lh = len(legend) * (F_LABEL_SMALL.size + 5) + 12
    lx, ly = 12, height - lh - 12
    placer.placed.append((lx, ly, lx + lw, ly + lh))

    # 2) 结构判据区域（黄色，细框）
    for region in data.get("structural_regions") or []:
        rect = region.get("rect") or {}
        if int(rect.get("w", 0)) * int(rect.get("h", 0)) > width * height * 0.4:
            continue
        box = draw_box(
            draw, rect, C_STRUCT, 2,
            width_px=width, height_px=height,
        )
        if box and region.get("key") in STRUCT_KEY_LABELS:
            placer.add(draw, box, str(region.get("name", region.get("key", "结构"))), C_STRUCT, F_LABEL_SMALL)

    # 3) OCR 区域（蓝色）
    for region in data.get("ocr_regions") or []:
        box = draw_box(
            draw, region.get("rect") or {}, C_OCR, 2,
            width_px=width, height_px=height,
        )
        if box:
            name = str(region.get("name", "OCR"))
            text = str(region.get("text") or "")
            if text:
                short = text if len(text) <= 22 else text[:22] + "…"
                conf = region.get("confidence")
                status = f"{short} ({float(conf):.2f})" if isinstance(conf, (int, float)) else short
            else:
                status = "离线/不可验证"
            placer.add(draw, box, f"{name} [{status}]", C_OCR, F_LABEL_SMALL)

    # 4) 模板探针（绿色命中 / 橙色未命中）
    drawn_templates: set[str] = set()
    production = list(data.get("template_probes") or [])
    diagnostics = list(data.get("diagnostic_probes") or []) if DRAW_DIAGNOSTICS else []
    for probe in production:
        template = str(probe.get("template", ""))
        drawn_templates.add(template)
        name = template_names.get(template) or Path(template).stem
        rect = probe.get("hit_rect") or probe.get("search_rect") or {}
        if is_full_frame(rect, width, height) and not probe.get("hit_rect"):
            continue
        color = C_HIT if probe.get("pass") else C_MISS
        box = draw_box(
            draw, rect, color, 4, width_px=width, height_px=height
        )
        if box:
            placer.add(draw, box, annotation_label(probe, name), color, F_LABEL)
    for probe in diagnostics:
        template = str(probe.get("template", ""))
        name = template_names.get(template) or str(probe.get("label") or Path(template).stem)
        rect = probe.get("hit_rect") or probe.get("search_rect") or {}
        color = C_HIT if probe.get("pass") else C_MISS
        # 生产探针已画过的模板不重复画；全帧搜索且未命中时也不铺满整幅画。
        if template in drawn_templates:
            continue
        if is_full_frame(rect, width, height) and not probe.get("hit_rect"):
            continue
        box = draw_box(
            draw, rect, color, 3, width_px=width, height_px=height
        )
        if box:
            label = annotation_label(probe, name)
            if probe.get("source") == "diagnostic":
                label = "诊断·" + label
            if not probe.get("pass") and "hit_rect" not in probe:
                label += "（搜索区）"
            placer.add(draw, box, label, color, F_LABEL)

    # 5) 左下角图例（位置已在开始画框前占住）
    draw.rectangle([lx, ly, lx + lw, ly + lh], fill=(8, 8, 12, 210))
    yy = ly + 6
    for text, color in legend:
        draw.rectangle([lx + 8, yy + 3, lx + 20, yy + 13], outline=color + (255,), width=2)
        draw.text((lx + 26, yy), text, font=F_LABEL_SMALL, fill=C_TEXT)
        yy += F_LABEL_SMALL.size + 5

    Image.alpha_composite(base, overlay).convert("RGB").save(out_png)
    return size_info


# ── Markdown ──────────────────────────────────────────────────────────────


def structural_rows(structural: dict[str, Any]) -> list[tuple[str, str]]:
    rows: list[tuple[str, str]] = []

    def add(name: str, value: Any) -> None:
        if value is None:
            rows.append((name, "—"))
        elif isinstance(value, bool):
            rows.append((name, "true" if value else "false"))
        elif isinstance(value, dict):
            rows.append((name, ", ".join(f"{k}={v}" for k, v in value.items())))
        else:
            rows.append((name, str(value)))

    add("底栏白键（严格 255）", structural.get("hall_bar_exact"))
    add("底栏白键（近白 225）", structural.get("hall_bar_near_white"))
    add("派蒙宫格", structural.get("paimon_grid"))
    add("整屏色调", structural.get("screen_tone"))
    add("加载中心暗占比", structural.get("loading_center"))
    add("加载判据", structural.get("loading_rule"))
    map_anchor = structural.get("qianxing_map_anchor") or {}
    add(
        "千星地图右下锚点",
        {
            k: map_anchor.get(k)
            for k in ("score", "threshold", "bright_min", "hit_rect", "rect")
            if k in map_anchor
        }
        or None,
    )
    add("千星地图色调", structural.get("qianxing_map_tone"))
    add("F2 面板参考距离", structural.get("qianxing_panel_distance"))
    add("F2 面板距离上限", structural.get("qianxing_panel_max"))
    add("覆盖层关闭按钮", structural.get("overlay_close"))
    add("世界判据为提瓦特", structural.get("world_is_teyvat"))
    add("米黄面板占比", structural.get("beige_panel_fraction"))
    add("严阈值（千星锚点）", structural.get("anchor_threshold"))
    add("聊天标记阈值", structural.get("marker_threshold"))
    return rows


def render_markdown(
    data: dict[str, Any],
    verdict: dict[str, Any],
    size_info: dict[str, Any] | None,
    template_names: dict[str, str],
    *,
    title: str,
) -> str:
    image = data.get("image") or {}
    lines: list[str] = []
    lines.append(f"# UI 检测报告：{title}")
    lines.append("")
    lines.append("## 基本信息")
    lines.append("")
    lines.append(f"- 图片：`{image.get('path', '?')}`")
    original = size_info["original"] if size_info else (image.get("width"), image.get("height"))
    canvas = size_info["canvas"] if size_info else (image.get("canvas_width"), image.get("canvas_height"))
    lines.append(f"- 原尺寸：{original[0]}×{original[1]}")
    lines.append(
        f"- 检测画布：{canvas[0]}×{canvas[1]}"
        f"（缩放 {float(image.get('scale_x', 1.0)):.3f}×{float(image.get('scale_y', 1.0)):.3f}，"
        f"{'已缩放' if image.get('resized') else '未缩放'}）"
    )
    elapsed = data.get("elapsed_ms") or {}
    lines.append(
        f"- 耗时：归一化 {elapsed.get('normalize', '—')} ms / 分类 {elapsed.get('classify', '—')} ms"
        f" / 合计 {elapsed.get('total', '—')} ms"
    )
    lines.append("")
    lines.append("## 检测结果")
    lines.append("")
    lines.append("| 项 | 值 |")
    lines.append("| --- | --- |")
    lines.append(f"| kind | `{data.get('kind', '?')}` |")
    lines.append(f"| label | `{data.get('label', '?')}` |")
    lines.append(f"| final_rule | `{data.get('final_rule', '?')}` |")
    surface = data.get("surface") or {}
    lines.append(f"| surface | `{surface.get('debug', '?')}`（{surface.get('chinese', '')}）|")
    lines.append(f"| 结果 | **{verdict['result']}** |")
    if verdict.get("expected_raw"):
        lines.append(f"| 期望 | `{verdict['expected_raw']}` |")
    if verdict.get("failure_reason"):
        lines.append(f"| 失败原因 | {esc_md(verdict['failure_reason'])} |")
    lines.append("")
    if data.get("error"):
        lines.append(f"> 导出错误：{data['error']}")
        lines.append("")
        return "\n".join(lines)

    lines.append("## 模板探针")
    lines.append("")
    lines.append("| 来源 | 模板 | 搜索区 | 命中区 | 分数 | 阈值 | 结果 |")
    lines.append("| --- | --- | --- | --- | --- | --- | --- |")
    for probe in data.get("template_probes") or []:
        source = "生产（参与判定）"
        template = str(probe.get("template", "?"))
        name = template_names.get(template) or Path(template).stem
        lines.append(
            "| {} | `{}` | {} | {} | **{}** | {} | {} |".format(
                source,
                esc_md(name),
                rect_str(probe.get("search_rect")),
                rect_str(probe.get("hit_rect")),
                fmt_score(probe.get("score")),
                fmt_threshold(probe.get("threshold")),
                "命中" if probe.get("pass") else "未命中",
            )
        )
    for probe in data.get("diagnostic_probes") or []:
        template = str(probe.get("template", "?"))
        name = template_names.get(template) or str(probe.get("label") or Path(template).stem)
        if probe.get("outcome") == "skipped_cost_guard":
            verdict_text = "跳过（成本护栏，未测量）"
        else:
            verdict_text = "命中" if probe.get("pass") else "未命中"
        lines.append(
            "| 诊断（不参与判定） | `{}` | {} | {} | {} | {} | {} |".format(
                esc_md(name),
                rect_str(probe.get("search_rect")),
                rect_str(probe.get("hit_rect")),
                fmt_score(probe.get("score")),
                fmt_threshold(probe.get("threshold")),
                verdict_text,
            )
        )
    lines.append("")
    marker = data.get("marker")
    if marker:
        lines.append(
            "聊天标记探针：蓝 {} / 黄 {} / 粉 {}（区域 {}，阈值 {}）".format(
                marker.get("blue", 0),
                marker.get("yellow", 0),
                marker.get("pink", 0),
                rect_str(marker.get("search_rect")),
                fmt_threshold(marker.get("threshold")),
            )
        )
        lines.append("")
    world = data.get("world")
    if world:
        lines.append(
            "世界判据：区域 {}，青色计数 {}，判为 `{}`（teyvat ≤ {} < qianxing ≥ {}）".format(
                rect_str(world.get("region")),
                world.get("count"),
                world.get("guess"),
                world.get("teyvat_max"),
                world.get("qianxing_min"),
            )
        )
        lines.append("")

    lines.append("## OCR 区域")
    lines.append("")
    lines.append("| 名称 | 来源 | 矩形 | 文本 / 不可验证 |")
    lines.append("| --- | --- | --- | --- |")
    for region in data.get("ocr_regions") or []:
        text = region.get("text")
        text_cell = text or region.get("status") or "UNVERIFIABLE (offline)"
        lines.append(
            f"| {esc_md(region.get('name', '?'))} | `{esc_md(region.get('source', '?'))}` | "
            f"{rect_str(region.get('rect'))} | {esc_md(text_cell)} |"
        )
    lines.append("")
    backends = sorted({str(r.get("backend")) for r in (data.get("ocr_regions") or []) if r.get("backend")})
    if backends:
        lines.append(f"> OCR 后端：{', '.join(backends)}。OCR 文本仅作报告证据，不参与分类判定；大区域会跳过。")
    else:
        lines.append("> 本机没有 OCR 运行时：区域按配置照画，文本统一标 `UNVERIFIABLE (offline)`。")
    lines.append("")

    lines.append("## 结构判据测量值")
    lines.append("")
    lines.append("| 判据 | 测量值 |")
    lines.append("| --- | --- |")
    structural = data.get("structural") or {}
    for name, value in structural_rows(structural):
        lines.append(f"| {name} | {esc_md(value)} |")
    lines.append("")
    lines.append("<details><summary>完整结构测量 JSON</summary>")
    lines.append("")
    lines.append("```json")
    lines.append(json.dumps(structural, ensure_ascii=False, indent=2))
    lines.append("```")
    lines.append("")
    lines.append("</details>")
    lines.append("")

    if verdict.get("expected"):
        lines.append("## 期望对照")
        lines.append("")
        expected = verdict["expected"]
        lines.append(f"- 期望：`{json.dumps(expected, ensure_ascii=False)}`")
        lines.append(
            f"- 实际：`kind={data.get('kind')}`, `label={data.get('label')}`, "
            f"`surface={surface.get('debug')}`"
        )
        if verdict.get("differences"):
            lines.append("- 差异：")
            for diff in verdict["differences"]:
                lines.append(f"  - {diff}")
        else:
            lines.append("- 差异：无（PASS）")
        if expected.get("surface") == "Secondary(Chat)":
            lines.append(
                "- 说明：生产分类器只到 `secondary:chat`，当前大厅/好友私聊由会话证据再细分，"
                "本报告不替代会话证据。"
            )
        lines.append("")

    lines.append("## 已知限制")
    lines.append("")
    lines.append("- 诊断探针只把生产匹配函数再跑一遍用于展示，不参与分类判定。")
    if any(region.get("text") for region in (data.get("ocr_regions") or [])):
        lines.append("- OCR 文本来自本机离线 OCR，仅作报告证据；区域矩形来自配置，不是 OCR 实测框。")
    else:
        lines.append("- OCR 文本离线不可验证；区域矩形来自配置，不是 OCR 实测框。")
    lines.append("- 图片尺寸不等于画布时，先按生产 `resize_to_canvas` 规则缩放到画布再测量/画框。")
    lines.append("")
    return "\n".join(lines)


# ── 单张/批量 ─────────────────────────────────────────────────────────────


def unique_output_stem(stem: str, rel_path: str, used: set[str]) -> str:
    safe = re.sub(r"[^0-9A-Za-z_.\-\u4e00-\u9fff]+", "_", stem) or "frame"
    if safe not in used:
        used.add(safe)
        return safe
    parent = re.sub(r"[^0-9A-Za-z_.\-\u4e00-\u9fff]+", "_", Path(rel_path).parent.name) or "root"
    candidate = f"{safe}--{parent}"
    counter = 2
    while candidate in used:
        candidate = f"{safe}--{parent}-{counter}"
        counter += 1
    used.add(candidate)
    return candidate


def template_name_map(data: dict[str, Any]) -> dict[str, str]:
    mapping: dict[str, str] = {}
    for probe in data.get("diagnostic_probes") or []:
        template = str(probe.get("template", ""))
        label = probe.get("label")
        if template and label:
            mapping.setdefault(template, str(label))
    for probe in data.get("template_probes") or []:
        template = str(probe.get("template", ""))
        if template and template not in mapping:
            mapping[template] = Path(template).stem
    return mapping


@dataclass
class RowResult:
    source: str
    stem: str
    data: dict[str, Any]
    verdict: dict[str, Any]
    png: Path | None = None
    json_path: Path | None = None
    md_path: Path | None = None
    error: str = ""
    template_names: dict[str, str] = field(default_factory=dict)
    size_info: dict[str, Any] | None = None


def enrich_ocr(data: dict[str, Any], image: Path) -> None:
    """用 Linux 离线 OCR 填充报告里的 OCR 区域文本（不参与分类）。"""
    if not OCR_ENABLED or _ocr_rect is None:
        return
    backends = _ocr_backends()
    if not backends:
        return
    # OCR 区域的矩形在**检测画布坐标**里，截图本身可能不是画布尺寸；
    # 这里把矩形换算回原图坐标再裁剪（否则 4K/720p 帧会裁错位置）。
    info = data.get("image") or {}
    source_w = float(info.get("width") or 0)
    source_h = float(info.get("height") or 0)
    canvas_w = float(info.get("canvas_width") or source_w or 0)
    canvas_h = float(info.get("canvas_height") or source_h or 0)
    sx = source_w / canvas_w if source_w and canvas_w else 1.0
    sy = source_h / canvas_h if source_h and canvas_h else 1.0
    for region in data.get("ocr_regions") or []:
        rect = region.get("rect") or {}
        try:
            x = int(round(float(rect["x"]) * sx))
            y = int(round(float(rect["y"]) * sy))
            w = int(round(float(rect["w"]) * sx))
            h = int(round(float(rect["h"]) * sy))
        except (KeyError, TypeError, ValueError):
            continue
        if w <= 0 or h <= 0:
            continue
        area = w * h
        if area > MAX_OCR_AREA:
            region["status"] = "SKIPPED (region too large)"
            continue
        try:
            result = _ocr_rect(str(image), (x, y, w, h), scale=2)
        except Exception as exc:  # OCR 失败不影响主结果
            region["status"] = f"OCR ERROR: {exc}"
            continue
        if result.get("available") and result.get("text"):
            region["text"] = result["text"]
            region["confidence"] = result.get("confidence")
            region["backend"] = result.get("backend")
            region["status"] = "OK"
        else:
            region["status"] = "UNVERIFIABLE (offline)"


def process_one(
    image: Path,
    out_dir: Path,
    *,
    stem: str,
    expect: str | None,
    config: Path,
    write_files: bool = True,
    full_probes: bool = False,
) -> RowResult:
    json_path = out_dir / f"{stem}.json"
    data = run_export(image, json_path, config, expect, full_probes=full_probes)
    enrich_ocr(data, image)
    verdict = evaluate(data, expect)
    names = template_name_map(data)
    result = RowResult(
        source=str(image),
        stem=stem,
        data=data,
        verdict=verdict,
        json_path=json_path,
        template_names=names,
    )
    if data.get("error"):
        result.error = str(data["error"])
        if write_files:
            md_path = out_dir / f"{stem}.md"
            md_path.write_text(
                render_markdown(
                    data,
                    verdict,
                    None,
                    names,
                    title=str(image),
                ),
                encoding="utf-8",
            )
            result.md_path = md_path
        return result
    if write_files:
        png_path = out_dir / f"{stem}.annotated.png"
        result.size_info = annotate(image, png_path, data, verdict, names)
        result.png = png_path
        md_path = out_dir / f"{stem}.md"
        md_path.write_text(
            render_markdown(
                data,
                verdict,
                result.size_info,
                names,
                title=Path(stem).name,
            ),
            encoding="utf-8",
        )
        result.md_path = md_path
        data["report"] = {
            "image_path": str(image),
            "expected": verdict.get("expected"),
            "expected_raw": verdict.get("expected_raw"),
            "result": verdict["result"],
            "pass": verdict["pass"],
            "differences": verdict["differences"],
            "failure_reason": verdict["failure_reason"],
            "ocr_status": ("OCR(" + ",".join(sorted({str(r.get("backend")) for r in (data.get("ocr_regions") or []) if r.get("backend")})) + ")") if any(r.get("text") for r in (data.get("ocr_regions") or [])) else "UNVERIFIABLE (offline)",
            "annotated_png": str(png_path),
            "markdown": str(md_path),
            "generated_at": datetime.now(timezone.utc).isoformat(),
        }
        json_path.write_text(
            json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8"
        )
    return result


def read_manifest(path: Path) -> list[dict[str, str]]:
    with path.open(encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle, delimiter="\t")
        if not reader.fieldnames or "path" not in reader.fieldnames:
            raise SystemExit(f"manifest 缺少 path 列：{path}")
        return [dict(row) for row in reader]


def best_score(data: dict[str, Any]) -> float | None:
    scores: list[float] = []
    for probe in (data.get("template_probes") or []) + (data.get("diagnostic_probes") or []):
        score = probe.get("score")
        if score is not None:
            scores.append(float(score))
    return max(scores) if scores else None


def batch(args: argparse.Namespace) -> int:
    manifest = Path(args.manifest) if args.manifest else DEFAULT_MANIFEST
    if not manifest.is_absolute():
        manifest = ROOT / manifest
    rows = read_manifest(manifest)
    label_filter = args.label
    status_filter = args.status
    if not label_filter and not status_filter:
        # 与示例保持一致：只跑 confirmed，避免默认扫 5600+ 行非状态语料。
        status_filter = "confirmed"
    selected: list[dict[str, str]] = []
    for row in rows:
        if label_filter and row.get("expected_label") != label_filter:
            continue
        if status_filter and row.get("label_status") != status_filter:
            continue
        selected.append(row)
    if args.limit is not None:
        selected = selected[: max(0, args.limit)]
    if not selected:
        eprint("没有匹配的 manifest 行。")
        return 2
    out_dir = Path(args.out)
    if not out_dir.is_absolute():
        out_dir = Path.cwd() / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    used_stems: set[str] = set()
    results: list[RowResult] = []
    eprint(
        f"批量报告：{len(selected)} 张（manifest={manifest}，"
        f"label={label_filter or '—'}，status={status_filter or '—'}）"
    )
    for index, row in enumerate(selected, start=1):
        rel = row.get("path", "")
        image = Path(rel)
        if not image.is_absolute():
            image = ROOT / image
        stem = unique_output_stem(image.stem, rel, used_stems)
        expect = row.get("expected_label") or None
        try:
            result = process_one(
                image,
                out_dir,
                stem=stem,
                expect=expect,
                config=Path(args.config),
                write_files=True,
            )
        except Exception as error:  # noqa: BLE001 - 单张失败不能中止批量
            result = RowResult(
                source=str(image),
                stem=stem,
                data={},
                verdict={
                    "result": "FAIL",
                    "pass": False,
                    "differences": [],
                    "failure_reason": str(error),
                },
                error=str(error),
            )
            (out_dir / f"{stem}.json").write_text(
                json.dumps(
                    {
                        "schema": "miliastra-ui-report/1",
                        "image": {"path": rel},
                        "error": str(error),
                    },
                    ensure_ascii=False,
                    indent=2,
                ),
                encoding="utf-8",
            )
        results.append(result)
        mark = "PASS" if result.verdict.get("pass") else "FAIL"
        eprint(f"  [{index}/{len(selected)}] {mark} {rel}" + (f" ({result.error})" if result.error else ""))
    index_path = out_dir / "index.md"
    write_index(index_path, manifest, label_filter, status_filter, results)
    passed = sum(1 for result in results if result.verdict.get("pass"))
    eprint(
        f"批量完成：PASS {passed} / FAIL {len(results) - passed} -> {index_path}"
    )
    return 0 if passed == len(results) else 1


def write_index(
    path: Path,
    manifest: Path,
    label_filter: str | None,
    status_filter: str | None,
    results: list[RowResult],
) -> None:
    ordered = sorted(
        results,
        key=lambda result: (
            0 if result.verdict.get("result") == "FAIL" else 1,
            result.source,
        ),
    )
    passed = sum(1 for result in results if result.verdict.get("pass"))
    lines = [
        "# UI 检测批量报告",
        "",
        f"- manifest：`{manifest}`",
        f"- 过滤：label={label_filter or '—'} / status={status_filter or '—'}",
        f"- 总数：{len(results)}；PASS：{passed}；FAIL：{len(results) - passed}",
        f"- 生成时间：{datetime.now(timezone.utc).isoformat()}",
        "",
        "无 `--expect` 时 PASS 只表示分类器给出确定界面；批量默认用 manifest 的 "
        "`expected_label` 做对照。",
        "",
        "| 文件名 | label | 结果 | 模板最高分 | OCR 区域数 | 失败原因 |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for result in ordered:
        data = result.data
        label = data.get("label") or "—"
        result_text = result.verdict.get("result", "FAIL")
        score = best_score(data) if data else None
        ocr_count = len(data.get("ocr_regions") or []) if data else 0
        reason = result.verdict.get("failure_reason") or ""
        link = result.md_path.name if result.md_path else ""
        source_path = Path(result.source)
        try:
            name = str(source_path.resolve().relative_to(ROOT))
        except ValueError:
            name = str(source_path)
        name_cell = f"[{esc_md(name)}]({link})" if link else esc_md(name)
        if result.error and "解码失败" in result.error:
            result_text = "SKIP(decode)"
        lines.append(
            f"| {name_cell} | {esc_md(label)} | **{result_text}** | "
            f"{fmt_score(score)} | {ocr_count} | {esc_md(reason)} |"
        )
    lines.append("")
    lines.append("## 路径")
    lines.append("")
    for result in ordered:
        bits = [f"- `{result.source}`"]
        if result.png:
            bits.append(f"[annotated]({result.png.name})")
        if result.md_path:
            bits.append(f"[md]({result.md_path.name})")
        if result.json_path:
            bits.append(f"[json]({result.json_path.name})")
        lines.append(" · ".join(bits))
    lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def single(args: argparse.Namespace) -> int:
    image = Path(args.image)
    if not image.is_absolute():
        image = Path.cwd() / image
    if not image.exists():
        eprint(f"找不到图片：{image}")
        return 2
    out_dir = Path(args.out)
    if not out_dir.is_absolute():
        out_dir = Path.cwd() / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    used_stems: set[str] = set()
    stem = unique_output_stem(image.stem, str(image), used_stems)
    result = process_one(
        image,
        out_dir,
        stem=stem,
        expect=args.expect,
        config=Path(args.config),
        write_files=True,
        full_probes=True,
    )
    status = "PASS" if result.verdict.get("pass") else "FAIL"
    eprint(f"结果：{status}")
    if result.error:
        eprint(f"错误：{result.error}")
    for output in (result.png, result.json_path, result.md_path):
        if output:
            print(output)
    return 0 if result.verdict.get("pass") else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="导出界面检测证据并渲染 annotated PNG / JSON / Markdown 报告。"
    )
    parser.add_argument("image", nargs="?", help="单张截图路径")
    parser.add_argument("--manifest", help=f"批量 manifest（默认 {DEFAULT_MANIFEST}）")
    parser.add_argument("--label", help="按 manifest expected_label 精确过滤")
    parser.add_argument("--status", help="按 manifest label_status 精确过滤")
    parser.add_argument("--limit", type=int, help="批量最多处理多少张")
    parser.add_argument("--expect", help="期望标签（中文 manifest 名或生产 kind/label）")
    parser.add_argument("--out", default="target/ui-reports", help="输出目录")
    parser.add_argument("--no-ocr", action="store_true", help="跳过离线 OCR（批量更快）")
    parser.add_argument(
        "--all-probes",
        action="store_true",
        default=True,
        help="画出全部诊断探针（默认开启，保留该参数仅为兼容）",
    )
    parser.add_argument(
        "--production-only",
        action="store_true",
        help="只画生产探针，不画诊断探针（报告仍然保留诊断数据）",
    )
    parser.add_argument(
        "--config",
        default=str(DEFAULT_CONFIG),
        help=f"分类器配置（默认 {DEFAULT_CONFIG}）",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    global OCR_ENABLED, DRAW_DIAGNOSTICS
    OCR_ENABLED = not args.no_ocr
    DRAW_DIAGNOSTICS = not args.production_only
    if args.image and (args.manifest or args.label or args.status or args.limit is not None):
        parser.error("单张模式不能与 --manifest/--label/--status/--limit 同时使用")
    if not args.image:
        return batch(args)
    return single(args)


if __name__ == "__main__":
    sys.exit(main())
