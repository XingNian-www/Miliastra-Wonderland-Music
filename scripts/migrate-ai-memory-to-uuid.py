#!/usr/bin/env python3
"""把 AI 记忆的归属键从「昵称」迁移到「UUID」。

背景：记忆键原来是玩家昵称，而外部聊天源给的是玩家**当前显示名**（随时能改），
同一个人的记忆会分裂成多份。UUID 是唯一身份码，跨改名稳定。

用法：
    python3 scripts/migrate-ai-memory-to-uuid.py --db <playback.sqlite3> [--apply]

默认只打印计划（dry-run）。加 --apply 才真正写库，且会先备份。

备份与占用检查：
  - 备份用 `VACUUM INTO` 生成一致快照，**不是**复制主库文件。库开着 WAL 时
    已提交数据可能还在 `-wal` 里没 checkpoint，`shutil.copy2` 会漏掉它
    （同样的理由写在 `src/config/store/upgrade.rs` 的 `backup_before_schema_change`）。
  - 备份前先确认没有别的进程占着这个库：拿不到 `BEGIN IMMEDIATE` 就说明有写事务
    或另一个迁移进程在跑，此时直接退出，不动数据。

冲突处理（绝不删数据）：
  四张表的目标键都可能已经存在行。旧实现用 `UPDATE OR REPLACE`（SQLite 的
  OR REPLACE = 冲突时**删掉目标行**），无提示地丢掉目标侧已有数据。
  现在一律「先查目标是否存在 → 存在则合并或跳过 → 打印冲突」，不做任何覆盖删除：
    - `ai_chat_summaries`（scope TEXT PRIMARY KEY）：目标 scope 已存在时不合并
      （两段摘要是独立生成的，拼接会得到一段语义不连贯的文本，模型读起来反而更差），
      保留目标、跳过来源，并在报告里打印；来源行原样留着，不会丢。
    - `ai_chat_facts`（PK(actor, fact)）：目标已有同一条事实 → 删掉**来源侧**的重复行
      （目标侧那条是等价的，信息不丢），其余行改归属。
    - `ai_chat_actor_names`（PK(actor, name)）：同上，删来源侧重复的曾用名。
    - `ai_chat_turns`：无唯一约束，直接改归属。
    - `ai_chat_hall_turns.actor_key`：大厅行的受众键，同样按 actor 映射改写。

映射规则不写死：从 config_sections 的 identity 段读取。JSON 键名沿用旧名，
对应关系是 nickname=备注串(remark)、note=软件显示名(display_name)、real_nickname=真实昵称。
三者都当作 actor 的候选名，与运行时 canonical_actor_name / friend_search_names 的口径一致。
未匹配到的 actor 视为路人，保持昵称键不动。
"""
import argparse
import json
import os
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
    """三个名字都指向同一个 UUID；显示名与真实昵称可能为空。"""
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
        real = (mapping.get("real_nickname") or "").strip()
        if real:
            by_key.setdefault(real, uuid)
    return by_key


TABLES = ["ai_chat_turns", "ai_chat_facts", "ai_chat_summaries", "ai_chat_actor_names"]


def collect_actors(conn):
    actors = set()
    for table, column in [
        ("ai_chat_turns", "actor"),
        ("ai_chat_facts", "actor"),
        ("ai_chat_actor_names", "actor"),
        # 大厅行的受众键：玩家行是提问者、BOT 行是回复对象。
        # 漏了它，大厅里「对谁说的」会一直停在旧昵称上；而
        # `hall_context` 的过滤条件是 `actor_key <> ?1 OR actor_key = ''`，
        # 改名后这个人的大厅记忆取不回来，「遗忘」也删不掉（见
        # src/features/ai_chat/memory.rs 的 hall_context）。
        ("ai_chat_hall_turns", "actor_key"),
    ]:
        try:
            for (value,) in conn.execute(f"SELECT DISTINCT {column} FROM {table}"):
                if value:
                    actors.add(value)
        except sqlite3.OperationalError:
            # 旧库可能还没有这张表/这一列（schema 迁移未跑）；这类键无从迁移。
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
    """一个 actor 一个 actor 地搬，**绝不覆盖删除**目标侧已有数据。

    返回 (conflicts, merged)：conflicts 是需要人工看一眼的摘要冲突，merged 是
    合并掉的重复事实/曾用名条数。旧实现第一条 UPDATE 就是 `OR REPLACE`，
    而 `ai_chat_summaries.scope` 是主键 —— SQLite 的 OR REPLACE 在冲突时删掉
    目标行再改名，目标侧摘要直接消失（审阅 B21-P0-2）。这里改成先查后改。
    """
    conflicts = []
    merged = 0
    for old, new in moves:
        # 摘要：scope 形如 actor:<名字>，是 TEXT PRIMARY KEY。
        source_scope = f"actor:{old}"
        target_scope = f"actor:{new}"
        try:
            target_exists = conn.execute(
                "SELECT 1 FROM ai_chat_summaries WHERE scope = ?", (target_scope,)
            ).fetchone()
        except sqlite3.OperationalError:
            # 旧库还没有这张表（schema 升级没跑过）：没有摘要可迁。
            target_exists = None
        else:
            if target_exists:
                # 两段摘要是各自独立压出来的，拼接会得到语义不连贯的文本；
                # 保留目标侧、跳过来源侧，来源行原样留在旧 scope 上（信息不丢）。
                conflicts.append(
                    (
                        old,
                        new,
                        "ai_chat_summaries",
                        f"{target_scope} 已存在，保留目标摘要，未迁移来源摘要",
                    )
                )
            else:
                conn.execute(
                    "UPDATE ai_chat_summaries SET scope = ? WHERE scope = ?",
                    (target_scope, source_scope),
                )

        # 事实：主键是 (actor, fact)。只删**来源侧**与目标重复的等价事实，再做改名。
        # 顺序不能反：先改名再删会把刚搬过去的那批当重复删掉。
        try:
            merged += conn.execute(
                "DELETE FROM ai_chat_facts WHERE actor = ? AND fact IN "
                "(SELECT fact FROM ai_chat_facts WHERE actor = ?)",
                (old, new),
            ).rowcount
            conn.execute(
                "UPDATE ai_chat_facts SET actor = ? WHERE actor = ?", (new, old)
            )
        except sqlite3.OperationalError:
            pass
        # 问答：无唯一约束，直接改归属。
        try:
            conn.execute(
                "UPDATE ai_chat_turns SET actor = ? WHERE actor = ?", (new, old)
            )
        except sqlite3.OperationalError:
            pass
        # 大厅行：受众键与 actor 同口径；空串是升级前的旧行（没有归属），保持原样。
        try:
            conn.execute(
                "UPDATE ai_chat_hall_turns SET actor_key = ? WHERE actor_key = ?",
                (new, old),
            )
        except sqlite3.OperationalError:
            # 旧库没有这张表/这一列，跳过即可（不是错误）。
            pass
        # 曾用名：主键是 (actor, name)。同样只删来源侧的重复名。
        try:
            merged += conn.execute(
                "DELETE FROM ai_chat_actor_names WHERE actor = ? AND name IN "
                "(SELECT name FROM ai_chat_actor_names WHERE actor = ?)",
                (old, new),
            ).rowcount
            conn.execute(
                "UPDATE ai_chat_actor_names SET actor = ? WHERE actor = ?", (new, old)
            )
        except sqlite3.OperationalError:
            pass
    return conflicts, merged


def backup_database(conn, db_path):
    """用 `VACUUM INTO` 生成一致快照备份，返回备份路径。

    为什么不是 `shutil.copy2`：库开着 WAL，已提交但没 checkpoint 的数据还在
    `-wal` 里，直接复制主库文件会漏掉它（`src/config/store/upgrade.rs` 的
    `backup_before_schema_change` 已写明同一理由）。`VACUUM INTO` 由 SQLite
    在同一连接上生成一致快照，WAL 内容一并落进备份。

    `VACUUM INTO` 要求目标文件**不存在**：名字带时间戳，已存在就追加序号。
    """
    base = f"{db_path}.bak-migrate-{time.strftime('%Y%m%d-%H%M%S')}"
    backup = base
    for index in range(1, 1000):
        if not os.path.exists(backup):
            break
        backup = f"{base}-{index}"
    else:
        raise SystemExit(f"备份文件名冲突过多，请手动清理: {base}*")

    # 参数化避免路径里的引号破坏 SQL。
    try:
        conn.execute("VACUUM INTO ?", (backup,))
    except sqlite3.OperationalError as error:
        raise SystemExit(
            f"VACUUM INTO 备份失败（{error}）。未做任何修改，请确认路径可写且磁盘空间足够: {backup}"
        )
    # VACUUM INTO 在有未完成事务时不可用；顺手 checkpoint 让主库文件本身也自洽。
    try:
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    except sqlite3.OperationalError:
        pass
    return backup


def ensure_not_in_use(conn):
    """确认没有别的连接正占着这个库的写锁。

    迁移是「读全量 → 算映射 → 批量改归属」的多步操作：如果此刻应用正在运行
    （或另一个迁移进程在跑），它会在中途写入，映射与数据就不同步。
    拿 `BEGIN IMMEDIATE` 是判断「有没有活跃写者」最直接的办法：
    成功即立即回滚（不真开事务），失败说明有别的写事务在跑，直接退出。

    **必须用 timeout=0**：sqlite3 默认 busy_timeout 是 5 秒，会先静默等待 ——
    实测「另一个进程持有写事务 6 秒」时，默认超时会一直等到对方释放然后成功，
    等于这条保护没生效。这里要的是「现在有没有人占着」，不是「等一会儿会不会有」。
    """
    original = conn.isolation_level
    conn.isolation_level = None  # 手动管理事务，避免 execute 自带隐式事务干扰
    try:
        conn.execute("PRAGMA busy_timeout = 0")
        try:
            conn.execute("BEGIN IMMEDIATE")
        except sqlite3.OperationalError as error:
            raise SystemExit(
                f"数据库被占用（{error}）。请先关闭正在运行的播放器再执行 --apply，"
                "避免迁移写一半被覆盖。"
            )
        conn.execute("ROLLBACK")
    finally:
        conn.isolation_level = original


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

        # 先确认没有别的进程在写这个库，再备份、再改。
        ensure_not_in_use(conn)
        backup = backup_database(conn, args.db)
        print(f"\n已备份: {backup}")

        with conn:
            conflicts, merged = apply_moves(conn, moves)
        print(f"迁移完成，共 {len(moves)} 个 actor。")
        if merged:
            print(f"按主键合并掉来源侧的重复行 {merged} 条（目标侧等价内容保留）。")
        if conflicts:
            print(f"\n有 {len(conflicts)} 处冲突被跳过（**目标侧数据完整保留，来源侧也原样留着**）：")
            for old, new, table, detail in conflicts:
                print(f"  {old!r} -> {new} @ {table}: {detail}")
            print("  处理办法：确认哪一段摘要该留下，手工合并后再删掉多余的那行；脚本不会替你覆盖。")
    finally:
        conn.close()


if __name__ == "__main__":
    main()

