//! Host-owned stdio worker process with a bounded lifetime.
use std::ffi::OsString;
use std::io::{self, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(windows)]
mod job {
    use std::{io, mem, os::windows::io::AsRawHandle, process::Child};
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    };

    pub(super) struct WorkerJob(HANDLE);
    impl WorkerJob {
        pub(super) fn attach(child: &mut Child) -> io::Result<Self> {
            let job = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
            let job = Self(job);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    mem::size_of_val(&limits) as u32,
                )
            }
            .map_err(io::Error::other)?;
            let process = HANDLE(child.as_raw_handle());
            unsafe { AssignProcessToJobObject(job.0, process) }.map_err(io::Error::other)?;
            Ok(job)
        }
        pub(super) fn raw(&self) -> usize {
            self.0.0 as usize
        }
        pub(super) fn terminate_raw(raw: usize) -> io::Result<()> {
            unsafe { TerminateJobObject(HANDLE(raw as *mut _), 1) }.map_err(io::Error::other)
        }
    }
    impl Drop for WorkerJob {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod job {
    use std::{io, process::Child};
    pub(super) struct WorkerJob;
    impl WorkerJob {
        pub(super) fn attach(_: &mut Child) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "worker process tree supervision requires Windows",
            ))
        }
        pub(super) fn raw(&self) -> usize {
            0
        }
        pub(super) fn terminate_raw(_: usize) -> io::Result<()> {
            Ok(())
        }
    }
}
use job::WorkerJob;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::debug_http_service::DebugHttpService;
use crate::debug_plugin_dispatch::SandboxWorkerHandler;
use crate::debug_plugin_session::WorkerSession;
use crate::debug_plugin_stdio::{Disconnect, serve_worker};
use crate::debug_sandbox::{CapabilityPolicy, PluginManifest};

#[derive(Debug)]
pub(crate) struct WorkerExit {
    pub disconnected: Disconnect,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
}

/// The caller must supply a trusted local executable. This is not an OS-level sandbox.
pub(crate) fn run_worker_process(
    executable: &Path,
    args: &[OsString],
    manifest: PluginManifest,
    policy: impl Fn() -> CapabilityPolicy + Send + 'static,
    sandbox: &DebugHttpService,
    lifetime: Duration,
) -> io::Result<WorkerExit> {
    if lifetime.is_zero() || lifetime > Duration::from_secs(300) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker lifetime must be 1ms to 5min",
        ));
    }
    let mut session = WorkerSession::new(manifest, &policy()).map_err(io::Error::other)?;
    let mut child = Command::new(executable)
        .args(args)
        .env("MILIASTRA_WORKER_TOKEN", session.token())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let worker_job = match WorkerJob::attach(&mut child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut input = BufReader::new(child.stdout.take().expect("piped worker stdout"));
    let mut output = child.stdin.take().expect("piped worker stdin");
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let expired = Arc::new(AtomicBool::new(false));
    let monitor = {
        let job_raw = worker_job.raw();
        let done = Arc::clone(&done);
        let expired = Arc::clone(&expired);
        std::thread::spawn(move || {
            let deadline = Instant::now() + lifetime;
            while !done.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    expired.store(true, Ordering::Release);
                    let _ = WorkerJob::terminate_raw(job_raw);
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };
    let mut handler = SandboxWorkerHandler::new(sandbox);
    let disconnected = serve_worker(&mut session, &mut input, &mut output, policy, &mut handler);
    done.store(true, Ordering::Release);
    drop(output);
    let _ = monitor.join();
    let mut child = child
        .lock()
        .map_err(|_| io::Error::other("worker process lock poisoned"))?;
    if matches!(disconnected, Ok(Disconnect::Shutdown)) && !expired.load(Ordering::Acquire) {
        let grace = Instant::now() + Duration::from_secs(1);
        while child.try_wait()?.is_none() && Instant::now() < grace {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    Ok(WorkerExit {
        disconnected: disconnected?,
        timed_out: expired.load(Ordering::Acquire),
        exit_code: status.code(),
    })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::debug_sandbox::{Capability, PluginApiVersion};
    fn manifest() -> PluginManifest {
        PluginManifest {
            manifest_version: 1,
            plugin_id: "worker".into(),
            name: "worker".into(),
            version: "1".into(),
            api: PluginApiVersion::new(1, 0),
            capabilities: [Capability::Observe].into_iter().collect(),
        }
    }
    #[test]
    fn process_eof_is_reaped() {
        let service = DebugHttpService::default();
        let args = ["/C".into(), "exit 0".into()];
        let outcome = run_worker_process(
            Path::new("cmd.exe"),
            &args,
            manifest(),
            CapabilityPolicy::default,
            &service,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(outcome.disconnected, Disconnect::Eof(_)));
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
    }
    #[test]
    fn blocked_process_is_killed_after_deadline() {
        let service = DebugHttpService::default();
        let args = [
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "[Console]::ReadLine()".into(),
        ];
        let outcome = run_worker_process(
            Path::new("pwsh.exe"),
            &args,
            manifest(),
            CapabilityPolicy::default,
            &service,
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(matches!(outcome.disconnected, Disconnect::Eof(_)));
        assert!(outcome.timed_out);
    }
    #[test]
    fn inherited_stdout_from_grandchild_is_closed_by_job_timeout() {
        let service = DebugHttpService::default();
        let args = ["/C".into(), "more".into()];
        let outcome = run_worker_process(
            Path::new("cmd.exe"),
            &args,
            manifest(),
            CapabilityPolicy::default,
            &service,
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(matches!(outcome.disconnected, Disconnect::Eof(_)));
        assert!(outcome.timed_out);
    }
    #[test]
    fn authenticated_roundtrip_with_real_child() {
        let service = DebugHttpService::default();
        let script = r#"$token = $env:MILIASTRA_WORKER_TOKEN; [Console]::WriteLine(('{' + '"jsonrpc":"2.0","id":1,"method":"initialize","params":{' + '"pluginId":"worker","token":"' + $token + '"}}')); $ready = [Console]::ReadLine(); if ($ready -notmatch '"ready":true') { exit 4 }; [Console]::WriteLine(('{' + '"jsonrpc":"2.0","id":2,"method":"shutdown","params":{' + '"pluginId":"worker","token":"' + $token + '"}}')); $bye = [Console]::ReadLine(); if ($bye -notmatch '"stopped":true') { exit 5 }"#;
        let args = [
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-Command".into(),
            script.into(),
        ];
        let outcome = run_worker_process(
            Path::new("pwsh.exe"),
            &args,
            manifest(),
            CapabilityPolicy::default,
            &service,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(matches!(outcome.disconnected, Disconnect::Shutdown));
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, Some(0));
    }
}
