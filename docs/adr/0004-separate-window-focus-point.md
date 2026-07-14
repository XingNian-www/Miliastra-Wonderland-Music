# 单独配置窗口安全聚焦点

新版本使用 `window.focus_point` 作为通用的安全聚焦点，不复用 `output.focus_point`。`focus_game` 只表达“让游戏窗口接收键盘输入”，并且只应出现在业务入口或用户显式配置的聚焦原子动作里；自动流程中的返回一级统一使用 ESC 逐级返回，不能再通过点击 `output.focus_point` 或重复点击全局聚焦点来隐式修正界面状态。`output.focus_point` 只保留给手动调试工具和坐标参考使用。
