use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;

use std::time::{Duration, Instant};
use uuid::Uuid;

#[cfg(windows)]
use windows::Win32::NetworkManagement::IpHelper::{
    GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
#[cfg(windows)]
use windows::Win32::Networking::WinSock::AF_UNSPEC;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_millis(500);

/// 酷狗 API sidecar 启动配置。
#[derive(Clone, Debug)]
pub(crate) struct KugouApiConfig {
    pub executable: PathBuf,
    pub device_directory: PathBuf,
    pub startup_timeout: Duration,
    pub poll_interval: Duration,
}

impl KugouApiConfig {
    pub(crate) fn new(executable: PathBuf, device_directory: PathBuf) -> Self {
        Self {
            executable,
            device_directory,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// 管理酷狗 API sidecar 进程及动态回环地址。
pub(crate) struct KugouApiSidecar {
    child: Option<Child>,
    base_url: String,
}

impl KugouApiSidecar {
    /// 绑定随机回环端口，启动 sidecar，并等待根路径可访问。
    pub(crate) fn start(config: &KugouApiConfig) -> io::Result<Self> {
        let listener = TcpListener::bind((DEFAULT_HOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);

        let device = load_or_create_device(&config.device_directory)?;
        let mut child = build_command(config, port, &device).spawn()?;
        let base_url = format!("http://{DEFAULT_HOST}:{port}");
        let client = reqwest::blocking::Client::builder()
            .timeout(HEALTH_CHECK_TIMEOUT)
            .build()
            .map_err(|error| {
                io::Error::other(format!("酷狗 API 健康检查客户端创建失败: {error}"))
            })?;
        let deadline = Instant::now() + config.startup_timeout;

        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "酷狗 API sidecar 提前退出: {status}"
                )));
            }
            if client
                .get(format!("{base_url}/"))
                .send()
                .and_then(|response| response.error_for_status())
                .is_ok()
            {
                return Ok(Self {
                    child: Some(child),
                    base_url,
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "酷狗 API sidecar 启动超时",
                ));
            }
            thread::sleep(config.poll_interval);
        }
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 终止并回收 sidecar。重复调用安全。
    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let already_exited = child.try_wait()?.is_some();
        if !already_exited {
            child.kill()?;
        }
        child.wait().map(|_| ())
    }
}

impl Drop for KugouApiSidecar {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn build_command(config: &KugouApiConfig, port: u16, device: &KugouDevice) -> Command {
    let mut command = Command::new(&config.executable);
    command
        .env("HOST", DEFAULT_HOST)
        .env("PORT", port.to_string())
        .env("platform", "lite")
        .env("KUGOU_API_GUID", &device.guid)
        .env("KUGOU_API_DEV", &device.dev)
        .env("KUGOU_API_MAC", &device.mac)
        .env("KUGOU_API_PLATFORM", "lite")
        // 不继承标准流，避免 sidecar 输出意外包含或泄露凭据。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[derive(serde::Serialize, serde::Deserialize)]
struct KugouDevice {
    guid: String,
    dev: String,
    mac: String,
}

fn load_or_create_device(directory: &PathBuf) -> io::Result<KugouDevice> {
    fs::create_dir_all(directory)?;
    let path = directory.join("kugou-device.json");
    if path.is_file() {
        let text = fs::read_to_string(&path)?;
        return serde_json::from_str(&text)
            .map_err(|error| io::Error::other(format!("读取酷狗设备标识失败: {error}")));
    }
    let device = KugouDevice {
        guid: Uuid::new_v4().to_string(),
        dev: Uuid::new_v4().to_string(),
        mac: generated_mac()?,
    };
    let text = serde_json::to_string_pretty(&device)
        .map_err(|error| io::Error::other(format!("序列化酷狗设备标识失败: {error}")))?;
    fs::write(path, text)?;
    Ok(device)
}

fn generated_mac() -> io::Result<String> {
    #[cfg(windows)]
    {
        let mut size = 0u32;
        let result = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0.into(),
                GET_ADAPTERS_ADDRESSES_FLAGS(0),
                None,
                None,
                &mut size,
            )
        };
        if result != 0 && size == 0 {
            return Err(io::Error::other(format!("读取本机网卡地址失败: {result}")));
        }
        let mut buffer = vec![0u8; size as usize];
        let addresses = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0.into(),
                GET_ADAPTERS_ADDRESSES_FLAGS(0),
                None,
                Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut size,
            )
        };
        if addresses != 0 {
            return Err(io::Error::other(format!(
                "读取本机网卡地址失败: {addresses}"
            )));
        }
        let mut current = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !current.is_null() {
            let adapter = unsafe { &*current };
            let length = adapter.PhysicalAddressLength as usize;
            if (6..=8).contains(&length) {
                let bytes = &adapter.PhysicalAddress[..length];
                if bytes.iter().any(|byte| *byte != 0) {
                    return Ok(bytes
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(":"));
                }
            }
            current = adapter.Next;
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "未找到可用本机网卡地址",
        ));
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "读取本机网卡地址仅支持 Windows",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 地址使用回环主机和指定端口() {
        let config = KugouApiConfig::new(
            PathBuf::from("kugou-api.exe"),
            PathBuf::from("data/credentials"),
        );
        let device = KugouDevice {
            guid: "guid".to_string(),
            dev: "dev".to_string(),
            mac: "mac".to_string(),
        };
        let command = build_command(&config, 43210, &device);
        assert_eq!(command.get_program(), "kugou-api.exe");
        let envs = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .map(|(key, value)| (key.to_string_lossy(), value.to_string_lossy()))
            .collect::<Vec<_>>();
        assert!(
            envs.iter()
                .any(|(key, value)| key == "HOST" && value == "127.0.0.1")
        );
        assert!(
            envs.iter()
                .any(|(key, value)| key == "PORT" && value == "43210")
        );
        assert_eq!("http://127.0.0.1:43210", "http://127.0.0.1:43210");
    }

    #[test]
    fn 命令配置保留可执行文件路径() {
        let path = PathBuf::from("tools/kugou-api.exe");
        let config = KugouApiConfig::new(path.clone(), PathBuf::from("data/credentials"));
        let device = KugouDevice {
            guid: "guid".to_string(),
            dev: "dev".to_string(),
            mac: "mac".to_string(),
        };
        let command = build_command(&config, 1, &device);
        assert_eq!(command.get_program(), path.as_os_str());
        assert!(command.get_args().next().is_none());
    }
}
