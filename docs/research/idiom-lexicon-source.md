# 内置成语词库来源核验

## 结论

项目内置 [`pwxcoo/chinese-xinhua`](https://github.com/pwxcoo/chinese-xinhua) 的成语文本，固定在提交 [`fe6d6c2e8baa82187f4c96bbe042e43f96c05666`](https://github.com/pwxcoo/chinese-xinhua/tree/fe6d6c2e8baa82187f4c96bbe042e43f96c05666)。该仓库自身的 [LICENSE](https://github.com/pwxcoo/chinese-xinhua/blob/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/LICENSE) 为 MIT，允许复制、修改和再分发，前提是保留版权和许可证声明。

源仓库的 [README](https://github.com/pwxcoo/chinese-xinhua/blob/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/README.md) 明确把 `data/idiom.json` 标为成语数据，并展示了每条记录的 `word` 字段。README 同时说明数据由作者从网上收集整理，并保留侵权删除声明。因此本项目只提取不含释义、例句或拼音的 `word` 字段，并在发布物中保留完整 MIT 归属；这不是对每个上游网站版权状态的独立核验。

## 可复现数据

- 原始文件：[data/idiom.json](https://raw.githubusercontent.com/pwxcoo/chinese-xinhua/fe6d6c2e8baa82187f4c96bbe042e43f96c05666/data/idiom.json)
- 原始记录数：30,895（固定提交下载后解析 JSON 数组得到）。
- 标准化：只保留 `word` 字段中完全由 CJK 汉字组成、长度至少为 2 的值，再排序去重。
- 内置结果：[assets/idioms.txt](../../assets/idioms.txt)，30,345 条，398,399 bytes，SHA-256 为 `c6ae28306826809bdb8181c75dce4e5376951fa161dd56305e75db85c21ce078`（UTF-8 无 BOM、LF 换行）。

词表作为项目资源保存在 `assets/idioms.txt`，发布工作流会复制整个 `assets/` 目录。程序启动时只读取并索引该 UTF-8 文本，不下载数据，也不解析原始 JSON。完整归属和许可证文本见 [THIRD_PARTY_NOTICES.md](../../THIRD_PARTY_NOTICES.md)。
