//! 牌局配置。

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandlordConfig {
    pub enabled: bool,
    pub lobby_timeout_seconds: u64,
    pub turn_timeout_seconds: u64,
    pub trustee_after_timeouts: u32,
    pub hand_cooldown_seconds: u64,
}

impl Default for LandlordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lobby_timeout_seconds: 120,
            turn_timeout_seconds: 90,
            trustee_after_timeouts: 2,
            hand_cooldown_seconds: 10,
        }
    }
}

impl LandlordConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.enabled
            && (self.lobby_timeout_seconds == 0
                || self.turn_timeout_seconds == 0
                || self.trustee_after_timeouts == 0)
        {
            bail!("牌局大厅、回合超时和托管触发次数必须大于 0");
        }
        Ok(())
    }
}
