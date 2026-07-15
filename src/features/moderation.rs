use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationCommand {
    pub action: ModerationAction,
    pub uid: String,
    pub requester: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModerationAction {
    Blacklist,
    BlockChat,
}

impl ModerationAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blacklist => "拉黑",
            Self::BlockChat => "屏蔽",
        }
    }
}

#[derive(Clone, Default)]
pub struct ModerationService {
    active_workflows: Arc<Mutex<HashSet<String>>>,
}

impl ModerationService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, key: &str) -> Result<bool> {
        Ok(self
            .active_workflows
            .lock()
            .map_err(|_| anyhow!("moderation workflow registry mutex poisoned"))?
            .insert(key.to_string()))
    }

    pub fn release(&self, key: &str) -> Result<bool> {
        Ok(self
            .active_workflows
            .lock()
            .map_err(|_| anyhow!("moderation workflow registry mutex poisoned"))?
            .remove(key))
    }

    pub fn release_best_effort(&self, key: &str) {
        if let Err(error) = self.release(key) {
            log::error!("无法释放管理工作流 {key}: {error:#}");
        }
    }

    pub fn release_guard(&self, key: String) -> ModerationWorkflowGuard {
        ModerationWorkflowGuard {
            service: self.clone(),
            key,
        }
    }

    #[cfg(test)]
    pub fn is_active(&self, key: &str) -> Result<bool> {
        Ok(self
            .active_workflows
            .lock()
            .map_err(|_| anyhow!("moderation workflow registry mutex poisoned"))?
            .contains(key))
    }
}

pub struct ModerationWorkflowGuard {
    service: ModerationService,
    key: String,
}

impl Drop for ModerationWorkflowGuard {
    fn drop(&mut self) {
        self.service.release_best_effort(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_accepts_one_workflow_per_key_until_release() {
        let service = ModerationService::new();

        assert!(service.begin("blacklist:123").unwrap());
        assert!(!service.begin("blacklist:123").unwrap());
        assert!(service.is_active("blacklist:123").unwrap());
        assert!(service.release("blacklist:123").unwrap());
        assert!(service.begin("blacklist:123").unwrap());
    }

    #[test]
    fn release_guard_cleans_up_on_every_exit_path() {
        let service = ModerationService::new();
        service.begin("block:456").unwrap();

        {
            let _guard = service.release_guard("block:456".to_string());
            assert!(service.is_active("block:456").unwrap());
        }

        assert!(!service.is_active("block:456").unwrap());
    }
}
