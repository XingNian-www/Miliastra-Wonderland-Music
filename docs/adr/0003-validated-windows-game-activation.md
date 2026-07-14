# Windows 游戏窗口激活必须校验成功

`activate_game` 在 Windows 下只能“尽最大可能激活游戏窗口并校验结果”，不能承诺绕过系统前台窗口限制。实现应依次尝试还原窗口、发送 ALT 输入、调用 `SetForegroundWindow`，必要时使用 `BringWindowToTop` 或短暂 `AttachThreadInput` 作为兜底，最后必须用当前前台窗口所属进程校验是否已经激活目标游戏；如果校验失败，当前业务流程必须中止并输出明确错误，不能继续执行点击、按键或粘贴。
