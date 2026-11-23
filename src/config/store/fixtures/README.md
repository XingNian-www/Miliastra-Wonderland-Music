# 夹具是测试运行期生成的合成数据，绝不入库

这里的三份 `v3_*.sqlite3` 是 **v3 schema 的合成配置库**，由
`src/config/store/upgrade_tests.rs` 按当前代码生成：

- `v3_minimal.sqlite3`：全部默认值，无用户自定义；
- `v3_customized.sqlite3`：自定义队列上限、占位密钥、确认按钮模板、占位身份映射；
- `v3_dirty.sqlite3`：在 `queue` 段塞 2 条当前 schema 已不声明的脏键。

## 为什么不在仓库里提交二进制

独立审查（`FINAL_REVIEW.md` §4.3-D2）确认历史版本的 `scripts/privacy-filter.py`
只扫 15 种文本后缀、**不扫 `.sqlite3`**：真实夹具里塞多少真实 UID/昵称，CI 都会
报 0 命中，而 Git 历史一旦写入就**不可回退**。所以：

1. 生成函数只用 `AppConfig::default()` + 明显的占位符
   （`示例玩家甲`、`uid-0001`、`token-placeholder`），不读取任何真实配置库；
2. 同目录的 `.gitignore` 忽略 `*.sqlite3`，`git add -A` 收不到生成物；
3. `scripts/privacy-filter.py` 新增「数据型文件禁止入库」检查：即使有人强制
   `git add -f`，`--repo --check` 也会失败并列出文件。

## 重新生成

```bash
UPDATE_CONFIG_FIXTURES=1 cargo test --lib --no-default-features \
  config::store::upgrade::upgrade_tests
```

首次运行（文件缺失时）也会自动生成。`expected/v4_*.json` 是**文本黄金快照**，
可以安全入库；它记录「同一份 v3 输入迁移后应该得到的 v4 sections」，用于发现
迁移行为漂移。改默认配置或迁移规则后，重新生成并人工审阅 `expected/` 的 diff。
