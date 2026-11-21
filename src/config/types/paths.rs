//! 路径解析与共享校验助手。
//!
//! 这些函数被多个业务域共用：路径类字段在加载时相对 EXE 根目录解析、显示时
//! 统一分隔符；几何/数值字段的边界校验必须与面板表单声明一致，所以放在这里
//! 而不是各自模块里复制一份。

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::general::{PointConfig, RectConfig};

/// 把字符串里的 `/` 与 `\` 统一成给定分隔符。
///
/// 只在**路径类字段**上调用（`FieldKind::Path`），不要拿它处理任意字符串。
/// 保留 Windows 盘符前缀（`C:`）、UNC 前缀（`\\server`）、verbatim 前缀
/// （`\\?\`）；连续分隔符折叠为一个；尾随分隔符去掉；空串与纯文件名不变。
/// 纯函数、与平台无关，便于在 Linux 上直接回归 Windows 行为。
pub(crate) fn normalize_separators_text(value: &str, separator: char) -> String {
    let is_separator = |character: char| character == '/' || character == '\\';
    let characters: Vec<char> = value.chars().collect();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    if characters.len() >= 2 && is_separator(characters[0]) && is_separator(characters[1]) {
        // UNC（`\\server\share`）或 verbatim（`\\?\C:\...`）：前两个分隔符必须保留。
        output.push(separator);
        output.push(separator);
        index = 2;
        if index < characters.len() && characters[index] == '?' {
            output.push('?');
            index += 1;
            if index < characters.len() && is_separator(characters[index]) {
                output.push(separator);
                index += 1;
            }
        }
    } else if characters.len() >= 2 && characters[1] == ':' && characters[0].is_ascii_alphabetic() {
        // Windows 盘符前缀：`C:` 不是分隔符，不能把盘符后的相对路径当绝对路径。
        output.push(characters[0]);
        output.push(':');
        index = 2;
        if index < characters.len() && is_separator(characters[index]) {
            output.push(separator);
            index += 1;
        }
    } else if !characters.is_empty() && is_separator(characters[0]) {
        // 根路径（`/foo` 或 `\foo`）。
        output.push(separator);
        index = 1;
    }
    let mut pending_separator = false;
    for character in &characters[index..] {
        if is_separator(*character) {
            pending_separator = !output.is_empty();
        } else {
            if pending_separator {
                output.push(separator);
                pending_separator = false;
            }
            output.push(*character);
        }
    }
    output
}

/// 运行期路径规范化：Windows 上把 `/` 与 `\` 统一成 `\`；其他平台原样返回
/// （Unix 的反斜杠是合法文件名字符，不能改写）。保留盘符与绝对路径语义。
pub(crate) fn normalize_native_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    #[cfg(windows)]
    {
        PathBuf::from(normalize_separators_text(
            &path.to_string_lossy(),
            std::path::MAIN_SEPARATOR,
        ))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// 字符串形态的 [`normalize_native_path`]（Web 显示/保存复用同一实现）。
pub(crate) fn normalize_native_path_string(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        normalize_native_path(Path::new(value))
            .to_string_lossy()
            .into_owned()
    }
}

pub(crate) fn resolve_path(root: &Path, path: &mut PathBuf) {
    if path.as_os_str().is_empty() {
        return;
    }
    if path.is_relative() {
        *path = root.join(&*path);
    }
    // `PathBuf::join` 只是在根后原样拼接相对组件：相对值里若带 `/`，Windows 上
    // 会得到 `C:\root\deps/assets/x` 这种混合分隔符。这里统一成平台原生形式。
    *path = normalize_native_path(path);
}

pub(crate) fn resolve_path_with_default(root: &Path, path: &mut PathBuf, default: &str) {
    if path.as_os_str().is_empty() {
        *path = PathBuf::from(default);
    }
    resolve_path(root, path);
}

pub(crate) fn resolve_optional_path(root: &Path, path: &mut Option<PathBuf>) {
    if let Some(path) = path {
        resolve_path(root, path);
    }
}

pub(crate) fn validate_unit_interval(value: f32, field: &str) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{} 必须是 0 到 1 之间的有限小数", field);
    }
    Ok(())
}

pub(crate) fn validate_rect(rect: RectConfig, field: &str) -> Result<()> {
    if rect.width == 0 || rect.height == 0 {
        bail!("{} 的 width 和 height 必须大于 0", field);
    }
    Ok(())
}

pub(crate) fn validate_rect_in_canvas(
    rect: RectConfig,
    field: &str,
    (canvas_width, canvas_height): (u32, u32),
) -> Result<()> {
    validate_rect(rect, field)?;
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if rect.x < 0
        || rect.y < 0
        || right > i64::from(canvas_width)
        || bottom > i64::from(canvas_height)
    {
        bail!(
            "{} 必须完整位于 {}x{} 画布内",
            field,
            canvas_width,
            canvas_height
        );
    }
    Ok(())
}

pub(crate) fn validate_point_in_canvas(
    point: PointConfig,
    field: &str,
    (canvas_width, canvas_height): (u32, u32),
) -> Result<()> {
    if point.x < 0
        || point.y < 0
        || i64::from(point.x) >= i64::from(canvas_width)
        || i64::from(point.y) >= i64::from(canvas_height)
    {
        bail!(
            "{} 必须位于 {}x{} 画布内",
            field,
            canvas_width,
            canvas_height
        );
    }
    Ok(())
}

pub(crate) fn validate_nonempty_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{} 不能为空", field);
    }
    Ok(())
}
