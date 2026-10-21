use super::*;
use crate::interfaces::http::ports::{HttpDebugCommand, HttpDebugPort};

fn debug_port(state: &HttpSharedState) -> Result<&dyn HttpDebugPort, AppError> {
    state.application.debug.as_deref().ok_or_else(|| AppError {
        status: 503,
        message: "调试接口未接线".into(),
    })
}
/// GET /debug/ui-state[?image=<路径>]：只读查询界面状态与判别证据。
/// 未分类时返回 classified=false 并给出原因，**绝不用一个猜出来的状态充数**。
///
/// 带 `image=` 时改为对**指定的已保存帧**跑**分类器本体**（`classify_at`），
/// 而不是读实时观测。这样"判别依据可被独立复现"才对分类器本身成立——
/// 否则保存的帧只能被探针重跑，分类器的判定无法离线复核。
pub(super) fn debug_ui_state_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    if let Some(path) = query_value(query, "image").filter(|value| !value.is_empty()) {
        let image = image::open(std::path::Path::new(path)).map_err(|error| AppError {
            status: 400,
            message: format!("读取指定图片失败: {error}"),
        })?;
        return classify_saved_frame(state, &image);
    }
    debug_port(state)?.ui_state_detail().map_err(command_error)
}

/// 对一幅**已保存**的帧运行分类器本体，并给出与实时接口同名的判别证据。
///
/// 注意两处与实时路径的**语义差异**，都会如实写进响应里：
/// - `frameId` 为 `null`：离线帧不属于运行时的帧序列。
/// - `stable` 恒为 `false`：稳定性来自跨帧累积，**单帧无法证明稳定**，不假装可以。
fn classify_saved_frame(
    state: &HttpSharedState,
    image: &image::DynamicImage,
) -> Result<String, AppError> {
    let config = state.live_configs.snapshot();
    // 与生产用同一条解析路径（见 composition/application.rs），避免离线用另一套模板。
    let templates =
        crate::ui::state::UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
    // 分类器消耗一份模板；fallback 探针还要用，所以先克隆一份。
    let mut classifier =
        crate::ui::state::TemplateUiStateClassifier::new(templates.clone(), config.screen.clone());
    let classification = classifier
        .classify_at(image, std::time::Instant::now())
        .map_err(|error| AppError {
            status: 500,
            message: format!("分类指定图片失败: {error}"),
        })?;
    let evidence = classification.evidence();
    // 证据里的矩形是 `UiEvidenceRect`（带访问器），与几何 `Rect` 不同型，这里单独转换。
    let evidence_rect = |rect: crate::runtime::ui::UiEvidenceRect| {
        json!({
            "x": rect.x(),
            "y": rect.y(),
            "width": rect.width(),
            "height": rect.height(),
        })
    };
    let probes: Vec<serde_json::Value> = evidence
        .template_probes()
        .iter()
        .map(|probe| {
            json!({
                "template": probe.template(),
                "searchRect": evidence_rect(probe.search_rect()),
                "bestScore": probe.best_score(),
                "threshold": probe.threshold(),
                "hitRect": probe.hit_rect().map(evidence_rect),
                "outcome": probe.outcome(),
            })
        })
        .collect();
    // 字段名与实时接口**逐字一致**，两条路径的结果才能直接对比。
    let marker = evidence.marker_probe().map(|marker| {
        let (width, height) = marker.coordinate_size();
        json!({
            "searchRect": evidence_rect(marker.search_rect()),
            "coordinateWidth": width,
            "coordinateHeight": height,
            "blue": marker.blue_count(),
            "yellow": marker.yellow_count(),
            "pink": marker.pink_count(),
        })
    });
    // 世界判据（提瓦特 vs 千星）：正交于界面状态的一维，只作为证据报出。
    // 字段名与实时接口逐字一致；count 为 null 表示画面太小、这条判据不适用。
    let world = evidence.world_probe().map(|world| {
        json!({
            "region": evidence_rect(world.region()),
            "count": world.count(),
            "teyvatMax": world.teyvat_max(),
            "qianxingMin": world.qianxing_min(),
            "guess": world.guess(),
        })
    });
    // **fallback 探针**：实时路径只有在**连续 2 帧**都判 unknown 后才会走 `probe_game_state`，
    // 单帧离线分类永远到不了那里。这里把它单独跑一遍并**只作为证据**报出，
    // 让离线也能看见 `world-wish` 的分数——但**绝不**据此改写上面的 kind。
    // 注：`game_entry` 需要 OCR 运行时，离线传 None，所以 `game_gate` 这条分支离线测不到。
    let fallback = if classification.kind() == crate::runtime::ui::UiStateKind::Unknown {
        match crate::ui::state::probe_game_state_without_entry(image, &templates, &config.screen) {
            Ok(probed) => {
                let probed_evidence = probed.evidence();
                let probed_probes: Vec<serde_json::Value> = probed_evidence
                    .template_probes()
                    .iter()
                    .map(|probe| {
                        json!({
                            "template": probe.template(),
                            "searchRect": evidence_rect(probe.search_rect()),
                            "bestScore": probe.best_score(),
                            "threshold": probe.threshold(),
                            "hitRect": probe.hit_rect().map(evidence_rect),
                            "outcome": probe.outcome(),
                        })
                    })
                    .collect();
                json!({
                    "kind": probed.kind().name(),
                    "label": probed.label(),
                    "finalRule": probed_evidence.final_rule(),
                    "templateProbes": probed_probes,
                    "note": "仅证据：实时路径需连续 2 帧 unknown 才采用本结果，离线单帧不采用",
                })
            }
            Err(error) => json!({ "error": error.to_string() }),
        }
    } else {
        serde_json::Value::Null
    };
    Ok(json!({
        "ok": true,
        "classified": true,
        "source": "savedFrame",
        "fallbackProbe": fallback,
        "note": "对已保存帧跑分类器本体；stable 恒为 false（单帧无法证明跨帧稳定）",
        "frameHash": frame_fingerprint(image),
        "frameId": serde_json::Value::Null,
        "kind": classification.kind().name(),
        "label": classification.label(),
        "stable": false,
        "evidence": {
            "finalRule": evidence.final_rule(),
            "templateProbes": probes,
            "marker": marker,
            "worldProbe": world,
        },
    })
    .to_string())
}

/// 帧指纹：把一次测量绑定到一帧具体的画面。屏幕在动时，没有指纹的测量无法复核。
///
/// FNV-1a 覆盖整幅灰度图；同一帧同一指纹，换一帧必然不同。
fn frame_fingerprint(image: &image::DynamicImage) -> String {
    let luma = image.to_luma8();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in luma.as_raw() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 一组灰度像素的总体标准差：量"这块区域到底有没有结构"。
///
/// **为什么需要单独量这个**：实测发现 `best_template_candidate` 在**平坦渐变背景**上
/// 也能给出 0.90~0.94 的高分，而那时画面里根本没有对应控件。
/// 原因是 SAD 按模板的 alpha 总量做归一化，而多个模板本身是被放大过的糊图，
/// 能量集中在低频，于是任何平滑区域都能得到很低的相对 SAD（也就是很高的分数）。
///
/// 实测对照（同一时刻、同一帧）：
/// - 平坦误配：`friend-block-chat` regionStd=3.7、`friend-blacklist` regionStd=4.1
/// - 真实特征：`yellow-marker` regionStd=32.9、`paimon-menu` regionStd=68.4
///
/// **这是标准统计量，没有为任何已观测截图调过参数。**
fn luma_std(pixels: &image::GrayImage) -> f64 {
    let n = pixels.as_raw().len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mean = pixels.pixels().map(|p| p.0[0] as f64).sum::<f64>() / n;
    let variance = pixels
        .pixels()
        .map(|p| {
            let delta = p.0[0] as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / n;
    variance.sqrt()
}

/// 皮尔逊相关系数：判断"命中区域与模板在形状上到底像不像"。
///
/// 用它而不是再设一个阈值，是因为平坦区域的方差趋近 0，相关系数会**自然**趋近 0，
/// 不需要人为规定"相关度够多少才算数"，就能看出这个匹配没有内容依据。
///
/// 实测对照：
/// - 平坦误配：`friend-block-chat` corr=-0.017、`friend-blacklist` corr=-0.058
/// - 形状歧义（图标不同、但都是深色底上的亮块）：`pink-marker` corr=0.634
/// - 真实命中：`yellow-marker` corr=0.996、`friend` corr=1.000
///
/// 尺寸不一致或任一方差为 0（纯色区域）时返回 `None`，
/// **不用 0 冒充"算不出来"**——两者含义不同，读者必须能区分。
fn pearson_luma(a: &image::GrayImage, b: &image::GrayImage) -> Option<f64> {
    if a.dimensions() != b.dimensions() || a.as_raw().is_empty() {
        return None;
    }
    let n = a.as_raw().len() as f64;
    let mean_a = a.pixels().map(|p| p.0[0] as f64).sum::<f64>() / n;
    let mean_b = b.pixels().map(|p| p.0[0] as f64).sum::<f64>() / n;
    let mut covariance = 0.0;
    let mut variance_a = 0.0;
    let mut variance_b = 0.0;
    for (pixel_a, pixel_b) in a.pixels().zip(b.pixels()) {
        let da = pixel_a.0[0] as f64 - mean_a;
        let db = pixel_b.0[0] as f64 - mean_b;
        covariance += da * db;
        variance_a += da * da;
        variance_b += db * db;
    }
    if variance_a <= 0.0 || variance_b <= 0.0 {
        return None;
    }
    Some(covariance / (variance_a.sqrt() * variance_b.sqrt()))
}

/// 量命中区域的"结构性"：区域灰度标准差、模板灰度标准差、两者的皮尔逊相关。
///
/// 三项都是**标准统计量**，没有为任何已观测截图调过参数；
/// 平坦命中处会表现为低 `regionStd` 与低 `correlation`，一眼可辨。
///
/// 从模板文件重新读一次图（而不是复用匹配器的内部缓存），是为了让这个证据
/// **独立于匹配器实现**：改动匹配器不会悄悄改变这里的含义。
fn structural_evidence(
    image: &image::DynamicImage,
    template_path: &std::path::Path,
    hit: crate::geometry::Rect,
) -> (f64, Option<f64>, Option<f64>) {
    let hit_region = image
        .crop_imm(hit.x as u32, hit.y as u32, hit.width, hit.height)
        .to_luma8();
    let region_std = luma_std(&hit_region);
    let Ok(template) = image::open(template_path) else {
        return (region_std, None, None);
    };
    let template_luma = template.to_luma8();
    let template_std = luma_std(&template_luma);
    let correlation = pearson_luma(&hit_region, &template_luma);
    (region_std, Some(template_std), correlation)
}

/// 用**分类器自己的那段代码路径**复测标记，并做同样的去重。
///
/// 为什么必须单独做一遍：探针里的 `bestTemplateScore` 走的是**灰度** SAD，
/// 而分类器 `find_chat_markers` 走的是**彩色**（RGB，三通道）SAD 取全部过阈值命中再去重。
/// 两者不是同一个判据，灰度分数不能拿来断言"分类器会怎么判"。
/// 这里直接调用分类器用的 `find_color_template_hit_groups` 与 `dedupe_chat_marker_hits`，
/// 保证"探针报的"就是"分类器算的"。
fn classifier_marker_evidence(
    image: &image::DynamicImage,
    region: crate::geometry::Rect,
    paths: &[&std::path::Path],
    threshold: f32,
    dedupe_x: i32,
    dedupe_y: i32,
) -> serde_json::Value {
    let kinds = ["blue", "yellow", "pink"];
    let groups = match crate::ui::template::find_color_template_hit_groups(
        image,
        Some(region),
        paths,
        threshold,
    ) {
        Ok(groups) => groups,
        Err(error) => {
            return json!({ "classifierError": error.to_string() });
        }
    };
    let mut hits = Vec::new();
    let mut raw = 0usize;
    for (mut group, kind) in groups.into_iter().zip(kinds) {
        raw += group.len();
        for hit in &mut group {
            hit.kind = kind.to_string();
        }
        hits.extend(group);
    }
    let best = hits
        .iter()
        .map(|hit| hit.score)
        .fold(None, |acc: Option<f32>, score| {
            Some(acc.map_or(score, |value| value.max(score)))
        });
    let deduped = crate::observation::chat::scan::dedupe_chat_marker_hits(hits, dedupe_x, dedupe_y);
    json!({
        "classifierThreshold": threshold,
        "classifierRawHits": raw,
        "classifierHitCount": deduped.len(),
        "classifierMatched": !deduped.is_empty(),
        "classifierBestScore": best,
        "classifierHits": deduped.iter().map(|hit| json!({
            "kind": hit.kind, "x": hit.x, "y": hit.y,
            "width": hit.width, "height": hit.height, "score": hit.score,
        })).collect::<Vec<_>>(),
    })
}

/// 搜索区域的 JSON 形式，与 `/debug/ui-state` 的证据保持一致。
fn rect_json(rect: crate::geometry::Rect) -> serde_json::Value {
    json!({ "x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height })
}

/// GET /debug/ui-catalogue：只读模板目录探针。
///
/// 应用本来就有这 19 个模板特征，但它们分散在各 routine 里（`moderation` /
/// `invite` / `startup` / `friend_delivery`），没有统一入口，所以"任意一张截图
/// 属于哪个界面"无法被独立查询。这里把**已有的**判据统一暴露：每个模板各自的
/// 得分、阈值与是否命中。命中就是命中，没命中就是没命中，不猜。
///
/// **刻意不参与统一状态机的判定**：`UiStateKind` 的结论一个字都不改。
/// 没命中只说明这个模板不成立，不代表该界面不存在。
///
/// 查询参数：
/// - `image=<路径>`：**不抓屏**，改为对指定的图片文件跑同一套探针。
///   与 `save=` 配对即可离线复现：先用 `save=` 存帧，之后对同一个文件重跑，
///   探针结果与 `frameHash` 必须完全一致。
/// - `save=<路径>`：把**本次测量所用的那一帧**一并落盘，并在返回里给出 `frameSaved`。
///   屏幕是动态的，事后重抓必然是不同的画面；只有把帧和数字一起存下来，
///   这次测量才可被第三方复核。
///   **落盘强制为无损 PNG**（扩展名不是 `.png` 会被改写，实际路径见 `frameSaved`）：
///   实测存成 jpg 后离线重跑，帧指纹与逐模板得分都会变，离线复现就不成立。
///
/// 返回里始终带 `frameHash`：即使不落盘，也能判断两次测量的数字**是否来自同一帧**
/// （指纹不同就不可直接比较）。
pub(super) fn debug_ui_catalogue_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let config = state.live_configs.snapshot();
    // `image=<路径>` 时不抓屏，改为对**指定的图片文件**跑同一套探针。
    // 保存下来的帧因此可以离线重跑并得到完全相同的数字——
    // 「判别依据可被独立复现」靠的是这条路径，而不是事后重新截一张屏。
    let image = match query_value(query, "image") {
        // `capture()` 返回 `Arc<DynamicImage>`，这里包一层保持两个分支同型。
        Some(path) if !path.is_empty() => Arc::new(
            image::open(std::path::Path::new(path)).map_err(|error| AppError {
                status: 400,
                message: format!("读取指定图片失败: {error}"),
            })?,
        ),
        _ => debug_port(state)?.capture().map_err(command_error)?,
    };

    // 每次测量都必须能绑定到**它量到的那一帧**：屏幕是动的，事后重抓一定是另一个画面，
    // 只报数字的结论无法复核。所以这里始终给出帧指纹，并支持把该帧一起落盘。
    let frame_hash = frame_fingerprint(&image);
    let saved_path = match query_value(query, "save") {
        Some(name) if !name.is_empty() => {
            // **一律存无损 PNG**。实测：同一个抓帧存成 jpg 之后再离线重跑，
            // 帧指纹与逐模板得分都会变（JPEG 有损，重新编码改变了像素），
            // 那样"离线复现"就不成立——而这正是保存这一帧的全部意义。
            // 扩展名不是 png 就改写为 png，实际写入路径在 frameSaved 里如实返回。
            let mut path = std::path::PathBuf::from(name);
            if path.extension().and_then(|ext| ext.to_str()) != Some("png") {
                path.set_extension("png");
            }
            image.save(&path).map_err(|error| AppError {
                status: 500,
                message: format!("保存测量帧失败: {error}"),
            })?;
            Some(path.display().to_string())
        }
        _ => None,
    };

    let screen = &config.screen;
    let invite = &config.invite;
    let moderation = &config.moderation;
    let startup = &config.startup;
    let templates = &config.templates;
    let marker = templates.marker_threshold;
    // **重要**：分类器扫标记时只在裁剪后聊天区左侧 `CHAT_MARKER_SEARCH_WIDTH` 像素内搜索
    // （`scan.rs` 的 `find_chat_markers`）。早先这个探针在整块 chat_rect 上搜索，
    // 于是"过阈值的平坦误配"会被当成标记，**与分类器的实际判定不等价**。
    // 这里改成同一窗口，使探针数值能真正代表分类器行为。
    let chat: crate::geometry::Rect = screen.chat_rect.into();
    let marker_region = crate::geometry::Rect::new(
        chat.x,
        chat.y,
        config.ocr.marker_search_width.min(chat.width),
        chat.height,
    );

    // 顺序与 GET /tools/templates 一致，便于互相对照。
    let entries: [(&str, &str, crate::geometry::Rect, &std::path::Path, f32); 19] = [
        (
            "blue-marker",
            "蓝色聊天标志",
            marker_region,
            &templates.blue_marker,
            marker,
        ),
        (
            "yellow-marker",
            "黄色聊天标志",
            marker_region,
            &templates.yellow_marker,
            marker,
        ),
        (
            "pink-marker",
            "粉色聊天标志",
            marker_region,
            &templates.pink_marker,
            marker,
        ),
        (
            "friend",
            "好友按钮",
            screen.friend_rect.into(),
            &templates.friend,
            marker,
        ),
        (
            "secondary-back",
            "二级聊天返回按钮",
            screen.secondary_back_rect.into(),
            &templates.secondary_back,
            marker,
        ),
        (
            "secondary-hall",
            "二级当前大厅",
            screen.secondary_hall_rect.into(),
            &templates.secondary_hall,
            marker,
        ),
        (
            "world-wish",
            "祈愿",
            screen.world_wish_rect.into(),
            &templates.world_wish,
            marker,
        ),
        (
            "invite-view-star",
            "邀请查看千星",
            invite.view_star_region.into(),
            &templates.invite_view_star,
            marker,
        ),
        (
            "invite-goto-hall",
            "邀请前往大厅",
            invite.goto_hall_region.into(),
            &templates.invite_goto_hall,
            marker,
        ),
        (
            "invite-enter-hall",
            "邀请进入大厅",
            invite.enter_hall_region.into(),
            &templates.invite_enter_hall,
            marker,
        ),
        (
            "friend-panel",
            "好友面板",
            moderation.friend_panel_region.into(),
            &templates.friend_panel,
            marker,
        ),
        (
            "friend-search-panel",
            "好友搜索面板",
            moderation.search_panel_region.into(),
            &templates.friend_search_panel,
            marker,
        ),
        (
            "friend-more-settings",
            "好友更多设置",
            moderation.more_settings_region.into(),
            &templates.friend_more_settings,
            marker,
        ),
        (
            "friend-block-chat",
            "屏蔽聊天",
            moderation.block_chat_region.into(),
            &templates.friend_block_chat,
            marker,
        ),
        (
            "friend-blacklist",
            "拉黑",
            moderation.blacklist_region.into(),
            &templates.friend_blacklist,
            marker,
        ),
        (
            "friend-confirm",
            "好友操作确认",
            moderation.confirm_region.into(),
            &templates.friend_confirm,
            marker,
        ),
        (
            "wonderland-confirm",
            "千星确认按钮",
            startup.wonderland_confirm_region.into(),
            &startup.templates.wonderland_confirm,
            startup.wonderland_confirm_threshold,
        ),
        (
            "paimon-menu",
            "派蒙主界面",
            startup.main_ui_region.into(),
            &startup.templates.paimon_menu,
            startup.template_threshold,
        ),
        (
            "wonderland-map-star",
            "千星地图入口",
            startup.wonderland_map_star_region.into(),
            &startup.templates.wonderland_map_star,
            startup.template_threshold,
        ),
    ];

    // 标记必须**成组**复测：分类器是拿 3 个模板一起搜、合并后再去重，
    // 只看单个模板的分数无法代表它的判定。
    let marker_paths = [
        templates.blue_marker.as_path(),
        templates.yellow_marker.as_path(),
        templates.pink_marker.as_path(),
    ];
    let marker_classifier = classifier_marker_evidence(
        &image,
        marker_region,
        &marker_paths,
        marker,
        config
            .ocr
            .marker_dedupe_x
            .max(config.ocr.marker_dedupe_min_x),
        config
            .ocr
            .marker_dedupe_y
            .max(config.ocr.marker_dedupe_min_y),
    );

    let mut probes = Vec::with_capacity(entries.len());
    let mut matched_names = Vec::new();
    for (name, label, region, path, threshold) in entries {
        let template = path.display().to_string();
        // 用带候选的版本：**未命中时也要拿到得分**，否则"没命中"这个结论本身无法复核。
        let entry = match crate::ui::template::best_template_candidate(&image, Some(region), path) {
            Ok(Some(hit)) => {
                let matched = hit.score >= threshold;
                if matched {
                    matched_names.push(name);
                }
                // 结构性证据：实测发现**平坦背景也能拿到 0.90 以上的高分**，
                // 所以"过阈值"不足以说明特征真的出现。这里额外量命中处到底有没有结构、
                // 以及它与模板的形状相关度。**只增加证据，不参与任何判定。**
                let (region_std, template_std, correlation) =
                    structural_evidence(&image, path, hit.rect());
                json!({
                    "name": name, "label": label, "template": template,
                    "region": rect_json(region), "threshold": threshold,
                    "bestScore": hit.score, "matched": matched,
                    "hitRect": rect_json(hit.rect()),
                    "regionStd": region_std, "templateStd": template_std,
                    "correlation": correlation,
                })
            }
            Ok(None) => json!({
                "name": name, "label": label, "template": template,
                "region": rect_json(region), "threshold": threshold,
                "bestScore": serde_json::Value::Null, "matched": false, "hitRect": serde_json::Value::Null,
                "note": "模板比搜索区域大或模板为空，无法比较",
            }),
            Err(err) => json!({
                "name": name, "label": label, "template": template,
                "region": rect_json(region), "threshold": threshold,
                "bestScore": serde_json::Value::Null, "matched": false, "hitRect": serde_json::Value::Null,
                "error": err.to_string(),
            }),
        };
        // 标记探针额外附上"分类器等价"的那组数字，二者分开报告、不混为一个结论。
        let mut entry = entry;
        if name.ends_with("-marker")
            && let (Some(object), Some(extra)) =
                (entry.as_object_mut(), marker_classifier.as_object())
        {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        probes.push(entry);
    }

    Ok(json!({
        "ok": true,
        "note": "只读探针：不参与 UiStateKind 判定；未命中不代表界面不存在",
        "templateCount": probes.len(),
        "frameHash": frame_hash,
        "frameSaved": saved_path,
        "matched": matched_names,
        "probes": probes,
    })
    .to_string())
}

/// GET /debug/window：只读查询目标窗口当前是否可用。
///
/// ⚠️ **复原说明**：本函数的原始实现曾在一次误操作中被截断丢失
/// （写回文件时用了带 limit 的读取结果），此处按调用方契约重建：
/// 签名与同文件其它 debug 路由一致；端口不可用时如实返回错误（不伪造成"不可用"）。
/// 若你手头有原始版本，请以原始版本为准。
pub(super) fn debug_window_route(
    _: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let port = debug_port(state)?;
    let available = port.window_available().map_err(command_error)?;
    Ok(json!({
        "ok": true,
        "note": "只读窗口状态；端口不可用时本路由返回错误而不是 availability=false",
        "available": available,
        "enabled": port.enabled(),
    })
    .to_string())
}

// ============================================================================
// 以下函数为 **2026-10-03 事故后的重建**。
//
// 事故经过：一次 `write` 回写时误用了带 `limit` 的读取结果，把本文件尾部截断，
// 丢失了 13 个路由处理器、2 个响应构造函数与 1 个模块可见性声明。
// 恢复来源：`protocol.rs` 的 RouteSpec/BodyRouteSpec（路径与处理器名）、
// `protocol/tests.rs` 的契约测试、`DebugHttpService` 的公开方法、
// `technical-docs/debug-sandbox.md` 与 `src/debug_events.rs` 的说明。
// **若你手头有原始版本，请以原始版本为准。**
// ============================================================================

/// 取调试沙盒服务；与 `debug_port` 一样，未接线时如实报错。
fn debug_service(
    state: &HttpSharedState,
) -> Result<&crate::debug_http_service::DebugHttpService, AppError> {
    Ok(debug_port(state)?.sandbox())
}

/// 运行类路由的参数名是 `runId`（与运行对象字段一致）。
/// 历史上也出现过 `id`，两者都接受，避免老客户端静默失效。
fn run_id_query(query: &[(String, String)]) -> Result<String, AppError> {
    required_query(query, "runId").or_else(|_| required_query(query, "id"))
}

/// 必填查询参数：缺失即 400，不用空串顶替。
fn required_query(query: &[(String, String)], key: &str) -> Result<String, AppError> {
    query_value(query, key)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError {
            status: 400,
            message: format!("缺少查询参数 {key}"),
        })
}

/// SSE 帧：`id` 用事件的单调序号，客户端断线后带 `since` 就能精确续传。
fn sse_frame(event: &crate::debug_events::DebugEvent) -> String {
    format!(
        "id: {}\nevent: {}\ndata: {}\n\n",
        event.sequence, event.kind, event.payload
    )
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadOnlyModeRequest {
    enabled: bool,
    state: crate::debug_read_only::ReadOnlyState,
}
pub(super) fn debug_read_only_mode_set_route(
    body: &[u8],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let request = parse_json_body::<ReadOnlyModeRequest>(body, "只读模式")?;
    let mode = debug_service(state)?
        .set_read_only_mode(request.enabled, request.state)
        .map_err(command_error)?;
    Ok(json!({"ok":true,"mode":mode}).to_string())
}
pub(super) fn debug_read_only_mode_route(
    _: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let _ = debug_service(state)?;
    Ok(json!({"ok":true,"mode":crate::debug_read_only::observer().state_snapshot()}).to_string())
}
pub(super) fn debug_read_only_read_route(
    body: &[u8],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let request =
        parse_json_body::<crate::debug_http_service::ReadOnlyReadRequest>(body, "只读读取")?;
    let evidence = debug_service(state)?
        .read_active(request)
        .map_err(command_error)?;
    Ok(json!({"ok":true,"evidence":evidence}).to_string())
}
/// GET /debug/read-only/evidence?id=ID[&section=&cursor=&limit=&maxBytes=][&instance=&revision=]
///
/// **兼容两种客户端**：
/// - 不带 `section`：返回与旧 POST 完整响应同形的完整证据（含内联 PNG，只有显式
///   分页才不内联）。旧客户端和推送事件的 `evidenceUrl` 依赖这条语义，
///   不能把它悄悄换成默认 metadata 分页。
/// - 显式 `section`：返回该分区的 UTF-8 JSON 文本分页，按预算分页且不内联 PNG。
///
/// `section=` 显式出现但为空是**无效分区**，不回退为完整包——拼错的 URL
/// 不应该变成"整个证据都发过来"。
///
/// 身份校验（`instance`）先于分区与版本解析；指定 `revision` 时两条路径都要核对，
/// 保留 revision_conflict(409) 与证据不存在(404)/已过期(410) 的既有语义，
/// 也不会在证据缺失时改抓新帧冒充被请求的旧证据。
pub(super) fn debug_read_only_evidence_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let _ = debug_service(state)?;
    let id = required_query(query, "id")?;
    if query_value(query, "instance")
        .is_some_and(|expected| expected != crate::debug_read_only::observer().instance())
    {
        return Err(readonly_evidence_error("instance_conflict".into()));
    }
    let revision = query_value(query, "revision")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| bad_request("revision必须为非负整数"))
        })
        .transpose()?;
    let Some(section) = query_value(query, "section") else {
        return read_full_evidence(&id, revision)
            .map(|evidence| json!({"ok": true, "evidence": evidence}).to_string())
            .map_err(readonly_evidence_error);
    };
    if section.is_empty() {
        return Err(readonly_evidence_error(
            "invalid_section: metadata|ocr|parameters|history".into(),
        ));
    }
    let cursor = readonly_query_number(query, "cursor", 0usize)?;
    let limit = readonly_query_number(query, "limit", 10usize)?.clamp(1, 100);
    let max_bytes = readonly_query_number(query, "maxBytes", 8192usize)?.min(65536);
    crate::debug_read_only::observer()
        .read_section(&id, section, cursor, limit, max_bytes, revision)
        .map(|value| value.to_string())
        .map_err(readonly_evidence_error)
}

/// 不带 `section` 的旧 GET 兼容读取：完整证据 + 版本一致性检查。
///
/// 先用**现有元数据**核对 `revision`，再 `read_entry`（它带内联 PNG），
/// 最后核对返回包里的 `revision`：两次读取之间可能有状态更新写入，
/// 只查一次会让"版本一致"这个承诺落空。任何一步失败都按既有错误语义上报，
/// 不改抓新帧，也不退回最近一帧冒充被请求的证据。
fn read_full_evidence(id: &str, revision: Option<u64>) -> Result<serde_json::Value, String> {
    let observer = crate::debug_read_only::observer();
    if let Some(expected) = revision {
        let metadata = observer.entry_metadata(id)?;
        if metadata["revision"].as_u64() != Some(expected) {
            return Err(format!(
                "revision_conflict: current={}",
                metadata["revision"]
            ));
        }
    }
    let evidence = observer.read_entry(id)?;
    if let Some(expected) = revision {
        let current = evidence["revision"].as_u64().unwrap_or(0);
        if current != expected {
            return Err(format!("revision_conflict: current={current}"));
        }
    }
    Ok(evidence)
}
pub(super) fn debug_read_only_entries_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let _ = debug_service(state)?;
    let since = query_value(query, "since")
        .unwrap_or("0")
        .parse::<u64>()
        .map_err(|_| bad_request("since必须为非负整数"))?;
    let limit = readonly_query_number(query, "limit", 10usize)?.clamp(1, 100);
    let max_bytes = readonly_query_number(query, "maxBytes", 8192usize)?.min(65536);
    let task_id = query_value(query, "taskId")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| bad_request("taskId必须为非负整数"))
        })
        .transpose()?;
    crate::debug_read_only::observer()
        .list_page(
            since,
            limit,
            max_bytes,
            task_id,
            query_value(query, "instance"),
        )
        .map(|value| value.to_string())
        .map_err(readonly_evidence_error)
}

fn readonly_query_number<T: std::str::FromStr>(
    query: &[(String, String)],
    key: &str,
    default: T,
) -> Result<T, AppError> {
    match query_value(query, key) {
        Some(value) => value
            .parse::<T>()
            .map_err(|_| bad_request(&format!("{key}必须为非负整数"))),
        None => Ok(default),
    }
}
fn readonly_evidence_error(reason: String) -> AppError {
    let status = if reason.starts_with("revision_conflict") {
        409
    } else if reason.starts_with("instance_conflict") {
        409
    } else if reason.starts_with("evidence_expired")
        || reason.starts_with("image_expired")
        || reason.starts_with("image_unavailable")
    {
        410
    } else if reason.starts_with("evidence_not_found") {
        404
    } else {
        400
    };
    AppError {
        status,
        message: reason,
    }
}

pub(super) fn debug_read_only_summary_route(
    _: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    Ok(json!({"ok":true,"summary":debug_service(state)?.read_only_summary()}).to_string())
}

pub(super) fn debug_read_only_image_response(
    request: &Request,
    state: &HttpSharedState,
) -> Result<Response, AppError> {
    let _ = debug_service(state)?;
    let id = required_query(&request.query, "id")?;
    let (bytes, metadata) = crate::debug_read_only::observer()
        .read_image(&id)
        .map_err(readonly_evidence_error)?;
    if query_value(&request.query, "instance")
        .is_some_and(|expected| metadata["instance"].as_str() != Some(expected))
    {
        return Err(readonly_evidence_error("instance_conflict".into()));
    }
    if let Some(expected) = query_value(&request.query, "revision") {
        let expected = expected
            .parse::<u64>()
            .map_err(|_| bad_request("revision必须为非负整数"))?;
        if metadata["revision"].as_u64() != Some(expected) {
            return Err(readonly_evidence_error("revision_conflict".into()));
        }
    }
    let mut headers = cors_headers(request, &state.config.http.host, state.config.http.port);
    headers.push(("Cache-Control".into(), "no-store".into()));
    headers.push((
        "X-ReadOnly-Instance".into(),
        crate::debug_read_only::observer().instance().into(),
    ));
    if let Some(revision) = metadata.get("revision").and_then(serde_json::Value::as_u64) {
        headers.push(("X-ReadOnly-Revision".into(), revision.to_string()));
    }
    for (key, field) in [
        ("X-Pixel-Sha256", "pixelSha256"),
        ("X-File-Sha256", "fileSha256"),
    ] {
        if let Some(hash) = metadata.get(field).and_then(serde_json::Value::as_str) {
            headers.push((key.into(), hash.into()));
        }
    }
    if let Some(hash) = metadata
        .get("fileSha256")
        .and_then(serde_json::Value::as_str)
    {
        headers.push(("ETag".into(), format!("\"{hash}\"")));
    }
    Ok(add_headers(
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "image/png")
            .body(Body::from(bytes)),
        headers,
    ))
}

/// GET /debug/tasks：列出已保存的沙盒任务。
pub(super) fn debug_tasks_route(
    _: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let tasks = debug_service(state)?.tasks().map_err(command_error)?;
    Ok(json!({ "ok": true, "tasks": tasks }).to_string())
}

/// GET /debug/runs：列出已知的运行记录（含已归档的）。
pub(super) fn debug_runs_route(
    _: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let runs = debug_service(state)?.runs().map_err(command_error)?;
    Ok(json!({ "ok": true, "runs": runs }).to_string())
}

/// GET /debug/run/evidence?id=&index=：取某次运行的证据；`index` 可选，用于只取某一步。
pub(super) fn debug_run_evidence_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let id = run_id_query(query)?;
    let index = query_value(query, "index").and_then(|value| value.parse::<usize>().ok());
    let evidence = debug_service(state)?
        .evidence(&id, index)
        .map_err(command_error)?;
    Ok(json!({ "ok": true, "evidence": evidence }).to_string())
}

/// GET /debug/run/replay?id=：以源运行的夹具与断言重放该任务。
pub(super) fn debug_run_replay_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let id = run_id_query(query)?;
    let run = debug_service(state)?.replay(&id).map_err(command_error)?;
    // `sourceRunId` 是重放的来源，调用方靠它把新运行与源运行对上。
    Ok(json!({ "ok": true, "sourceRunId": id, "run": run }).to_string())
}

/// GET /debug/run/step?runId=：单步推进一次运行。
pub(super) fn debug_run_step_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let id = run_id_query(query)?;
    let run = debug_service(state)?
        .advance(&id, false)
        .map_err(command_error)?;
    Ok(json!({ "ok": true, "run": run }).to_string())
}

/// GET /debug/run/all?runId=：推进到该运行结束。
pub(super) fn debug_run_all_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let id = run_id_query(query)?;
    let run = debug_service(state)?
        .advance(&id, true)
        .map_err(command_error)?;
    Ok(json!({ "ok": true, "run": run }).to_string())
}

/// GET /debug/run/status?id=：运行的聚合状态快照。
pub(super) fn debug_run_status_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let id = run_id_query(query)?;
    // 快照本身就已经是 `{ run, task, ... }` 的形状，**不要再包一层**，
    // 否则调用方按 `status.run` 取值只会拿到 null。
    let snapshot = debug_service(state)?
        .status_snapshot(&id)
        .map_err(command_error)?;
    Ok(snapshot.to_string())
}

/// GET /debug/run/cancel?id=：取消运行。
pub(super) fn debug_run_cancel_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let id = run_id_query(query)?;
    let run = debug_service(state)?.cancel(&id).map_err(command_error)?;
    Ok(json!({ "ok": true, "run": run }).to_string())
}

/// 全部原子命令名。应用与外部客户端共用同一套 `AtomicCommand`，
/// 这里是**清单**（只读能力枚举），不是可执行调用；新增原子命令时同步补一行。
const ATOMIC_COMMAND_NAMES: &[&str] = &[
    "capture",
    "ensure_window",
    "ensure_foreground",
    "ensure_ready",
    "activate",
    "focus",
    "input_text",
    "close_window",
    "press_key",
    "key_down",
    "key_up",
    "mouse_down",
    "mouse_up",
    "click",
    "scroll",
    "drag",
    "paste",
    "playback_queue_clear",
    "playback_queue_remove_front",
    "playback_play",
    "playback_pause",
    "playback_skip_next",
    "playback_skip_previous",
    "playback_set_volume",
    "request_song",
    "chat_send",
    "startup_task",
    "task_queue_cancel",
    "wait",
];

/// GET /debug/state：调试层只读状态：开关、是否允许游戏内输入、全部原子命令名。
pub(super) fn debug_state_route(
    _: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    Ok(json!({
        "ok": true,
        "enabled": debug_port(state)?.enabled(),
        // 调试模式开启时会拦下游戏内输入：这里由**端口自身的开关**派生，
        // 不读进程级全局（全局会被并行测试互相污染，得出的值不可复现）。
        "gameInputAllowed": !debug_port(state)?.enabled(),
        "commands": ATOMIC_COMMAND_NAMES,
    })
    .to_string())
}

/// GET /debug/mode[?mode=enabled|disabled]：读回或切换调试模式开关。
/// 不传参数时**只读**，不会改变任何状态。
pub(super) fn debug_mode_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> Result<String, AppError> {
    let port = debug_port(state)?;
    let requested = query_value(query, "enabled").or_else(|| query_value(query, "mode"));
    if let Some(raw) = requested {
        let enabled = matches!(
            raw.to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "enabled" | "open"
        );
        port.set_enabled(enabled).map_err(command_error)?;
    }
    Ok(json!({ "ok": true, "enabled": port.enabled() }).to_string())
}

/// POST /debug/tasks：保存（或覆盖）一个沙盒任务。
pub(super) fn debug_task_save_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let task = parse_json_body::<crate::debug_sandbox::SandboxTask>(body, "沙盒任务")?;
    let id = debug_service(state)?.save(task).map_err(command_error)?;
    Ok(json!({ "ok": true, "taskId": id }).to_string())
}

/// POST /debug/run：按请求启动一次运行。
pub(super) fn debug_run_start_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request =
        parse_json_body::<crate::interfaces::http::HttpDebugRunRequest>(body, "运行请求")?;
    let run = debug_service(state)?
        .start(request)
        .map_err(command_error)?;
    Ok(json!({ "ok": true, "run": run }).to_string())
}

/// POST /debug/command：直接执行一条原子命令（不建任务、不建运行记录）。
pub(super) fn debug_command_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let command = parse_json_body::<HttpDebugCommand>(body, "原子命令")?;
    let run = debug_service(state)?
        .execute_direct(command.into_atomic())
        .map_err(command_error)?;
    // `captured` 表示这条命令是否真的抓了帧（由结果里的 frameSequence 判定）；
    // 宽高取当前缓存帧。没有帧时如实报 0×0，不用 0 冒充"抓到过"。
    let captured = run
        .results
        .iter()
        .any(|result| result.frame_sequence.is_some());
    let image = state
        .latest_frame
        .lock()
        .ok()
        .and_then(|cache| cache.image());
    let (width, height) = image.map_or((0, 0), |image| (image.width(), image.height()));
    Ok(json!({
        "ok": true,
        "captured": captured,
        "width": width,
        "height": height,
    })
    .to_string())
}

/// GET /debug/screenshot[?quality=]：从调试端口**实时抓帧**并编码为 JPEG。
/// 与 `/screenshot`（用主扫描缓存帧）区分：这里取的是调试设备的当前画面。
pub(super) fn debug_screenshot_response(
    request: &Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    let quality = parse_jpeg_quality(query_value(&request.query, "quality"))?;
    let image = debug_port(state)?.capture().map_err(command_error)?;
    encoded_screenshot_response(
        request,
        quality,
        &image,
        &state.config.http.host,
        state.config.http.port,
    )
}

/// GET /debug/events[?drain=true][&since=N]：调试事件的 **SSE 流**。
///
/// - `since=N`：只发序号大于 N 的缓存事件（断线续传；同时作为 SSE 的 `id`）。
/// - `drain=true`：把缓存事件一次性写完就结束响应。
///   这是给脚本与测试用的——否则读缓冲的客户端会一直等下去。
/// - 不带 `drain`：先补发缓存，再转入实时广播。
pub(super) fn debug_events_response(
    request: &Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    use tokio_stream::StreamExt as _;

    let drain = matches!(
        query_value(&request.query, "drain"),
        Some("true") | Some("1")
    );
    let since = query_value(&request.query, "since")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .max(
            request
                .headers
                .get("last-event-id")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        );
    let bus = if request.path == "/debug/read-only/events" {
        let _ = debug_service(state)?;
        crate::debug_read_only::observer().bus()
    } else {
        crate::debug_events::bus()
    };
    // Always obtain the retained prefix and live subscription in a single atomic join.
    let (retained, receiver, latest) = bus.subscribe_replay_snapshot(0);
    let instance = (request.path == "/debug/read-only/events")
        .then(|| crate::debug_read_only::observer().instance().to_string());
    let reset = since > latest
        || instance.as_deref().is_some_and(|current| {
            query_value(&request.query, "instance").is_some_and(|requested| requested != current)
        });
    let cursor = if reset { 0 } else { since };
    let replay = retained
        .into_iter()
        .filter(|event| event.sequence > cursor)
        .collect::<Vec<_>>();
    let gap = reset
        || replay
            .first()
            .is_some_and(|event| event.sequence > cursor.saturating_add(1));
    let mut prefix = if drain {
        String::new()
    } else {
        instance
            .as_ref()
            .map(|value| format!(": instance {value}\n\n"))
            .unwrap_or_default()
    };
    if gap {
        prefix.push_str(&format!("event: readonly.gap\ndata: {}\n\n",json!({"kind":"gap","instance":instance,"reset":reset,
            "since":since,"nextCursor":cursor,"latestSequence":latest,"oldestSequence":replay.first().map(|e|e.sequence),
            "reason":if reset {"instance_or_cursor_reset"} else {"replay_retention_gap"}})));
    }

    let mut headers = cors_headers(request, &state.config.http.host, state.config.http.port);
    // SSE 必须禁缓存，否则会被中间层攒成批量轮询。
    headers.push(("cache-control".into(), "no-store".into()));

    if drain {
        let mut body = prefix;
        for event in replay {
            body.push_str(&sse_frame(&event));
        }
        return Ok(body_response(
            StatusCode::OK,
            "text/event-stream",
            body,
            headers,
        ));
    }

    let mut last_sequence = replay.last().map_or(cursor, |event| event.sequence);
    let replay_stream = tokio_stream::iter(
        std::iter::once(Ok::<String, std::convert::Infallible>(prefix))
            .chain(replay.into_iter().map(|event| Ok(sse_frame(&event)))),
    );
    let live = tokio_stream::wrappers::BroadcastStream::new(receiver).map(move |item| {
        let frame=match item {
            Ok(event)=>{last_sequence=event.sequence;sse_frame(&event)},
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(count))=>
                format!("event: readonly.gap\ndata: {}\n\n",json!({"kind":"gap","instance":instance,
                    "reason":"subscriber_lagged","missing":count,"nextCursor":last_sequence,"since":last_sequence})),
        };
        Ok::<String,std::convert::Infallible>(frame)
    });
    // Comments keep an idle connection alive; they are not workflow data or producer events.
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let heartbeat = tokio_stream::wrappers::IntervalStream::new(heartbeat)
        .map(|_| Ok::<String, std::convert::Infallible>(": heartbeat\n\n".to_string()));
    let stream = replay_stream.chain(live.merge(heartbeat));
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream));
    Ok(add_headers(response, headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, GrayImage, Luma};

    /// 同一幅画面必须给同一个指纹，否则它就不能把测量绑定到具体一帧。
    #[test]
    fn frame_fingerprint_is_stable_for_the_same_pixels() {
        let image = gray_image(&[[10, 20], [30, 40]]);
        assert_eq!(frame_fingerprint(&image), frame_fingerprint(&image));
    }

    /// 换一帧必须换指纹：屏幕在动时，"同一帧"这个前提不能靠假设。
    #[test]
    fn frame_fingerprint_changes_when_one_pixel_changes() {
        let a = gray_image(&[[10, 20], [30, 40]]);
        let b = gray_image(&[[10, 20], [30, 41]]);
        assert_ne!(frame_fingerprint(&a), frame_fingerprint(&b));
    }

    /// 指纹是 16 位小写十六进制：长度固定，方便人眼比对与写进文档。
    #[test]
    fn frame_fingerprint_is_sixteen_lowercase_hex_chars() {
        let fingerprint = frame_fingerprint(&gray_image(&[[1, 2], [3, 4]]));
        assert_eq!(fingerprint.len(), 16);
        assert!(
            fingerprint
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
        );
    }

    /// 纯色区域的标准差是 0 —— 这正是"平坦背景误配"的量化特征。
    #[test]
    fn luma_std_is_zero_for_a_flat_region() {
        let flat = gray_image(&[[7, 7], [7, 7]]);
        assert_eq!(luma_std(&flat.to_luma8()), 0.0);
    }

    /// 有内容的区域标准差大于 0。
    #[test]
    fn luma_std_is_positive_for_textured_region() {
        let textured = gray_image(&[[0, 255], [0, 255]]);
        assert!(luma_std(&textured.to_luma8()) > 0.0);
    }

    /// 自己与自己必然完全相关：这是"相关系数"能当判据的前提。
    #[test]
    fn pearson_luma_is_one_for_identical_images() {
        let luma = gray_image(&[[0, 120], [200, 255]]).to_luma8();
        let correlation = pearson_luma(&luma, &luma).expect("同图应有相关系数");
        assert!((correlation - 1.0).abs() < 1e-9);
    }

    /// 平坦区域没有方差，相关系数无从定义，必须返回 None 而不是 0 或 1。
    #[test]
    fn pearson_luma_is_none_when_a_side_has_no_variance() {
        let flat = gray_image(&[[5, 5], [5, 5]]).to_luma8();
        let textured = gray_image(&[[0, 200], [10, 250]]).to_luma8();
        assert!(pearson_luma(&flat, &textured).is_none());
        assert!(pearson_luma(&textured, &flat).is_none());
    }

    /// 造一幅灰度图。测试里只用 2x2，够验证统计量的定义。
    fn gray_image(rows: &[[u8; 2]; 2]) -> DynamicImage {
        let mut image = GrayImage::new(2, 2);
        for (y, row) in rows.iter().enumerate() {
            for (x, value) in row.iter().enumerate() {
                image.put_pixel(x as u32, y as u32, Luma([*value]));
            }
        }
        DynamicImage::ImageLuma8(image)
    }
}
