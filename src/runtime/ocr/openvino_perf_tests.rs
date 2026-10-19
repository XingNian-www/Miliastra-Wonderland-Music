//! Equivalence and native-resource checks for the OpenVINO hot path.
use super::*;

#[test]
fn persistent_engine_remains_send_for_the_ocr_worker() {
    fn assert_send<T: Send>() {}
    assert_send::<OpenVinoEngine>();
}

// Frozen pre-optimization decoder: an oracle for text AND confidence semantics.
fn legacy_decode(
    data: &[f32],
    shape: &[usize],
    charset: &[char],
    valid: Option<usize>,
) -> Result<(String, f32)> {
    let (full, classes) = ctc_output_dimensions(shape)?;
    let steps = valid.unwrap_or(full).min(full);
    if classes == 0 || steps == 0 {
        return Ok((String::new(), 0.0));
    }
    if classes != charset.len() {
        bail!("class mismatch");
    }
    let required = full
        .checked_mul(classes)
        .ok_or_else(|| anyhow!("overflow"))?;
    if data.len() < required {
        bail!("short output");
    }
    let mut previous = 0;
    let mut characters = Vec::new();
    for step in 0..steps {
        let row = &data[step * classes..(step + 1) * classes];
        let (index, value) = row
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .ok_or_else(|| anyhow!("empty"))?;
        let score = if row.iter().all(|x| *x >= 0.0 && *x <= 1.0)
            && (row.iter().sum::<f32>() - 1.0).abs() <= 0.05
        {
            *value
        } else {
            let maximum = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let denominator = row.iter().map(|x| (*x - maximum).exp()).sum::<f32>();
            (*value - maximum).exp() / denominator.max(f32::MIN_POSITIVE)
        };
        if index != 0
            && index != previous
            && index < charset.len()
            && score >= RECOGNITION_CHARACTER_THRESHOLD
        {
            characters.push((charset[index], score));
        }
        previous = index;
    }
    let confidence = if characters.is_empty() {
        0.0
    } else {
        characters.iter().map(|(_, score)| score).sum::<f32>() / characters.len() as f32
    };
    Ok((
        characters.into_iter().map(|(ch, _)| ch).collect(),
        confidence,
    ))
}

fn assert_equivalent(data: &[f32], classes: usize, valid: Option<usize>) {
    let charset = (0..classes)
        .map(|i| char::from_u32(0x4e00 + i as u32).unwrap())
        .collect::<Vec<_>>();
    let shape = [1, data.len() / classes, classes];
    let old = legacy_decode(data, &shape, &charset, valid).unwrap();
    let new = decode_ctc(data, &shape, &charset, valid).unwrap();
    assert_eq!(new.0, old.0, "shape={shape:?} valid={valid:?}");
    assert_eq!(
        new.1.to_bits(),
        old.1.to_bits(),
        "confidence changed: {new:?} vs {old:?}"
    );
}

#[test]
fn decoder_preserves_ties_thresholds_nonfinite_and_collapse_state() {
    let values = [
        0.0,
        -0.0,
        0.3,
        f32::from_bits(0.3f32.to_bits() - 1),
        f32::from_bits(0.3f32.to_bits() + 1),
        0.5,
        1.0,
        -1.0,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::from_bits(0x7fc00001),
        f32::from_bits(0xffc00001),
        f32::from_bits(0x7fc00002),
    ];
    for &a in &values {
        for &b in &values {
            for &c in &values {
                // Include an identical row, a blank separator, then the same token again.
                assert_equivalent(&[a, b, c, a, b, c, 1.0, 0.0, 0.0, a, b, c], 3, None);
            }
        }
    }
    for sum in [0.9499999, 0.95, 0.9500001, 1.0499999, 1.05, 1.0500001] {
        assert_equivalent(&[0.0, 0.3, sum - 0.3, 0.2, 0.4, 0.4], 3, None);
    }
    // Below-threshold winners still block the same index in the next row.
    assert_equivalent(&[0.24, 0.28, 0.24, 0.24, 0.01, 0.97, 0.01, 0.01], 4, None);
    let (text, _) = decode_ctc(
        &[0.24, 0.28, 0.24, 0.24, 0.01, 0.97, 0.01, 0.01],
        &[1, 2, 4],
        &[' ', '甲', '乙', '丙'],
        None,
    )
    .unwrap();
    assert!(
        text.is_empty(),
        "low-confidence winner must still collapse its repeat"
    );
    assert_equivalent(&[0.05, 0.9, 0.05, 0.05, 0.05, 0.9], 3, Some(1));
    assert_equivalent(&[0.05, 0.9, 0.05, 0.05, 0.05, 0.9], 3, Some(0));
}

#[test]
fn decoder_matches_legacy_for_mixed_probability_and_logit_sequences() {
    let mut state = 0x10_19_2026_u64;
    for classes in [3, 17, 97] {
        for _case in 0..100 {
            let mut data = Vec::with_capacity(96 * classes);
            for step in 0..96 {
                let start = data.len();
                for _ in 0..classes {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    data.push(((state >> 40) as f32 / (1u32 << 24) as f32) * 16.0 - 8.0);
                }
                if step % 2 == 0 {
                    let maximum = data[start..]
                        .iter()
                        .copied()
                        .fold(f32::NEG_INFINITY, f32::max);
                    for value in &mut data[start..] {
                        *value = (*value - maximum).exp();
                    }
                    let sum = data[start..].iter().sum::<f32>();
                    for value in &mut data[start..] {
                        *value /= sum;
                    }
                }
            }
            assert_equivalent(&data, classes, None);
            assert_equivalent(&data, classes, Some(37));
        }
    }
}

// Native IR test: run explicitly, never silently count missing runtime as a pass.
#[test]
#[ignore = "requires native OpenVINO runtime"]
fn oversized_successful_input_releases_the_request_binding() -> Result<()> {
    // A tiny reduction model exercises a >16 MiB input without the recognizer's
    // enormous class output. Keep dimensions because OpenVINO's C shape getter
    // rejects rank-0 scalar outputs. Keep fixture files for failure diagnosis.
    let root = std::env::temp_dir().join(format!("ocr-input-budget-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let xml = root.join("reduce.xml");
    let bin = root.join("reduce.bin");
    std::fs::write(
        &xml,
        r#"<net name="input_budget" version="11"><layers>
<layer id="0" name="input" type="Parameter" version="opset1"><data shape="1,3,48,-1" element_type="f32"/><output><port id="0" precision="FP32"><dim>1</dim><dim>3</dim><dim>48</dim><dim>-1</dim></port></output></layer>
<layer id="1" name="axes" type="Const" version="opset1"><data element_type="i64" shape="4" offset="0" size="32"/><output><port id="0" precision="I64"><dim>4</dim></port></output></layer>
<layer id="2" name="sum" type="ReduceSum" version="opset1"><data keep_dims="true"/><input><port id="0" precision="FP32"><dim>1</dim><dim>3</dim><dim>48</dim><dim>-1</dim></port><port id="1" precision="I64"><dim>4</dim></port></input><output><port id="2" precision="FP32"><dim>1</dim><dim>1</dim><dim>1</dim><dim>1</dim></port></output></layer>
<layer id="3" name="result" type="Result" version="opset1"><input><port id="0" precision="FP32"><dim>1</dim><dim>1</dim><dim>1</dim><dim>1</dim></port></input></layer>
</layers><edges><edge from-layer="0" from-port="0" to-layer="2" to-port="0"/><edge from-layer="1" from-port="0" to-layer="2" to-port="1"/><edge from-layer="2" from-port="2" to-layer="3" to-port="0"/></edges></net>"#,
    )?;
    std::fs::write(
        &bin,
        [0_i64, 1, 2, 3]
            .into_iter()
            .flat_map(i64::to_le_bytes)
            .collect::<Vec<_>>(),
    )?;
    prepare_local_runtime_loading();
    let mut core = Core::new()?;
    let ir = core.read_model_from_file(path_string(&xml)?, path_string(&bin)?)?;
    let mut model = OpenVinoModel::compile(&mut core, &ir, DeviceType::CPU, "input_budget")?;
    for width in [96, INPUT_TENSOR_CACHE_BYTES / (3 * 48 * 4) + 1, 96] {
        let data = vec![0.0; 3 * 48 * width];
        model.run(&data, &[1, 3, 48, width], |values, shape| {
            assert_eq!(shape, &[1, 1, 1, 1]);
            assert_eq!(values, &[0.0]);
            Ok(())
        })?;
        let oversized = data.len() * std::mem::size_of::<f32>() > INPUT_TENSOR_CACHE_BYTES;
        assert_eq!(model.request.is_none(), oversized);
        assert!(
            model
                .inputs
                .iter()
                .all(|input| input.bytes <= INPUT_TENSOR_CACHE_BYTES)
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires OPENVINO_OCR_IR_ROOT and native OpenVINO runtime"]
fn persistent_request_matches_fresh_outputs_across_shapes_and_recovery() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("OPENVINO_OCR_IR_ROOT").context("OPENVINO_OCR_IR_ROOT required")?,
    );
    prepare_local_runtime_loading();
    let mut core = Core::new()?;
    let ir = core.read_model_from_file(
        path_string(&root.join("PP-OCRv6_small_rec.xml"))?,
        path_string(&root.join("PP-OCRv6_small_rec.bin"))?,
    )?;
    let mut model = OpenVinoModel::compile(&mut core, &ir, DeviceType::CPU, "recognizer")?;
    // Real screenshot pixels; alternate shapes and different values, then revisit each.
    let image = image::load_from_memory(include_bytes!("fixtures/chat-title.png"))?;
    let mut first_pointer = None;
    for width in [96, 768, 128, 96, 1024, 128, 768] {
        let crop = image.resize_exact(width, 48, image::imageops::FilterType::Lanczos3);
        let input = preprocess_for_rec(&crop, 48, None)?;
        let got = model.run(&input.data, &input.shape, |data, shape| {
            Ok((data.to_vec(), shape.to_vec()))
        })?;
        let dimensions = input.shape.iter().map(|&x| x as i64).collect::<Vec<_>>();
        let mut tensor = Tensor::new(ElementType::F32, &Shape::new(&dimensions)?)?;
        tensor.get_data_mut::<f32>()?.copy_from_slice(&input.data);
        let mut fresh = model.compiled.create_infer_request()?;
        fresh.set_input_tensor(&tensor)?;
        fresh.infer()?;
        let expected = fresh.get_output_tensor_by_index(0)?;
        assert_eq!(got.0, expected.get_data::<f32>()?);
        let shape = expected.get_shape()?;
        assert_eq!(
            got.1,
            shape
                .get_dimensions()
                .iter()
                .map(|&x| x as usize)
                .collect::<Vec<_>>()
        );
        if width == 96 {
            let ptr = model
                .inputs
                .iter()
                .find(|entry| entry.shape == input.shape)
                .unwrap()
                .tensor
                .get_data::<f32>()?
                .as_ptr();
            if let Some(first) = first_pointer {
                assert_eq!(first, ptr, "same-shape Tensor must be reused");
            }
            first_pointer = Some(ptr);
        }
    }
    let input = preprocess_for_rec(&image, 48, None)?;
    assert!(
        model
            .run(&input.data[..1], &input.shape, |_, _| Ok(()))
            .is_err()
    );
    assert!(model.request.is_none());
    model.run(&input.data, &input.shape, |data, _| {
        assert!(!data.is_empty());
        Ok(())
    })?;
    assert!(model.request.is_some());
    // Resource limits hold even when diagnostic requests introduce extra widths.
    for width in (32..=320).step_by(32) {
        let shape = [1, 3, 48, width];
        let input = model.take_input(&shape)?;
        model.return_input(input);
    }
    assert!(model.inputs.len() <= INPUT_TENSOR_CACHE_ENTRIES);
    assert!(
        model.inputs.iter().map(|entry| entry.bytes).sum::<usize>() <= INPUT_TENSOR_CACHE_BYTES
    );
    Ok(())
}
