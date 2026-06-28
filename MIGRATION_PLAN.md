# 迁移状态

目标：做一个 Windows-only Rust exe，替代旧 `main.js` 自动化，并合并旧 `rust-server` 的非 BGI 行为

## 已迁移

- 目标窗口客户区截图和坐标换算
- OCR、模板匹配、聊天消息分块和旧式 OCR 文本合并
- UI 状态检测和聊天区变化触发 OCR
- 命令解析、屏幕锁、邀请序号去重
- 点歌、播放控制、队列、状态、歌词、帮助、大厅检测
- 点歌确认、跳过、换源、AI 匹配兜底
- 邀请、麦克风、非公共大厅邀请确认
- FeelUOwn TCP RPC 客户端
- HTTP/Web 面板主要路由、队列/state/history、AI、通知、active-window/admin-status
- 全局热键和手动调试工具

## 已按要求移除

- BGI 内存监控
- BGI 重启
- BGI restart intent/ready 握手
- `@内存`
- `@重启`
- `/bgi-*`
- `/restart-bgi`

## 保留但降级

- `/restart-admin`：只返回兼容 JSON，不自动 UAC 提权重启
- OCR worker `memory_rebuild_limit_bytes`：保留配置，自动重建尚未实现

## 验证

当前 Linux 环境可运行：

```bash
cargo fmt
cargo check --target x86_64-pc-windows-gnu --features ocr-rs/docsrs
```

最终发布仍需 Windows/MSVC：

```powershell
cargo build --release --target x86_64-pc-windows-msvc
```
