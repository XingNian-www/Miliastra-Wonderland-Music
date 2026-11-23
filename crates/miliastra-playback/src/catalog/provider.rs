use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::Song;

/// 一条音源登记。`PROVIDERS` 是 [`ProviderId`] 的**唯一**登记点：
/// 枚举变体、`ALL`、`as_str()`、`FromStr`、`Display`、serde 线上值
/// 以及面板显示名全部由它派生。
///
/// 为什么只有这几个字段：
/// - `id` 是对外契约（配置值、HTTP 查询参数、凭据文件名 `{id}.json`），换字即破坏兼容；
/// - `display_name` 是面板/日志用的中文名。历史上它和 `id` 分住在
///   `adapters/login_helper.rs` 与两份 Vue 页的 `sourceNames` 里，共 3 份；
///   这里接管登记权，尚未迁走的消费方（`login_helper` / Web 面板，均在本切片
///   文件域之外）由 `provider_display_names_are_stable` 对拍钉住，待 W4c 迁移。
///
/// 「是否需要凭据 / 是否支持手动刷新 / 支持哪些歌单链接」刻意**不**放在这里：
/// 它们目前由 `credentials.rs` 的 `ProviderCredential` 变体与 `playlist_import.rs`
/// 的 host 表按格式而非按 id 判定，搬进本表会造出第二份事实源。
pub struct ProviderSpec {
    pub variant: ProviderId,
    /// 稳定线上标识（配置值、HTTP 参数、凭据文件名）。
    pub id: &'static str,
    /// 中文显示名。
    pub display_name: &'static str,
}

/// 由一张表生成 `ProviderId` 的全部重复定义。
///
/// 加变体必须写进 `provider_registry!`；漏登记的地方（`ALL`、`as_str`、
/// `FromStr`、`PROVIDERS`）由宏一起补齐，而**别处**对 `ProviderId` 的穷尽
/// `match`（`interfaces/http/facade.rs`、`runtime.rs`、`adapters/login_helper.rs`）
/// 会因缺分支直接编译失败——这就是「不登记即失败」的编译器护栏。
macro_rules! provider_registry {
    ($( $variant:ident => $id:literal, $display_name:literal ),+ $(,)?) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize,
        )]
        pub enum ProviderId {
            $(
                #[serde(rename = $id)]
                $variant,
            )+
        }

        impl ProviderId {
            pub const ALL: [Self; PROVIDERS.len()] = [ $( Self::$variant, )+ ];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $id, )+
                }
            }

            pub const fn display_name(self) -> &'static str {
                match self {
                    $( Self::$variant => $display_name, )+
                }
            }
        }

        pub const PROVIDERS: &[ProviderSpec] = &[
            $(
                ProviderSpec {
                    variant: ProviderId::$variant,
                    id: $id,
                    display_name: $display_name,
                },
            )+
        ];

        impl FromStr for ProviderId {
            type Err = UnknownProvider;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $( $id => Ok(Self::$variant), )+
                    _ => Err(UnknownProvider(value.to_owned())),
                }
            }
        }
    };
}

provider_registry! {
    QqMusic => "qqmusic", "QQ音乐",
    Netease => "netease", "网易云音乐",
    Bilibili => "bilibili", "哔哩哔哩",
    Kugou => "kugou", "酷狗音乐",
}

/// `PROVIDERS` 与 `ALL` 必须同长、同序：表是派生源，手写数组不是。
const _: () = assert!(PROVIDERS.len() == ProviderId::ALL.len());

impl Display for ProviderId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<ProviderId> for String {
    fn from(provider: ProviderId) -> Self {
        provider.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider: {0}")]
pub struct UnknownProvider(pub String);

/// 原生运行时始终存在的提供商注册表。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderRegistry;

impl ProviderRegistry {
    pub fn known(&self, provider: &str) -> bool {
        provider.parse::<ProviderId>().is_ok()
    }

    pub fn enabled(&self) -> Vec<String> {
        ProviderId::ALL.into_iter().map(String::from).collect()
    }

    pub fn require_enabled(&self, provider: &str) -> Result<ProviderId, crate::catalog::Failure> {
        let provider_id = provider.parse::<ProviderId>().map_err(|_| {
            crate::catalog::Failure::new("unknown_provider", "provider identifier is unknown")
                .with_provider(provider)
        })?;
        Ok(provider_id)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackEligibility {
    /// 可直接播放。
    Eligible,
    /// 需要 VIP/付费才能完整播放。
    #[serde(rename = "vip")]
    VipRequired,
    /// 无版权/版权受限（含仅试听），不可完整播放。
    #[serde(rename = "no_copyright")]
    NoCopyright,
    /// 需要单独购买（数字专辑/单曲）。
    #[serde(rename = "paid")]
    PaidRequired,
    /// 不可播放（无有效音源等）。
    Ineligible,
    #[default]
    Unknown,
}

impl PlaybackEligibility {
    pub const fn preference_rank(self) -> u8 {
        match self {
            Self::Eligible => 2,
            Self::Unknown => 1,
            Self::VipRequired | Self::NoCopyright | Self::PaidRequired | Self::Ineligible => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSearchCandidate {
    #[serde(flatten)]
    pub song: Song,
    pub eligibility: PlaybackEligibility,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSearchOutcome {
    pub provider: String,
    pub candidates: Vec<ProviderSearchCandidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::catalog::Failure>,
}

impl ProviderSearchOutcome {
    pub fn success(provider: impl Into<String>, candidates: Vec<ProviderSearchCandidate>) -> Self {
        Self {
            provider: provider.into(),
            candidates,
            failure: None,
        }
    }

    pub fn failed(provider: impl Into<String>, failure: crate::catalog::Failure) -> Self {
        let provider = provider.into();
        // 一个结果只属于一个请求中的提供商，底层错误不能误将此分支归因到其他提供商。
        let failure = failure.with_provider(provider.clone());
        Self {
            provider,
            candidates: Vec::new(),
            failure: Some(failure),
        }
    }

    pub fn is_success(&self) -> bool {
        self.failure.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表的唯一性：`as_str` → `FromStr` 必须逐项往返，且 `ALL` 与 enum 变体
    /// 数量一致。
    ///
    /// 变异靶子：把 `provider_registry!` 里某一行的 `id` 改掉（但 `PROVIDERS`
    /// 的 `display_name` 等其它列不动），往返立刻红；把该行整条删掉，
    /// `PROVIDERS.len() == ALL.len()` 与 `entries_cover_every_variant` 同时红。
    #[test]
    fn provider_ids_round_trip() {
        assert_eq!(
            PROVIDERS.len(),
            ProviderId::ALL.len(),
            "PROVIDERS 与 ALL 必须同长"
        );
        for (index, spec) in PROVIDERS.iter().enumerate() {
            assert_eq!(
                spec.variant,
                ProviderId::ALL[index],
                "PROVIDERS[{}] 的顺序必须与 ALL 一致",
                index
            );
            assert_eq!(
                spec.id,
                spec.variant.as_str(),
                "PROVIDERS[{}] 的 id 必须等于 as_str()",
                index
            );
            let parsed: ProviderId = spec.id.parse().expect("登记过的 id 必须能解析");
            assert_eq!(parsed, spec.variant, "{} 往返不一致", spec.id);
            assert_eq!(parsed.to_string(), spec.id, "{} Display 不一致", spec.id);
            assert_eq!(
                serde_json::to_string(&spec.variant).unwrap(),
                format!("\"{}\"", spec.id),
                "{} serde 线上值不一致",
                spec.id
            );
        }
        // `ALL` 恰好覆盖每个变体一次：任何变体漏登记都会让这里少一项。
        let mut ids: Vec<&str> = ProviderId::ALL.iter().map(|id| id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PROVIDERS.len(), "ALL 里有重复变体");
    }

    /// 显示名的登记权在 `PROVIDERS`：面板（`LoginPage.vue` / `PlaybackPage.vue`
    /// 的 `sourceNames`）与 `adapters/login_helper.rs` 的 `provider_display_name`
    /// 是尚未迁走的第二份/第三份，本测试逐字对拍，改动任何一处都会红。
    #[test]
    fn provider_display_names_are_stable() {
        assert_eq!(
            ProviderId::ALL.map(ProviderId::display_name),
            ["QQ音乐", "网易云音乐", "哔哩哔哩", "酷狗音乐"]
        );
    }

    #[test]
    fn canonical_ids_are_strict_and_registry_lists_all_known_providers() {
        assert_eq!(
            ProviderId::ALL.map(ProviderId::as_str),
            ["qqmusic", "netease", "bilibili", "kugou"]
        );
        assert!("tx".parse::<ProviderId>().is_err());
        let registry = ProviderRegistry;
        assert_eq!(
            registry.enabled(),
            vec!["qqmusic", "netease", "bilibili", "kugou"]
        );
    }

    #[test]
    fn availability_failures_are_stable_and_provider_attributed() {
        let registry = ProviderRegistry;
        assert_eq!(
            registry.require_enabled("bogus").unwrap_err().code,
            "unknown_provider"
        );
    }

    #[test]
    fn empty_success_is_distinct_from_failed_search() {
        let outcome = ProviderSearchOutcome::success("qqmusic", Vec::new());
        assert!(outcome.is_success());
        let failure = ProviderSearchOutcome::failed(
            "qqmusic",
            crate::catalog::Failure::new("provider_timeout", "timed out"),
        )
        .failure
        .unwrap();
        assert_eq!(failure.provider.as_deref(), Some("qqmusic"));
    }

    #[test]
    fn failed_outcome_is_always_attributed_to_its_provider_branch() {
        let failure = ProviderSearchOutcome::failed(
            "netease",
            crate::catalog::Failure::new("provider_timeout", "timed out").with_provider("qqmusic"),
        )
        .failure
        .unwrap();

        assert_eq!(failure.provider.as_deref(), Some("netease"));
    }
}
