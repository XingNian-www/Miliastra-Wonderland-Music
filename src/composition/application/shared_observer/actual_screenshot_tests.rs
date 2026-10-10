//! Opt-in replay of exact, real screenshots. No game device, input, or business dispatch.
use super::*;
use crate::features::command::{CommandEnvelope, CommandObservation, CommandPrefix};
use crate::interfaces::chat::{parse_command_envelope, parse_structured_song_envelope};
use crate::runtime::ocr::{OcrDevice, OcrLine};
use crate::runtime::ui::{CaptureFrame, UiDevice};
use crate::ui::state::{TemplateUiStateClassifier, UiTemplateArgs};
use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    output_dir: PathBuf,
    frames: Vec<FrameExpectation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrameExpectation {
    path: PathBuf,
    expected_layout: String,
    expected_contains: Option<String>,
    expected_sender: Option<String>,
    expected_route_contains: Option<String>,
    expected_identity: Option<String>,
    expected_message_type: Option<String>,
}

struct ImageOnlyDevice(DynamicImage);
impl UiDevice for ImageOnlyDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        Ok(self.0.clone())
    }
}

// Record the exact calls made by production helpers, rather than inferencing crops again.
// These are raw OcrDevice outputs (after engine filtering/spacing), not pre-normalization logits.
struct RecordingMnn {
    inner: ProductionOcrDevice,
    calls: Arc<Mutex<Vec<Value>>>,
    input_directory: Arc<Mutex<PathBuf>>,
}
impl OcrDevice for RecordingMnn {
    fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
        let index = self
            .calls
            .lock()
            .map_err(|_| anyhow!("OCR evidence lock poisoned"))?
            .len();
        let input_path = self
            .input_directory
            .lock()
            .map_err(|_| anyhow!("OCR evidence directory lock poisoned"))?
            .join(format!("ocr-input-{index:02}.png"));
        image.save(&input_path)?;
        let result = self.inner.recognize_lines(image);
        let mut record = match &result {
            Ok(lines) => json!({"width": image.width(), "height": image.height(),
                "inputPixelsSha256": sha(image.as_bytes()), "lines": lines}),
            Err(error) => json!({"width": image.width(), "height": image.height(),
                "inputPixelsSha256": sha(image.as_bytes()), "error": format!("{error:#}")}),
        };
        record["inputCrop"] = json!(input_path);
        record["bboxCoordinates"] = json!("local to this exact OCR input crop");
        self.calls
            .lock()
            .map_err(|_| anyhow!("OCR evidence lock poisoned"))?
            .push(record);
        result
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn rooted(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }
}

fn save_crop(image: &DynamicImage, rect: Rect, path: &Path) -> Result<Value> {
    let crop = crop_canvas(image, rect)?;
    crop.save(path)?;
    Ok(
        json!({"path": path, "rect": rect, "pixelsSha256": sha(crop.as_bytes()),
        "coordinateSpace": "1920x1080 source canvas"}),
    )
}

fn observation(message: &SharedChatInput, frame: &Frame) -> CommandObservation {
    CommandObservation {
        captured_at: Some(frame.captured_at),
        message_text: Some(message.text.clone()),
        message_y: Some(message.block.y),
        ..Default::default()
    }
}

fn route_diagnostic(message: &SharedChatInput, frame: &Frame, primary: bool) -> Value {
    let router = ChatCommandRouter::without_custom_workflow();
    let obs = observation(message, frame);
    let mut attempts = Vec::new();
    let mut chosen = None;
    let mut try_envelope = |stage: &str, envelope: Option<CommandEnvelope>| {
        let routed = envelope
            .as_ref()
            .and_then(|envelope| router.route(envelope, None));
        attempts.push(
            json!({"stage": stage, "envelope": envelope.as_ref().map(|e| json!({
            "userCommand": e.user_command(), "commandText": e.command_text(),
            "username": e.username(), "messageType": e.message_type()})),
            "route": routed.as_ref().map(|r| json!({"matched": r.matched, "raw": r.raw,
                "userCommand": r.user_command, "username": r.username,
                "command": format!("{:?}", r.command)}))}),
        );
        if routed.is_some() {
            chosen = routed;
            true
        } else {
            false
        }
    };
    let sender = message
        .sender
        .as_deref()
        .unwrap_or(SECONDARY_HALL_FALLBACK_SENDER);
    if primary {
        try_envelope(
            "primary-parse-command-envelope",
            parse_command_envelope(&message.text, &message.message_type, obs),
        );
    } else if !try_envelope(
        "secondary-structured-song",
        parse_structured_song_envelope(&message.text, sender, &message.message_type, obs.clone()),
    ) {
        let hash = CommandEnvelope::new(
            &message.text,
            sender,
            &message.message_type,
            message.text.trim(),
            obs.clone(),
        )
        .filter(|e| e.prefix() == CommandPrefix::Hash);
        if !try_envelope("secondary-hash", hash) {
            let at = message.text.find(['@', '＠']).and_then(|index| {
                CommandEnvelope::new(
                    &message.text,
                    sender,
                    &message.message_type,
                    message.text[index..].trim(),
                    obs,
                )
            });
            try_envelope("secondary-at", at);
        }
    }
    // The final candidate may be embedded in a bot's feedback. This harness does not apply
    // self-nickname filtering, identity permissions, active entertainment, or shared queues.
    json!({"scope": "syntax-only; no identity/custom-workflow/business dispatch",
        "activeEntertainment": Value::Null, "senderConfirmed": message.sender.is_some(),
        "embeddedAtCandidate": message.text.find(['@', '＠']).map(|i| &message.text[i..]),
        "selfFeedbackFiltered": false, "attempts": attempts,
        "selected": chosen.map(|r| json!({"matched": r.matched, "raw": r.raw,
            "userCommand": r.user_command, "command": format!("{:?}", r.command)}))})
}

fn capture_fixture_frame(image: DynamicImage, config: &AppConfig) -> Result<(Frame, u64)> {
    let classifier = TemplateUiStateClassifier::new(
        UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
        config.screen.clone(),
    );
    let ui = UiRuntime::start_with_state_classifier(
        ImageOnlyDevice(image),
        8,
        classifier,
        config.resolve_stability_count(config.stability.ui_state_count),
    )?;
    let capture = ui.handle().submit(CaptureFrame)?.wait()?;
    // Always stop the image-only worker, including when CaptureFrame returns an error.
    ui.shutdown()?;
    let capture = capture?;
    let frame = from_captured_frame(
        &capture,
        &Canvas {
            width: config.screen.expected_width,
            height: config.screen.expected_height,
            resize: true,
        },
    );
    Ok((frame, capture.id()))
}

#[test]
fn real_menus_and_known_secondary_panels_are_not_passive_chat() -> Result<()> {
    let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml"))?;
    let templates = TemplateArgs::default().resolve(&config.templates, &config.ocr);
    for (path, expected_kind, expected_evidence) in [
        (
            "technical-docs/ui-state-evidence/wonderland-menu.png",
            UiStateKind::MainMenu,
            "main_menu_template",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxchat-open.jpg",
            UiStateKind::MainMenu,
            "main_menu_template",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxchat-from-hall.jpg",
            UiStateKind::Secondary,
            "secondary:qianxing_panel",
        ),
        (
            "tests/fixtures/ui/menu-paimon-1920x1080.jpg",
            UiStateKind::MainMenu,
            "paimon_menu_panel_template",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/r10-now.jpg",
            UiStateKind::Secondary,
            "secondary:qianxing_panel",
        ),
        (
            "technical-docs/ui-state-evidence/qianxing-ui-59/qxmap-a.jpg",
            UiStateKind::Secondary,
            "secondary:qianxing_map",
        ),
    ] {
        let (frame, _) = capture_fixture_frame(image::open(path)?, &config)?;
        let Some(crate::runtime::ui::UiStateObservation::Classified(state)) =
            frame.ui_state.as_ref()
        else {
            panic!("missing classifier evidence: {path}");
        };
        assert_eq!(state.classification().kind(), expected_kind, "{path}");
        let observed = if expected_kind == UiStateKind::MainMenu {
            state.classification().evidence().final_rule()
        } else {
            state.classification().label()
        };
        assert_eq!(observed, expected_evidence, "{path}");
        assert!(
            matches!(
                passive_layout(&frame, &config, &templates)?,
                PassiveLayout::NoChat
            ),
            "{path}"
        );
    }
    Ok(())
}

#[test]
fn real_scrolled_friend_selection_is_above_active_search_region() -> Result<()> {
    let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml"))?;
    let templates = TemplateArgs::default().resolve(&config.templates, &config.ocr);
    let (frame, _) = capture_fixture_frame(
        image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")?,
        &config,
    )?;
    let PassiveLayout::Secondary {
        hall,
        selected: Some(row),
        bubbles,
        ..
    } = passive_layout(&frame, &config, &templates)?
    else {
        panic!("real friend frame must provide a selected row");
    };
    assert!(!hall);
    assert!(row.y < config.invite.friend_list_region.y);
    assert!((row.y..row.bottom()).contains(&260));
    assert!(!bubbles.is_empty());
    Ok(())
}

fn replay_frame(
    spec: &FrameExpectation,
    root: &Path,
    directory: &Path,
    config: &AppConfig,
    templates: &ResolvedTemplateArgs,
    ocr: &OcrRuntimeHandle,
    report: &mut Value,
) -> Result<()> {
    fs::create_dir_all(directory)?;
    let source = rooted(root, &spec.path);
    let bytes = fs::read(&source).with_context(|| format!("read {}", source.display()))?;
    let extension = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("image");
    let original = directory.join(format!("original.{extension}"));
    fs::write(&original, &bytes)?;
    report["source"] = json!({"path": source, "originalCopy": original,
        "fileSha256": sha(&bytes), "sourceType": "manifest-provided real screenshot"});
    let image = image::open(&original)?;
    report["source"]["width"] = json!(image.width());
    report["source"]["height"] = json!(image.height());
    report["source"]["pixelsSha256"] = json!(sha(image.as_bytes()));
    ensure!(
        (image.width(), image.height()) == (1920, 1080),
        "real screenshot must be exactly 1920x1080, got {}x{}",
        image.width(),
        image.height()
    );
    let (frame, capture_id) = capture_fixture_frame(image, config)?;
    report["frame"] = json!({"frameId": capture_id,
        "capturedAtMeaning": "offline replay capture time, not original game screenshot time",
        "pixelsSha256": sha(frame.image.as_bytes()),
        "uiState": format!("{:?}", frame.ui_state)});
    let layout = passive_layout(&frame, config, templates)?;
    let mut title = String::new();
    let mut regions: Vec<(Rect, Option<Rect>)> = Vec::new();
    let (layout_name, identity, messages) = match layout {
        PassiveLayout::Primary(_) => {
            let prepared =
                prepare_chat_scan(&frame.image, templates, config.screen.chat_rect.into())?;
            let messages = recognize_prepared_chat(
                ocr,
                OcrPriority::ChatObservation,
                templates,
                prepared,
                None,
            )?;
            let messages = messages
                .into_iter()
                .map(|m| {
                    // Production message.block remains chat-local; evidence crops are canvas-local.
                    regions.push((
                        Rect::new(
                            m.block.x + config.screen.chat_rect.x,
                            m.block.y + config.screen.chat_rect.y,
                            m.block.width,
                            m.block.height,
                        ),
                        None,
                    ));
                    SharedChatInput {
                        text: m.text,
                        sender: None,
                        raw_sender: None,
                        message_type: m.message_type,
                        block: m.block,
                    }
                })
                .collect::<Vec<_>>();
            ("primary", ChatIdentity::PrimaryHall, messages)
        }
        PassiveLayout::Secondary {
            hall,
            selected,
            bubbles,
            latest,
        } => {
            report["titleCrop"] = save_crop(
                &frame.image,
                PASSIVE_SECONDARY_TITLE,
                &directory.join("title.png"),
            )?;
            title = passive_text(
                ocr,
                &frame.image,
                PASSIVE_SECONDARY_TITLE,
                config.ocr.same_line_y_tolerance,
            )?;
            report["selectedRowCrop"] = selected
                .map(|row| save_crop(&frame.image, row, &directory.join("selected-row.png")))
                .transpose()?
                .unwrap_or(Value::Null);
            report["titleRole"] = json!("diagnostic only; never used for identity or sender");
            let (identity, messages) = recognize_passive_secondary(
                ocr,
                &frame.image,
                config,
                hall,
                selected,
                &bubbles,
                latest,
            )?;
            if !bubbles.is_empty() {
                regions = bubbles
                    .iter()
                    .map(|b| (b.rect, Some(b.sender_rect())))
                    .collect();
            } else if let Some(rect) = latest {
                if !messages.is_empty() {
                    regions.push((rect, selected));
                }
            }
            let name = match &identity {
                ChatIdentity::SecondaryHall => "secondary-hall",
                ChatIdentity::Friend(_) => "secondary-friend",
                _ => "secondary-unknown",
            };
            (name, identity, messages)
        }
        PassiveLayout::NoChat => ("no-chat", ChatIdentity::Unknown, Vec::new()),
    };
    report["layout"] = json!(layout_name);
    report["chatIdentity"] = json!(format!("{identity:?}"));
    report["title"] = json!({"rawOcr": title,
        "comparisonNormalized": crate::text::normalize_chat_text(&title)});
    let mut message_reports = Vec::new();
    for (index, (message, (body, sender))) in messages.iter().zip(regions).enumerate() {
        let body_crop = save_crop(
            &frame.image,
            body,
            &directory.join(format!("message-{index:02}-body.png")),
        )?;
        let sender_crop = sender
            .map(|rect| {
                save_crop(
                    &frame.image,
                    rect,
                    &directory.join(format!("message-{index:02}-sender.png")),
                )
            })
            .transpose()?;
        message_reports.push(json!({"rawOcr": message.text,
            "comparisonNormalized": crate::text::normalize_chat_text(&message.text),
            "sender": message.sender, "rawSenderOcr": message.raw_sender,
            "senderComparisonNormalized": message.raw_sender.as_deref().map(crate::text::normalize_chat_text),
            "messageType": message.message_type, "productionBlock": message.block,
            "productionBlockCoordinates": if layout_name == "primary" { "chat-local" } else { "canvas" },
            "bodyCrop": body_crop, "senderCrop": sender_crop,
            "routeDiagnostic": route_diagnostic(message, &frame, layout_name == "primary")}));
    }
    report["messages"] = json!(message_reports);
    let contains = |actual: &str, expected: &str| {
        crate::text::normalize_chat_text(actual)
            .contains(&crate::text::normalize_chat_text(expected))
    };
    let mut failures = Vec::new();
    if layout_name != spec.expected_layout {
        failures.push(format!(
            "layout expected {:?}, observed {layout_name:?}",
            spec.expected_layout
        ));
    }
    if spec
        .expected_identity
        .as_deref()
        .is_some_and(|expected| expected != format!("{identity:?}"))
    {
        failures.push(format!(
            "identity expected {:?}, observed {identity:?}",
            spec.expected_identity
        ));
    }
    // Bind optional body/sender/route/type expectations to ONE message, not unrelated bubbles.
    let matches = message_reports.iter().any(|m| {
        spec.expected_message_type
            .as_deref()
            .is_none_or(|expected| m["messageType"] == expected)
            && spec
                .expected_contains
                .as_deref()
                .is_none_or(|e| contains(m["rawOcr"].as_str().unwrap_or(""), e))
            && spec
                .expected_sender
                .as_deref()
                .is_none_or(|e| contains(m["sender"].as_str().unwrap_or(""), e))
            && spec.expected_route_contains.as_deref().is_none_or(|e| {
                !m["routeDiagnostic"]["selected"].is_null()
                    && contains(&m["routeDiagnostic"]["selected"].to_string(), e)
            })
    });
    if (spec.expected_contains.is_some()
        || spec.expected_sender.is_some()
        || spec.expected_route_contains.is_some()
        || spec.expected_message_type.is_some())
        && !matches
    {
        failures.push("no single message satisfies body/sender/route expectations".to_owned());
    }
    report["expectationFailures"] = json!(failures);
    Ok(())
}

#[test]
#[ignore = "requires MILIASTRA_REAL_SCREENSHOT_MANIFEST and real screenshots plus CPU MNN assets"]
fn manifest_real_screenshots_follow_production_observation_and_syntax_route() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = rooted(
        root,
        &PathBuf::from(
            std::env::var("MILIASTRA_REAL_SCREENSHOT_MANIFEST")
                .context("set MILIASTRA_REAL_SCREENSHOT_MANIFEST to a JSON manifest")?,
        ),
    );
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    ensure!(
        !manifest.frames.is_empty(),
        "manifest frames must not be empty"
    );
    let output = rooted(root, &manifest.output_dir);
    fs::create_dir_all(&output)?;
    let mut report = json!({"schemaVersion": 1, "manifest": manifest_path,
        "scope": "exact source screenshots through production observation and syntax route only",
        "limitations": ["no game input or business dispatch", "no identity/self-nickname authorization",
            "no active entertainment/custom workflows", "no cross-frame baseline, dedup, locks or navigation acceptance",
            "embedded feedback commands remain separately reported syntax candidates"],
        "frames": [], "errors": []});
    let calls = Arc::new(Mutex::new(Vec::new()));
    let input_directory = Arc::new(Mutex::new(output.clone()));
    let setup = (|| -> Result<_> {
        let config =
            AppConfig::load_from_root(&root.join("tests/fixtures/config.full.yaml"), root)?;
        ensure!(
            config.ocr.backend_priority == ["cpu"]
                && config.ocr.min_confidence == 0.9
                && !config.ocr.batch_recognize,
            "fixture must retain CPU MNN, min_confidence=0.9, batch=false"
        );
        report["config"] = json!({"ocr": config.ocr, "screen": config.screen,
            "templates": config.templates, "fixture": "tests/fixtures/config.full.yaml"});
        let templates = TemplateArgs::default().resolve(&config.templates, &config.ocr);
        let device = RecordingMnn {
            inner: ProductionOcrDevice::new(OcrArgs::default().resolve(&config.ocr))?,
            calls: calls.clone(),
            input_directory: input_directory.clone(),
        };
        let runtime = OcrRuntime::start_configured(
            device,
            OcrRuntimeConfig {
                queue_capacity: 64,
                request_timeout: Duration::from_millis(config.ocr.request_timeout_ms),
                shutdown_timeout: Duration::from_millis(config.ocr.shutdown_timeout_ms),
            },
        )?;
        Ok((config, templates, runtime))
    })();
    let mut failures = Vec::new();
    match setup {
        Ok((config, templates, runtime)) => {
            for (index, spec) in manifest.frames.iter().enumerate() {
                calls
                    .lock()
                    .map_err(|_| anyhow!("OCR evidence lock poisoned"))?
                    .clear();
                let mut frame_report = json!({"index": index, "path": spec.path,
                    "expected": {"layout": spec.expected_layout, "contains": spec.expected_contains,
                        "sender": spec.expected_sender, "routeContains": spec.expected_route_contains,
                        "identity": spec.expected_identity, "messageType": spec.expected_message_type}});
                let directory = output.join(format!("frame-{index:03}"));
                *input_directory
                    .lock()
                    .map_err(|_| anyhow!("OCR evidence directory lock poisoned"))? =
                    directory.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    replay_frame(
                        spec,
                        root,
                        &directory,
                        &config,
                        &templates,
                        &runtime.handle(),
                        &mut frame_report,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow!(
                        "frame processing panicked; retained previous evidence"
                    ))
                });
                frame_report["rawOcrCalls"] = json!(
                    *calls
                        .lock()
                        .map_err(|_| anyhow!("OCR evidence lock poisoned"))?
                );
                if let Err(error) = result {
                    frame_report["error"] = json!(format!("{error:#}"));
                    failures.push(format!("frame {index}: {error:#}"));
                }
                if let Some(items) = frame_report["expectationFailures"].as_array() {
                    for failure in items {
                        failures.push(format!("frame {index}: {failure}"));
                    }
                }
                report["frames"]
                    .as_array_mut()
                    .expect("frames array")
                    .push(frame_report);
            }
            if let Err(error) = runtime.shutdown() {
                failures.push(format!("OCR shutdown: {error}"));
            }
        }
        Err(error) => failures.push(format!("initialization: {error:#}")),
    }
    report["errors"] = json!(failures);
    report["passed"] = json!(failures.is_empty());
    let results = output.join("results.json");
    fs::write(&results, serde_json::to_vec_pretty(&report)?)?;
    println!("actual screenshot evidence: {}", results.display());
    assert!(
        failures.is_empty(),
        "real screenshot expectations failed; see {}: {}",
        results.display(),
        failures.join("; ")
    );
    Ok(())
}
