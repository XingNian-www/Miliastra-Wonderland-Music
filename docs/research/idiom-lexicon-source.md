# 内置成语词库来源核验

## 结论

项目内置 [`pwxcoo/chinese-xinhua`](https://github.com/pwxcoo/chinese-xinhua) 的成语文本，固定在提交 [`fe6d6c2e8baa82187f4c96bbe042e43f96c05666`](https://github.com/pwxcoo/chinese-xinhua/tree/fe6d6c2e8baa82187f4c96bbe042e43f96c05666)。该仓库自身的 [LICENSE](https://github.com/pwxcoo/chinese-xinhua/blob/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/LICENSE) 为 MIT，允许复制、修改和再分发，前提是保留版权和许可证声明。

源仓库的 [README](https://github.com/pwxcoo/chinese-xinhua/blob/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/README.md) 明确把 `data/idiom.json` 标为成语数据。README 同时说明数据由作者从网上收集整理，并保留侵权删除声明。本项目当前词库保存 `成语:来源:解释` 三个字段，并在发布物中保留完整 MIT 归属；这不是对每个上游网站版权状态的独立核验。

## 可复现数据

- 原始文件：[data/idiom.json](https://raw.githubusercontent.com/pwxcoo/chinese-xinhua/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/data/idiom.json)
- 格式：每行保存 `word:derivation:explanation`；只有前两个半角冒号是字段分隔符。
- 标准化：成语键必须完全由 CJK 汉字组成、长度至少为 2；重复键由程序保留第一条。
- 内置结果：[assets/idioms.txt](../../assets/idioms.txt)，49,674 条非注释记录、49,644 个唯一成语键，SHA-256 为 `60a3b05a09ed5b909a1e75f5f3d237637d65cf80ff6eadac2ef8bc852ec51cd0`。

词表作为项目资源保存在 `assets/idioms.txt`，发布工作流会复制整个 `assets/` 目录。程序启动时只读取并索引该 UTF-8 文本，不下载数据，也不解析原始 JSON。完整归属和许可证文本见 [THIRD_PARTY_NOTICES.md](../../THIRD_PARTY_NOTICES.md)。
