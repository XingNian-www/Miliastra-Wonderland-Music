#!/usr/bin/env python3
"""把 AI 记忆的归属键从「昵称」迁移到「UUID」。

背景：记忆键原来是玩家昵称，而外部聊天源给的是玩家**当前显示名**（随时能改），
同一个人的记忆会分裂成多份。UUID 是唯一身份码，跨改名稳定。

用法：
    python3 scripts/migrate-ai-memory-to-uuid.py --db <playback.sqlite3> [--apply]

默认只打印计划（dry-run）。加 --apply 才真正写库，且会先备份。

映射规则不写死：从 config_sections 的 identity 段读取，
按 nickname / note 两个字段匹配，与运行时 canonical_name 的口径一致。
未匹配到的 actor 视为路人，保持昵称键不动。
"""
import argparse
import json
import os
import shutil
import sqlite3
import sys
import time


def load_identity(conn):
    row = conn.execute(
        "SELECT value_json FROM config_sections WHERE section = 'identity'"
    ).fetchone()
    if not row:
        return []
    return json.loads(row[0]).get("mappings", [])


def build_index(mappings):
    """nickname 与 note 都指向同一个 UUID；note 可能为空。"""
    by_key = {}
    for mapping in mappings:
        uuid = mapping.get("id")
        if not uuid:
            continue
        nickname = (mapping.get("nickname") or "").strip()
        note = (mapping.get("note") or "").strip()
        if nickname:
            by_key[nickname] = uuid
        if note:
            by_key.setdefault(note, uuid)
    return by_key


TABLES = ["ai_chat_turns", "ai_chat_facts", "ai_chat_summaries", "ai_chat_actor_names"]


def collect_actors(conn):
    actors = set()
    for table, column in [
        ("ai_chat_turns", "actor"),
        ("ai_chat_facts", "actor"),
        ("ai_chat_actor_names", "actor"),
    ]:
        try:
            for (value,) in conn.execute(f"SELECT DISTINCT {column} FROM {table}"):
                if value:
                    actors.add(value)
        except sqlite3.OperationalError:
            pass
    try:
        for (scope,) in conn.execute("SELECT scope FROM ai_chat_summaries"):
            if scope and scope.startswith("actor:"):
                actors.add(scope[len("actor:"):])
    except sqlite3.OperationalError:
        pass
    return actors


def plan(conn, by_key):
    moves = []
    skipped = []
    for actor in sorted(collect_actors(conn)):
        uuid = by_key.get(actor)
        if uuid and uuid != actor:
            moves.append((actor, uuid))
        elif not uuid:
            skipped.append(actor)
    return moves, skipped


def apply_moves(conn, moves):
    """一个 actor 一个 actor 地搬。目标 UUID 可能已有数据，全部走 UPSERT 语义。"""
    for old, new in moves:
        # 摘要：scope 形如 actor:<名字>
        conn.execute(
            "UPDATE OR REPLACE ai_chat_summaries SET scope = ? WHERE scope = ?",
            (f"actor:{new}", f"actor:{old}"),
        )
        # 事实：主键是 (actor, fact)，先删掉目标侧已存在的同文事实再搬。
        conn.execute(
            "DELETE FROM ai_chat_facts WHERE actor = ? AND fact IN "
            "(SELECT fact FROM ai_chat_facts WHERE actor = ?)",
            (new, old),
        )
        conn.execute(
            "UPDATE ai_chat_facts SET actor = ? WHERE actor = ?", (new, old)
        )
        # 问答：无唯一约束，直接改归属。
        conn.execute(
            "UPDATE ai_chat_turns SET actor = ? WHERE actor = ?", (new, old)
        )
        # 曾用名：主键是 (actor, name)，冲突时保留目标侧。
        conn.execute(
            "DELETE FROM ai_chat_actor_names WHERE actor = ? AND name IN "
            "(SELECT name FROM ai_chat_actor_names WHERE actor = ?)",
            (new, old),
        )
        conn.execute(
            "UPDATE ai_chat_actor_names SET actor = ? WHERE actor = ?", (new, old)
        )


def main():
    # Windows 控制台默认 GBK，昵称里的装饰字符会触发 UnicodeEncodeError。
    # 迁移只关心键值，输出用 backslashreplace 兜底即可，不值得为它加依赖。
    sys.stdout.reconfigure(errors="backslashreplace")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()

    if not os.path.exists(args.db):
        sys.exit(f"数据库不存在: {args.db}")

    conn = sqlite3.connect(args.db)
    try:
        by_key = build_index(load_identity(conn))
        print(f"身份映射可识别的键: {len(by_key)} 个")
        moves, skipped = plan(conn, by_key)

        print(f"\n将迁移 {len(moves)} 个 actor：")
        for old, new in moves:
            print(f"  {old!r} -> {new}")

        print(f"\n保持原样（未登记身份，按路人处理）{len(skipped)} 个：")
        for actor in skipped:
            print(f"  {actor!r}")

        if not args.apply:
            print("\n这是 dry-run。确认无误后加 --apply 真正写库。")
            return

        if not moves:
            print("\n没有需要迁移的数据。")
            return

        backup = f"{args.db}.bak-migrate-{time.strftime('%Y%m%d-%H%M%S')}"
        shutil.copy2(args.db, backup)
        print(f"\n已备份: {backup}")

        with conn:
            apply_moves(conn, moves)
        print(f"迁移完成，共 {len(moves)} 个 actor。")
    finally:
        conn.close()


if __name__ == "__main__":
    main()

