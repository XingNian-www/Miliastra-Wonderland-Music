// 假数据模式的编译期开关。
//
// 为什么单独一个文件：`api.js` 是唯一出网口，也是 Node 单测会直接 import 的
// 模块。它里面不能出现裸的 `__PANEL_MOCK__` —— 那是 Vite 的 define 常量，
// Node 里不存在（`typeof` 取值才是安全的）。把判定收在这里，两边都能加载：
//   - `npm run build`：__PANEL_MOCK__ = false，整个假数据模块会被 tree-shake 掉；
//   - `npm run build --mode mock`：__PANEL_MOCK__ = true，面板改用内置假响应。
export const MOCK_ENABLED = typeof __PANEL_MOCK__ === 'boolean' ? __PANEL_MOCK__ : false
