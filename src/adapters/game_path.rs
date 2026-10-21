//! 游戏可执行文件定位（跨平台纯逻辑 + 平台注册表查询分离）。
//!
//! 路径校验与候选生成是纯字符串/路径逻辑，放在跨平台模块里可测；
//! 只有"从启动器注册表读安装目录"这一步是 Windows 专属。

use std::path::{Path, PathBuf};

/// 解析目标游戏的可执行文件路径。
///
/// 优先级：配置里显式路径（文件或目录）→ 平台默认安装位置（Windows 读注册表）。
pub(crate) fn resolve_game_executable(
    configured_path: &Path,
    target_process: &str,
) -> anyhow::Result<PathBuf> {
    if !configured_path.as_os_str().is_empty() {
        if configured_path.is_dir() {
            return resolve_from_directory(configured_path, target_process);
        }
        // 安全加固：直接文件路径必须命中目标游戏可执行文件白名单
        // （target_process 候选或 YuanShen/GenshinImpact 兜底），防止
        // Web 面板改写 exe_path 后启动任意程序。
        let candidates = executable_candidates(target_process);
        let file_name = configured_path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        let allowed = candidates
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&file_name));
        if !allowed {
            anyhow::bail!(
                "startup.exe_path 必须指向目标游戏可执行文件(允许: {})",
                candidates.join(", ")
            );
        }
        return Ok(configured_path.to_path_buf());
    }
    platform_game_path().ok_or_else(|| {
        anyhow::anyhow!("startup.exe_path is empty and no launcher registry path was found")
    })
}

fn resolve_from_directory(dir: &Path, target_process: &str) -> anyhow::Result<PathBuf> {
    for candidate in executable_candidates(target_process) {
        let path = dir.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("no target game executable exists in {}", dir.display())
}

pub(crate) fn executable_candidates(target_process: &str) -> Vec<String> {
    let mut candidates = target_process
        .split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            if item.to_ascii_lowercase().ends_with(".exe") {
                item.to_string()
            } else {
                format!("{item}.exe")
            }
        })
        .collect::<Vec<_>>();
    for fallback in ["YuanShen.exe", "GenshinImpact.exe"] {
        if !candidates
            .iter()
            .any(|item| item.eq_ignore_ascii_case(fallback))
        {
            candidates.push(fallback.to_string());
        }
    }
    candidates
}

/// 平台默认安装位置。非 Windows 没有启动器注册表，直接返回 None。
#[cfg(target_os = "windows")]
fn platform_game_path() -> Option<PathBuf> {
    crate::adapters::windows::registry_game_path()
}

#[cfg(not(target_os = "windows"))]
fn platform_game_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_candidates_adds_suffix_and_fallbacks() {
        let candidates = executable_candidates("yuanshen.exe, GenshinImpact");
        assert_eq!(candidates[0], "yuanshen.exe");
        assert_eq!(candidates[1], "GenshinImpact.exe");
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case("YuanShen.exe"))
        );
    }

    #[test]
    fn configured_executable_outside_the_allowlist_is_rejected() {
        let error = resolve_game_executable(Path::new("C:/Windows/System32/cmd.exe"), "YuanShen")
            .expect_err("非白名单可执行文件必须被拒绝");
        assert!(error.to_string().contains("必须指向目标游戏可执行文件"));
    }

    #[test]
    fn allowlisted_executable_is_returned_verbatim() {
        let path = resolve_game_executable(Path::new("D:/Games/YuanShen.exe"), "YuanShen")
            .expect("白名单内的路径应通过");
        assert_eq!(path, PathBuf::from("D:/Games/YuanShen.exe"));
    }
}
