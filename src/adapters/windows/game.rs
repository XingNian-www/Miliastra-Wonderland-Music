use std::path::{Path, PathBuf};

use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::{PCWSTR, w};

pub(crate) fn resolve_game_executable(
    configured_path: &Path,
    target_process: &str,
) -> anyhow::Result<PathBuf> {
    if !configured_path.as_os_str().is_empty() {
        if configured_path.is_dir() {
            return resolve_from_directory(configured_path, target_process);
        }
        return Ok(configured_path.to_path_buf());
    }
    registry_game_path().ok_or_else(|| {
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

fn executable_candidates(target_process: &str) -> Vec<String> {
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

fn registry_game_path() -> Option<PathBuf> {
    for (key, exe) in [
        (w!("Software\\miHoYo\\HYP\\1_1\\hk4e_cn"), "YuanShen.exe"),
        (
            w!("Software\\Cognosphere\\HYP\\1_0\\hk4e_global"),
            "GenshinImpact.exe",
        ),
    ] {
        let Some(dir) = registry_string(key, w!("GameInstallPath")) else {
            continue;
        };
        let path = Path::new(dir.trim()).join(exe);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn registry_string(key: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0_u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 || byte_len == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; (byte_len as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
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
}
