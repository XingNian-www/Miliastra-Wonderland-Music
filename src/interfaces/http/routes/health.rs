use super::*;

/// `GET /health/services` —— 进程级只读服务快照（W0-C）。
///
/// **为什么不复用既有的 `GET /health`**：`/health` 是早就存在的探活约定，
/// 返回纯文本 `OK`、且**免令牌**——面板 (`web/src/store.js`) 靠它判断「后端连上了
/// 没有」，`scripts/release-pipeline.sh` 靠它判 200，`SettingsPage.vue` 的注释也
/// 明说「/health 不需要 token」。把它改成 JSON 并加鉴权是**行为变更**，与 W0
/// 的「零行为改动」红线冲突，所以新增本端点，`/health` 逐字节保持原样。
///
/// 三条约束：
/// - **鉴权与其它面板端点一致**：路径不在 `requires_access_token` 的豁免表里，
///   因此自动复用既有的 `X-Miliastra-Token` 校验，没有新造鉴权。
/// - **只读**：`RouteSpec.mutating = false`，GET 直接放行；处理函数只读一次
///   注册表，不调用任何业务端口。
/// - **拉取式**：不订阅、不写 `MonitorEvent`（那条通道无界，心跳进去会无限增长）。
pub(super) fn health_services_route(
    _query: &[(String, String)],
    _state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    render_health_snapshot(&crate::runtime::health::snapshot())
}

/// 纯渲染：把快照转成 JSON 文本。不读业务状态、不写计数器，可安全重复调用。
pub(super) fn render_health_snapshot(
    snapshot: &crate::runtime::health::HealthSnapshot,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(snapshot).map_err(internal_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 连续两次 GET 不改变任何服务计数、不产生副作用。
    ///
    /// 不在此处起真实 TCP 服务：完整 HTTP 夹具在 `protocol/tests.rs`，属其它
    /// agent 的文件域，本波不碰。这里锁住三条等价事实：
    /// 1. 路由登记为只读（`mutating = false`）且按 JSON 返回；
    /// 2. 渲染是纯函数：同一份快照渲染两次逐字节相同；
    /// 3. 两次「拉取」之间计数**只增不减**——读操作不会清零或回写任何计数。
    #[test]
    fn health_endpoint_is_read_only() {
        let spec = route_spec("/health/services").expect("health route must be registered");
        assert!(!spec.mutating, "health 端点必须是只读路由");
        assert!(spec.json, "health 端点必须按 JSON 返回");

        let before = crate::runtime::health::snapshot();
        let first_body = render_health_snapshot(&before).expect("render health snapshot");
        let rerender = render_health_snapshot(&before).expect("render health snapshot");
        assert_eq!(first_body, rerender, "同一份快照必须渲染出相同文本");

        let after = crate::runtime::health::snapshot();
        let second_body = render_health_snapshot(&after).expect("render health snapshot");
        assert_eq!(before.services.len(), after.services.len());
        for (earlier, later) in before.services.iter().zip(after.services.iter()) {
            assert_eq!(earlier.name, later.name);
            assert_eq!(earlier.start_step, later.start_step);
            assert_eq!(earlier.queue_depth, later.queue_depth);
            // 只增不减：读端点不可能清零/回写计数（真实 actor 只可能把它推高）。
            for (field, earlier_value, later_value) in [
                (
                    "in_flight",
                    earlier.in_flight as u64,
                    later.in_flight as u64,
                ),
                ("accepted", earlier.accepted, later.accepted),
                ("completed", earlier.completed, later.completed),
                ("rejected", earlier.rejected, later.rejected),
                ("dropped", earlier.dropped, later.dropped),
            ] {
                assert!(
                    later_value >= earlier_value,
                    "{}：{field} 只能单调不减（{earlier_value} -> {later_value}）",
                    earlier.name
                );
            }
            if let (Some(earlier_at), Some(later_at)) =
                (earlier.last_progress_at, later.last_progress_at)
            {
                assert!(
                    later_at >= earlier_at,
                    "{}：last_progress_at 只能向前",
                    earlier.name
                );
            }
        }

        // 隐私：响应体里没有路径、昵称、令牌。
        for forbidden in ["/", "\\", "token", "uid"] {
            assert!(
                !second_body.to_ascii_lowercase().contains(forbidden),
                "health 响应出现敏感片段 {forbidden:?}：{second_body}"
            );
        }
    }

    /// 端点契约：`{ services: [...], generated_at_ms }`，字段名与类型固定。
    #[test]
    fn health_endpoint_returns_the_documented_shape() {
        let body = render_health_snapshot(&crate::runtime::health::snapshot()).expect("render");
        let json: Value = serde_json::from_str(&body).expect("json");
        let services = json["services"].as_array().expect("services array");
        assert_eq!(services.len(), 5);
        let expected = [
            "accepted",
            "completed",
            "dropped",
            "in_flight",
            "last_progress_at",
            "name",
            "queue_depth",
            "rejected",
            "start_step",
            "state",
        ];
        for service in services {
            let mut keys: Vec<_> = service
                .as_object()
                .expect("service object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, expected);
        }
        assert!(json["generated_at_ms"].is_u64());
    }
}
