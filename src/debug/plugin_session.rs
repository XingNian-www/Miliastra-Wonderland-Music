//! Bounded admission for one plugin JSON-RPC connection. Transport stays outside this module.
use crate::debug_sandbox::{Capability, CapabilityPolicy, PluginApiVersion, PluginManifest};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 32;

#[derive(Debug, PartialEq)]
pub(crate) enum WorkerAction {
    Ready(u64),
    Invoke {
        id: u64,
        capability: Capability,
        method: String,
        arguments: Value,
    },
    Cancel {
        id: u64,
        target: u64,
    },
    Shutdown(u64),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Params,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    token: String,
    plugin_id: String,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    target_id: Option<u64>,
}

pub(crate) struct WorkerSession {
    manifest: PluginManifest,
    token: String,
    ready: bool,
    closed: bool,
    last_id: Option<u64>,
    pending: BTreeMap<u64, Capability>,
    cancelled: BTreeSet<u64>,
}
impl WorkerSession {
    pub(crate) fn new(manifest: PluginManifest, policy: &CapabilityPolicy) -> Result<Self, String> {
        manifest
            .validate(PluginApiVersion::new(1, 0), policy)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            manifest,
            token: uuid::Uuid::new_v4().to_string(),
            ready: false,
            closed: false,
            last_id: None,
            pending: BTreeMap::new(),
            cancelled: BTreeSet::new(),
        })
    }
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
    pub(crate) fn admit(
        &mut self,
        bytes: &[u8],
        policy: &CapabilityPolicy,
    ) -> Result<WorkerAction, String> {
        if bytes.len() > MAX_BYTES {
            return Err("worker message exceeds 64 KiB".into());
        }
        if self.closed {
            return Err("worker session closed".into());
        }
        let request: Request =
            serde_json::from_slice(bytes).map_err(|_| "invalid JSON-RPC request")?;
        if request.jsonrpc != "2.0" {
            return Err("unsupported JSON-RPC version".into());
        }
        if !constant_time_equal(request.params.token.as_bytes(), self.token.as_bytes())
            || request.params.plugin_id != self.manifest.plugin_id
        {
            return Err("worker authentication failed".into());
        }
        if self.last_id.is_some_and(|last| request.id <= last) {
            return Err("worker request ID must increase".into());
        }
        if !self.ready {
            if request.method != "initialize" || request.params.target_id.is_some() {
                return Err("worker must initialize first".into());
            }
            self.manifest
                .validate(PluginApiVersion::new(1, 0), policy)
                .map_err(|e| e.to_string())?;
            self.ready = true;
            self.last_id = Some(request.id);
            return Ok(WorkerAction::Ready(request.id));
        }
        self.last_id = Some(request.id);
        if request.method == "shutdown" {
            if !self.pending.is_empty() {
                return Err("worker has pending requests".into());
            }
            self.closed = true;
            return Ok(WorkerAction::Shutdown(request.id));
        }
        if request.method == "cancel" {
            let target = request.params.target_id.ok_or("cancel requires targetId")?;
            if !self.pending.contains_key(&target) {
                return Err("unknown pending request".into());
            }
            self.cancelled.insert(target);
            return Ok(WorkerAction::Cancel {
                id: request.id,
                target,
            });
        }
        let capability = match request.method.as_str() {
            "observe.status" | "observe.evidence" => Capability::Observe,
            "input.execute" => Capability::Input,
            "task.start" | "task.step" | "task.cancel" => Capability::Task,
            "assert.evaluate" => Capability::Assert,
            _ => return Err("unknown worker method".into()),
        };
        if request.params.target_id.is_some() {
            return Err("targetId only valid for cancel".into());
        }
        if self.pending.contains_key(&request.id) || self.cancelled.contains(&request.id) {
            return Err("duplicate worker request ID".into());
        }
        if self.pending.len() >= MAX_PENDING {
            return Err("worker request capacity exceeded".into());
        }
        self.manifest
            .authorize(capability, PluginApiVersion::new(1, 0), policy)
            .map_err(|e| e.to_string())?;
        if request.method == "task.start"
            && request
                .params
                .arguments
                .get("assertions")
                .is_some_and(|value| value.as_array().is_some_and(|items| !items.is_empty()))
        {
            self.manifest
                .authorize(Capability::Assert, PluginApiVersion::new(1, 0), policy)
                .map_err(|e| e.to_string())?;
        }
        self.pending.insert(request.id, capability);
        Ok(WorkerAction::Invoke {
            id: request.id,
            capability,
            method: request.method,
            arguments: request.params.arguments,
        })
    }
    pub(crate) fn finish(&mut self, id: u64) -> Result<bool, String> {
        if self.pending.remove(&id).is_none() {
            return Err("unknown pending request".into());
        }
        Ok(self.cancelled.remove(&id))
    }
    pub(crate) fn crash(&mut self) -> Vec<u64> {
        self.closed = true;
        self.cancelled.clear();
        std::mem::take(&mut self.pending).into_keys().collect()
    }
}
fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |diff, (l, r)| diff | (l ^ r))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> PluginManifest {
        PluginManifest {
            manifest_version: 1,
            plugin_id: "ai-debug".into(),
            name: "AI Debug".into(),
            version: "1.0".into(),
            api: PluginApiVersion::new(1, 0),
            capabilities: [Capability::Observe, Capability::Task, Capability::Input]
                .into_iter()
                .collect(),
        }
    }
    fn policy(debug_mode: bool) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed: [Capability::Observe, Capability::Task, Capability::Input]
                .into_iter()
                .collect(),
            debug_mode,
        }
    }
    fn request(id: u64, method: &str, token: &str, target: Option<u64>) -> Vec<u8> {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":{
            "token":token,"pluginId":"ai-debug","targetId":target}})
        .to_string()
        .into_bytes()
    }
    #[test]
    fn handshake_auth_policy_cancel_and_shutdown() {
        let mut session = WorkerSession::new(manifest(), &policy(true)).unwrap();
        let secret = session.token().to_owned();
        assert!(
            session
                .admit(&request(1, "task.start", &secret, None), &policy(true))
                .is_err()
        );
        assert!(
            session
                .admit(&request(1, "initialize", "wrong", None), &policy(true))
                .is_err()
        );
        assert_eq!(
            session
                .admit(&request(1, "initialize", &secret, None), &policy(true))
                .unwrap(),
            WorkerAction::Ready(1)
        );
        assert!(
            session
                .admit(&request(2, "input.execute", &secret, None), &policy(false))
                .is_err()
        );
        assert!(matches!(
            session
                .admit(&request(3, "task.start", &secret, None), &policy(true))
                .unwrap(),
            WorkerAction::Invoke {
                id: 3,
                capability: Capability::Task,
                ..
            }
        ));
        assert!(
            session
                .admit(&request(3, "task.start", &secret, None), &policy(true))
                .is_err()
        );
        assert_eq!(
            session
                .admit(&request(4, "cancel", &secret, Some(3)), &policy(true))
                .unwrap(),
            WorkerAction::Cancel { id: 4, target: 3 }
        );
        assert!(session.finish(3).unwrap());
        assert!(
            session
                .admit(&request(3, "task.start", &secret, None), &policy(true))
                .is_err()
        );
        assert_eq!(
            session
                .admit(&request(5, "shutdown", &secret, None), &policy(true))
                .unwrap(),
            WorkerAction::Shutdown(5)
        );
        assert!(
            session
                .admit(&request(5, "observe.status", &secret, None), &policy(true))
                .is_err()
        );
    }
    #[test]
    fn task_cancel_requires_task_capability() {
        let mut session = WorkerSession::new(manifest(), &policy(true)).unwrap();
        let token = session.token().to_owned();
        session
            .admit(&request(1, "initialize", &token, None), &policy(true))
            .unwrap();
        assert!(matches!(
            session.admit(&request(2, "task.cancel", &token, None), &policy(true)),
            Ok(WorkerAction::Invoke {
                id: 2,
                capability: Capability::Task,
                ..
            })
        ));
        session.finish(2).unwrap();
        let mut restricted = policy(true);
        restricted.allowed.remove(&Capability::Task);
        assert!(
            session
                .admit(&request(3, "task.cancel", &token, None), &restricted)
                .is_err()
        );
    }

    #[test]
    fn task_start_with_assertions_requires_assert_capability() {
        let mut allowed = policy(true);
        allowed.allowed.insert(Capability::Assert);
        let mut session = WorkerSession::new(manifest(), &allowed).unwrap();
        let token = session.token().to_owned();
        session
            .admit(&request(1, "initialize", &token, None), &allowed)
            .unwrap();
        let make_start = |id, assertions: Value| {
            serde_json::json!({
            "jsonrpc":"2.0", "id":id, "method":"task.start",
            "params":{"token":token, "pluginId":"ai-debug","arguments":{"assertions":assertions}}
        }).to_string().into_bytes()
        };
        let denied = session
            .admit(&make_start(2, serde_json::json!([{}])), &allowed)
            .unwrap_err();
        assert!(denied.contains("Assert"), "{denied}");
        assert!(session.pending.is_empty());
        assert!(matches!(
            session.admit(&make_start(3, serde_json::json!([])), &allowed),
            Ok(WorkerAction::Invoke { id: 3, .. })
        ));
        session.finish(3).unwrap();

        let mut with_assert = manifest();
        with_assert.capabilities.insert(Capability::Assert);
        let mut session = WorkerSession::new(with_assert, &allowed).unwrap();
        let token = session.token().to_owned();
        session
            .admit(&request(1, "initialize", &token, None), &allowed)
            .unwrap();
        let admitted = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"task.start","params":{"token":token,"pluginId":"ai-debug","arguments":{"assertions":[{}]}}});
        assert!(matches!(
            session.admit(admitted.to_string().as_bytes(), &allowed),
            Ok(WorkerAction::Invoke { id: 2, .. })
        ));
    }
    #[test]
    fn crash_drops_pending_and_rejects_new_requests() {
        let mut session = WorkerSession::new(manifest(), &policy(true)).unwrap();
        let secret = session.token().to_owned();
        session
            .admit(&request(1, "initialize", &secret, None), &policy(true))
            .unwrap();
        session
            .admit(&request(7, "observe.status", &secret, None), &policy(true))
            .unwrap();
        session
            .admit(&request(9, "task.step", &secret, None), &policy(true))
            .unwrap();
        assert_eq!(session.crash(), vec![7, 9]);
        assert!(
            session
                .admit(&request(11, "task.step", &secret, None), &policy(true))
                .is_err()
        );
    }
    #[test]
    fn rejects_oversized_and_unknown_messages() {
        let mut session = WorkerSession::new(manifest(), &policy(true)).unwrap();
        assert!(
            session
                .admit(&vec![b'x'; MAX_BYTES + 1], &policy(true))
                .is_err()
        );
        let secret = session.token().to_owned();
        session
            .admit(&request(1, "initialize", &secret, None), &policy(true))
            .unwrap();
        assert!(
            session
                .admit(&request(2, "filesystem.read", &secret, None), &policy(true))
                .is_err()
        );
    }
}
