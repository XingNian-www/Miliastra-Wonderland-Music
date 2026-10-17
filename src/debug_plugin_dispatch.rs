//! Session-scoped simulated sandbox commands for a plugin worker.
use crate::debug_http_service::DebugHttpService;
use crate::debug_plugin_stdio::WorkerCommandHandler;
use crate::debug_sandbox::Capability;
use crate::interfaces::http::{HttpDebugCommand, HttpDebugRunRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunArgs {
    run_id: String,
}

pub(crate) struct SandboxWorkerHandler<'a> {
    sandbox: &'a DebugHttpService,
    owned_runs: HashSet<String>,
}
impl<'a> SandboxWorkerHandler<'a> {
    pub(crate) fn new(sandbox: &'a DebugHttpService) -> Self {
        Self {
            sandbox,
            owned_runs: HashSet::new(),
        }
    }
    fn own(&self, id: &str) -> Result<(), String> {
        if self.owned_runs.contains(id) {
            Ok(())
        } else {
            Err("run does not belong to worker".into())
        }
    }
}
impl Drop for SandboxWorkerHandler<'_> {
    fn drop(&mut self) {
        for id in &self.owned_runs {
            if self.sandbox.cancel(id).is_ok() {
                let _ = self.sandbox.advance(id, false);
            }
        }
    }
}

impl WorkerCommandHandler for SandboxWorkerHandler<'_> {
    fn invoke(
        &mut self,
        capability: Capability,
        method: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        match (capability, method) {
            (Capability::Task, "task.start") => {
                let request: HttpDebugRunRequest =
                    serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                if request.mode != "simulated"
                    || request.plugin_id.is_some()
                    || request.manifest.is_some()
                {
                    return Err(
                        "worker supports only simulated tasks without delegated manifests".into(),
                    );
                }
                let run = self.sandbox.start(request).map_err(|e| e.to_string())?;
                self.owned_runs.insert(run.run_id.clone());
                Ok(json!(run))
            }
            (Capability::Task, "task.step") => {
                let args: RunArgs = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                self.own(&args.run_id)?;
                self.sandbox
                    .advance(&args.run_id, false)
                    .map(|run| json!(run))
                    .map_err(|e| e.to_string())
            }
            (Capability::Task, "task.cancel") => {
                let args: RunArgs = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                self.own(&args.run_id)?;
                self.sandbox
                    .cancel(&args.run_id)
                    .map_err(|e| e.to_string())?;
                self.sandbox
                    .advance(&args.run_id, false)
                    .map(|run| json!(run))
                    .map_err(|e| e.to_string())
            }
            (Capability::Observe, "observe.status") => {
                let args: RunArgs = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                self.own(&args.run_id)?;
                self.sandbox
                    .status_snapshot(&args.run_id)
                    .map_err(|e| e.to_string())
            }
            (Capability::Observe, "observe.evidence") => {
                let args: RunArgs = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                self.own(&args.run_id)?;
                self.sandbox
                    .evidence(&args.run_id, None)
                    .map(|values| json!(values))
                    .map_err(|e| e.to_string())
            }
            (Capability::Input, "input.execute") => {
                let command: HttpDebugCommand = serde_json::from_value(arguments)
                    .map_err(|e| format!("invalid input command: {e}"))?;
                // 边界校验由任务模型统一负责（execute_direct 内部会走 SandboxTask::validate）。
                let run = self
                    .sandbox
                    .execute_direct(command.into_atomic())
                    .map_err(|e| e.to_string())?;
                self.owned_runs.insert(run.run_id.clone());
                Ok(json!(run))
            }
            (Capability::Assert, "assert.evaluate") => Err(
                "assert.evaluate requires a run-bound assertion API and is not implemented".into(),
            ),
            _ => Err("worker command is not implemented".into()),
        }
    }
    fn cancel(&mut self, _: u64) -> Result<(), String> {
        Err("synchronous worker dispatch cannot cancel an in-flight command".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_sandbox::{AtomicCommand, SandboxTask};

    #[test]
    fn worker_can_only_step_and_observe_its_own_simulated_runs() {
        let service = DebugHttpService::default();
        service
            .save(SandboxTask {
                id: "worker-task".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let mut first = SandboxWorkerHandler::new(&service);
        let mut second = SandboxWorkerHandler::new(&service);
        let args = json!({"taskId":"worker-task","mode":"simulated","fixture":{"width":2,"height":2,"rgba":[1,2,3,255]}});
        let started = first
            .invoke(Capability::Task, "task.start", args.clone())
            .unwrap();
        let id = started["runId"].as_str().unwrap();
        let own = json!({"runId":id});
        assert!(
            second
                .invoke(Capability::Task, "task.step", own.clone())
                .is_err()
        );
        assert!(
            second
                .invoke(Capability::Observe, "observe.evidence", own.clone())
                .is_err()
        );
        assert_eq!(
            first
                .invoke(Capability::Task, "task.step", own.clone())
                .unwrap()["status"],
            "passed"
        );
        assert_eq!(
            first
                .invoke(Capability::Observe, "observe.status", own.clone())
                .unwrap()["run"]["taskVersion"],
            1
        );
        assert!(
            first
                .invoke(Capability::Observe, "observe.evidence", own)
                .unwrap()[0]["imagePngBase64"]
                .as_str()
                .is_some()
        );
        assert!(
            first
                .invoke(
                    Capability::Task,
                    "task.start",
                    json!({"mode":"live","taskId":"worker-task"})
                )
                .is_err()
        );
        assert!(first.invoke(Capability::Task, "task.start", json!({"taskId":"worker-task","mode":"simulated","fixture":{"width":2,"height":2,"rgba":[1,2,3,255]},"pluginId":"other"})).is_err());
        assert!(
            second
                .invoke(Capability::Observe, "observe.status", json!({"runId":id}))
                .is_err()
        );
    }

    #[test]
    fn plugin_input_execute_requires_enabled_debug_service() {
        let service = DebugHttpService::default();
        let mut worker = SandboxWorkerHandler::new(&service);
        let error = worker
            .invoke(
                Capability::Input,
                "input.execute",
                json!({"command":"wait","milliseconds":0}),
            )
            .unwrap_err();
        assert!(error.contains("启用调试模式"), "{error}");
    }

    #[test]
    fn plugin_input_execute_rejects_unknown_command_fields_before_runtime_access() {
        let service = DebugHttpService::default();
        let mut worker = SandboxWorkerHandler::new(&service);
        let error = worker
            .invoke(
                Capability::Input,
                "input.execute",
                json!({"command":"click","x":10,"y":20,"unexpected":true}),
            )
            .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn worker_cancels_only_its_own_simulated_run_and_preserves_prior_evidence() {
        let service = DebugHttpService::default();
        service
            .save(SandboxTask {
                id: "worker-cancel".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture, AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let mut owner = SandboxWorkerHandler::new(&service);
        let mut other = SandboxWorkerHandler::new(&service);
        let started = owner.invoke(Capability::Task, "task.start",
            json!({"taskId":"worker-cancel","mode":"simulated","fixture":{"width":2,"height":2,"rgba":[1,2,3,255]}})).unwrap();
        let id = started["runId"].as_str().unwrap();
        let args = json!({"runId":id});
        assert!(
            other
                .invoke(Capability::Task, "task.cancel", args.clone())
                .is_err()
        );
        assert_eq!(
            owner
                .invoke(Capability::Task, "task.step", args.clone())
                .unwrap()["status"],
            "running"
        );
        let cancelled = owner
            .invoke(Capability::Task, "task.cancel", args.clone())
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["results"].as_array().unwrap().len(), 1);
        assert_eq!(service.evidence(id, None).unwrap().len(), 1);
        assert!(owner.invoke(Capability::Task, "task.cancel", args).is_err());
    }

    #[test]
    fn stdio_worker_routes_owned_task_cancel_and_retains_evidence() {
        use crate::debug_plugin_session::WorkerSession;
        use crate::debug_plugin_stdio::{Disconnect, serve_worker};
        use crate::debug_sandbox::{CapabilityPolicy, PluginApiVersion, PluginManifest};
        use std::io::{BufReader, Cursor};

        let service = DebugHttpService::default();
        service
            .save(SandboxTask {
                id: "stdio-cancel".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture, AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let mut handler = SandboxWorkerHandler::new(&service);
        let started = handler.invoke(Capability::Task, "task.start",
            json!({"taskId":"stdio-cancel","mode":"simulated","fixture":{"width":2,"height":2,"rgba":[1,2,3,255]}})).unwrap();
        let id = started["runId"].as_str().unwrap().to_owned();
        let policy = CapabilityPolicy {
            allowed: [Capability::Task, Capability::Observe]
                .into_iter()
                .collect(),
            debug_mode: false,
        };
        let manifest = PluginManifest {
            manifest_version: 1,
            plugin_id: "worker".into(),
            name: "worker".into(),
            version: "1".into(),
            api: PluginApiVersion::new(1, 0),
            capabilities: policy.allowed.clone(),
        };
        let mut session = WorkerSession::new(manifest, &policy).unwrap();
        let token = session.token().to_owned();
        let frame = |number, method, auth: &str, arguments: Value| {
            json!({"jsonrpc":"2.0","id":number,"method":method,"params":{
                "token":auth,"pluginId":"worker","arguments":arguments
            }})
            .to_string()
                + "\n"
        };
        let frames = [
            frame(1, "initialize", &token, Value::Null),
            frame(2, "task.step", &token, json!({"runId":id})),
            frame(3, "task.cancel", "wrong-token", json!({"runId":id})),
            frame(4, "task.cancel", &token, json!({"runId":id})),
            frame(5, "observe.status", &token, json!({"runId":id})),
            frame(6, "observe.evidence", &token, json!({"runId":id})),
            frame(7, "shutdown", &token, Value::Null),
        ]
        .concat();
        let mut output = Vec::new();
        assert_eq!(
            serve_worker(
                &mut session,
                &mut BufReader::new(Cursor::new(frames)),
                &mut output,
                || policy.clone(),
                &mut handler
            )
            .unwrap(),
            Disconnect::Shutdown
        );
        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 7);
        assert_eq!(responses[1]["result"]["status"], "running");
        assert!(
            responses[2]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("authentication")
        );
        assert_eq!(responses[3]["result"]["status"], "cancelled");
        assert_eq!(responses[4]["result"]["run"]["status"], "cancelled");
        assert_eq!(responses[5]["result"].as_array().unwrap().len(), 1);
        assert!(
            responses[5]["result"][0]["imagePngBase64"]
                .as_str()
                .is_some()
        );
        assert_eq!(responses[6]["result"]["stopped"], true);
    }

    #[test]
    fn disconnected_worker_finalizes_pending_run_without_executing_next_step() {
        let service = DebugHttpService::default();
        service
            .save(SandboxTask {
                id: "disconnect-task".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture, AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let args = json!({"taskId":"disconnect-task","mode":"simulated","fixture":{"width":2,"height":2,"rgba":[1,2,3,255]}});
        let mut worker = SandboxWorkerHandler::new(&service);
        let started = worker.invoke(Capability::Task, "task.start", args).unwrap();
        let id = started["runId"].as_str().unwrap().to_owned();
        worker
            .invoke(Capability::Task, "task.step", json!({"runId":id}))
            .unwrap();
        assert_eq!(service.status(&id).unwrap().results.len(), 1);
        drop(worker);
        let run = service.status(&id).unwrap();
        assert_eq!(run.status, crate::debug_sandbox::SandboxStatus::Cancelled);
        assert_eq!(run.results.len(), 1);
        assert_eq!(service.evidence(&id, None).unwrap().len(), 1);
    }

    #[test]
    fn disconnected_worker_partial_evidence_survives_restart() {
        let path =
            std::env::temp_dir().join(format!("worker-disconnect-{}.db", uuid::Uuid::new_v4()));
        let id = {
            let service = DebugHttpService::with_journal(None, &path).unwrap();
            service
                .save(SandboxTask {
                    id: "persist-worker".into(),
                    version: 1,
                    description: String::new(),
                    commands: vec![AtomicCommand::Capture, AtomicCommand::Capture],
                    capture_after_each: true,
                })
                .unwrap();
            let mut worker = SandboxWorkerHandler::new(&service);
            let started = worker.invoke(Capability::Task, "task.start",
                json!({"taskId":"persist-worker","mode":"simulated","fixture":{"width":2,"height":2,"rgba":[1,2,3,255]}})).unwrap();
            let id = started["runId"].as_str().unwrap().to_owned();
            worker
                .invoke(Capability::Task, "task.step", json!({"runId":id}))
                .unwrap();
            drop(worker);
            id
        };
        {
            let restored = DebugHttpService::with_journal(None, &path).unwrap();
            let record = restored.status_snapshot(&id).unwrap();
            assert_eq!(record["run"]["status"], "cancelled");
            assert_eq!(record["run"]["results"].as_array().unwrap().len(), 1);
            let evidence = restored.evidence(&id, None).unwrap();
            assert_eq!(evidence.len(), 1);
            assert!(
                evidence[0]["imagePngBase64"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
