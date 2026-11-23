// 由 scripts/panel-mock-gen.py 生成，请勿手改；改 Rust schema 后重跑。
export default {
  "_generated_by": "scripts/panel-mock-gen.py（请勿手改；改 Rust schema 后重跑）",
  "_sources": [
    "src/config/schema/schema.snapshot.json",
    "src/config/schema/*.rs"
  ],
  "schemaVersion": 1,
  "sections": [
    {
      "name": "visual",
      "label": "视觉档案",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 0,
      "fields": [
        {
          "path": "visual.name",
          "label": "档案名称",
          "hint": "默认 1080P；在下方视觉档案编辑器里修改",
          "kind": {
            "type": "string"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "1080P"
        }
      ]
    },
    {
      "name": "command_access",
      "label": "命令权限",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 33,
      "fields": [
        {
          "path": "command_access.overrides",
          "label": "命令权限策略",
          "hint": "命令键到启用状态与最低身份的覆盖：user、friend、admin、owner；保存后立即生效",
          "kind": {
            "type": "object"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": {}
        },
        {
          "path": "command_access.default_song_source",
          "label": "默认点歌平台",
          "hint": "普通@点歌使用的平台；请在命令面板选择，实际平台权限仍会检查",
          "kind": {
            "type": "string"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": null
        }
      ]
    },
    {
      "name": "chat_source",
      "label": "外部聊天源",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 34,
      "fields": [
        {
          "path": "chat_source.enabled",
          "label": "启用外部聊天源",
          "hint": "启用后停止 OCR 的聊天内容识别，改由 Miliastra-Wonderland-ChatBridge 的 WS 推送提供消息；界面识别（一级/二级/未聊天）仍由模板匹配完成。关闭则完全走 OCR",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "chat_source.url",
          "label": "WebSocket 地址",
          "hint": "Miliastra-Wonderland-ChatBridge 的推送地址，默认 ws://127.0.0.1:18889/stream（端口 = http.port + 1）",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "ws://127.0.0.1:18889/stream"
        },
        {
          "path": "chat_source.token",
          "label": "访问令牌",
          "hint": "Miliastra-Wonderland-ChatBridge 的鉴权令牌，通常与 http.access_token 相同；留空则连接会被拒绝",
          "kind": {
            "type": "secret"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "••••••"
        },
        {
          "path": "chat_source.reconnect_ms",
          "label": "重连间隔",
          "hint": "WS 断开后等待多久重连，单位毫秒",
          "kind": {
            "max": 600000,
            "min": 100,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 3000
        },
        {
          "path": "chat_source.lease_timeout_ms",
          "label": "失联回落阈值",
          "hint": "外部源多久没有消息就回落到 OCR，单位毫秒；防止 Miliastra-Wonderland-ChatBridge 挂掉后聊天完全失聪",
          "kind": {
            "max": 600000,
            "min": 1000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 10000
        },
        {
          "path": "chat_source.auto_start",
          "label": "自动启动 Miliastra-Wonderland-ChatBridge",
          "hint": "开启后 BOT 发现 Miliastra-Wonderland-ChatBridge 没在监听时替你拉起它；要求游戏未运行，因为 Miliastra-Wonderland-ChatBridge 会自己拉起游戏并注入",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "chat_source.executable",
          "label": "Miliastra-Wonderland-ChatBridge 路径",
          "hint": "miliastra-wonderland-chatbridge.exe 的完整路径；留空则依次查找 BOT 目录及其上一级的 deps/bin、Miliastra-Wonderland-ChatBridge 目录，以及解压出的带版本号发布目录",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "chat_source.disconnect_action",
          "label": "失联超过回落阈值后",
          "hint": "外部聊天源「已经连上过」之后失联（DLL/游戏链路断了），超过上面的「失联回落阈值」仍未重连时做什么：回落 OCR 只是换回截图识别（默认）；自动重启游戏会关闭当前游戏再由聊天桥重新拉起并注入 DLL。从未连上过的启动失败（桥没起来）不会触发；注意程序无法区分「游戏崩溃」和「你手动退出游戏」，开启后手动退出也会被自动重开，需要手动退出时请先关掉这个选项",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "fallback_ocr",
                "label": "回退 OCR"
              },
              {
                "value": "restart_game",
                "label": "重启游戏"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "fallback_ocr"
        },
        {
          "path": "chat_source.self_uid",
          "label": "本机账号 UID",
          "hint": "本机游戏账号的 UID。Miliastra-Wonderland-ChatBridge 用它区分自己和玩家的消息，填错会把 BOT 自己的发言当成命令再执行一次。仅「自动启动」时由 BOT 传给对方；自行部署时请在你的启动命令里填 --self-uid，此处可留 0",
          "kind": {
            "max": 4294967295,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        }
      ]
    },
    {
      "name": "window",
      "label": "窗口",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 1,
      "fields": [
        {
          "path": "window.target_process",
          "label": "目标进程名",
          "hint": "目标游戏进程名，按进程文件名匹配，大小写不敏感；多个进程名可用逗号分隔",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "yuanshen.exe,GenshinImpact.exe"
        },
        {
          "path": "window.content_width",
          "label": "画面宽度",
          "hint": "配置坐标对应的游戏有效画面宽度，必须与 screen.expected_width 一致",
          "kind": {
            "max": 65535,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 1920
        },
        {
          "path": "window.content_height",
          "label": "画面高度",
          "hint": "配置坐标对应的游戏有效画面高度，必须与 screen.expected_height 一致",
          "kind": {
            "max": 65535,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 1080
        },
        {
          "path": "window.auto_activate_window",
          "label": "自动激活窗口",
          "hint": "/active-window 接口是否尝试自动切回目标窗口",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "window.capture_method",
          "label": "截图采集方式",
          "hint": "选择 Windows 截图来源；GDI 通常最兼容，PrintWindow 可尝试采集被遮挡窗口，桌面方式读取屏幕可见内容，WGC 使用系统图形捕获 API（Windows 10 1903+，建会话失败时自动回落到 GDI）",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "gdi",
                "label": "GDI 窗口采集"
              },
              {
                "value": "print_window",
                "label": "PrintWindow 窗口绘制"
              },
              {
                "value": "desktop",
                "label": "桌面可见区域"
              },
              {
                "value": "wgc",
                "label": "Windows Graphics Capture"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "gdi"
        },
        {
          "path": "window.focus_point",
          "label": "安全聚焦点",
          "hint": "全局安全聚焦点，只在业务入口显式激活/聚焦游戏窗口时使用一次",
          "kind": {
            "type": "point"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "x": 1919,
            "y": 1000
          }
        }
      ]
    },
    {
      "name": "screen",
      "label": "屏幕区域",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 2,
      "fields": [
        {
          "path": "screen.expected_width",
          "label": "预期宽度",
          "hint": "截图会缩放到这个宽度后再做模板匹配和 OCR；必须与 window.content_width 一致",
          "kind": {
            "max": 65535,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 1920
        },
        {
          "path": "screen.expected_height",
          "label": "预期高度",
          "hint": "截图会缩放到这个高度后再做模板匹配和 OCR；必须与 window.content_height 一致",
          "kind": {
            "max": 65535,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 1080
        },
        {
          "path": "screen.warn_on_size_mismatch",
          "label": "尺寸不符警告",
          "hint": "截图尺寸和预期不一致时是否记录 warning",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": true
        },
        {
          "path": "screen.chat_rect",
          "label": "聊天区域",
          "hint": "聊天区域，用于匹配蓝/黄/粉聊天标志和 OCR 聊天文本",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 143,
            "width": 416,
            "x": 39,
            "y": 879
          }
        },
        {
          "path": "screen.friend_rect",
          "label": "好友按钮区域",
          "hint": "一级聊天界面无聊天内容时好友按钮模板检测区域；有聊天内容时由蓝/黄/粉标识判定",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 40,
            "width": 50,
            "x": 170,
            "y": 1018
          }
        },
        {
          "path": "screen.secondary_back_rect",
          "label": "返回按钮区域",
          "hint": "二级聊天界面左上角返回按钮模板检测区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 65,
            "width": 65,
            "x": 15,
            "y": 15
          }
        },
        {
          "path": "screen.secondary_hall_rect",
          "label": "二级大厅区域",
          "hint": "二级大厅/面板界面模板检测区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 55,
            "width": 65,
            "x": 10,
            "y": 190
          }
        },
        {
          "path": "screen.secondary_chat_submit_rect",
          "label": "二级聊天发送按钮区域",
          "hint": "二级聊天输入行「发送」圆钮的搜索区；与返回按钮一起判二级聊天",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 124,
            "width": 150,
            "x": 930,
            "y": 950
          }
        },
        {
          "path": "screen.main_menu_rect",
          "label": "奇域主菜单区域",
          "hint": "千星奇域主菜单右下角「返回提瓦特」按钮的搜索区；该按钮只有枢纽节点才有，用来在二级聊天判定的最后一刻否决它",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 44,
            "width": 153,
            "x": 1668,
            "y": 998
          }
        },
        {
          "path": "screen.hall_bar_rect",
          "label": "大厅底栏区域",
          "hint": "千星大厅底栏 👥 O 图标组的搜索区；提瓦特底栏没有这一组，用来把提瓦特的世界聊天气泡与千星聊天标记区分开",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 95,
            "width": 600,
            "x": 0,
            "y": 985
          }
        },
        {
          "path": "screen.qianxing_f2_invite_rect",
          "label": "F2 邀请玩家按钮区域",
          "hint": "F2 千星大厅页右下「邀请玩家」按钮的搜索区；左侧「大厅列表」锚点复用 hall_bar_rect",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 54,
            "width": 140,
            "x": 1650,
            "y": 993
          }
        },
        {
          "path": "screen.world_wish_rect",
          "label": "主世界祈愿区域",
          "hint": "主世界右上角祈愿图标搜索区域，覆盖图标栏以适应横向位置变化；仅持续未知时探测",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 110,
            "width": 620,
            "x": 1300,
            "y": 0
          }
        },
        {
          "path": "screen.hall_name_rect",
          "label": "大厅名称区域",
          "hint": "F2 大厅页顶部大厅名称 OCR 区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 40,
            "width": 325,
            "x": 75,
            "y": 425
          }
        },
        {
          "path": "screen.hall_member_count_rect",
          "label": "成员人数区域",
          "hint": "大厅成员人数 OCR 区域，用于判断是否需要滚动右侧成员列表",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 50,
            "width": 450,
            "x": 75,
            "y": 470
          }
        },
        {
          "path": "screen.hall_time_rect",
          "label": "剩余时间区域",
          "hint": "F2 大厅页剩余时间 OCR 区域，只保留识别到的分钟数字",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 40,
            "width": 110,
            "x": 430,
            "y": 520
          }
        },
        {
          "path": "screen.hall_member_list_rect",
          "label": "成员列表区域",
          "hint": "F2 大厅页成员列表滚动区域；拖动点位约在画布宽度的 2/3 处",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 850,
            "width": 560,
            "x": 1280,
            "y": 110
          }
        }
      ]
    },
    {
      "name": "stability",
      "label": "稳定性",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 3,
      "fields": [
        {
          "path": "stability.default_count",
          "label": "全局稳定确认次数",
          "hint": "连续确认次数的全局默认值；只有大于 1 才生效，否则使用内置默认值 2",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 2
        },
        {
          "path": "stability.ui_state_count",
          "label": "UI 状态确认次数",
          "hint": "公共 UI 模板状态的局部覆盖；大于 1 时优先，否则继承 default_count",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0
        },
        {
          "path": "stability.secondary_hall_count",
          "label": "二级大厅确认次数",
          "hint": "二级大厅气泡与发送者关联的局部覆盖；大于 1 时优先，否则继承 default_count；保存后立即生效",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0
        }
      ]
    },
    {
      "name": "timing",
      "label": "时序",
      "domain": "system",
      "domainLabel": "系统",
      "order": 4,
      "fields": [
        {
          "path": "timing.watchdog_restart_ms",
          "label": "看门狗重启等待",
          "hint": "监听子进程异常退出后的重启等待时间，单位毫秒；父看门狗会在下次异常退出时直接读取最新值，无需重载子进程",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "主循环",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2000
        },
        {
          "path": "timing.loop_idle_ms",
          "label": "主循环空转间隔",
          "hint": "监听主循环空转间隔；脚本暂停或每轮扫描结束后等待多久再继续，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "主循环",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 60
        },
        {
          "path": "timing.chat_scan.fallback_ms",
          "label": "聊天兜底扫描间隔",
          "hint": "聊天 OCR 兜底扫描间隔；画面没变化时按这个间隔强制扫描一次，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "聊天扫描",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2000
        },
        {
          "path": "timing.chat_scan.change_debounce_ms",
          "label": "变化去抖等待",
          "hint": "聊天变化后等待画面稳定再 OCR 的时间，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "聊天扫描",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 120
        },
        {
          "path": "timing.chat_scan.change_cooldown_ms",
          "label": "变化触发冷却",
          "hint": "两次变化触发 OCR 之间的最小间隔，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "聊天扫描",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 250
        },
        {
          "path": "timing.command.ui_timeout_ms",
          "label": "返回一级界面超时",
          "hint": "执行命令前等待回到一级界面的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "命令执行",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 15000
        },
        {
          "path": "timing.command.return_retry_ms",
          "label": "ESC 返回重试间隔",
          "hint": "返回一级界面时每次 ESC 后等待重新检测的基础时间；连续失败后会递增，超过 5 次固定为 2000ms",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "命令执行",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1000
        },
        {
          "path": "timing.command.post_settle_ms",
          "label": "命令后稳定等待",
          "hint": "命令执行后等待聊天列表/动画稳定再复扫的时间，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "命令执行",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 500
        },
        {
          "path": "timing.command.help_batch_ms",
          "label": "帮助消息间隔",
          "hint": "@帮助及其他批量消息之间的间隔，单位毫秒；保存后立即生效（由 delivery::reply_batch 在运行期重新读取 live 快照）",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "命令执行",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 500
        },
        {
          "path": "timing.input.after_activate_ms",
          "label": "激活后等待",
          "hint": "自动激活游戏窗口后等待前台窗口切换完成的时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "输入等待",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        },
        {
          "path": "timing.input.focus_ms",
          "label": "聚焦后等待",
          "hint": "手动调试工具在显式聚焦游戏窗口后的等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "输入等待",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 300
        },
        {
          "path": "timing.input.open_chat_ms",
          "label": "打开聊天等待",
          "hint": "按回车打开聊天输入后的等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "输入等待",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 300
        },
        {
          "path": "timing.input.click_ms",
          "label": "点击后等待",
          "hint": "每次点击聊天输入区域后的等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "输入等待",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 150
        },
        {
          "path": "timing.input.text_ms",
          "label": "输入文本等待",
          "hint": "输入文本后到发送前的等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "输入等待",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 250
        },
        {
          "path": "timing.input.send_ms",
          "label": "发送后稳定等待",
          "hint": "发送聊天后等待界面稳定的时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "输入等待",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 300
        },
        {
          "path": "timing.workflow.default_timeout_ms",
          "label": "模板等待超时",
          "hint": "wait_template/click_template 默认等待模板出现的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "自定义流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 5000
        },
        {
          "path": "timing.workflow.default_poll_ms",
          "label": "模板轮询间隔",
          "hint": "等待模板出现时的轮询间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "自定义流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        },
        {
          "path": "timing.workflow.default_step_wait_ms",
          "label": "步骤间等待",
          "hint": "每个步骤执行后的默认等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "自定义流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 300
        },
        {
          "path": "timing.hall.page_settle_ms",
          "label": "大厅页面稳定等待",
          "hint": "进入大厅页面或滚动成员列表后的初始等待时间，单位毫秒；之后继续检测页面是否可读",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "大厅",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 800
        },
        {
          "path": "timing.hall.ocr_sample_interval_ms",
          "label": "大厅 OCR 采样间隔",
          "hint": "大厅信息多次 OCR 采样之间的间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "大厅",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 120
        },
        {
          "path": "timing.invite.open_chat_ms",
          "label": "邀请打开面板等待",
          "hint": "邀请流程打开好友/聊天面板前后的固定等待，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "邀请流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 400
        },
        {
          "path": "timing.invite.step_ms",
          "label": "邀请步骤等待",
          "hint": "点击好友后发送私聊反馈、以及输入大厅密码前的短暂等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "邀请流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 800
        },
        {
          "path": "timing.invite.confirm_timeout_ms",
          "label": "邀请确认超时",
          "hint": "非公共大厅邀请时等待邀请确认/拒绝命令的最长时间，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "邀请流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 30000
        },
        {
          "path": "timing.invite.confirm_poll_ms",
          "label": "邀请确认扫描间隔",
          "hint": "邀请确认扫描间隔，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "邀请流程",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2000
        },
        {
          "path": "timing.moderation.vote_timeout_ms",
          "label": "投票等待超时",
          "hint": "拉黑/屏蔽 UID 请求的好友私聊投票等待时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "管理投票",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 120000
        },
        {
          "path": "timing.moderation.vote_poll_ms",
          "label": "投票扫描间隔",
          "hint": "投票 OCR 扫描间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "管理投票",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2000
        },
        {
          "path": "timing.moderation.search_result_timeout_ms",
          "label": "搜索结果超时",
          "hint": "点击 UID 搜索按钮后等待搜索结果出现的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "管理投票",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 5000
        },
        {
          "path": "timing.moderation.confirm_wait_ms",
          "label": "确认动作等待",
          "hint": "点击确认按钮后等待动作完成的时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "管理投票",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2000
        },
        {
          "path": "timing.playback.status_poll_ms",
          "label": "播放状态查询间隔",
          "hint": "点歌后查询播放状态的间隔，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1000
        },
        {
          "path": "timing.playback.monitor_tick_ms",
          "label": "播放监控循环间隔",
          "hint": "播放监控线程循环间隔，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        },
        {
          "path": "timing.playback.monitor_status_ms",
          "label": "播放状态校准间隔",
          "hint": "播放结束监控向原生播放运行时校准状态的间隔，单位毫秒；保存后立即生效",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1000
        },
        {
          "path": "timing.playback.lyrics_lead_seconds",
          "label": "歌词提前秒数",
          "hint": "按当前播放进度向前查询歌词的秒数，支持小数，范围 0 到 60；保存后立即生效",
          "kind": {
            "max": 60.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.0
        },
        {
          "path": "timing.playback.uri_stable_samples",
          "label": "URI 稳定确认次数",
          "hint": "播放 URI 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        },
        {
          "path": "timing.playback.transport_stable_samples",
          "label": "播放状态确认次数",
          "hint": "播放状态连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        },
        {
          "path": "timing.playback.stale_timeout_ms",
          "label": "陈旧观测保留超时",
          "hint": "播放器异常时保留上一条稳定观测的最长时间，必须为正整数，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "播放器",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 5000
        },
        {
          "path": "timing.decision.timeout_ms",
          "label": "确认等待超时",
          "hint": "匹配失败/AI 自动匹配后等待用户确认的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "决策窗口",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 20000
        },
        {
          "path": "timing.decision.poll_ms",
          "label": "确认扫描间隔",
          "hint": "匹配失败/AI 自动匹配期间扫描确认命令的间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "决策窗口",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2000
        },
        {
          "path": "timing.external.volume_smooth_step_ms",
          "label": "音量渐变步进间隔",
          "hint": "内置播放器音量渐变（防爆音）：每个平滑步进之间的等待时间，单位毫秒；异步执行不阻塞命令",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "外部程序",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 300
        },
        {
          "path": "timing.external.ai_request_timeout_ms",
          "label": "AI 请求超时",
          "hint": "AI HTTP 请求超时，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "外部程序",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 35000
        }
      ]
    },
    {
      "name": "ocr",
      "label": "OCR",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 5,
      "fields": [
        {
          "path": "ocr.det_model",
          "label": "检测模型",
          "hint": "MNN/PaddleOCR 检测模型路径；OpenVINO-only 配置可设为 null 或省略",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "模型与置信度",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/PP-OCRv6_small_det.mnn"
        },
        {
          "path": "ocr.rec_model",
          "label": "识别模型",
          "hint": "MNN/PaddleOCR 识别模型路径；OpenVINO-only 配置可设为 null 或省略",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "模型与置信度",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/PP-OCRv6_small_rec.mnn"
        },
        {
          "path": "ocr.charset",
          "label": "字符集",
          "hint": "PaddleOCR 字符集路径",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "模型与置信度",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/ppocr_keys_v6_small.txt"
        },
        {
          "path": "ocr.min_confidence",
          "label": "最低置信度",
          "hint": "OCR 最低置信度，低于该值的结果会被过滤",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "模型与置信度",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.9
        },
        {
          "path": "ocr.threads",
          "label": "OCR 线程数",
          "hint": "OCR 线程数",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "模型与置信度",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4
        },
        {
          "path": "ocr.request_timeout_ms",
          "label": "请求超时",
          "hint": "单次 OCR 请求从入队到返回的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "运行控制",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 10000
        },
        {
          "path": "ocr.shutdown_timeout_ms",
          "label": "关闭超时",
          "hint": "关闭时等待底层 OCR 推理退出的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "运行控制",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 5000
        },
        {
          "path": "ocr.backend_priority",
          "label": "后端优先级",
          "hint": "OCR 后端优先级；可用值: cuda, vulkan, opencl, openvino, cpu。默认 openvino、cpu：OpenVINO 优先，初始化失败自动回退 MNN CPU",
          "kind": {
            "type": "stringArray"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "运行控制",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": [
            "openvino",
            "cpu"
          ]
        },
        {
          "path": "ocr.openvino.det_model",
          "label": "OpenVINO 检测模型",
          "hint": "检测模型 XML 文件（OpenVINO IR）；选择 openvino 后端时不能为空",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/PP-OCRv6_small_det.xml"
        },
        {
          "path": "ocr.openvino.det_weights",
          "label": "OpenVINO 检测权重",
          "hint": "检测模型权重文件，与 det_model 配对；选择 openvino 后端时不能为空",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/PP-OCRv6_small_det.bin"
        },
        {
          "path": "ocr.openvino.rec_model",
          "label": "OpenVINO 识别模型",
          "hint": "识别模型 XML 文件（OpenVINO IR）；选择 openvino 后端时不能为空",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/PP-OCRv6_small_rec.xml"
        },
        {
          "path": "ocr.openvino.rec_weights",
          "label": "OpenVINO 识别权重",
          "hint": "识别模型权重文件，与 rec_model 配对；选择 openvino 后端时不能为空",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/models/PP-OCRv6_small_rec.bin"
        },
        {
          "path": "ocr.openvino.rec_fixed_width",
          "label": "OpenVINO 识别固定宽度",
          "hint": "仅自训 48x768、输出 stride 8 模型设为 768：超宽压缩、短行右侧归一化补零；官方模型保留 null，按等比宽度分桶",
          "kind": {
            "max": 768,
            "min": 768,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": null
        },
        {
          "path": "ocr.openvino.device",
          "label": "OpenVINO 设备",
          "hint": "OpenVINO 设备名，通常为 CPU（安装时可选用 GPU/NPU）",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "CPU"
        },
        {
          "path": "ocr.openvino.cache_dir",
          "label": "OpenVINO 编译缓存",
          "hint": "OpenVINO GPU/CPU 编译缓存，首次启动会预热聊天 OCR 的固定形状；设置为空可关闭缓存",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "OpenVINO",
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/openvino-cache"
        },
        {
          "path": "ocr.det_max_side_len",
          "label": "检测最长边",
          "hint": "检测模型最长边限制；保持 960 与 PaddleOCR/BetterGI 常用配置一致",
          "kind": {
            "max": 8192,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "检测参数",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 960
        },
        {
          "path": "ocr.det_score_threshold",
          "label": "检测分割阈值",
          "hint": "检测分割阈值；越低越容易检出细小文字，也更容易产生噪声框",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "检测参数",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.3
        },
        {
          "path": "ocr.det_unclip_ratio",
          "label": "文本框外扩比例",
          "hint": "文本框外扩比例；2.0 更接近 BetterGI，减少裁掉边缘字符",
          "kind": {
            "max": 10.0,
            "min": 0.1,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "检测参数",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 2.0
        },
        {
          "path": "ocr.det_min_area",
          "label": "最小文本框面积",
          "hint": "最小文本框面积；小聊天文字用较低值，避免漏掉短命令",
          "kind": {
            "max": 100000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "检测参数",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 9
        },
        {
          "path": "ocr.det_box_border",
          "label": "额外裁剪边框",
          "hint": "OCR 库额外裁剪边框；BetterGI 主要依赖 unclip 外扩，这里关闭额外扩边",
          "kind": {
            "max": 100,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "检测参数",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0
        },
        {
          "path": "ocr.change_mean_threshold",
          "label": "画面变化均值阈值",
          "hint": "聊天区缩略图平均像素差超过该值时认为画面有变化",
          "kind": {
            "max": 255.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "变化检测",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 6.0
        },
        {
          "path": "ocr.change_pixel_threshold",
          "label": "画面变化像素阈值",
          "hint": "聊天区缩略图变化像素比例超过该值时认为画面有变化",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "变化检测",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.03
        },
        {
          "path": "ocr.text_left_gap",
          "label": "标志右侧间距",
          "hint": "聊天标志右侧到文本区域的间距",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 8
        },
        {
          "path": "ocr.block_top_padding",
          "label": "块顶部扩展",
          "hint": "聊天消息块顶部向上扩展像素",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 2
        },
        {
          "path": "ocr.block_bottom_padding",
          "label": "块底部收缩",
          "hint": "聊天消息块底部向上收缩像素，避免吃到下一条标志",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 2
        },
        {
          "path": "ocr.max_block_height",
          "label": "消息块最大高度",
          "hint": "单条聊天消息 OCR 块最大高度",
          "kind": {
            "max": 10000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 120
        },
        {
          "path": "ocr.same_line_y_tolerance",
          "label": "同行 Y 容差",
          "hint": "OCR 结果合并为同一行的 Y 轴容差",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 10
        },
        {
          "path": "ocr.marker_search_width",
          "label": "标志搜索条带宽度",
          "hint": "基准画布像素；聊天区左侧标志搜索范围",
          "kind": {
            "max": 10000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "气泡定位",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 60
        },
        {
          "path": "ocr.marker_dedupe_min_x",
          "label": "标志去重 X 下限",
          "hint": "与标志去重 X 容差取较大值",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "气泡定位",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 22
        },
        {
          "path": "ocr.marker_dedupe_min_y",
          "label": "标志去重 Y 下限",
          "hint": "与标志去重 Y 容差取较大值",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "气泡定位",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 14
        },
        {
          "path": "ocr.marker_dedupe_x",
          "label": "标志去重 X 容差",
          "hint": "聊天标志去重 X 轴容差",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "气泡定位",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 8
        },
        {
          "path": "ocr.marker_dedupe_y",
          "label": "标志去重 Y 容差",
          "hint": "聊天标志去重 Y 轴容差",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "气泡定位",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 8
        },
        {
          "path": "ocr.next_marker_min_gap",
          "label": "标志最小间隔",
          "hint": "判定下一条聊天标志的最小 Y 轴间隔",
          "kind": {
            "max": 10000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 12
        },
        {
          "path": "ocr.right_padding",
          "label": "文本右侧留白",
          "hint": "聊天文本区域右侧留白",
          "kind": {
            "max": 1000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "文本排版",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 4
        },
        {
          "path": "ocr.batch_recognize",
          "label": "批量识别",
          "hint": "实验性：将聊天块拼接为单张图片一次性 OCR，减少推理次数",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        }
      ]
    },
    {
      "name": "templates",
      "label": "模板图片",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 6,
      "fields": [
        {
          "path": "templates.panel_downsample_factors",
          "label": "面板用途降采样倍数",
          "hint": "用途名到 1–4 倍的覆盖；空对象保留现有 1/4 倍默认值，不影响流程点击精度",
          "kind": {
            "type": "object"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {}
        },
        {
          "path": "templates.blue_marker",
          "label": "蓝色聊天标志",
          "hint": "蓝色聊天标志模板，通常是自己/普通聊天行标志",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/chat-marker-blue.png"
        },
        {
          "path": "templates.yellow_marker",
          "label": "黄色聊天标志",
          "hint": "黄色聊天标志模板，通常是系统/高亮聊天行标志",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/chat-marker-yellow.png"
        },
        {
          "path": "templates.pink_marker",
          "label": "粉色聊天标志",
          "hint": "粉色聊天标志模板，用于识别好友私聊命令",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/chat-marker-pink.png"
        },
        {
          "path": "templates.secondary_back",
          "label": "二级返回按钮模板",
          "hint": "二级聊天界面左上角返回按钮模板，用于稳定判断二级界面",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/ui-secondary-back.png"
        },
        {
          "path": "templates.secondary_hall",
          "label": "二级大厅模板",
          "hint": "二级大厅/面板界面模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/ui-secondary-hall.png"
        },
        {
          "path": "templates.secondary_chat_submit",
          "label": "二级聊天发送按钮模板",
          "hint": "二级聊天输入行「发送」圆钮；用来挡掉只有返回按钮的主菜单过渡帧",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/ui-secondary-chat-submit.png"
        },
        {
          "path": "templates.qianxing_f2_hall_list",
          "label": "F2 大厅列表按钮模板",
          "hint": "F2 千星大厅页左下「大厅列表」按钮（图标+文字）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-f2-hall-list.png"
        },
        {
          "path": "templates.qianxing_f2_invite_player",
          "label": "F2 邀请玩家按钮模板",
          "hint": "F2 千星大厅页右下「邀请玩家」按钮（图标+文字）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-f2-invite-player.png"
        },
        {
          "path": "templates.qianxing_exit_menu",
          "label": "千星退出菜单锚点",
          "hint": "继续游戏/退出登录/退出桌面",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-exit-menu.png"
        },
        {
          "path": "templates.qianxing_map_label",
          "label": "千星地图右下角锚点",
          "hint": "右下角「千星奇域·大厅」文字 + 庙宇图标（不含 UID 与背景），千星地图首选判据",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-map-label.png"
        },
        {
          "path": "templates.qianxing_profile_page_wishlist",
          "label": "个人资料页「心愿单」按钮模板",
          "hint": "个人资料页右下角「心愿单」圆角按钮；必须与「衣橱」同时命中才判个人资料页",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-profile-page-wishlist.png"
        },
        {
          "path": "templates.qianxing_profile_page_wardrobe",
          "label": "个人资料页「衣橱」按钮模板",
          "hint": "个人资料页右下角「衣橱」圆角按钮；与「心愿单」成对判据，单命中不成立",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-profile-page-wardrobe.png"
        },
        {
          "path": "templates.teyvat_profile_adventure_rank",
          "label": "提瓦特资料卡「冒险等阶」锚点",
          "hint": "提瓦特资料卡左页「冒险等阶」纯文字模板；与「世界等级」同时命中才判提瓦特资料卡",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/teyvat-profile-adventure-rank.png"
        },
        {
          "path": "templates.teyvat_profile_world_level",
          "label": "提瓦特资料卡「世界等级」锚点",
          "hint": "提瓦特资料卡左页「世界等级」纯文字模板；与「冒险等阶」成对判据",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/teyvat-profile-world-level.png"
        },
        {
          "path": "templates.teyvat_profile_add_friend",
          "label": "提瓦特资料卡「添加好友」锚点",
          "hint": "提瓦特资料卡右下角深色「添加好友」按钮；命中=非好友，未命中=好友",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/teyvat-profile-add-friend.png"
        },
        {
          "path": "templates.qianxing_profile_domain_level",
          "label": "千星资料卡「奇域等级」锚点",
          "hint": "千星资料卡上部「奇域等级」纯文字模板；命中即千星资料卡",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-profile-domain-level.png"
        },
        {
          "path": "templates.qianxing_profile_add_friend",
          "label": "千星资料卡「添加好友」锚点",
          "hint": "千星资料卡下部白色胶囊「添加好友」；命中=非好友",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/qianxing-profile-add-friend.png"
        },
        {
          "path": "templates.qianxing_profile_view_profile",
          "label": "千星资料卡「查看资料」锚点",
          "hint": "千星资料卡下部白色胶囊「查看资料」；无「添加好友」但命中它=好友",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/invite-view-profile.png"
        },
        {
          "path": "templates.main_menu",
          "label": "奇域主菜单模板",
          "hint": "千星奇域主菜单右下角「返回提瓦特」按钮模板，命中即判为奇域主菜单而不是二级聊天",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/ui-main-menu-return.png"
        },
        {
          "path": "templates.world_wish",
          "label": "主世界祈愿模板",
          "hint": "主世界右上角祈愿图标的透明 PNG 模板；仅持续未知时参与状态识别",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/world-wish.png"
        },
        {
          "path": "templates.invite_view_star",
          "label": "查看千星按钮模板",
          "hint": "邀请流程里的“查看千星”按钮模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/invite-view-star.png"
        },
        {
          "path": "templates.invite_goto_hall",
          "label": "前往大厅按钮模板",
          "hint": "邀请流程里的“前往其大厅”按钮模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/invite-goto-hall.png"
        },
        {
          "path": "templates.invite_enter_hall",
          "label": "进入大厅按钮模板",
          "hint": "邀请流程里的“进入大厅”按钮模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/invite-enter-hall.png"
        },
        {
          "path": "templates.friend_panel",
          "label": "好友界面模板",
          "hint": "好友界面模板，用于 UID 拉黑/屏蔽流程",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/friend-panel.png"
        },
        {
          "path": "templates.friend_search_panel",
          "label": "好友搜索模板",
          "hint": "好友搜索界面模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/friend-search-panel.png"
        },
        {
          "path": "templates.friend_more_settings",
          "label": "更多设置模板",
          "hint": "好友搜索结果里的更多设置按钮模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/friend-more-settings.png"
        },
        {
          "path": "templates.friend_block_chat",
          "label": "屏蔽聊天模板",
          "hint": "更多设置里的屏蔽聊天按钮模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/friend-block-chat.png"
        },
        {
          "path": "templates.friend_blacklist",
          "label": "拉黑按钮模板",
          "hint": "更多设置里的拉黑按钮模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/friend-blacklist.png"
        },
        {
          "path": "templates.confirm_button",
          "label": "确认按钮模板",
          "hint": "通用「确认」按钮模板；好友操作与启动流程共用",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/confirm-button.png"
        },
        {
          "path": "templates.confirm_message_random_hall",
          "label": "进入大厅文案模板",
          "hint": "确认弹窗「随机公共大厅」关键词模板；确认弹窗必须按钮+文案双命中",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/confirm-message-random-hall.png"
        },
        {
          "path": "templates.confirm_message_return_teyvat",
          "label": "离开大厅文案模板",
          "hint": "确认弹窗「返回提瓦特」关键词模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/confirm-message-return-teyvat.png"
        },
        {
          "path": "templates.confirm_message_block",
          "label": "屏蔽玩家文案模板",
          "hint": "确认弹窗「屏蔽吗」关键词模板",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/confirm-message-block.png"
        },
        {
          "path": "templates.marker_threshold",
          "label": "匹配阈值",
          "hint": "UI/聊天标志模板匹配阈值，越高越严格",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.9
        }
      ]
    },
    {
      "name": "output",
      "label": "输出",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 7,
      "fields": [
        {
          "path": "output.send_enabled",
          "label": "发送回复开关",
          "hint": "是否真的向游戏内发送回复；false 时只写日志；保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "output.focus_point",
          "label": "安全点击点",
          "hint": "保留给手动调试工具使用的聊天面板安全点击点；自动流程返回一级只使用 ESC",
          "kind": {
            "type": "point"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "x": 1919,
            "y": 1000
          }
        },
        {
          "path": "output.chat_click_2",
          "label": "聊天输入框点击点",
          "hint": "打开聊天输入后的输入框位置",
          "kind": {
            "type": "point"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "x": 600,
            "y": 1013
          }
        }
      ]
    },
    {
      "name": "moderation",
      "label": "管理（拉黑/屏蔽）",
      "domain": "system",
      "domainLabel": "系统",
      "order": 8,
      "fields": [
        {
          "path": "moderation.stable_vote_samples",
          "label": "连续稳定识别次数",
          "hint": "同一好友同一判决需要连续稳定识别次数",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 3
        },
        {
          "path": "moderation.required_vote_margin",
          "label": "投票差额",
          "hint": "同意人数 - 不同意人数 达到该值才执行",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 3
        },
        {
          "path": "moderation.friend_panel_region",
          "label": "好友界面区域",
          "hint": "好友界面模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 50,
            "width": 75,
            "x": 770,
            "y": 20
          }
        },
        {
          "path": "moderation.search_panel_region",
          "label": "搜索按钮区域",
          "hint": "搜索按钮模板搜索区域；命中后会点击按钮左侧 500 像素处粘贴 UID，再点击按钮",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 49,
            "width": 74,
            "x": 1645,
            "y": 114
          }
        },
        {
          "path": "moderation.search_input_point",
          "label": "搜索输入框点击点",
          "hint": "UID 搜索输入框点击点",
          "kind": {
            "type": "point"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "x": 1180,
            "y": 135
          }
        },
        {
          "path": "moderation.search_button_point",
          "label": "搜索按钮点击点",
          "hint": "UID 搜索按钮点击点",
          "kind": {
            "type": "point"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "x": 1680,
            "y": 135
          }
        },
        {
          "path": "moderation.more_settings_region",
          "label": "更多设置区域",
          "hint": "更多设置按钮模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 70,
            "width": 80,
            "x": 1140,
            "y": 180
          }
        },
        {
          "path": "moderation.block_chat_region",
          "label": "屏蔽聊天区域",
          "hint": "屏蔽聊天按钮模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 112,
            "width": 124,
            "x": 1400,
            "y": 362
          }
        },
        {
          "path": "moderation.blacklist_region",
          "label": "拉黑按钮区域",
          "hint": "拉黑按钮模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 117,
            "width": 146,
            "x": 1388,
            "y": 494
          }
        },
        {
          "path": "moderation.confirm_button_region",
          "label": "确认按钮区域",
          "hint": "通用「确认」按钮模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 58,
            "width": 81,
            "x": 1140,
            "y": 728
          }
        },
        {
          "path": "moderation.confirm_message_region",
          "label": "确认弹窗文案区域",
          "hint": "确认弹窗提示文案行搜索区域；确认按钮与关键词文案必须同时命中",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 90,
            "width": 620,
            "x": 650,
            "y": 460
          }
        }
      ]
    },
    {
      "name": "playback",
      "label": "播放器",
      "domain": "playback",
      "domainLabel": "播放与点歌",
      "order": 9,
      "fields": [
        {
          "path": "playback.credential_directory",
          "label": "凭据目录",
          "hint": "账号凭据目录，敏感；三个平台凭据的版本化存储目录（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "playbackIdleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/credentials"
        },
        {
          "path": "playback.login_helper_executable",
          "label": "登录助手程序",
          "hint": "受控登录使用的短生命周期助手",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/bin/miliastra-login-helper.exe"
        },
        {
          "path": "playback.login_timeout_ms",
          "label": "登录超时",
          "hint": "单次交互登录的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 180000
        },
        {
          "path": "playback.audio_cache",
          "label": "音频缓存",
          "hint": "音频数据缓存整体配置；为 null 时不启用（默认）。启用后编辑下方子字段",
          "kind": {
            "type": "object"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": true,
          "optionalParent": false,
          "managedBy": null,
          "default": {
            "directory": "deps/cache/audio",
            "enabled": true,
            "max_bytes_mb": 20480,
            "max_concurrent_downloads": 2,
            "request_timeout_ms": 15000,
            "seek_wait_timeout_ms": 10000
          }
        },
        {
          "path": "playback.audio_cache.enabled",
          "label": "启用音频缓存",
          "hint": "音频数据缓存（本地代理 + 磁盘缓存）；播放时音源先落本地缓存，重复播放与断网时直接从磁盘服务",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": true,
          "managedBy": null,
          "default": true
        },
        {
          "path": "playback.audio_cache.directory",
          "label": "缓存目录",
          "hint": "音频文件目录；相对路径基于程序 exe 所在目录，留空默认 deps/cache/audio",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": true,
          "managedBy": null,
          "default": "deps/cache/audio"
        },
        {
          "path": "playback.audio_cache.max_bytes_mb",
          "label": "磁盘占用上限",
          "hint": "磁盘占用上限，单位 MiB",
          "kind": {
            "max": 1048576,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": true,
          "managedBy": null,
          "default": 20480
        },
        {
          "path": "playback.audio_cache.max_concurrent_downloads",
          "label": "并发下载上限",
          "hint": "同时进行的源站下载任务上限",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": true,
          "managedBy": null,
          "default": 2
        },
        {
          "path": "playback.audio_cache.request_timeout_ms",
          "label": "源站请求超时",
          "hint": "源站连接/响应超时，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": true,
          "managedBy": null,
          "default": 15000
        },
        {
          "path": "playback.audio_cache.seek_wait_timeout_ms",
          "label": "跳转等待超时",
          "hint": "请求尚未下载完成的位置时，等待下载推进的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": true,
          "managedBy": null,
          "default": 10000
        }
      ]
    },
    {
      "name": "http",
      "label": "Web/API 面板",
      "domain": "system",
      "domainLabel": "系统",
      "order": 10,
      "fields": [
        {
          "path": "http.enabled",
          "label": "启用 Web 面板",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；是否启用 Web/API 面板",
          "kind": {
            "type": "bool"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "http.host",
          "label": "监听地址",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；Web/API 面板监听地址",
          "kind": {
            "type": "string"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "127.0.0.1"
        },
        {
          "path": "http.port",
          "label": "监听端口",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；Web/API 面板端口",
          "kind": {
            "max": 65535,
            "min": 1,
            "type": "int"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 18888
        },
        {
          "path": "http.access_token",
          "label": "访问令牌",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；非本机监听时必须设置；首次留空时程序自动生成并写回",
          "kind": {
            "type": "secret"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "••••••"
        }
      ]
    },
    {
      "name": "logging",
      "label": "日志",
      "domain": "system",
      "domainLabel": "系统",
      "order": 11,
      "fields": [
        {
          "path": "logging.dir",
          "label": "日志目录",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；日志目录（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/logs"
        },
        {
          "path": "logging.level",
          "label": "日志级别",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；日志级别",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "error",
                "label": "错误"
              },
              {
                "value": "warn",
                "label": "警告"
              },
              {
                "value": "info",
                "label": "信息"
              },
              {
                "value": "debug",
                "label": "调试"
              },
              {
                "value": "trace",
                "label": "跟踪"
              }
            ]
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "info"
        },
        {
          "path": "logging.rotate_daily",
          "label": "按日轮转",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；是否按自然日分文件，启用后主日志和性能日志都会在跨日时自动轮转",
          "kind": {
            "type": "bool"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "logging.retain_days",
          "label": "日志保留天数",
          "hint": "启动引导配置，在 config.yaml 修改后重启生效；最多保留多少个自然日的按日日志",
          "kind": {
            "max": 3650,
            "min": 1,
            "type": "int"
          },
          "effect": "restart",
          "source": "bootstrap",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 7
        }
      ]
    },
    {
      "name": "tui",
      "label": "终端 TUI",
      "domain": "system",
      "domainLabel": "系统",
      "order": 12,
      "fields": [
        {
          "path": "tui.enabled",
          "label": "启用 TUI",
          "hint": "是否启用终端 TUI 面板；启用后实时显示事件日志、OCR 内容和队列状态",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "tui.refresh_ms",
          "label": "刷新间隔",
          "hint": "TUI 刷新间隔，单位毫秒",
          "kind": {
            "max": 60000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 100
        },
        {
          "path": "tui.log_lines",
          "label": "日志行数",
          "hint": "TUI 保留最近多少行日志",
          "kind": {
            "max": 10000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        }
      ]
    },
    {
      "name": "state",
      "label": "状态文件",
      "domain": "system",
      "domainLabel": "系统",
      "order": 13,
      "fields": [
        {
          "path": "state.hall_state_path",
          "label": "大厅状态路径",
          "hint": "大厅倒计时状态持久化路径（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/hall-state.json"
        },
        {
          "path": "state.executed_commands_log_path",
          "label": "命令记录路径",
          "hint": "最终执行命令记录路径（相对 EXE 根目录）；保存后立即生效",
          "kind": {
            "type": "path"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/executed-commands.log"
        }
      ]
    },
    {
      "name": "diagnostics",
      "label": "诊断存档",
      "domain": "system",
      "domainLabel": "系统",
      "order": 44,
      "fields": [
        {
          "path": "diagnostics.ui_frames",
          "label": "保存界面识别画面",
          "hint": "开启后把界面识别时的画面存到程序目录 deps/diagnostics，用于排查「界面持续无法识别」这类问题（存的是画面，文件名不含正文/昵称/UID）。默认关闭：无人看管时会持续占磁盘，排查完请记得关掉。失败帧（deps/logs/ui-failures）不受此开关影响，始终保存",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        }
      ]
    },
    {
      "name": "queue",
      "label": "点歌队列",
      "domain": "playback",
      "domainLabel": "播放与点歌",
      "order": 14,
      "fields": [
        {
          "path": "queue.max_size",
          "label": "队列最大长度",
          "hint": "队列最大长度",
          "kind": {
            "max": 100000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 5
        },
        {
          "path": "queue.pool_max_size",
          "label": "播放池容量",
          "hint": "播放池最大容量；已确认播放过的点歌会进入播放池，队列播完后从播放池随机播放；0 表示禁用播放池",
          "kind": {
            "max": 1000000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1000
        },
        {
          "path": "queue.protect_current_song_until_finished",
          "label": "保护当前歌曲",
          "hint": "机器人确认播放的歌曲是否保护；true 时新点歌会排队，未结束只能用 @下一首 提前切换；歌曲播完由自然结束自动出队；保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "queue.external_playback_protect_after_seconds",
          "label": "外部播放保护时间",
          "hint": "非点歌歌曲连续正常播放多少秒后加入当前歌曲保护；0 表示外部播放永不保护，单位秒；保存后立即生效",
          "kind": {
            "max": 86400,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 20
        }
      ]
    },
    {
      "name": "song_dedup",
      "label": "同歌去重",
      "domain": "playback",
      "domainLabel": "播放与点歌",
      "order": 15,
      "fields": [
        {
          "path": "song_dedup.enabled",
          "label": "启用去重",
          "hint": "长时间同歌去重；播放/入队前检查，确认播放成功后记录；保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "song_dedup.window_seconds",
          "label": "统计窗口",
          "hint": "最近多少秒内统计同一首歌播放次数；保存后立即生效",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 3600
        },
        {
          "path": "song_dedup.max_count",
          "label": "最多播放次数",
          "hint": "最近窗口内同一首歌最多允许播放次数；达到该值后拒绝或跳过；保存后立即生效",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1
        },
        {
          "path": "song_dedup.console_bypass",
          "label": "控制台豁免",
          "hint": "控制台来源是否默认豁免长时间同歌去重",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "song_dedup.history_path",
          "label": "历史记录路径",
          "hint": "长时间同歌去重历史持久化路径（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/song-dedup-history.json"
        }
      ]
    },
    {
      "name": "idiom_chain",
      "label": "成语接龙",
      "domain": "entertainment",
      "domainLabel": "娱乐玩法",
      "order": 16,
      "fields": [
        {
          "path": "idiom_chain.enabled",
          "label": "启用接龙",
          "hint": "是否启用项目内置成语词库的接龙功能",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "idiom_chain.lexicon_path",
          "label": "词库路径",
          "hint": "成语词库路径；相对路径以主程序 EXE 所在目录为基准",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/assets/idioms.txt"
        },
        {
          "path": "idiom_chain.history_limit",
          "label": "历史上限",
          "hint": "最多保留多少个最近成语用于会话历史；本局完整已用集合始终禁止重复",
          "kind": {
            "max": 10000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        },
        {
          "path": "idiom_chain.idle_timeout_seconds",
          "label": "空闲超时",
          "hint": "多久无人接词就自动结束本局；启用接龙时须大于 0，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 300
        },
        {
          "path": "idiom_chain.allow_consecutive_player",
          "label": "允许连续接词（兼容字段）",
          "hint": "为兼容旧配置保留；现在始终允许同一玩家连续接词",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "idiom_chain.allow_anyone_stop",
          "label": "允许任意结束",
          "hint": "false 时仅开局玩家可使用 #结束",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        }
      ]
    },
    {
      "name": "landlord",
      "label": "斗地主",
      "domain": "entertainment",
      "domainLabel": "娱乐玩法",
      "order": 17,
      "fields": [
        {
          "path": "landlord.enabled",
          "label": "启用斗地主",
          "hint": "三名好友参与的单局斗地主娱乐模块",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "landlord.lobby_timeout_seconds",
          "label": "组局超时",
          "hint": "等待三人组局的最长时间，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 120
        },
        {
          "path": "landlord.turn_timeout_seconds",
          "label": "出牌时间",
          "hint": "每名玩家的出牌时间，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 90
        },
        {
          "path": "landlord.trustee_after_timeouts",
          "label": "托管触发次数",
          "hint": "连续超时多少次后进入自动托管；启用斗地主时须大于 0",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2
        },
        {
          "path": "landlord.hand_cooldown_seconds",
          "label": "手牌查询冷却",
          "hint": "好友私聊 #手牌 的单人查询冷却，单位秒",
          "kind": {
            "max": 86400,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 10
        }
      ]
    },
    {
      "name": "undercover",
      "label": "谁是卧底",
      "domain": "entertainment",
      "domainLabel": "娱乐玩法",
      "order": 18,
      "fields": [
        {
          "path": "undercover.enabled",
          "label": "启用卧底",
          "hint": "谁是卧底娱乐模块；启用前复制 undercover.example.yaml 为 undercover.yaml 并审核词对",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "undercover.word_bank_path",
          "label": "词库路径",
          "hint": "词对词库路径（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/undercover.yaml"
        },
        {
          "path": "undercover.used_state_path",
          "label": "已用词记录路径",
          "hint": "已使用词对持久化路径（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/undercover-used.yaml"
        },
        {
          "path": "undercover.min_players",
          "label": "最少玩家",
          "hint": "开局最少玩家数；启用时需 4..=max_players",
          "kind": {
            "max": 11,
            "min": 4,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4
        },
        {
          "path": "undercover.double_min_players",
          "label": "双卧底最少玩家",
          "hint": "双卧底模式所需最少玩家数；启用时需 6..=max_players",
          "kind": {
            "max": 11,
            "min": 6,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 6
        },
        {
          "path": "undercover.max_players",
          "label": "最多玩家",
          "hint": "开局最多玩家数；4..=11 且 >= min_players",
          "kind": {
            "max": 11,
            "min": 4,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 11
        },
        {
          "path": "undercover.lobby_timeout_seconds",
          "label": "大厅等待超时",
          "hint": "等待玩家进入大厅的最长时间，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 180
        },
        {
          "path": "undercover.phase_timeout_seconds",
          "label": "阶段超时",
          "hint": "每个游戏阶段的最长时间，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 180
        },
        {
          "path": "undercover.progress_interval_seconds",
          "label": "进度公布间隔",
          "hint": "投票阶段公布未投票位置的间隔，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 20
        },
        {
          "path": "undercover.description_max_width",
          "label": "描述最大宽度",
          "hint": "单条公屏 #内容 的最大显示宽度",
          "kind": {
            "max": 1000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 70
        }
      ]
    },
    {
      "name": "turtle_soup",
      "label": "海龟汤",
      "domain": "entertainment",
      "domainLabel": "娱乐玩法",
      "order": 19,
      "fields": [
        {
          "path": "turtle_soup.enabled",
          "label": "启用海龟汤",
          "hint": "海龟汤娱乐模块；启用前必须准备正式题库并配置独立 AI Provider",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "turtle_soup.question_bank_path",
          "label": "题库路径",
          "hint": "正式题库路径，格式参考 turtle_soup.example.yaml（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/turtle_soup.yaml"
        },
        {
          "path": "turtle_soup.used_state_path",
          "label": "已用题目记录路径",
          "hint": "永久已使用题目记录；程序不提供重置接口，只能手动修改此文件",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "deps/data/turtle-soup-used.json"
        },
        {
          "path": "turtle_soup.idle_timeout_seconds",
          "label": "空闲超时",
          "hint": "进行中多久没有新问题就自动结束，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 600
        },
        {
          "path": "turtle_soup.max_session_seconds",
          "label": "单局最长时长",
          "hint": "单局最长时间，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 3600
        },
        {
          "path": "turtle_soup.max_concurrency",
          "label": "并发裁决上限",
          "hint": "AI 同时裁决的最大请求数",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4
        },
        {
          "path": "turtle_soup.max_pending",
          "label": "待裁决上限",
          "hint": "除正在裁决外，最多等待多少条问题",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 32
        },
        {
          "path": "turtle_soup.batch_max_parts",
          "label": "批量答案分段上限",
          "hint": "单个昵称的一份批量长答案最多暂存多少段；每段仍受游戏聊天长度限制",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 32
        },
        {
          "path": "turtle_soup.nickname_stable_count",
          "label": "昵称稳定确认次数",
          "hint": "昵称 OCR 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        },
        {
          "path": "turtle_soup.content_stable_count",
          "label": "正文稳定确认次数",
          "hint": "正文 OCR 连续确认次数；大于 1 时覆盖全局值，0 或 1 表示继承",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        },
        {
          "path": "turtle_soup.request_timeout_seconds",
          "label": "AI 请求超时",
          "hint": "单次 AI 请求超时，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 20
        },
        {
          "path": "turtle_soup.retry_count",
          "label": "重试次数",
          "hint": "单次裁决请求失败后的重试次数；实际最多请求 retry_count + 1 次",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2
        },
        {
          "path": "turtle_soup.retry_delay_ms",
          "label": "重试间隔",
          "hint": "AI 请求重试间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 500
        },
        {
          "path": "turtle_soup.custom_prompt",
          "label": "自定义规则",
          "hint": "附加在裁决提示词后的房间规则；不要要求模型输出自由解释或汤底",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "turtle_soup.system_prompt",
          "label": "AI 系统提示词",
          "hint": "海龟汤 AI 的系统提示词；留空使用内置默认值，可使用 {{role}}、{{verification_rules}}",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "{{role}}{{verification_rules}}只返回合法 json 对象。"
        },
        {
          "path": "turtle_soup.review_prompt",
          "label": "AI 裁决提示词",
          "hint": "海龟汤 AI 的裁决模板；可使用 {{verification_instruction}}、{{context}}、{{custom_prompt}}",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数"
        },
        {
          "path": "turtle_soup.model",
          "label": "专用模型",
          "hint": "海龟汤专用模型；留空沿用 ai.model。裁决偏重复杂逻辑，可用更强模型",
          "kind": {
            "type": "modelName"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "turtle_soup.reasoning",
          "label": "思考强度",
          "hint": "海龟汤裁决的思考强度；留空跟随服务端默认。裁决逻辑复杂，通常用中或高",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "unset",
                "label": "不指定（服务端默认）"
              },
              {
                "value": "minimal",
                "label": "最低"
              },
              {
                "value": "low",
                "label": "低"
              },
              {
                "value": "medium",
                "label": "中"
              },
              {
                "value": "high",
                "label": "高"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "unset"
        },
        {
          "path": "turtle_soup.max_tokens",
          "label": "最大 Token",
          "hint": "单次裁决允许生成的最大 Token",
          "kind": {
            "max": 1000000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1024
        }
      ]
    },
    {
      "name": "number_bomb",
      "label": "数字炸弹",
      "domain": "entertainment",
      "domainLabel": "娱乐玩法",
      "order": 31,
      "fields": [
        {
          "path": "number_bomb.enabled",
          "label": "启用数字炸弹",
          "hint": "大厅发起、轮流猜数字的娱乐玩法",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "number_bomb.min",
          "label": "范围下界",
          "hint": "炸弹与猜测范围的固定下界；必须小于上限",
          "kind": {
            "max": 1000000000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1
        },
        {
          "path": "number_bomb.max",
          "label": "默认上限",
          "hint": "不超过 5 人时的范围上限；必须大于下界",
          "kind": {
            "max": 1000000000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 100
        },
        {
          "path": "number_bomb.turn_timeout_seconds",
          "label": "回合超时",
          "hint": "当前玩家多久未猜就自动在剩余范围内随机猜一个，单位秒",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 60
        },
        {
          "path": "number_bomb.min_players",
          "label": "最少玩家",
          "hint": "开局所需最少玩家数；必须至少为 2",
          "kind": {
            "max": 64,
            "min": 2,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2
        },
        {
          "path": "number_bomb.max_players",
          "label": "最多玩家",
          "hint": "房间最多玩家数；必须不小于最少玩家数",
          "kind": {
            "max": 64,
            "min": 2,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 11
        },
        {
          "path": "number_bomb.extra_range_per_player",
          "label": "超员上限加成",
          "hint": "超过 5 人后，每多 1 人给默认上限增加多少；0 表示不加",
          "kind": {
            "max": 1000000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 100
        },
        {
          "path": "number_bomb.upper_bound_cap",
          "label": "上限封顶",
          "hint": "人数再多，范围上限也不会超过这个值；不能小于默认上限",
          "kind": {
            "max": 1000000000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1000
        }
      ]
    },
    {
      "name": "ai",
      "label": "AI 连接",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 20,
      "fields": [
        {
          "path": "ai.provider",
          "label": "服务商",
          "hint": "所有 AI 功能共用的服务商。二段式配置：先选服务商，再按服务商填写参数；custom 必须填写 endpoint 与 model",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "mimo",
                "label": "小米 MiMo"
              },
              {
                "value": "openai",
                "label": "OpenAI"
              },
              {
                "value": "deepseek",
                "label": "DeepSeek"
              },
              {
                "value": "custom",
                "label": "自定义网关"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "openai"
        },
        {
          "path": "ai.api_key",
          "label": "API Key",
          "hint": "所有 AI 功能共用的 API Key。留空表示全部 AI 功能关闭；不要提交到 Git，也不要放进 URL 查询参数",
          "kind": {
            "type": "secret"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "••••••"
        },
        {
          "path": "ai.endpoint",
          "label": "接口地址",
          "hint": "Chat Completions 地址，通常以 /chat/completions 结尾；Responses 地址默认由它推导。非 custom 留空时使用默认地址",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai.responses_endpoint",
          "label": "Responses 地址",
          "hint": "可选的 Responses 地址；留空时把接口地址的 /chat/completions 换成 /responses",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai.model",
          "label": "默认模型",
          "hint": "所有 AI 功能的默认模型；各功能可单独覆盖，custom 必须填写",
          "kind": {
            "type": "modelName"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "gpt-5.6-mini"
        },
        {
          "path": "ai.http_proxy",
          "label": "代理",
          "hint": "可选的独立 HTTP(S) 代理；作用于全部 AI 功能，留空时沿用环境代理设置",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai.wire_api",
          "label": "线上协议",
          "hint": "默认走 Responses。仅当网关（Kimi、GLM、部分自建网关）只支持 Chat Completions 时切换",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "responses",
                "label": "Responses（推荐）"
              },
              {
                "value": "chat_completions",
                "label": "Chat Completions"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "responses"
        },
        {
          "path": "ai.extra_body",
          "label": "第三方兼容字段",
          "hint": "服务商专属请求参数。Web 对 MiMo/OpenAI/DeepSeek 提供官方字段结构化表单；只有 custom 使用自由 JSON。与程序固定字段同名时程序值优先",
          "kind": {
            "type": "object"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": {}
        },
        {
          "path": "ai.reasoning_rules",
          "label": "思考强度适配规则",
          "hint": "按模型改写或拒绝某个思考强度：不同模型接受的 effort 词不同，写错会 400。规则按 from + 模型匹配（exact/prefix/suffix），to 写 deny 表示不支持。匹配不到的模型原样发送；若上游仍以 400 拒绝，程序会自动去掉该字段重试一次",
          "kind": {
            "type": "object"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": []
        }
      ]
    },
    {
      "name": "song_ai",
      "label": "点歌 AI",
      "domain": "playback",
      "domainLabel": "播放与点歌",
      "order": 21,
      "fields": [
        {
          "path": "song_ai.model",
          "label": "专用模型",
          "hint": "点歌专用模型；留空沿用 ai.model。点歌偏重低延迟",
          "kind": {
            "type": "modelName"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "song_ai.reasoning",
          "label": "思考强度",
          "hint": "点歌匹配的思考强度；留空跟随服务端默认。点歌偏重低延迟，通常用最低",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "unset",
                "label": "不指定（服务端默认）"
              },
              {
                "value": "minimal",
                "label": "最低"
              },
              {
                "value": "low",
                "label": "低"
              },
              {
                "value": "medium",
                "label": "中"
              },
              {
                "value": "high",
                "label": "高"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "unset"
        },
        {
          "path": "song_ai.local_recommend",
          "label": "本地曲库推荐",
          "hint": "在线匹配偏低时额外给出本地曲库推荐；默认开启，保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "song_ai.local_recommend_min_score",
          "label": "本地推荐触发分数",
          "hint": "在线候选分数低于该值才比较本地曲库；只有本地分数更高才会推荐",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.6
        },
        {
          "path": "song_ai.system_prompt",
          "label": "系统提示词",
          "hint": "点歌 AI 的系统提示词；留空使用内置默认值",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "你是点歌 JSON 结构化输出助手。必须只返回合法 JSON。"
        },
        {
          "path": "song_ai.recognize_prompt",
          "label": "识别提示词",
          "hint": "识别点歌文本的模板；可使用 {{text}}",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板"
        },
        {
          "path": "song_ai.match_prompt",
          "label": "匹配提示词",
          "hint": "判断歌曲匹配的模板；可使用 {{request}}、{{songName}}、{{songSinger}}",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文"
        },
        {
          "path": "song_ai.candidate_pick_prompt",
          "label": "选歌提示词",
          "hint": "从候选中选歌的模板；可使用 {{request}}、{{preferAccompaniment}}、{{preferAccompanimentInstruction}}、{{candidates}}",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出"
        }
      ]
    },
    {
      "name": "song_review",
      "label": "歌曲审核",
      "domain": "playback",
      "domainLabel": "播放与点歌",
      "order": 22,
      "fields": [
        {
          "path": "song_review.enabled",
          "label": "启用审核",
          "hint": "候选歌曲 AI 审核；只审核最终候选歌曲/URI，控制台来源最高权限免审",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "song_review.max_allowed_level",
          "label": "最高允许等级",
          "hint": "审核返回 level 为 1-10，超过该阈值会拒绝点歌",
          "kind": {
            "max": 10,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4
        },
        {
          "path": "song_review.failure_policy",
          "label": "失败策略",
          "hint": "审核服务多次失败后的策略：reject 拒绝，allow 放行并写警告日志",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "reject",
                "label": "拒绝"
              },
              {
                "value": "allow",
                "label": "放行"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "reject"
        },
        {
          "path": "song_review.retry_count",
          "label": "重试次数",
          "hint": "审核请求失败后的重试次数；实际最多请求 retry_count + 1 次",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2
        },
        {
          "path": "song_review.retry_delay_ms",
          "label": "重试间隔",
          "hint": "审核重试间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 500
        },
        {
          "path": "song_review.reply_reason_max_chars",
          "label": "拒绝原因字符上限",
          "hint": "游戏内拒绝原因最多显示的字符数；完整原因写日志",
          "kind": {
            "max": 10000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 40
        },
        {
          "path": "song_review.system_prompt",
          "label": "系统提示词",
          "hint": "歌曲审核 AI 的系统提示词；留空使用内置默认值",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "你是点歌审核助手，负责判断候选歌曲是否适合舒缓、轻松、不吵闹的房间氛围。必须只返回合法 JSON。"
        },
        {
          "path": "song_review.review_prompt",
          "label": "审核提示词",
          "hint": "歌曲审核提示词模板；可使用 {{policy_prompt}}、{{custom_prompt}}、{{candidate}}",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位"
        },
        {
          "path": "song_review.policy_prompt",
          "label": "审核条件",
          "hint": "审核条件；可以按房间氛围修改，但不要要求模型改变 JSON 输出格式",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于"
        },
        {
          "path": "song_review.custom_prompt",
          "label": "追加规则",
          "hint": "追加审核规则；会附加在 policy_prompt 后面，用于临时补充口径",
          "kind": {
            "type": "text"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "song_review.model",
          "label": "审核模型",
          "hint": "审核专用模型；留空沿用 ai.model。审核偏重判断质量，建议用更强模型",
          "kind": {
            "type": "modelName"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "song_review.reasoning",
          "label": "思考强度",
          "hint": "歌曲审核的思考强度；审核偏重判断质量，通常用中或高",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "unset",
                "label": "不指定（服务端默认）"
              },
              {
                "value": "minimal",
                "label": "最低"
              },
              {
                "value": "low",
                "label": "低"
              },
              {
                "value": "medium",
                "label": "中"
              },
              {
                "value": "high",
                "label": "高"
              }
            ]
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "unset"
        },
        {
          "path": "song_review.web_search",
          "label": "联网审核",
          "hint": "是否要求模型联网搜索后再判断；需要 ai.wire_api=responses，网关不支持时关闭",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "song_review.max_tokens",
          "label": "最大 Token",
          "hint": "单次审核允许生成的最大 Token",
          "kind": {
            "max": 1000000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 512
        }
      ]
    },
    {
      "name": "matching",
      "label": "歌名匹配",
      "domain": "recognition",
      "domainLabel": "识别与画面",
      "order": 23,
      "fields": [
        {
          "path": "matching.min_song_name_score",
          "label": "歌名最低分数",
          "hint": "歌名最低匹配分数；保存后立即生效",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.5
        },
        {
          "path": "matching.short_chinese_song_max_miss",
          "label": "短中文歌名漏字上限",
          "hint": "4 字以内中文歌名最多允许漏字数；保存后立即生效",
          "kind": {
            "max": 100,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1
        },
        {
          "path": "matching.long_chinese_song_min_score",
          "label": "长中文歌名最低比例",
          "hint": "长中文歌名最低命中比例；保存后立即生效",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.5
        },
        {
          "path": "matching.max_ocr_noise_chars",
          "label": "OCR 噪声字符上限",
          "hint": "完整歌名后最多忽略的 OCR 噪声字符数；保存后立即生效",
          "kind": {
            "max": 100,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1
        },
        {
          "path": "matching.enable_fuzzy_singer",
          "label": "歌手模糊匹配",
          "hint": "是否启用中文歌手模糊匹配；保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "matching.short_chinese_singer_max_miss",
          "label": "短中文歌手漏字上限",
          "hint": "4 字以内中文歌手最多允许漏字数；保存后立即生效",
          "kind": {
            "max": 100,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1
        },
        {
          "path": "matching.long_chinese_singer_min_score",
          "label": "长中文歌手最低比例",
          "hint": "长中文歌手最低命中比例；保存后立即生效",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.8
        },
        {
          "path": "matching.en_max_edit_fraction",
          "label": "英文歌名编辑距离上限",
          "hint": "英文歌名编辑距离占比上限；保存后立即生效",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.3
        },
        {
          "path": "matching.en_singer_max_edit_fraction",
          "label": "英文歌手编辑距离上限",
          "hint": "英文歌手编辑距离占比上限；保存后立即生效",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.35
        }
      ]
    },
    {
      "name": "hotkeys",
      "label": "全局热键",
      "domain": "entertainment",
      "domainLabel": "娱乐玩法",
      "order": 24,
      "fields": [
        {
          "path": "hotkeys.enabled",
          "label": "启用热键",
          "hint": "是否启用全局热键",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "hotkeys.pause_key",
          "label": "暂停/恢复热键",
          "hint": "暂停/恢复热键",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "F7"
        },
        {
          "path": "hotkeys.exit_key",
          "label": "退出热键",
          "hint": "退出热键",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "F12"
        }
      ]
    },
    {
      "name": "startup",
      "label": "启动流程",
      "domain": "system",
      "domainLabel": "系统",
      "order": 25,
      "fields": [
        {
          "path": "startup.enabled",
          "label": "启用启动流程",
          "hint": "启用游戏启动及根据界面状态自动进入游戏、进入千星",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "startup.launch_game",
          "label": "启动游戏",
          "hint": "找不到游戏窗口时是否启动游戏",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "startup.enter_game",
          "label": "自动进入游戏",
          "hint": "持续未知时 OCR 检测到“点击进入”，确认大门界面后自动进入游戏；空闲后整程序重启时生效",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "startup.enter_wonderland",
          "label": "自动进入千星",
          "hint": "持续未知时匹配到祈愿图标，确认主世界后自动进入千星奇域大厅",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "startup.exe_path",
          "label": "启动 EXE 路径",
          "hint": "启动 EXE 路径；可填完整 exe 文件路径，也可填 exe 所在目录；留空时自动从米哈游启动器注册表查找",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "startup.game_args",
          "label": "启动参数",
          "hint": "启动游戏时附加的命令行参数",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "startup.launch_wait_ms",
          "label": "窗口出现等待",
          "hint": "启动游戏后每次等待窗口出现的时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 3000
        },
        {
          "path": "startup.launch_retries",
          "label": "窗口检查次数",
          "hint": "启动游戏后最多检查窗口次数",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 20
        },
        {
          "path": "startup.enter_game_timeout_ms",
          "label": "点击进入超时",
          "hint": "OCR 等待并点击“点击进入”的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 45000
        },
        {
          "path": "startup.enter_wonderland_timeout_ms",
          "label": "千星确认超时",
          "hint": "等待千星奇域界面/大厅确认的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 60000
        },
        {
          "path": "startup.wonderland_map_star_retries",
          "label": "星型入口匹配次数",
          "hint": "地图星型入口阶段最多匹配次数",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 6
        },
        {
          "path": "startup.wonderland_map_star_retry_ms",
          "label": "星型入口轮询间隔",
          "hint": "地图星型入口匹配轮询间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2500
        },
        {
          "path": "startup.wonderland_hall_retries",
          "label": "千星大厅重试次数",
          "hint": "千星大厅 OCR 阶段最多重试次数",
          "kind": {
            "max": 1024,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 6
        },
        {
          "path": "startup.wonderland_hall_retry_ms",
          "label": "千星大厅轮询间隔",
          "hint": "千星大厅 OCR 轮询间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 2500
        },
        {
          "path": "startup.wonderland_transition_timeout_ms",
          "label": "阶段过渡超时",
          "hint": "千星入口和确认弹窗各阶段过渡等待的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4000
        },
        {
          "path": "startup.wonderland_confirm_stable_timeout_ms",
          "label": "确认稳定超时",
          "hint": "确认按钮消失后等待该区域像素稳定的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 3000
        },
        {
          "path": "startup.final_primary_timeout_ms",
          "label": "主界面等待超时",
          "hint": "启动游戏结束时等待主菜单大世界模板的最长时间，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 10000
        },
        {
          "path": "startup.restart_game_on_loading",
          "label": "加载过久自动重启游戏",
          "hint": "界面长时间停在加载界面时自动重启游戏重试，默认关闭。需要先启用上面的「启用启动流程」；正常游戏里的传送加载一般只有几秒，不会碰到这个超时",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "startup.loading_restart_timeout_ms",
          "label": "加载超时",
          "hint": "停在加载界面超过这个时长就重启游戏，单位毫秒；默认 60000（60 秒）",
          "kind": {
            "max": 600000,
            "min": 10000,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 60000
        },
        {
          "path": "startup.poll_ms",
          "label": "轮询间隔",
          "hint": "启动流程 OCR/截图轮询间隔，单位毫秒",
          "kind": {
            "max": 86400000,
            "min": 1,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1000
        },
        {
          "path": "startup.stable_mean_threshold",
          "label": "像素稳定平均差阈值",
          "hint": "像素稳定平均差阈值",
          "kind": {
            "max": 255.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "稳定判定",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 2.0
        },
        {
          "path": "startup.stable_changed_ratio_threshold",
          "label": "像素稳定变化比例",
          "hint": "像素稳定变化比例阈值",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": "稳定判定",
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.01
        },
        {
          "path": "startup.template_threshold",
          "label": "模板匹配阈值",
          "hint": "启动界面模板匹配阈值（例如主菜单大世界）；越高越严格",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.9
        },
        {
          "path": "startup.wonderland_confirm_threshold",
          "label": "千星确认阈值",
          "hint": "千星确认按钮模板匹配阈值；越高越严格",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": 0.9
        },
        {
          "path": "startup.templates.wonderland_map_star",
          "label": "星型入口模板",
          "hint": "地图右下角星型入口模板（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/startup-wonderland-map-star.png"
        },
        {
          "path": "startup.templates.paimon_menu",
          "label": "主菜单大世界模板",
          "hint": "主菜单大世界模板；启动游戏完成判断使用（相对 EXE 根目录）",
          "kind": {
            "type": "path"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": "deps/assets/startup-paimon-menu.png"
        },
        {
          "path": "startup.enter_game_text_region",
          "label": "点击进入 OCR 区域",
          "hint": "“点击进入”按钮 OCR 区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 40,
            "width": 130,
            "x": 900,
            "y": 1000
          }
        },
        {
          "path": "startup.wonderland_hall_ocr_region",
          "label": "千星大厅 OCR 区域",
          "hint": "右侧千星大厅选项 OCR 区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 1040,
            "width": 640,
            "x": 1280,
            "y": 0
          }
        },
        {
          "path": "startup.main_ui_region",
          "label": "主界面区域",
          "hint": "主菜单大世界/主界面模板区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 270,
            "width": 480,
            "x": 0,
            "y": 0
          }
        },
        {
          "path": "startup.wonderland_map_star_region",
          "label": "星型入口区域",
          "hint": "地图右下角星型入口模板区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 230,
            "width": 220,
            "x": 1700,
            "y": 850
          }
        }
      ]
    },
    {
      "name": "invite",
      "label": "邀请流程",
      "domain": "system",
      "domainLabel": "系统",
      "order": 26,
      "fields": [
        {
          "path": "invite.friend_name_stable_count",
          "label": "好友备注确认次数",
          "hint": "好友备注连续完整匹配次数；大于 1 时覆盖全局值，0 或 1 表示继承",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        },
        {
          "path": "invite.fuzzy_friend_match",
          "label": "好友候选评分匹配",
          "hint": "所有列表页精确匹配失败后，回到顶部搜索候选；默认关闭",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "invite.friend_list_region",
          "label": "好友列表区域",
          "hint": "好友列表 OCR 区域，用于查找发起邀请的用户名",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 600,
            "width": 170,
            "x": 80,
            "y": 280
          }
        },
        {
          "path": "invite.friend_chat_region",
          "label": "好友聊天区域",
          "hint": "点击好友后用于确认备注的二级聊天内容区域，不使用顶部原昵称",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 850,
            "width": 920,
            "x": 260,
            "y": 100
          }
        },
        {
          "path": "invite.view_star_region",
          "label": "查看千星区域",
          "hint": "“查看千星”模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 860,
            "width": 217,
            "x": 498,
            "y": 80
          }
        },
        {
          "path": "invite.goto_hall_region",
          "label": "前往大厅区域",
          "hint": "“前往其大厅”模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 112,
            "width": 372,
            "x": 780,
            "y": 560
          }
        },
        {
          "path": "invite.enter_hall_region",
          "label": "进入大厅区域",
          "hint": "“进入大厅”模板搜索区域",
          "kind": {
            "type": "rect"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": "visual",
          "default": {
            "height": 54,
            "width": 164,
            "x": 896,
            "y": 730
          }
        }
      ]
    },
    {
      "name": "friend_delivery",
      "label": "好友投递",
      "domain": "system",
      "domainLabel": "系统",
      "order": 27,
      "fields": [
        {
          "path": "friend_delivery.auto_retry_count",
          "label": "自动重试次数",
          "hint": "好友投递中确认尚未发送项的自动重试次数；0 表示禁用；保存后立即生效",
          "kind": {
            "max": 1024,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0
        }
      ]
    },
    {
      "name": "custom_workflows",
      "label": "自定义流程",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 28,
      "fields": [
        {
          "path": "custom_workflows.enabled",
          "label": "启用自定义流程",
          "hint": "是否启用配置驱动的自定义流程命令；保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "custom_workflows.default_threshold",
          "label": "模板默认阈值",
          "hint": "模板默认匹配阈值；保存后立即生效",
          "kind": {
            "max": 1.0,
            "min": 0.0,
            "type": "float"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 0.9
        },
        {
          "path": "custom_workflows.wait_template_absent_stable_default",
          "label": "模板消失后等稳定",
          "hint": "wait_template_absent 默认在模板消失后继续等待像素稳定；可在步骤上设置 stable_after_absent: false 关闭；保存后立即生效",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "custom_workflows.max_hold_key_seconds",
          "label": "按住按键上限",
          "hint": "hold_key 单次按住按键允许的最大秒数；时长从命令参数读取；保存后立即生效",
          "kind": {
            "max": 86400,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 10
        },
        {
          "path": "custom_workflows.templates",
          "label": "自定义模板映射",
          "hint": "自定义模板名到图片路径的映射，步骤里通过 template 引用这里的名字（高级 JSON 编辑）；保存后立即生效",
          "kind": {
            "type": "object"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": {}
        },
        {
          "path": "custom_workflows.workflows",
          "label": "自定义流程定义",
          "hint": "自定义流程定义列表；支持的步骤与变量见配置模板注释（高级 JSON 编辑）；保存后立即生效",
          "kind": {
            "type": "object"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": null
        }
      ]
    },
    {
      "name": "identity",
      "label": "身份映射",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 29,
      "fields": [
        {
          "path": "identity.mappings",
          "label": "身份映射",
          "hint": "备注串（游戏内好友备注，也就是屏幕上显示的名字）与 OCR 精确匹配；显示名用于游戏回复、Web 和 API，可留空，留空时依次回退真实昵称与「玩家」；真实昵称由聊天桥自动写入，只做展示与搜索兜底；角色控制命令权限。保存后立即生效。",
          "kind": {
            "type": "object"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": []
        }
      ]
    },
    {
      "name": "kugou",
      "label": "酷狗设备",
      "domain": "system",
      "domainLabel": "系统",
      "order": 30,
      "fields": [
        {
          "path": "kugou.device_guid",
          "label": "设备 GUID",
          "hint": "填写你自己酷狗客户端的设备 GUID（形如 550e8400-e29b-41d4-a716-446655440000）。程序不会自动生成设备信息；三项留空时沿用凭据目录下已有的 kugou-device.json。保存后在下一次酷狗登录时生效。",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "kugou.device_dev",
          "label": "设备型号",
          "hint": "填写该设备上报的设备标识（设备型号名，任意稳定字符串即可，例如 KuGouMusicApi）。三项必须同时填写。保存后在下一次酷狗登录时生效。",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "kugou.device_mac",
          "label": "设备 MAC",
          "hint": "填写该设备的 MAC 地址（形如 AA:BB:CC:DD:EE:FF）。三项必须同时填写。保存后在下一次酷狗登录时生效。",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        }
      ]
    },
    {
      "name": "ai_chat",
      "label": "AI 聊天",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 32,
      "fields": [
        {
          "path": "ai_chat.enabled",
          "label": "启用 AI 聊天",
          "hint": "关键词触发的大厅聊天机器人；未配置 ai.api_key 时不会回复",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "ai_chat.keyword",
          "label": "触发关键词",
          "hint": "消息包含该关键词时触发；默认 橘子",
          "kind": {
            "type": "string"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "橘子"
        },
        {
          "path": "ai_chat.trigger_mode",
          "label": "触发方式",
          "hint": "默认只在消息以关键词开头时触发",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "prefix",
                "label": "以关键词开头"
              },
              {
                "value": "contains",
                "label": "包含关键词"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "prefix"
        },
        {
          "path": "ai_chat.min_access",
          "label": "触发权限",
          "hint": "谁能让 AI 回应；身份映射里查不到的人，除「所有人」外一律拒绝",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "everyone",
                "label": "所有人"
              },
              {
                "value": "friend",
                "label": "好友及以上"
              },
              {
                "value": "admin",
                "label": "管理员及以上"
              },
              {
                "value": "owner",
                "label": "仅所有者"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "everyone"
        },
        {
          "path": "ai_chat.actor_token_budget",
          "label": "单人近期原文预算",
          "hint": "每个玩家的近期问答原文 token 上限；超过 82% 时把较早原文压进中期摘要（压到 15%），历史只追加不滑动",
          "kind": {
            "max": 200000,
            "min": 1000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 96000
        },
        {
          "path": "ai_chat.actor_summary_budget",
          "label": "单人中期摘要预算",
          "hint": "每个玩家的中期摘要 token 上限；摘要滚动覆盖，不是硬截断",
          "kind": {
            "max": 50000,
            "min": 200,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4000
        },
        {
          "path": "ai_chat.actor_history_token_budget",
          "label": "个人历史注入预算",
          "hint": "个人历史作为 messages 注入的 token 上限；超出从最旧裁剪、保最近；默认 12000",
          "kind": {
            "max": 200000,
            "min": 1000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 12000
        },
        {
          "path": "ai_chat.inject_actor_history",
          "label": "注入个人问答历史",
          "hint": "默认注入个人问答历史，hall 只含其他玩家/对别人回复；关闭后个人历史不注入，仅保留 facts/摘要/大厅上下文。",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "ai_chat.hall_token_budget",
          "label": "大厅近期原文预算",
          "hint": "大厅共享记忆的原文 token 上限；记录所有人在大厅的发言",
          "kind": {
            "max": 400000,
            "min": 1000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 32000
        },
        {
          "path": "ai_chat.hall_summary_budget",
          "label": "大厅中期摘要预算",
          "hint": "大厅共享记忆的中期摘要 token 上限",
          "kind": {
            "max": 100000,
            "min": 200,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4000
        },
        {
          "path": "ai_chat.bot_token_budget",
          "label": "机器人记忆预算",
          "hint": "机器人自身发言的全局 token 上限",
          "kind": {
            "max": 400000,
            "min": 1000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 32000
        },
        {
          "path": "ai_chat.cooldown_seconds",
          "label": "冷却秒数",
          "hint": "同一玩家两次触发之间的最小间隔",
          "kind": {
            "max": 86400,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 8
        },
        {
          "path": "ai_chat.allow_commands",
          "label": "允许执行命令",
          "hint": "允许 AI 通过原生工具调用执行命令；程序按提问者身份与命令权限策略复核后执行",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "ai_chat.model",
          "label": "专用模型",
          "hint": "留空沿用 ai.model",
          "kind": {
            "type": "modelName"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.reasoning",
          "label": "思考强度",
          "hint": "AI 聊天的思考强度；留空跟随服务端默认",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "unset",
                "label": "不指定（服务端默认）"
              },
              {
                "value": "minimal",
                "label": "最低"
              },
              {
                "value": "low",
                "label": "低"
              },
              {
                "value": "medium",
                "label": "中"
              },
              {
                "value": "high",
                "label": "高"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "unset"
        },
        {
          "path": "ai_chat.max_tokens",
          "label": "最大 Token",
          "hint": "单次回答允许生成的最大 token",
          "kind": {
            "max": 100000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1500
        },
        {
          "path": "ai_chat.system_prompt",
          "label": "系统提示词",
          "hint": "留空使用内置默认值",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "你是一个游戏大厅里的聊天伙伴，名字由用户设定。需要执行游戏命令时使用提供的工具，依据工具回执回答，不要声称尚未执行的操作已经成功。"
        },
        {
          "path": "ai_chat.answer_prompt",
          "label": "回答模板",
          "hint": "可使用 {{keyword}}、{{facts}}、{{summary}}、{{hall}}、{{asker}}；{{hall}} 只含其他玩家的发言与 BOT 对他人的回复（显示为「你（角色名）：内容」），当前玩家自己的问答默认作为独立角色消息传输（{{history}} 仅作兼容占位）；{{question}}、{{recent_actions}}、{{commands}} 由程序统一追加到末尾，模板里写不写都不会重复",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行"
        },
        {
          "path": "ai_chat.extract_prompt",
          "label": "事实抽取提示词",
          "hint": "留空使用内置默认值；从每轮对话中抽取值得长期记住的信息",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占"
        },
        {
          "path": "ai_chat.tool_prompts.look",
          "label": "look 工具提示词",
          "hint": "只在实际暴露 look 工具时注入；留空使用内置默认文案",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "工具提示词",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高"
        },
        {
          "path": "ai_chat.tool_prompts.speak",
          "label": "speak 工具提示词",
          "hint": "只在实际暴露 speak 工具时注入；留空使用内置默认文案",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "工具提示词",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用"
        },
        {
          "path": "ai_chat.tool_prompts.command",
          "label": "命令工具提示词",
          "hint": "只在 execute_game_command 暴露时注入；留空使用内置默认文案",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "工具提示词",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出"
        },
        {
          "path": "ai_chat.vision.enabled",
          "label": "识别截图",
          "hint": "开启 AI 识图；模型不支持图片时自动降级为纯文本",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "ai_chat.vision.mode",
          "label": "识图调用方式",
          "hint": "auto 每轮自动附一级大厅截图；tool 不自动附图，由模型按需调用 look；both 两者都启用；enabled=false 时全部关闭",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "auto",
                "label": "每轮自动附图"
              },
              {
                "value": "tool",
                "label": "仅 look 工具"
              },
              {
                "value": "both",
                "label": "自动附图 + look 工具"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "auto"
        },
        {
          "path": "ai_chat.vision.direct_to_main",
          "label": "直接给主模型看图",
          "hint": "仅当 AI 调用 look 时把当前截图直接给主聊天模型，不每轮自动截图；需主模型支持图片；auto/both 仍按原模式自动附图；截图失败或不支持图片时回退 look 视觉子模型",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "ai_chat.vision.capture_scope",
          "label": "截图范围",
          "hint": "当前仅支持整个一级大厅；整屏可能包含 UID/HUD，日志不会保存截图内容",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "primary_screen",
                "label": "整个一级大厅"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "primary_screen"
        },
        {
          "path": "ai_chat.vision.max_edge",
          "label": "截图长边上限",
          "hint": "截图长边上限（默认 1600）；超过时等比缩小，服务端仍可自行降采样",
          "kind": {
            "max": 2048,
            "min": 512,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 1600
        },
        {
          "path": "ai_chat.vision.jpeg_quality",
          "label": "截图 JPEG 质量",
          "hint": "JPEG 编码质量（40..=100）；越大越清晰、字节越多，默认 100",
          "kind": {
            "max": 100,
            "min": 40,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 100
        },
        {
          "path": "ai_chat.vision.max_bytes",
          "label": "截图字节上限",
          "hint": "编码后超过该字节数会降采样/降质量，仍超限则本轮不附带图片",
          "kind": {
            "max": 20000000,
            "min": 100000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 4000000
        },
        {
          "path": "ai_chat.vision.timeout_ms",
          "label": "截图超时",
          "hint": "截图与编码的截止时间，单位毫秒；超时降级纯文本",
          "kind": {
            "max": 5000,
            "min": 100,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 800
        },
        {
          "path": "ai_chat.vision.model",
          "label": "look 子请求模型",
          "hint": "只用于 look 的视觉子请求；主聊天始终使用 ai_chat.model；留空沿用 ai_chat.model",
          "kind": {
            "type": "modelName"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.vision.allow_full_screen",
          "label": "允许整屏截图",
          "hint": "用户明确选择整屏时开启；整屏可能包含昵称/UID/HUD",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": "识图",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "ai_chat.ai_reply.mode",
          "label": "AI回复 生成方式",
          "hint": "回复文案里写入 {AI回复} 时的生成方式；改写模式会让 AI 重写整条，事实可能被改动，默认补充一句",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "append",
                "label": "补充一句（模板为主）"
              },
              {
                "value": "rewrite",
                "label": "改写整条（AI 为主）"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "append"
        },
        {
          "path": "ai_chat.ai_reply.include_template",
          "label": "把回复内容给 AI 看",
          "hint": "开启后 AI 能看到已填充变量的真实回复（如「小明点播的《晴天》已排上」）；关闭只能看到占位符骨架，不建议关闭",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "ai_chat.ai_reply.include_command",
          "label": "把命令名给 AI 看",
          "hint": "开启后 AI 知道自己在响应哪条命令，如 @QQ点歌",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": true
        },
        {
          "path": "ai_chat.ai_reply.system_prompt",
          "label": "系统提示词",
          "hint": "留空使用内置默认值；用于约束 AI 的语气与输出格式",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "你在为一款原神游戏机器人生成简短的聊天回复。只输出一句话，不要引号、不要解释。"
        },
        {
          "path": "ai_chat.ai_reply.user_prompt",
          "label": "用户提示词",
          "hint": "可使用 {{command}}、{{template}}；留空使用内置默认值",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "命令：{{command}}\n已知回复：{{template}}\n请补一句自然、符合情境的话。"
        },
        {
          "path": "ai_chat.ai_reply.max_tokens",
          "label": "生成上限",
          "hint": "单次生成的最大 token 数；回复本来就很短，默认 120 足够",
          "kind": {
            "max": 4096,
            "min": 16,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 512
        },
        {
          "path": "ai_chat.ai_reply.timeout_seconds",
          "label": "超时秒数",
          "hint": "超过该时长按失败处理，AI回复 留空继续发送",
          "kind": {
            "max": 120,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": "AI 回复",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 8
        },
        {
          "path": "ai_chat.protocols.memory",
          "label": "记忆与回复规范",
          "hint": "留空用内置；讲「记忆只是参考资料、回答要简短」",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "正文协议",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.protocols.identity",
          "label": "身份与归属规范",
          "hint": "留空用内置；讲「本轮只有一个人在说话、别人的名字不算你的」",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "正文协议",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.protocols.time",
          "label": "时间规范",
          "hint": "留空用内置",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "正文协议",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.protocols.action",
          "label": "动作规范",
          "hint": "留空用内置",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "正文协议",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.protocols.reply",
          "label": "回复格式（JSON 契约）",
          "hint": "必须保留 \"action\" 与 \"text\" 两个键名，否则回复会被丢弃",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "正文协议",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.protocols.tool",
          "label": "工具调用协议",
          "hint": "留空用内置；不要写「禁止调用工具」这类否定",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "正文协议",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.tool_descriptions.look",
          "label": "look 工具描述",
          "hint": "进 tools 数组的短描述；留空用内置",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "工具描述",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.tool_descriptions.speak",
          "label": "speak 工具描述",
          "hint": "进 tools 数组的短描述；留空用内置；模型相关的示例后缀由代码追加",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "工具描述",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.tool_descriptions.command",
          "label": "命令工具描述",
          "hint": "进 tools 数组的短描述；留空用内置",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "工具描述",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.summary_prompt",
          "label": "记忆压缩提示词",
          "hint": "可用 {{previous}}、{{records}}；留空用内置",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "长文本提示词",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "ai_chat.vision_prompt",
          "label": "视觉子模型提示词",
          "hint": "可用 {{question}}、{{instruction}}；留空用内置",
          "kind": {
            "type": "text"
          },
          "effect": "live",
          "source": "db",
          "group": "长文本提示词",
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        }
      ]
    },
    {
      "name": "tts",
      "label": "语音 TTS",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 35,
      "fields": [
        {
          "path": "tts.enabled",
          "label": "启用 TTS",
          "hint": "允许 AI 聊天工具 speak 请求 fish.audio；未配置 api_key 时不发请求",
          "kind": {
            "type": "bool"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "tts.endpoint",
          "label": "fish 接口地址",
          "hint": "fish.audio 基础地址；默认 https://api.fish.audio，实际请求 {endpoint}/v1/tts",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "https://api.fish.audio"
        },
        {
          "path": "tts.http_proxy",
          "label": "TTS 代理",
          "hint": "可选独立 HTTP(S)/SOCKS 代理，作用于 fish 音色目录与 /v1/tts 合成；留空沿用环境代理",
          "kind": {
            "type": "string"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "tts.api_key",
          "label": "fish API Key",
          "hint": "fish.audio API key，只以 Authorization Bearer 发送；空闲后整程序重启时生效",
          "kind": {
            "type": "secret"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "••••••"
        },
        {
          "path": "tts.model",
          "label": "TTS 模型",
          "hint": "作为 HTTP model 头发送，默认 s2.1-pro",
          "kind": {
            "type": "modelName"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "s2.1-pro"
        },
        {
          "path": "tts.reference_id",
          "label": "参考音色 ID",
          "hint": "fish reference_id；留空时可使用 voice",
          "kind": {
            "type": "string"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "tts.voice",
          "label": "音色别名",
          "hint": "reference_id 为空时作为 fish reference_id 发送",
          "kind": {
            "type": "string"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": ""
        },
        {
          "path": "tts.format",
          "label": "音频格式",
          "hint": "当前仅支持 WAV；SpeechPlayer 直接解码 WAV",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "wav",
                "label": "WAV"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "wav"
        },
        {
          "path": "tts.duck_volume",
          "label": "说话时音乐音量",
          "hint": "说话期间实际输出增益百分比；100=不压低，默认 30",
          "kind": {
            "max": 100,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 30
        },
        {
          "path": "tts.duck_fade_ms",
          "label": "duck 淡入淡出",
          "hint": "压低/恢复增益的渐变时长，默认 200ms",
          "kind": {
            "max": 2000,
            "min": 0,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        },
        {
          "path": "tts.max_chars",
          "label": "单条文本上限",
          "hint": "超过该字符数截断，控制费用与延迟，默认 200",
          "kind": {
            "max": 2000,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 200
        },
        {
          "path": "tts.timeout_ms",
          "label": "请求超时",
          "hint": "fish 请求超时，默认 8000ms；超时静默跳过语音",
          "kind": {
            "max": 60000,
            "min": 1000,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 8000
        },
        {
          "path": "tts.queue_max",
          "label": "语音队列上限",
          "hint": "排队等待播放的语音条数上限，默认 8",
          "kind": {
            "max": 64,
            "min": 1,
            "type": "int"
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": 8
        },
        {
          "path": "tts.trigger_scope",
          "label": "触发范围",
          "hint": "当前仅 AI 聊天工具 speak 可触发，普通回复不自动朗读",
          "kind": {
            "type": "enum",
            "values": [
              {
                "value": "ai_chat",
                "label": "仅 AI 聊天"
              }
            ]
          },
          "effect": "live",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": "ai_chat"
        }
      ]
    },
    {
      "name": "emoji",
      "label": "二级聊天表情",
      "domain": "interaction",
      "domainLabel": "AI 与交互",
      "order": 36,
      "fields": [
        {
          "path": "emoji.enabled",
          "label": "启用表情发送",
          "hint": "开启后二级当前聊天支持 {表情包:页签:文字} 标签；关闭不影响文字发送",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "emoji.ai_enabled",
          "label": "允许 AI 回复使用表情",
          "hint": "开启后 AI 只能使用下面白名单里的表情；关闭时 AI 回复中的表情标签会被剥离",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        },
        {
          "path": "emoji.ai_allowed",
          "label": "AI 可用表情",
          "hint": "每项格式 页签序号|表情文字，例如 2|交给我吧；页签 1=收藏，2..6=角色；第 7 个页签会把收藏页挤走并隐藏，不支持",
          "kind": {
            "type": "stringArray"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": []
        },
        {
          "path": "emoji.capture_debug",
          "label": "保存表情调试截图",
          "hint": "默认关闭；开启后表情流程的完整截图落到 deps/emoji-debug（可能含聊天内容）",
          "kind": {
            "type": "bool"
          },
          "effect": "idleReload",
          "source": "db",
          "group": null,
          "nullable": false,
          "optionalParent": false,
          "managedBy": null,
          "default": false
        }
      ]
    }
  ],
  "values": {
    "visual": {
      "name": "1080P"
    },
    "command_access": {
      "overrides": {},
      "default_song_source": null
    },
    "chat_source": {
      "enabled": false,
      "url": "ws://127.0.0.1:18889/stream",
      "token": "••••••",
      "reconnect_ms": 3000,
      "lease_timeout_ms": 10000,
      "auto_start": false,
      "executable": "",
      "disconnect_action": "fallback_ocr",
      "self_uid": 0
    },
    "window": {
      "target_process": "yuanshen.exe,GenshinImpact.exe",
      "content_width": 1920,
      "content_height": 1080,
      "auto_activate_window": false,
      "capture_method": "gdi",
      "focus_point": {
        "x": 1919,
        "y": 1000
      }
    },
    "screen": {
      "expected_width": 1920,
      "expected_height": 1080,
      "warn_on_size_mismatch": true,
      "chat_rect": {
        "height": 143,
        "width": 416,
        "x": 39,
        "y": 879
      },
      "friend_rect": {
        "height": 40,
        "width": 50,
        "x": 170,
        "y": 1018
      },
      "secondary_back_rect": {
        "height": 65,
        "width": 65,
        "x": 15,
        "y": 15
      },
      "secondary_hall_rect": {
        "height": 55,
        "width": 65,
        "x": 10,
        "y": 190
      },
      "secondary_chat_submit_rect": {
        "height": 124,
        "width": 150,
        "x": 930,
        "y": 950
      },
      "main_menu_rect": {
        "height": 44,
        "width": 153,
        "x": 1668,
        "y": 998
      },
      "hall_bar_rect": {
        "height": 95,
        "width": 600,
        "x": 0,
        "y": 985
      },
      "qianxing_f2_invite_rect": {
        "height": 54,
        "width": 140,
        "x": 1650,
        "y": 993
      },
      "world_wish_rect": {
        "height": 110,
        "width": 620,
        "x": 1300,
        "y": 0
      },
      "hall_name_rect": {
        "height": 40,
        "width": 325,
        "x": 75,
        "y": 425
      },
      "hall_member_count_rect": {
        "height": 50,
        "width": 450,
        "x": 75,
        "y": 470
      },
      "hall_time_rect": {
        "height": 40,
        "width": 110,
        "x": 430,
        "y": 520
      },
      "hall_member_list_rect": {
        "height": 850,
        "width": 560,
        "x": 1280,
        "y": 110
      }
    },
    "stability": {
      "default_count": 2,
      "ui_state_count": 0,
      "secondary_hall_count": 0
    },
    "timing": {
      "watchdog_restart_ms": 2000,
      "loop_idle_ms": 60,
      "chat_scan": {
        "fallback_ms": 2000,
        "change_debounce_ms": 120,
        "change_cooldown_ms": 250
      },
      "command": {
        "ui_timeout_ms": 15000,
        "return_retry_ms": 1000,
        "post_settle_ms": 500,
        "help_batch_ms": 500
      },
      "input": {
        "after_activate_ms": 200,
        "focus_ms": 300,
        "open_chat_ms": 300,
        "click_ms": 150,
        "text_ms": 250,
        "send_ms": 300
      },
      "workflow": {
        "default_timeout_ms": 5000,
        "default_poll_ms": 200,
        "default_step_wait_ms": 300
      },
      "hall": {
        "page_settle_ms": 800,
        "ocr_sample_interval_ms": 120
      },
      "invite": {
        "open_chat_ms": 400,
        "step_ms": 800,
        "confirm_timeout_ms": 30000,
        "confirm_poll_ms": 2000
      },
      "moderation": {
        "vote_timeout_ms": 120000,
        "vote_poll_ms": 2000,
        "search_result_timeout_ms": 5000,
        "confirm_wait_ms": 2000
      },
      "playback": {
        "status_poll_ms": 1000,
        "monitor_tick_ms": 200,
        "monitor_status_ms": 1000,
        "lyrics_lead_seconds": 0.0,
        "uri_stable_samples": 0,
        "transport_stable_samples": 0,
        "stale_timeout_ms": 5000
      },
      "decision": {
        "timeout_ms": 20000,
        "poll_ms": 2000
      },
      "external": {
        "volume_smooth_step_ms": 300,
        "ai_request_timeout_ms": 35000
      }
    },
    "ocr": {
      "det_model": "deps/models/PP-OCRv6_small_det.mnn",
      "rec_model": "deps/models/PP-OCRv6_small_rec.mnn",
      "charset": "deps/models/ppocr_keys_v6_small.txt",
      "min_confidence": 0.9,
      "threads": 4,
      "request_timeout_ms": 10000,
      "shutdown_timeout_ms": 5000,
      "backend_priority": [
        "openvino",
        "cpu"
      ],
      "openvino": {
        "det_model": "deps/models/PP-OCRv6_small_det.xml",
        "det_weights": "deps/models/PP-OCRv6_small_det.bin",
        "rec_model": "deps/models/PP-OCRv6_small_rec.xml",
        "rec_weights": "deps/models/PP-OCRv6_small_rec.bin",
        "rec_fixed_width": null,
        "device": "CPU",
        "cache_dir": "deps/data/openvino-cache"
      },
      "det_max_side_len": 960,
      "det_score_threshold": 0.3,
      "det_unclip_ratio": 2.0,
      "det_min_area": 9,
      "det_box_border": 0,
      "change_mean_threshold": 6.0,
      "change_pixel_threshold": 0.03,
      "text_left_gap": 8,
      "block_top_padding": 2,
      "block_bottom_padding": 2,
      "max_block_height": 120,
      "same_line_y_tolerance": 10,
      "marker_search_width": 60,
      "marker_dedupe_min_x": 22,
      "marker_dedupe_min_y": 14,
      "marker_dedupe_x": 8,
      "marker_dedupe_y": 8,
      "next_marker_min_gap": 12,
      "right_padding": 4,
      "batch_recognize": false
    },
    "templates": {
      "panel_downsample_factors": {},
      "blue_marker": "deps/assets/chat-marker-blue.png",
      "yellow_marker": "deps/assets/chat-marker-yellow.png",
      "pink_marker": "deps/assets/chat-marker-pink.png",
      "secondary_back": "deps/assets/ui-secondary-back.png",
      "secondary_hall": "deps/assets/ui-secondary-hall.png",
      "secondary_chat_submit": "deps/assets/ui-secondary-chat-submit.png",
      "qianxing_f2_hall_list": "deps/assets/qianxing-f2-hall-list.png",
      "qianxing_f2_invite_player": "deps/assets/qianxing-f2-invite-player.png",
      "qianxing_exit_menu": "deps/assets/qianxing-exit-menu.png",
      "qianxing_map_label": "deps/assets/qianxing-map-label.png",
      "qianxing_profile_page_wishlist": "deps/assets/qianxing-profile-page-wishlist.png",
      "qianxing_profile_page_wardrobe": "deps/assets/qianxing-profile-page-wardrobe.png",
      "teyvat_profile_adventure_rank": "deps/assets/teyvat-profile-adventure-rank.png",
      "teyvat_profile_world_level": "deps/assets/teyvat-profile-world-level.png",
      "teyvat_profile_add_friend": "deps/assets/teyvat-profile-add-friend.png",
      "qianxing_profile_domain_level": "deps/assets/qianxing-profile-domain-level.png",
      "qianxing_profile_add_friend": "deps/assets/qianxing-profile-add-friend.png",
      "qianxing_profile_view_profile": "deps/assets/invite-view-profile.png",
      "main_menu": "deps/assets/ui-main-menu-return.png",
      "world_wish": "deps/assets/world-wish.png",
      "invite_view_star": "deps/assets/invite-view-star.png",
      "invite_goto_hall": "deps/assets/invite-goto-hall.png",
      "invite_enter_hall": "deps/assets/invite-enter-hall.png",
      "friend_panel": "deps/assets/friend-panel.png",
      "friend_search_panel": "deps/assets/friend-search-panel.png",
      "friend_more_settings": "deps/assets/friend-more-settings.png",
      "friend_block_chat": "deps/assets/friend-block-chat.png",
      "friend_blacklist": "deps/assets/friend-blacklist.png",
      "confirm_button": "deps/assets/confirm-button.png",
      "confirm_message_random_hall": "deps/assets/confirm-message-random-hall.png",
      "confirm_message_return_teyvat": "deps/assets/confirm-message-return-teyvat.png",
      "confirm_message_block": "deps/assets/confirm-message-block.png",
      "marker_threshold": 0.9
    },
    "output": {
      "send_enabled": true,
      "focus_point": {
        "x": 1919,
        "y": 1000
      },
      "chat_click_2": {
        "x": 600,
        "y": 1013
      }
    },
    "moderation": {
      "stable_vote_samples": 3,
      "required_vote_margin": 3,
      "friend_panel_region": {
        "height": 50,
        "width": 75,
        "x": 770,
        "y": 20
      },
      "search_panel_region": {
        "height": 49,
        "width": 74,
        "x": 1645,
        "y": 114
      },
      "search_input_point": {
        "x": 1180,
        "y": 135
      },
      "search_button_point": {
        "x": 1680,
        "y": 135
      },
      "more_settings_region": {
        "height": 70,
        "width": 80,
        "x": 1140,
        "y": 180
      },
      "block_chat_region": {
        "height": 112,
        "width": 124,
        "x": 1400,
        "y": 362
      },
      "blacklist_region": {
        "height": 117,
        "width": 146,
        "x": 1388,
        "y": 494
      },
      "confirm_button_region": {
        "height": 58,
        "width": 81,
        "x": 1140,
        "y": 728
      },
      "confirm_message_region": {
        "height": 90,
        "width": 620,
        "x": 650,
        "y": 460
      }
    },
    "playback": {
      "credential_directory": "deps/data/credentials",
      "login_helper_executable": "deps/bin/miliastra-login-helper.exe",
      "login_timeout_ms": 180000,
      "audio_cache": {
        "directory": "deps/cache/audio",
        "enabled": true,
        "max_bytes_mb": 20480,
        "max_concurrent_downloads": 2,
        "request_timeout_ms": 15000,
        "seek_wait_timeout_ms": 10000
      }
    },
    "http": {
      "enabled": true,
      "host": "127.0.0.1",
      "port": 18888,
      "access_token": "••••••"
    },
    "logging": {
      "dir": "deps/logs",
      "level": "info",
      "rotate_daily": true,
      "retain_days": 7
    },
    "tui": {
      "enabled": true,
      "refresh_ms": 100,
      "log_lines": 200
    },
    "state": {
      "hall_state_path": "deps/data/hall-state.json",
      "executed_commands_log_path": "deps/data/executed-commands.log"
    },
    "diagnostics": {
      "ui_frames": false
    },
    "queue": {
      "max_size": 5,
      "pool_max_size": 1000,
      "protect_current_song_until_finished": true,
      "external_playback_protect_after_seconds": 20
    },
    "song_dedup": {
      "enabled": true,
      "window_seconds": 3600,
      "max_count": 1,
      "console_bypass": true,
      "history_path": "deps/data/song-dedup-history.json"
    },
    "idiom_chain": {
      "enabled": true,
      "lexicon_path": "deps/assets/idioms.txt",
      "history_limit": 200,
      "idle_timeout_seconds": 300,
      "allow_consecutive_player": false,
      "allow_anyone_stop": false
    },
    "landlord": {
      "enabled": true,
      "lobby_timeout_seconds": 120,
      "turn_timeout_seconds": 90,
      "trustee_after_timeouts": 2,
      "hand_cooldown_seconds": 10
    },
    "undercover": {
      "enabled": false,
      "word_bank_path": "deps/undercover.yaml",
      "used_state_path": "deps/data/undercover-used.yaml",
      "min_players": 4,
      "double_min_players": 6,
      "max_players": 11,
      "lobby_timeout_seconds": 180,
      "phase_timeout_seconds": 180,
      "progress_interval_seconds": 20,
      "description_max_width": 70
    },
    "turtle_soup": {
      "enabled": false,
      "question_bank_path": "deps/turtle_soup.yaml",
      "used_state_path": "deps/data/turtle-soup-used.json",
      "idle_timeout_seconds": 600,
      "max_session_seconds": 3600,
      "max_concurrency": 4,
      "max_pending": 32,
      "batch_max_parts": 32,
      "nickname_stable_count": 0,
      "content_stable_count": 0,
      "request_timeout_seconds": 20,
      "retry_count": 2,
      "retry_delay_ms": 500,
      "custom_prompt": "",
      "system_prompt": "{{role}}{{verification_rules}}只返回合法 json 对象。",
      "review_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数",
      "model": "",
      "reasoning": "unset",
      "max_tokens": 1024
    },
    "number_bomb": {
      "enabled": true,
      "min": 1,
      "max": 100,
      "turn_timeout_seconds": 60,
      "min_players": 2,
      "max_players": 11,
      "extra_range_per_player": 100,
      "upper_bound_cap": 1000
    },
    "ai": {
      "provider": "openai",
      "api_key": "••••••",
      "endpoint": "",
      "responses_endpoint": "",
      "model": "gpt-5.6-mini",
      "http_proxy": "",
      "wire_api": "responses",
      "extra_body": {},
      "reasoning_rules": []
    },
    "song_ai": {
      "model": "",
      "reasoning": "unset",
      "local_recommend": true,
      "local_recommend_min_score": 0.6,
      "system_prompt": "你是点歌 JSON 结构化输出助手。必须只返回合法 JSON。",
      "recognize_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板",
      "match_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文",
      "candidate_pick_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出"
    },
    "song_review": {
      "enabled": false,
      "max_allowed_level": 4,
      "failure_policy": "reject",
      "retry_count": 2,
      "retry_delay_ms": 500,
      "reply_reason_max_chars": 40,
      "system_prompt": "你是点歌审核助手，负责判断候选歌曲是否适合舒缓、轻松、不吵闹的房间氛围。必须只返回合法 JSON。",
      "review_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位",
      "policy_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于",
      "custom_prompt": "",
      "model": "",
      "reasoning": "unset",
      "web_search": true,
      "max_tokens": 512
    },
    "matching": {
      "min_song_name_score": 0.5,
      "short_chinese_song_max_miss": 1,
      "long_chinese_song_min_score": 0.5,
      "max_ocr_noise_chars": 1,
      "enable_fuzzy_singer": true,
      "short_chinese_singer_max_miss": 1,
      "long_chinese_singer_min_score": 0.8,
      "en_max_edit_fraction": 0.3,
      "en_singer_max_edit_fraction": 0.35
    },
    "hotkeys": {
      "enabled": true,
      "pause_key": "F7",
      "exit_key": "F12"
    },
    "startup": {
      "enabled": false,
      "launch_game": false,
      "enter_game": false,
      "enter_wonderland": false,
      "exe_path": "",
      "game_args": "",
      "launch_wait_ms": 3000,
      "launch_retries": 20,
      "enter_game_timeout_ms": 45000,
      "enter_wonderland_timeout_ms": 60000,
      "wonderland_map_star_retries": 6,
      "wonderland_map_star_retry_ms": 2500,
      "wonderland_hall_retries": 6,
      "wonderland_hall_retry_ms": 2500,
      "wonderland_transition_timeout_ms": 4000,
      "wonderland_confirm_stable_timeout_ms": 3000,
      "final_primary_timeout_ms": 10000,
      "restart_game_on_loading": false,
      "loading_restart_timeout_ms": 60000,
      "poll_ms": 1000,
      "stable_mean_threshold": 2.0,
      "stable_changed_ratio_threshold": 0.01,
      "template_threshold": 0.9,
      "wonderland_confirm_threshold": 0.9,
      "templates": {
        "wonderland_map_star": "deps/assets/startup-wonderland-map-star.png",
        "paimon_menu": "deps/assets/startup-paimon-menu.png"
      },
      "enter_game_text_region": {
        "height": 40,
        "width": 130,
        "x": 900,
        "y": 1000
      },
      "wonderland_hall_ocr_region": {
        "height": 1040,
        "width": 640,
        "x": 1280,
        "y": 0
      },
      "main_ui_region": {
        "height": 270,
        "width": 480,
        "x": 0,
        "y": 0
      },
      "wonderland_map_star_region": {
        "height": 230,
        "width": 220,
        "x": 1700,
        "y": 850
      }
    },
    "invite": {
      "friend_name_stable_count": 0,
      "fuzzy_friend_match": false,
      "friend_list_region": {
        "height": 600,
        "width": 170,
        "x": 80,
        "y": 280
      },
      "friend_chat_region": {
        "height": 850,
        "width": 920,
        "x": 260,
        "y": 100
      },
      "view_star_region": {
        "height": 860,
        "width": 217,
        "x": 498,
        "y": 80
      },
      "goto_hall_region": {
        "height": 112,
        "width": 372,
        "x": 780,
        "y": 560
      },
      "enter_hall_region": {
        "height": 54,
        "width": 164,
        "x": 896,
        "y": 730
      }
    },
    "friend_delivery": {
      "auto_retry_count": 0
    },
    "custom_workflows": {
      "enabled": true,
      "default_threshold": 0.9,
      "wait_template_absent_stable_default": true,
      "max_hold_key_seconds": 10,
      "templates": {},
      "workflows": null
    },
    "identity": {
      "mappings": []
    },
    "kugou": {
      "device_guid": "",
      "device_dev": "",
      "device_mac": ""
    },
    "ai_chat": {
      "enabled": false,
      "keyword": "橘子",
      "trigger_mode": "prefix",
      "min_access": "everyone",
      "actor_token_budget": 96000,
      "actor_summary_budget": 4000,
      "actor_history_token_budget": 12000,
      "inject_actor_history": true,
      "hall_token_budget": 32000,
      "hall_summary_budget": 4000,
      "bot_token_budget": 32000,
      "cooldown_seconds": 8,
      "allow_commands": true,
      "model": "",
      "reasoning": "unset",
      "max_tokens": 1500,
      "system_prompt": "你是一个游戏大厅里的聊天伙伴，名字由用户设定。需要执行游戏命令时使用提供的工具，依据工具回执回答，不要声称尚未执行的操作已经成功。",
      "answer_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行",
      "extract_prompt": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占",
      "tool_prompts": {
        "look": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高",
        "speak": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用",
        "command": "这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出真实行高。这是面板假数据用的提示词占位文本，用于撑出"
      },
      "vision": {
        "enabled": true,
        "mode": "auto",
        "direct_to_main": false,
        "capture_scope": "primary_screen",
        "max_edge": 1600,
        "jpeg_quality": 100,
        "max_bytes": 4000000,
        "timeout_ms": 800,
        "model": "",
        "allow_full_screen": true
      },
      "ai_reply": {
        "mode": "append",
        "include_template": true,
        "include_command": true,
        "system_prompt": "你在为一款原神游戏机器人生成简短的聊天回复。只输出一句话，不要引号、不要解释。",
        "user_prompt": "命令：{{command}}\n已知回复：{{template}}\n请补一句自然、符合情境的话。",
        "max_tokens": 512,
        "timeout_seconds": 8
      },
      "protocols": {
        "memory": "",
        "identity": "",
        "time": "",
        "action": "",
        "reply": "",
        "tool": ""
      },
      "tool_descriptions": {
        "look": "",
        "speak": "",
        "command": ""
      },
      "summary_prompt": "",
      "vision_prompt": ""
    },
    "tts": {
      "enabled": false,
      "endpoint": "https://api.fish.audio",
      "http_proxy": "",
      "api_key": "••••••",
      "model": "s2.1-pro",
      "reference_id": "",
      "voice": "",
      "format": "wav",
      "duck_volume": 30,
      "duck_fade_ms": 200,
      "max_chars": 200,
      "timeout_ms": 8000,
      "queue_max": 8,
      "trigger_scope": "ai_chat"
    },
    "emoji": {
      "enabled": false,
      "ai_enabled": false,
      "ai_allowed": [],
      "capture_debug": false
    }
  },
  "missingMeta": []
}
