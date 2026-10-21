//! Bounded newline-delimited framing for a plugin worker connection.
use crate::debug_plugin_session::{WorkerAction, WorkerSession};
use crate::debug_sandbox::{Capability, CapabilityPolicy};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

pub(crate) const MAX_FRAME: usize = 64 * 1024;

pub(crate) fn next_frame(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let buffer = input.fill_buf()?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete worker frame",
                ))
            };
        }
        let count = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if line.len().saturating_add(count) > MAX_FRAME + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker frame exceeds 64 KiB",
            ));
        }
        let complete = buffer[count - 1] == b'\n';
        line.extend_from_slice(&buffer[..count]);
        input.consume(count);
        if complete {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "worker frame exceeds 64 KiB",
                ));
            }
            return Ok(Some(line));
        }
    }
}

pub(crate) trait WorkerCommandHandler {
    fn invoke(
        &mut self,
        capability: Capability,
        method: &str,
        arguments: Value,
    ) -> Result<Value, String>;
    fn cancel(&mut self, target: u64) -> Result<(), String>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Disconnect {
    Shutdown,
    Eof(Vec<u64>),
}

pub(crate) fn serve_worker(
    session: &mut WorkerSession,
    input: &mut impl BufRead,
    output: &mut impl Write,
    policy: impl Fn() -> CapabilityPolicy,
    handler: &mut impl WorkerCommandHandler,
) -> io::Result<Disconnect> {
    loop {
        let frame = match next_frame(input) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(Disconnect::Eof(session.crash())),
            Err(error) => {
                session.crash();
                return Err(error);
            }
        };
        let (id, result, shutdown) = match session.admit(&frame, &policy()) {
            Ok(WorkerAction::Ready(id)) => (json!(id), Ok(json!({"ready":true})), false),
            Ok(WorkerAction::Invoke {
                id,
                capability,
                method,
                arguments,
            }) => {
                let result = handler.invoke(capability, &method, arguments);
                session.finish(id).map_err(io::Error::other)?;
                (json!(id), result, false)
            }
            Ok(WorkerAction::Cancel { id, target }) => (
                json!(id),
                handler.cancel(target).map(|_| json!({"cancelled":target})),
                false,
            ),
            Ok(WorkerAction::Shutdown(id)) => (json!(id), Ok(json!({"stopped":true})), true),
            Err(message) => (Value::Null, Err(message), false),
        };
        let response = match result {
            Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
            Err(message) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":message}})
            }
        };
        if let Err(error) = (|| -> io::Result<()> {
            serde_json::to_writer(&mut *output, &response)?;
            output.write_all(b"\n")?;
            output.flush()
        })() {
            session.crash();
            return Err(error);
        }
        if shutdown {
            return Ok(Disconnect::Shutdown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_sandbox::{PluginApiVersion, PluginManifest};
    use std::io::{BufReader, Cursor};
    struct Handler(usize);
    impl WorkerCommandHandler for Handler {
        fn invoke(&mut self, _: Capability, _: &str, _: Value) -> Result<Value, String> {
            self.0 += 1;
            Ok(json!({"calls":self.0}))
        }
        fn cancel(&mut self, _: u64) -> Result<(), String> {
            Ok(())
        }
    }
    fn setup() -> (WorkerSession, CapabilityPolicy) {
        let policy = CapabilityPolicy::default();
        let manifest = PluginManifest {
            manifest_version: 1,
            plugin_id: "worker".into(),
            name: "worker".into(),
            version: "1".into(),
            api: PluginApiVersion::new(1, 0),
            capabilities: [Capability::Observe].into_iter().collect(),
        };
        (WorkerSession::new(manifest, &policy).unwrap(), policy)
    }
    fn frame(id: u64, method: &str, token: &str) -> String {
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"token":token,"pluginId":"worker"}}).to_string() + "\n"
    }
    #[test]
    fn handshake_dispatch_failure_and_shutdown() {
        let (mut session, policy) = setup();
        let token = session.token().to_owned();
        let frames = format!(
            "{}{}{}{}",
            frame(1, "initialize", &token),
            frame(2, "invalid", &token),
            frame(3, "observe.status", &token),
            frame(4, "shutdown", &token)
        );
        let mut input = BufReader::new(Cursor::new(frames));
        let mut output = Vec::new();
        let mut handler = Handler(0);
        assert_eq!(
            serve_worker(
                &mut session,
                &mut input,
                &mut output,
                || policy.clone(),
                &mut handler
            )
            .unwrap(),
            Disconnect::Shutdown
        );
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["result"]["ready"], true);
        assert_eq!(lines[1]["error"]["code"], -32600);
        assert_eq!(lines[2]["result"]["calls"], 1);
        assert_eq!(lines[3]["result"]["stopped"], true);
        assert_eq!(handler.0, 1);
    }
    #[test]
    fn eof_and_oversize_close_session() {
        let (mut session, policy) = setup();
        let token = session.token().to_owned();
        let mut input = BufReader::new(Cursor::new(frame(1, "initialize", &token)));
        let mut handler = Handler(0);
        assert_eq!(
            serve_worker(
                &mut session,
                &mut input,
                &mut Vec::new(),
                || policy.clone(),
                &mut handler
            )
            .unwrap(),
            Disconnect::Eof(vec![])
        );
        assert!(
            session
                .admit(frame(2, "observe.status", &token).as_bytes(), &policy)
                .is_err()
        );
        let (mut session, policy) = setup();
        let mut oversized = BufReader::new(Cursor::new(vec![b'x'; MAX_FRAME + 2]));
        assert_eq!(
            serve_worker(
                &mut session,
                &mut oversized,
                &mut Vec::new(),
                || policy.clone(),
                &mut handler
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
