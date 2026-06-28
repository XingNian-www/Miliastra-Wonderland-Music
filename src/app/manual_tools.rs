use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use super::chat_output::ChatOutput;
use super::config::{AppConfig, PointConfig};
use super::ocr::{
    make_ocr_engine, probe_ocr_backend_support, recognize_lines, OcrArgs, OcrBackendProbeStatus,
};
use super::{
    best_template_hit, click_game_point, crop_canvas, detect_ui_state, find_template_hits,
    load_frame, parse_key, parse_rect, press_key, print_json, scan_chat, window, Canvas, FrameArgs,
    TemplateArgs, UiTemplateArgs,
};
use super::{
    change_stats, configured_chat_change_fingerprint, count_chat_markers,
    rect_chat_change_fingerprint,
};

pub fn run(config_path: &Path) -> Result<()> {
    loop {
        println!();
        println!("手动控制");
        println!("1. 截图保存");
        println!("2. OCR 识别");
        println!("3. 扫描聊天区");
        println!("4. 检测 UI 状态");
        println!("5. 模板匹配");
        println!("6. 点击坐标");
        println!("7. 按键");
        println!("8. 发送聊天");
        println!("9. OCR GPU 支持检测");
        println!("10. 监测聊天区变动");
        println!("11. 检测面板响应速度");
        println!("0. 返回");

        match prompt("请选择")?.trim() {
            "1" => capture_to_file(config_path)?,
            "2" => run_ocr(config_path)?,
            "3" => run_scan_chat(config_path)?,
            "4" => run_ui_state(config_path)?,
            "5" => run_template_match(config_path)?,
            "6" => run_click(config_path)?,
            "7" => run_key(config_path)?,
            "8" => run_send_chat(config_path)?,
            "9" => run_ocr_gpu_probe(config_path)?,
            "10" => run_chat_change_monitor(config_path)?,
            "11" => run_panel_response_benchmark(config_path)?,
            "0" => return Ok(()),
            "" => continue,
            other => println!("未知选项: {}", other),
        }
    }
}

struct PanelTransition {
    elapsed_ms: u128,
    first_change_ms: u128,
    fingerprint: super::ChangeFingerprint,
    max_mean_abs_diff: f32,
    max_changed_ratio: f32,
}

fn run_panel_response_benchmark(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let canvas = default_canvas(&config);
    let detect_rect = panel_response_rect(&config);
    let rounds = prompt_optional_u64("测试次数，留空 10")?
        .unwrap_or(10)
        .max(1);
    let poll_interval_ms = prompt_optional_u64("检测间隔毫秒，留空 20")?
        .unwrap_or(20)
        .max(5);
    let timeout_ms = prompt_optional_u64("单次超时毫秒，留空 1000")?
        .unwrap_or(1000)
        .max(poll_interval_ms);
    let settle_ms = prompt_optional_u64("每轮间隔毫秒，留空 200")?.unwrap_or(200);
    let stable_samples = prompt_optional_u64("稳定连续次数，留空 3")?
        .unwrap_or(3)
        .max(1) as usize;
    let mean_threshold = prompt_optional_f32("平均像素差阈值，留空使用配置")?
        .unwrap_or(config.ocr.change_mean_threshold);
    let ratio_threshold = prompt_optional_f32("变化像素比例阈值，留空使用配置")?
        .unwrap_or(config.ocr.change_pixel_threshold);

    println!(
        "测试 {} 轮，区域 x={} y={} w={} h={}，检测间隔={}ms，超时={}ms，连续稳定={}次，阈值 mean>={:.3} 或 ratio>={:.5}",
        rounds,
        detect_rect.x,
        detect_rect.y,
        detect_rect.width,
        detect_rect.height,
        poll_interval_ms,
        timeout_ms,
        stable_samples,
        mean_threshold,
        ratio_threshold
    );
    println!("测试会按 Enter 打开聊天面板，再按 Esc 收起，不发送消息");

    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    let mut game_window = window::GameWindow::find(&config.window)?;
    game_window.focus_for_keyboard(&mut enigo)?;
    sleep(Duration::from_millis(config.output.focus_delay_ms));

    let mut open_times = Vec::new();
    let mut close_times = Vec::new();
    let mut failures = 0u64;

    for round in 1..=rounds {
        game_window.click(&mut enigo, config.output.focus_point)?;
        enigo
            .key(Key::Escape, Direction::Click)
            .context("close chat before benchmark")?;
        sleep(Duration::from_millis(settle_ms));

        let closed = capture_panel_fingerprint(&config, &canvas, detect_rect)?;
        let open_started = Instant::now();
        enigo
            .key(Key::Return, Direction::Click)
            .context("open chat panel")?;
        let Some(opened) = wait_for_panel_stable(
            &config,
            &canvas,
            detect_rect,
            &closed,
            open_started,
            timeout_ms,
            poll_interval_ms,
            stable_samples,
            mean_threshold,
            ratio_threshold,
        )?
        else {
            failures += 1;
            println!("第 {} 轮: 打开面板超时", round);
            continue;
        };

        let close_started = Instant::now();
        enigo
            .key(Key::Escape, Direction::Click)
            .context("close chat panel")?;
        let Some(closed_again) = wait_for_panel_stable(
            &config,
            &canvas,
            detect_rect,
            &opened.fingerprint,
            close_started,
            timeout_ms,
            poll_interval_ms,
            stable_samples,
            mean_threshold,
            ratio_threshold,
        )?
        else {
            failures += 1;
            println!(
                "第 {} 轮: 打开={}ms，关闭面板超时",
                round, opened.elapsed_ms
            );
            continue;
        };

        open_times.push(opened.elapsed_ms);
        close_times.push(closed_again.elapsed_ms);
        println!(
            "第 {} 轮: 打开稳定={}ms(首变={}ms max_mean={:.3} max_ratio={:.5})，关闭稳定={}ms(首变={}ms max_mean={:.3} max_ratio={:.5})",
            round,
            opened.elapsed_ms,
            opened.first_change_ms,
            opened.max_mean_abs_diff,
            opened.max_changed_ratio,
            closed_again.elapsed_ms,
            closed_again.first_change_ms,
            closed_again.max_mean_abs_diff,
            closed_again.max_changed_ratio
        );
        sleep(Duration::from_millis(settle_ms));
    }

    print_latency_summary("打开面板", &open_times);
    print_latency_summary("关闭面板", &close_times);
    println!("失败次数: {}", failures);
    Ok(())
}

fn capture_panel_fingerprint(
    config: &AppConfig,
    canvas: &Canvas,
    rect: super::Rect,
) -> Result<super::ChangeFingerprint> {
    let frame = load_frame(&FrameArgs { image: None }, canvas, &config.window)?;
    rect_chat_change_fingerprint(&frame.image, rect)
}

fn wait_for_panel_stable(
    config: &AppConfig,
    canvas: &Canvas,
    rect: super::Rect,
    baseline: &super::ChangeFingerprint,
    started: Instant,
    timeout_ms: u64,
    poll_interval_ms: u64,
    stable_samples: usize,
    mean_threshold: f32,
    ratio_threshold: f32,
) -> Result<Option<PanelTransition>> {
    let timeout = Duration::from_millis(timeout_ms);
    let mut previous = baseline.clone();
    let mut first_change_ms = None;
    let mut stable_count = 0usize;
    let mut max_mean_abs_diff = 0.0f32;
    let mut max_changed_ratio = 0.0f32;

    loop {
        let current = capture_panel_fingerprint(config, canvas, rect)?;
        let baseline_stats = change_stats(baseline, &current);
        let sample_stats = change_stats(&previous, &current);
        let changed_from_baseline = baseline_stats.mean_abs_diff >= mean_threshold
            || baseline_stats.changed_ratio >= ratio_threshold;
        let stable_from_previous = sample_stats.mean_abs_diff < mean_threshold
            && sample_stats.changed_ratio < ratio_threshold;

        if changed_from_baseline {
            first_change_ms.get_or_insert_with(|| started.elapsed().as_millis());
            max_mean_abs_diff = max_mean_abs_diff.max(baseline_stats.mean_abs_diff);
            max_changed_ratio = max_changed_ratio.max(baseline_stats.changed_ratio);
        }

        if first_change_ms.is_some() {
            if stable_from_previous {
                stable_count += 1;
                if stable_count >= stable_samples {
                    return Ok(Some(PanelTransition {
                        elapsed_ms: started.elapsed().as_millis(),
                        first_change_ms: first_change_ms.unwrap_or_default(),
                        fingerprint: current,
                        max_mean_abs_diff,
                        max_changed_ratio,
                    }));
                }
            } else {
                stable_count = 0;
            }
        }

        if started.elapsed() >= timeout {
            return Ok(None);
        }
        previous = current;
        sleep(Duration::from_millis(poll_interval_ms));
    }
}

fn panel_response_rect(config: &AppConfig) -> super::Rect {
    let chat = config.screen.chat_rect;
    let point = config.output.chat_click_2;
    let x = chat.x.min(point.x - 80).max(0);
    let y = chat.y.min(point.y - 80).max(0);
    let right = (chat.x + chat.width as i32).max(point.x + 360);
    let bottom = (chat.y + chat.height as i32).max(point.y + 50);
    let max_right = config.screen.expected_width as i32;
    let max_bottom = config.screen.expected_height as i32;
    super::Rect::new(
        x,
        y,
        (right.min(max_right) - x).max(1) as u32,
        (bottom.min(max_bottom) - y).max(1) as u32,
    )
}

fn print_latency_summary(label: &str, values: &[u128]) {
    if values.is_empty() {
        println!("{}: 无有效样本", label);
        return;
    }
    let total = values.iter().sum::<u128>();
    let max = values.iter().copied().max().unwrap_or(0);
    println!(
        "{}: 样本={} 平均={}ms 最大={}ms",
        label,
        values.len(),
        total / values.len() as u128,
        max
    );
}

fn run_chat_change_monitor(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let canvas = default_canvas(&config);
    let interval_ms = prompt_optional_u64("采样间隔毫秒，留空使用配置")?
        .unwrap_or(config.ocr.change_poll_interval_ms);
    let mean_threshold = prompt_optional_f32("平均像素差阈值，留空使用配置")?
        .unwrap_or(config.ocr.change_mean_threshold);
    let ratio_threshold = prompt_optional_f32("变化像素比例阈值，留空使用配置")?
        .unwrap_or(config.ocr.change_pixel_threshold);
    let templates = TemplateArgs::default().resolve(&config);

    println!(
        "聊天区域: x={} y={} w={} h={}",
        config.screen.chat_rect.x,
        config.screen.chat_rect.y,
        config.screen.chat_rect.width,
        config.screen.chat_rect.height
    );
    println!(
        "采样间隔={}ms，触发阈值: mean>={:.3} 或 ratio>={:.5}",
        interval_ms, mean_threshold, ratio_threshold
    );
    println!("按 Ctrl+C 停止");

    let first_frame = load_frame(&FrameArgs { image: None }, &canvas, &config.window)?;
    let mut previous =
        configured_chat_change_fingerprint(&first_frame.image, config.screen.chat_rect)?;
    let mut previous_event = previous.clone();
    let mut sample_index = 0u64;
    let started = Instant::now();

    loop {
        sleep(Duration::from_millis(interval_ms));
        sample_index += 1;
        let capture_started = Instant::now();
        let frame = load_frame(&FrameArgs { image: None }, &canvas, &config.window)?;
        let capture_ms = capture_started.elapsed().as_millis();

        let fingerprint_started = Instant::now();
        let current = configured_chat_change_fingerprint(&frame.image, config.screen.chat_rect)?;
        let fingerprint_ms = fingerprint_started.elapsed().as_millis();

        let sample_compare_started = Instant::now();
        let sample_stats = change_stats(&previous, &current);
        let sample_compare_us = sample_compare_started.elapsed().as_micros();
        previous = current.clone();

        if sample_stats.mean_abs_diff < mean_threshold
            && sample_stats.changed_ratio < ratio_threshold
        {
            continue;
        }

        let event_compare_started = Instant::now();
        let event_stats = change_stats(&previous_event, &current);
        let event_compare_us = event_compare_started.elapsed().as_micros();
        previous_event = current;

        let template_started = Instant::now();
        let (blue_count, yellow_count, pink_count) =
            count_chat_markers(&frame.image, &templates, config.screen.chat_rect)?;
        let template_ms = template_started.elapsed().as_millis();

        println!(
            "{}ms 第{}次变动: 相邻 mean={:.3} ratio={:.5} 对比={}us; 距上次变动 mean={:.3} ratio={:.5} 对比={}us; 截图={}ms 指纹={}ms 模板={}ms(蓝{} 黄{} 粉{})",
            started.elapsed().as_millis(),
            sample_index,
            sample_stats.mean_abs_diff,
            sample_stats.changed_ratio,
            sample_compare_us,
            event_stats.mean_abs_diff,
            event_stats.changed_ratio,
            event_compare_us,
            capture_ms,
            fingerprint_ms,
            template_ms,
            blue_count,
            yellow_count,
            pink_count
        );
    }
}

fn capture_to_file(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let output = prompt_with_default("输出路径", &default_screenshot_path())?;
    let image = window::capture_game(&config.window)?;
    ensure_parent(Path::new(&output))?;
    image
        .save(&output)
        .with_context(|| format!("保存截图 {}", output))?;
    println!("已保存: {}", output);
    Ok(())
}

fn run_ocr(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let canvas = default_canvas(&config);
    let frame = load_frame(&FrameArgs { image: None }, &canvas, &config.window)?;
    let image = if let Some(rect) = prompt_optional_rect("OCR 区域 x,y,width,height，留空为整张图")?
    {
        crop_canvas(&frame.image, rect)?
    } else {
        frame.image
    };
    let ocr = OcrArgs::default().resolve(&config);
    let init_started = Instant::now();
    let engine = make_ocr_engine(&ocr)?;
    println!("OCR 初始化耗时: {}ms", init_started.elapsed().as_millis());
    let recognize_started = Instant::now();
    let lines = recognize_lines(&engine, &image)?;
    println!(
        "OCR 识别耗时: {}ms",
        recognize_started.elapsed().as_millis()
    );
    print_json(&lines)
}

fn run_scan_chat(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let canvas = default_canvas(&config);
    let frame = load_frame(&FrameArgs { image: None }, &canvas, &config.window)?;
    let ocr = OcrArgs::default().resolve(&config);
    let templates = TemplateArgs::default().resolve(&config);
    let init_started = Instant::now();
    let engine = make_ocr_engine(&ocr)?;
    println!("OCR 初始化耗时: {}ms", init_started.elapsed().as_millis());
    let scan_started = Instant::now();
    let messages = scan_chat(
        &frame.image,
        &engine,
        &templates,
        config.screen.chat_rect.into(),
    )?;
    println!("聊天区扫描耗时: {}ms", scan_started.elapsed().as_millis());
    print_chat_scan(&messages, config.screen.chat_rect);
    Ok(())
}

fn run_ocr_gpu_probe(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let ocr = OcrArgs::default().resolve(&config);
    let results = probe_ocr_backend_support(&ocr);
    let gpu_checked = results.iter().any(|result| result.gpu);
    let gpu_available = results
        .iter()
        .any(|result| result.gpu && result.is_available());

    println!("OCR GPU 支持检测");
    println!("检测模型: {}", ocr.det_model.display());
    println!("识别模型: {}", ocr.rec_model.display());
    println!("字符集: {}", ocr.charset.display());
    println!(
        "说明: 这是 MNN 后端初始化加首次推理检测；MNN 内部如果自行回退，Rust 侧拿不到更细的运行设备信息"
    );

    for result in &results {
        let kind = if result.gpu { "GPU" } else { "CPU" };
        match &result.status {
            OcrBackendProbeStatus::Available {
                init_ms,
                detect_ms,
                rec_ms,
            } => println!(
                "{} [{}] 可用，初始化 {}ms，检测推理 {}ms，识别推理 {}ms",
                result.name, kind, init_ms, detect_ms, rec_ms
            ),
            OcrBackendProbeStatus::Failed { elapsed_ms, error } => println!(
                "{} [{}] 不可用，耗时 {}ms，原因: {}",
                result.name, kind, elapsed_ms, error
            ),
        }
    }

    if !gpu_checked {
        println!("GPU 结论: 未检测，ocr.backend_priority 未包含 vulkan/opencl");
    } else if gpu_available {
        println!("GPU 结论: 可用，至少一个 GPU 后端完成初始化和首次推理");
    } else {
        println!("GPU 结论: 不可用，自动运行会继续回退 CPU");
    }
    Ok(())
}

fn print_chat_scan(messages: &[super::ChatMessage], chat_rect: super::config::RectConfig) {
    let blue_count = count_messages(messages, "blue");
    let yellow_count = count_messages(messages, "yellow");
    let pink_count = count_messages(messages, "pink");

    println!(
        "聊天识别区域: x={} y={} w={} h={}",
        chat_rect.x, chat_rect.y, chat_rect.width, chat_rect.height
    );
    println!(
        "第 1 轮: 蓝色标志 {} 个，黄色标志 {} 个，粉色标志 {} 个",
        blue_count, yellow_count, pink_count
    );

    if messages.is_empty() {
        println!("没有找到聊天标志，请检查模板、聊天区域和阈值");
        return;
    }

    for message in messages {
        let text = if message.text.is_empty() {
            "<空>"
        } else {
            message.text.as_str()
        };
        println!("识别结果: {}", text);
        println!(
            "  来源: {}标志，文本区域 x={} y={} w={} h={}",
            marker_label(&message.message_type),
            message.block.x,
            message.block.y,
            message.block.width,
            message.block.height
        );
    }
}

fn count_messages(messages: &[super::ChatMessage], kind: &str) -> usize {
    messages
        .iter()
        .filter(|message| message.message_type.as_str() == kind)
        .count()
}

fn marker_label(kind: &str) -> &str {
    match kind {
        "blue" => "蓝色",
        "yellow" => "黄色",
        "pink" => "粉色",
        other => other,
    }
}

fn run_ui_state(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let canvas = default_canvas(&config);
    let frame = load_frame(&FrameArgs { image: None }, &canvas, &config.window)?;
    let templates = UiTemplateArgs::default().resolve(&config);
    let started = Instant::now();
    let state = detect_ui_state(&frame.image, &templates, &config.screen)?;
    println!("{}", state);
    println!("UI 状态匹配耗时: {}ms", started.elapsed().as_millis());
    Ok(())
}

fn run_template_match(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let canvas = default_canvas(&config);
    let frame = load_frame(&FrameArgs { image: None }, &canvas, &config.window)?;
    let template = choose_template(&config)?;
    let rect = prompt_optional_rect("匹配区域 x,y,width,height，留空为整张图")?;
    let threshold =
        prompt_optional_f32("阈值，留空使用配置")?.unwrap_or(config.templates.marker_threshold);
    let started = Instant::now();
    let hits = find_template_hits(&frame.image, rect, &template, threshold)?;
    println!("模板匹配耗时: {}ms", started.elapsed().as_millis());
    print_json(&hits)?;

    if prompt_yes_no("点击最佳匹配？", false)? {
        let started = Instant::now();
        let hit = best_template_hit(&frame.image, rect, &template, threshold)?
            .ok_or_else(|| anyhow!("没有找到超过阈值的模板"))?;
        println!("最佳模板匹配耗时: {}ms", started.elapsed().as_millis());
        let center = hit.center();
        click_game_point(PointConfig::new(center.x, center.y), &config.window)?;
    }
    Ok(())
}

fn run_click(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let x = prompt_i32("x")?;
    let y = prompt_i32("y")?;
    if prompt_yes_no(&format!("确认点击 {},{}？", x, y), true)? {
        click_game_point(PointConfig::new(x, y), &config.window)?;
    }
    Ok(())
}

fn run_key(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let key = prompt("按键，例如 Return/Escape/F2/N")?;
    let key = parse_key(&key)?;
    if prompt_yes_no("确认发送按键？", true)? {
        press_key(key, &config.window)?;
    }
    Ok(())
}

fn run_send_chat(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let message = prompt("消息内容")?;
    if message.trim().is_empty() {
        return Ok(());
    }
    if prompt_yes_no("确认发送聊天？", true)? {
        ChatOutput::new(&config.output, &config.window).send(&message)?;
    }
    Ok(())
}

fn choose_template(config: &AppConfig) -> Result<PathBuf> {
    println!("模板：");
    println!("1. 蓝色聊天标志");
    println!("2. 黄色聊天标志");
    println!("3. 粉色聊天标志");
    println!("4. 回车 UI");
    println!("5. 大厅 UI");
    println!("6. 自定义路径");

    let path = match prompt("请选择模板")?.trim() {
        "1" => config.templates.blue_marker.clone(),
        "2" => config.templates.yellow_marker.clone(),
        "3" => config.templates.pink_marker.clone(),
        "4" => config.templates.enter.clone(),
        "5" => config.templates.dating.clone(),
        "6" => PathBuf::from(prompt("模板路径")?),
        other => return Err(anyhow!("未知模板选项: {}", other)),
    };
    Ok(path)
}

fn default_canvas(config: &AppConfig) -> Canvas {
    Canvas {
        width: config.screen.expected_width,
        height: config.screen.expected_height,
        resize: true,
    }
}

fn prompt_optional_rect(label: &str) -> Result<Option<super::Rect>> {
    let value = prompt(label)?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(parse_rect(&value)?))
    }
}

fn prompt_optional_f32(label: &str) -> Result<Option<f32>> {
    let value = prompt(label)?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        value.trim().parse::<f32>().map(Some).context("解析数字")
    }
}

fn prompt_optional_u64(label: &str) -> Result<Option<u64>> {
    let value = prompt(label)?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        value.trim().parse::<u64>().map(Some).context("解析数字")
    }
}

fn prompt_i32(label: &str) -> Result<i32> {
    prompt(label)?
        .trim()
        .parse::<i32>()
        .with_context(|| format!("解析 {}", label))
}

fn prompt_yes_no(label: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "Y/n" } else { "y/N" };
    let value = prompt(&format!("{} [{}]", label, suffix))?;
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(value.as_str(), "y" | "yes" | "1" | "true"))
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let value = prompt(&format!("{} [{}]", label, default))?;
    if value.trim().is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{}: ", label);
    io::stdout().flush().context("flush stdout")?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).context("read stdin")?;
    Ok(input.trim().to_string())
}

fn default_screenshot_path() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("debug/game-{}.png", seconds)
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("创建目录 {}", parent.display()))?;
    }
    Ok(())
}
