#!/usr/bin/env python3
"""把 Rust 侧的配置 schema 快照 + 字段中文名/提示编译成面板假数据。

为什么需要它：面板视觉核验要在**没有游戏进程、没有真后端**的情况下跑起来，
假数据必须与真实 `/config/schema`、`/config` 逐字段同形。手抄 399 个字段
既不可能保持同步，也会在 schema 一变就静默失配（截出来的页面是错的）。
所以这里不手写字段：把已经入库的 schema 快照
(`src/config/schema/schema.snapshot.json`，由 Rust 测试生成并断言) 当作唯一
字段来源，再从声明点把中文 label / hint 取出来。

数据流（单向，不反向写 Rust）：
    schema.snapshot.json  +  src/config/schema/*.rs 的构造函数
        ->  web/src/mock/fixtures.config.js       (被 web/src/mock/index.js import)
    web/src/mock/routes.js  （手写：非 schema 路由的响应）
        ->  web/src/mock/index.js

用法：
    python3 scripts/panel-mock-gen.py            # 重新生成 fixtures.config.js
    python3 scripts/panel-mock-gen.py --check    # 只校验产物是否与源同步（供 CI / make 用）
    python3 scripts/panel-mock-gen.py --json     # 打印统计

退出码：0 = 正常 / 已同步；1 = --check 发现产物过期；2 = 源文件缺失。

字段来源对照（写报告时要引用）：
    section/字段元数据  -> src/config/schema/schema.json 快照 (tests_snapshot.rs 生成)
    label / hint        -> src/config/schema/{general,ui,ai,games,system,playback,mod}.rs
    enum 选项显示名     -> 同上 + src/config/ai.rs 的 AI_PROVIDERS / REASONING_EFFORTS 表
    段默认值            -> 快照 `default`（即 AppConfig::default() 序列化后的值）
    secret 掩码值       -> src/config/store/codec.rs 的 SECRET_MASK
    视觉档案接管的字段  -> src/config/visual.rs 的 managed_paths() 输入清单
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SNAPSHOT = ROOT / "src/config/schema/schema.snapshot.json"
SCHEMA_DIR = ROOT / "src/config/schema"
OUT = ROOT / "web/src/mock/fixtures.config.js"

# Domain 枚举的中文名（src/config/schema/mod.rs 的 Domain::label）。
DOMAIN_LABELS = {
    "recognition": "识别与画面",
    "playback": "播放与点歌",
    "entertainment": "娱乐玩法",
    "interaction": "AI 与交互",
    "system": "系统",
}

# 快照里的占位符：这些不是真实默认值，必须换成假数据能用的替身。
#   <null>        Option 字段无默认值
#   <secret>      secret 字段的默认值被刻意抹掉
#   <string len=N> 提示词过长时的摘要占位
PLACEHOLDER_PREFIX = "<"

# 真实后端在 `/config` 里把 secret 换成这串掩码（src/config/store/codec.rs 的 SECRET_MASK）。
# 假数据必须同形：前端用「文本框是否等于 SECRET_MASK」判断「已保存密钥」，
# 填一个假明文会让页面显示成「用户填了新密钥」。
SECRET_MASK = "\u2022" * 6

# 视觉档案接管的点路径（src/config/visual.rs 的 managed_paths()）。
# 形状是「段 + 字段」，逐条抄自那 10 个 for 循环与 3 个直接 add()。
# 不在 JS 里重复推导：真正的准绳是 `/config/schema` 的 `managedBy`，
# 而那个键由 Rust 按同一份清单一字段一字段算出来。
VISUAL_MANAGED = {
    # templates.<key>（含 startup.templates.<key>）
    *[f"templates.{key}" for key in (
        "blue_marker", "yellow_marker", "pink_marker", "secondary_back", "secondary_hall",
        "secondary_chat_submit", "qianxing_f2_hall_list", "qianxing_f2_invite_player",
        "qianxing_exit_menu", "qianxing_map_label", "qianxing_profile_page_wishlist",
        "qianxing_profile_page_wardrobe", "teyvat_profile_adventure_rank",
        "teyvat_profile_world_level", "teyvat_profile_add_friend",
        "qianxing_profile_domain_level", "qianxing_profile_add_friend",
        "qianxing_profile_view_profile", "main_menu", "world_wish", "invite_view_star",
        "invite_goto_hall", "invite_enter_hall", "friend_panel", "friend_search_panel",
        "friend_more_settings", "friend_block_chat", "friend_blacklist", "confirm_button",
        "confirm_message_random_hall", "confirm_message_return_teyvat", "confirm_message_block",
    )],
    "startup.templates.wonderland_map_star",
    "startup.templates.paimon_menu",
    # thresholds
    "templates.marker_threshold",
    "startup.template_threshold",
    "startup.wonderland_confirm_threshold",
    # regions
    *[f"screen.{key}_rect" for key in (
        "chat", "friend", "secondary_back", "secondary_hall", "secondary_chat_submit",
        "main_menu", "hall_bar", "qianxing_f2_invite", "world_wish", "hall_name",
        "hall_member_count", "hall_time", "hall_member_list",
    )],
    *[f"{section}.{key}_region"
      for section, keys in (
          ("moderation", ("friend_panel", "search_panel", "more_settings", "block_chat", "blacklist", "confirm_button", "confirm_message")),
          ("invite", ("friend_list", "friend_chat", "view_star", "goto_hall", "enter_hall")),
          ("startup", ("enter_game_text", "wonderland_hall_ocr", "main_ui", "wonderland_map_star")),
      )
      for key in keys],
    # points
    "window.focus_point",
    "output.focus_point",
    "output.chat_click_2",
    "moderation.search_input_point",
    "moderation.search_button_point",
    # chat_geometry
    *[f"ocr.{key}" for key in (
        "marker_search_width", "marker_dedupe_min_x", "marker_dedupe_min_y", "marker_dedupe_x",
        "marker_dedupe_y", "text_left_gap", "block_top_padding", "block_bottom_padding",
        "max_block_height", "same_line_y_tolerance", "next_marker_min_gap", "right_padding",
        "change_mean_threshold", "change_pixel_threshold",
    )],
    # capture / matching
    "window.capture_method",
    "screen.warn_on_size_mismatch",
    "templates.panel_downsample_factors",
    *[path for path in (
        "ocr.min_confidence", "ocr.det_max_side_len", "ocr.det_score_threshold",
        "ocr.det_unclip_ratio", "ocr.det_min_area", "ocr.det_box_border",
        "stability.default_count", "stability.ui_state_count",
        "stability.secondary_hall_count", "startup.stable_mean_threshold",
        "startup.stable_changed_ratio_threshold", "moderation.stable_vote_samples",
        "moderation.required_vote_margin",
    )],
    # managed_paths() 的固定前缀
    "visual.name",
    "window.content_width",
    "window.content_height",
    "screen.expected_width",
    "screen.expected_height",
}

# Enum 选项的显示名。后端 `/config/schema` 回的是 {value,label} 对，只给 value
# 会让下拉显示成「(未知 openai)」——截出来的页面与真实页面不同。
# 这里只补「值 -> 显示名」，取值清单仍以快照为准（多出来的值会被忽略）。
ENUM_LABELS = {
    "mimo": "小米 MiMo",
    "openai": "OpenAI",
    "deepseek": "DeepSeek",
    "custom": "自定义网关",
    "unset": "不指定（服务端默认）",
    "minimal": "最低",
    "low": "低",
    "medium": "中",
    "high": "高",
    "reject": "拒绝",
    "allow": "放行",
    "error": "错误",
    "warn": "警告",
    "info": "信息",
    "debug": "调试",
    "trace": "跟踪",
    "gdi": "GDI 窗口采集",
    "print_window": "PrintWindow 窗口绘制",
    "desktop": "桌面可见区域",
    "wgc": "Windows Graphics Capture",
    "fallback_ocr": "回退 OCR",
    "restart_game": "重启游戏",
    "responses": "Responses（推荐）",
    "chat_completions": "Chat Completions",
    "prefix": "以关键词开头",
    "contains": "包含关键词",
    "everyone": "所有人",
    "friend": "好友及以上",
    "admin": "管理员及以上",
    "owner": "仅所有者",
    "auto": "每轮自动附图",
    "tool": "仅 look 工具",
    "both": "自动附图 + look 工具",
    "primary_screen": "整个一级大厅",
    "append": "补充一句（模板为主）",
    "rewrite": "改写整条（AI 为主）",
    "wav": "WAV",
    "ai_chat": "仅 AI 聊天",
}


def section_for_line(text: str, offset: int) -> str:
    """`offset` 所在的 `*_section()` 函数名（去掉 `_section` 后缀即段名）。

    为什么必须按函数分段：schema 里 `model` / `enabled` / `backend_priority` 这类
    字段名在多个段里重复出现。只按「段内路径」归并的话，后读到的段会把前面
    覆盖掉——实际症状是 AI 配置页的 `ai.model` 提示词显示成「海龟汤专用模型」。
    真实响应里每个字段的 label/hint 都来自它自己的声明点，假数据也必须如此。
    """
    current = ""
    for match in re.finditer(r"fn (\w+)_section\(\)", text):
        if match.start() <= offset:
            current = match.group(1)
        else:
            break
    return current


def scan_field_meta(text: str) -> dict[str, dict[str, str]]:
    """从 schema 声明里取每个字段的中文 label 与 hint，键是「段名.段内路径」。

    为什么用配平括号扫描而不是正则：构造函数实参里有嵌套的 `int(1, 65535)`、
    `enum_of(&[("a","b")])`、甚至跨行字符串，正则会在第一层 `)` 处截断，
    799 个字段里会漏掉 600 多个。扫描器只认顶层逗号分出来的字符串实参。
    """
    meta: dict[str, dict[str, str]] = {}
    needle = "ConfigFieldSchema::"
    cursor = 0
    while True:
        start = text.find(needle, cursor)
        if start < 0:
            break
        open_paren = text.find("(", start)
        if open_paren < 0:
            break
        close_paren = balanced_close(text, open_paren)
        if close_paren < 0:
            break
        constructor = text[start + len(needle):open_paren].strip()
        args = top_level_string_args(text[open_paren + 1:close_paren])
        # 约定：第 1 个字符串实参是段内路径，第 2 个是中文名，最后一个才是 hint；
        # db_live_grouped 末位是分组名，所以 hint 取倒数第二个字符串实参。
        if len(args) >= 3:
            path, label = args[0], args[1]
            hint = args[-2] if constructor == "db_live_grouped" else args[-1]
            section = section_for_line(text, start)
            key = f"{section}.{path}" if section else path
            meta.setdefault(key, {"ctor": constructor, "label": label, "hint": hint})
        cursor = open_paren + 1
    return meta


def balanced_close(text: str, open_index: int) -> int:
    """返回与 text[open_index] 配对的右括号下标；跳过字符串字面量里的括号。"""
    depth = 0
    index = open_index
    in_string = False
    escaped = False
    while index < len(text):
        char = text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        elif char == '"':
            in_string = True
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return -1


def top_level_string_args(body: str) -> list[str]:
    """取实参列表里**顶层**的字符串字面量，忽略嵌套在 `vec![]`/`int(..)` 里的。"""
    out: list[str] = []
    depth = 0
    index = 0
    while index < len(body):
        char = body[index]
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        elif char == '"' and depth == 0:
            value, index = read_string(body, index)
            out.append(value)
            continue
        index += 1
    return out


def read_string(text: str, open_index: int) -> tuple[str, int]:
    out: list[str] = []
    index = open_index + 1
    while index < len(text):
        char = text[index]
        if char == "\\" and index + 1 < len(text):
            nxt = text[index + 1]
            out.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\"}.get(nxt, nxt))
            index += 2
            continue
        if char == '"':
            return "".join(out), index + 1
        out.append(char)
        index += 1
    return "".join(out), index


def substitute(value):
    """把快照里的占位符换成假数据可用的值。

    `<secret>` 换成看起来像密钥的假串（面板只做展示与掩码判断，不走真实请求）；
    `<string len=N>` 换成长度接近的假提示词，避免表单高度被一个 12 字符的
    占位符压扁——布局核验测的就是真实高度。
    """
    if isinstance(value, str) and value.startswith(PLACEHOLDER_PREFIX):
        if value == "<null>":
            return None
        if value == "<secret>":
            return SECRET_MASK
        match = re.match(r"<string len=(\d+)>", value)
        if match:
            length = int(match.group(1))
            line = "这是面板假数据用的提示词占位文本，用于撑出真实行高。"
            return (line * (length // len(line) + 1))[:length]
        return None
    if isinstance(value, list):
        return [substitute(item) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item) for key, item in value.items()}
    return value


def kind_with_labels(kind: dict) -> dict:
    """把 enum 的 `values` 从裸字符串补成 [{value,label}]。

    真实 `/config/schema` 就回对象数组（src/interfaces/http/routes/config.rs 的
    kind_json）；快照为了 diff 稳定只存了 value 字符串。少了 label，下拉框
    会把值原样显示成「(未知 openai)」——页面看着像坏了，其实是数据形状不对。
    """
    if kind.get("type") != "enum":
        return kind
    return {
        "type": "enum",
        "values": [
            {"value": value, "label": ENUM_LABELS.get(value, value)}
            for value in kind.get("values", [])
        ],
    }


def build_section_value(fields: list[dict]) -> dict:
    """按字段点路径把默认值铺成 `/config` 的 `sections[段名]` 对象。

    真实 `/config` 返回的是脱敏后的整段配置对象，字段层级与 `AppConfig` 结构
    一一对应；这里用同一个 schema 的 `default` 铺回去，保证前端读
    `sections[段].路径` 时每一层都存在（缺层会让页面报 undefined 错误）。
    """
    root: dict = {}
    for field in fields:
        path = field["path"]
        segments = path.split(".")[1:]  # 去掉段名前缀
        if not segments:
            continue
        cursor = root
        for segment in segments[:-1]:
            if not isinstance(cursor.get(segment), dict):
                cursor[segment] = {}
            cursor = cursor[segment]
        cursor[segments[-1]] = substitute(field["default"])
    return root


def build() -> dict:
    snapshot = json.loads(SNAPSHOT.read_text(encoding="utf-8"))
    meta: dict[str, dict[str, str]] = {}
    for path in sorted(SCHEMA_DIR.glob("*.rs")):
        if path.name == "tests_snapshot.rs":
            continue
        meta.update(scan_field_meta(path.read_text(encoding="utf-8")))

    sections: list[dict] = []
    values: dict[str, dict] = {}
    missing_meta: list[str] = []
    for section in snapshot["sections"]:
        name = section["name"]
        out_fields = []
        for field in section["fields"]:
            inner = field["path"].split(".", 1)[1]
            # 先按「段名.段内路径」精确取，失败再退回段内路径：
            # `mod.rs` 里 visual/command_access 等段不是 *_section() 函数，
            # 它们直接写在 config_sections() 的 vec! 里，没有段作用域可归。
            found = meta.get(field["path"]) or meta.get(inner)
            if not found:
                missing_meta.append(field["path"])
            out_fields.append(
                {
                    "path": field["path"],
                    "label": (found or {}).get("label", inner),
                    "hint": (found or {}).get("hint", ""),
                    "kind": kind_with_labels(field["kind"]),
                    "effect": field["effect"],
                    "source": field["source"],
                    "group": field["group"],
                    "nullable": field["nullable"],
                    "optionalParent": field["optionalParent"],
                    "managedBy": "visual" if field["path"] in VISUAL_MANAGED else None,
                    "default": substitute(field["default"]),
                }
            )
        sections.append(
            {
                "name": name,
                "label": section["label"],
                "domain": section["domain"],
                "domainLabel": DOMAIN_LABELS.get(section["domain"], section["domain"]),
                "order": section["order"],
                "fields": out_fields,
            }
        )
        values[name] = build_section_value(out_fields)

    return {
        "_generated_by": "scripts/panel-mock-gen.py（请勿手改；改 Rust schema 后重跑）",
        "_sources": [
            "src/config/schema/schema.snapshot.json",
            "src/config/schema/*.rs",
        ],
        "schemaVersion": snapshot["schemaVersion"],
        "sections": sections,
        "values": values,
        "missingMeta": missing_meta,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="只校验产物是否与源同步")
    parser.add_argument("--json", action="store_true", help="打印统计而不是写文件")
    args = parser.parse_args()

    if not SNAPSHOT.is_file() or not SCHEMA_DIR.is_dir():
        print(f"::error::找不到配置 schema 源：{SNAPSHOT} / {SCHEMA_DIR}（请在仓库根目录执行）", file=sys.stderr)
        return 2

    payload = build()
    # 产物写成 .js 模块而不是 .json：Vite 与 node:test 都能直接 import，
    # 不需要 import attribute（Node 的 .json import 必须带 `with {type:'json'}`，
    # 那会让同一个文件在两种运行环境里要求不同写法）。
    text = "// 由 scripts/panel-mock-gen.py 生成，请勿手改；改 Rust schema 后重跑。\n"
    text += "export default " + json.dumps(payload, ensure_ascii=False, indent=2) + "\n"
    if payload["missingMeta"]:
        # 不是致命错误：字段仍带默认 label（段内路径），页面不会崩；
        # 但要打印出来，提醒「这些字段的中文名取不到，可能需要更新扫描器」。
        print(f"提醒：{len(payload['missingMeta'])} 个字段取不到中文名，已退回段内路径：", file=sys.stderr)
        for path in payload["missingMeta"][:10]:
            print(f"  - {path}", file=sys.stderr)

    if args.json:
        print(
            json.dumps(
                {
                    "sections": len(payload["sections"]),
                    "fields": sum(len(item["fields"]) for item in payload["sections"]),
                    "missingMeta": len(payload["missingMeta"]),
                    "bytes": len(text.encode("utf-8")),
                },
                ensure_ascii=False,
            )
        )
        return 0

    if args.check:
        if not OUT.is_file():
            print(f"::error::缺少假数据产物 {OUT.relative_to(ROOT)}；请运行 python3 scripts/panel-mock-gen.py", file=sys.stderr)
            return 1
        current = OUT.read_text(encoding="utf-8")
        if current != text:
            print(
                f"::error::{OUT.relative_to(ROOT)} 与 Rust schema 不同步；"
                "请运行 python3 scripts/panel-mock-gen.py 重新生成",
                file=sys.stderr,
            )
            return 1
        print(f"假数据与 Rust schema 同步：{OUT.relative_to(ROOT)}")
        return 0

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(text, encoding="utf-8")
    total = sum(len(item["fields"]) for item in payload["sections"])
    print(f"已生成 {OUT.relative_to(ROOT)}：{len(payload['sections'])} 段 / {total} 字段")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
