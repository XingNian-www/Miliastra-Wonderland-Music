# UI 自动化内核只公开完整类型化契约

第一阶段向业务与观察模块公开 `EstablishResidency`、`SendHallBatch`、`SendFriendDeliveries`、`ExecuteInvite`、`ExecuteModeration`、`ReadHallInfo`、`DetectPublicHall`、`ToggleMicrophone`、彼此独立的 `EnterGame` 与 `EnterWonderland`、`ProcessSecondaryUnread`，以及只供自定义工作流机械段使用的 `CustomActionPlan`。打开好友会话、发送当前聊天、返回一级、点击大厅模板、模板或 OCR 等待、截图、点击、按键和粘贴只能作为内核子例程或原子能力；Web 诊断仍可按最低调度优先级提交单个原子操作，但不能组合或绕过调度。接受维护多个独立请求与结果类型的成本，是为了让每个 interface 隐藏完整机械事务，并防止再次形成包含全部业务操作的万能 UI 枚举。
