# OpenVINO OCR 模型调研

更新时间：2026-07-24

## 结论

当前 OpenVINO 适配器应使用与仓库 MNN 模型同一套 PaddleOCR/PP-OCRv6 small 检测和识别模型，最合适的公开来源是 PaddlePaddle 官方 Hugging Face 仓库中的 ONNX 模型：

- 检测：[PaddlePaddle/PP-OCRv6_small_det_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx)
- 识别：[PaddlePaddle/PP-OCRv6_small_rec_onnx](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx)

这两个仓库各自提供 `inference.onnx` 和 `inference.yml`。仓库不是预生成的 OpenVINO IR，但可以直接用 OpenVINO 2026.2.1 的 `ov.convert_model()`/`ov.save_model()` 转成当前配置要求的一对 XML/BIN。转换后的 IR 应作为实验性 OpenVINO 后端模型，不替换默认 MNN 模型，直到对聊天截图做准确率和延迟回归。

## 为什么这对模型匹配当前适配器

`src/runtime/ocr/openvino.rs` 使用项目自己的预处理、DB 后处理和 CTC 解码实现，但为了读取 PP-OCRv6 IR 仍遵循以下模型契约。OpenVINO 运行时不依赖 Paddle/PaddleOCR 的 Python、Rust 类型或处理函数：

1. 检测输入为动态 `[N,3,H,W]`，第一个输出必须是单 batch/单 channel、空间尺寸与输入相同的 F32 DB 概率图；代码拒绝其他输出形状，避免把下采样或多 batch 输出误当 mask。
2. 识别输入为 `[N,3,48,W]`，第一个输出必须能解释为 `[N,T,C]`（或等价的二维/四维 CTC logits/probabilities）。
3. 识别类别数必须和 `models/ppocr_keys_v6_small.txt` 对齐。该字典当前有 18,708 行，Rust 加入首尾两个 blank 后为 18,710 类。

官方 PP-OCRv6 small ONNX 的图元信息（本地用 ONNX 1.22 检查）为：

| 模型 | 输入 | 输出 | 适配器结论 |
| --- | --- | --- | --- |
| `PP-OCRv6_small_det` | `x: [?,3,?,?]` | `fetch_name_0: [?,1,?,?]`，末两维是 DB mask | 兼容 OpenVINO 模块内的 DB `extract_boxes_with_unclip` |
| `PP-OCRv6_small_rec` | `x: [?,3,48,?]` | `fetch_name_0: [?, ?, 18710]`，Softmax CTC 分布 | 兼容当前 `decode_ctc` 和字典 |

用 OpenVINO 2026.2.1 对转换结果做了 CPU smoke test：检测 `1x3x96x320` 输出 `1x1x96x320`；识别 `1x3x48x192` 输出 `1x24x18710`。因此动态宽高不会触发当前 Rust 张量形状检查。上述形状是本地验证结果，不是对任意导出版本的保证；每次替换模型都应重新打印输入/输出节点。

PP-OCRv6 的官方配置也明确 small 检测是 DB 模型、small 识别使用 CTC 头；模型配置和下载入口见 [PP-OCRv6 small detection config](https://github.com/PaddlePaddle/PaddleOCR/blob/main/configs/det/PP-OCRv6/PP-OCRv6_small_det.yml) 与 [PP-OCRv6 small recognition config](https://github.com/PaddlePaddle/PaddleOCR/blob/main/configs/rec/PP-OCRv6/PP-OCRv6_small_rec.yml)。官方推理模型包入口为：

- [PP-OCRv6_small_det_infer.tar](https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/PP-OCRv6_small_det_infer.tar)
- [PP-OCRv6_small_rec_infer.tar](https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/PP-OCRv6_small_rec_infer.tar)

如果官方 ONNX 仓库不可用，也可以下载这两个 Paddle inference 包后转换；OpenVINO 官方 Paddle 转换文档说明 inference 模型由 `.pdmodel` 和 `.pdiparams` 组成，可用 `ovc` 或 `openvino.convert_model()` 转换：[OpenVINO Paddle conversion](https://github.com/openvinotoolkit/openvino/blob/master/docs/articles_en/openvino-workflow/model-preparation/convert-model-paddle.rst)。

## 已验证的 ONNX 到 IR 流程

OpenVINO 官方 ONNX 文档说明 ONNX 可以直接被 Runtime 读取，但转换为 IR 可降低模型加载开销；`ovc` 或 `ov.convert_model()` 都是支持的转换入口：[OpenVINO ONNX conversion](https://github.com/openvinotoolkit/openvino/blob/master/docs/articles_en/openvino-workflow/model-preparation/convert-model-onnx.rst)。本机 OpenVINO 2026.2.1 的可复现命令如下（模型和输出目录放在 `.build`，不会污染 Git）：

```powershell
$ov = (Resolve-Path ".build\openvino-2026.2.1\runtime\openvino_toolkit_windows_vc_mt_2026.2.1.21919.ede283a88e3_x86_64").Path
$env:PYTHONPATH = "$ov\python"
$env:OPENVINO_LIB_PATHS = "$ov\runtime\bin\intel64\Release;$ov\runtime\3rdparty\tbb\bin"

# 下载的输入文件：
# .build\PP-OCRv6_small_det.onnx
# .build\PP-OCRv6_small_rec.onnx

New-Item -ItemType Directory -Force .build\ppocrv6_ir | Out-Null
python -c "import openvino as ov; ov.save_model(ov.convert_model('.build/PP-OCRv6_small_det.onnx'), '.build/ppocrv6_ir/PP-OCRv6_small_det.xml', compress_to_fp16=False)"
python -c "import openvino as ov; ov.save_model(ov.convert_model('.build/PP-OCRv6_small_rec.onnx'), '.build/ppocrv6_ir/PP-OCRv6_small_rec.xml', compress_to_fp16=False)"
```

生成文件（本地已验证）：

```text
.build/ppocrv6_ir/PP-OCRv6_small_det.xml
.build/ppocrv6_ir/PP-OCRv6_small_det.bin
.build/ppocrv6_ir/PP-OCRv6_small_rec.xml
.build/ppocrv6_ir/PP-OCRv6_small_rec.bin
```

转换时不要使用 `compress_to_fp16=True` 作为首次验证方案；当前 Rust 适配器只接受 F32 输出，先保持 FP32，完成准确率基线后再单独验证 FP16。

PaddleOCR 自己的 ONNX 导出说明也要求 OCR 模型使用动态形状，否则不同尺寸图片的结果可能变化；参考 [PaddleOCR Paddle2ONNX guide](https://github.com/PaddlePaddle/PaddleOCR/blob/main/deploy/paddle2onnx/readme_ch.md)。官方 PP-OCRv6 ONNX 模型已经带有动态输入：检测 `[?,3,?,?]`，识别 `[?,3,48,?]`，所以不需要再用固定 `960x960` 或 `48x320` 覆盖模型输入。

## Open Model Zoo 替代方案评估

Open Model Zoo 有可下载的 IR 文本模型，但不能直接填入当前配置，因为模型家族和输出契约不同：

- [horizontal-text-detection-0001](https://github.com/openvinotoolkit/open_model_zoo/blob/master/models/intel/horizontal-text-detection-0001/README.md) 输入固定为 `1x3x704x704` BGR，输出是 `boxes [100,5]` 和 `labels [100]`。它不是 DB 概率图，当前 `det()` 会把错误的张量当 mask，不能直接使用。
- [text-recognition-0012](https://github.com/openvinotoolkit/open_model_zoo/blob/master/models/intel/text-recognition-0012/README.md) 输入是灰度 `1x32x120x1`，只识别 36 个大小写字母数字字符，输出 `30x1x37`。它不支持当前中文聊天字典，也不匹配 `[N,3,48,W]` 预处理。
- [text-recognition-0014](https://github.com/openvinotoolkit/open_model_zoo/blob/master/models/intel/text-recognition-0014/README.md) 同样是灰度图和 37 类英数字 CTC 输出；虽然输出形式接近 CTC，但仍不能覆盖中文且需要改输入预处理/字典。

若将来要采用 Model Zoo，必须新增 detector/recognizer 适配器（包括 boxes 解码、灰度预处理、字符集和可能的自回归 decoder），不能只替换 XML/BIN 路径。对于当前目标“无架构改动地启用 OpenVINO”，应优先采用 PaddlePaddle 官方 PP-OCRv6 small ONNX 再转 IR；转换完成后，发布和运行只需要 OpenVINO IR/BIN、字符集和本项目二进制。

## 运行配置建议

```yaml
ocr:
  # OpenVINO-only 构建可以省略这两个 MNN/PaddleOCR 路径
  det_model: null
  rec_model: null
  backend_priority:
    - openvino
  openvino:
    det_model: .build/ppocrv6_ir/PP-OCRv6_small_det.xml
    det_weights: .build/ppocrv6_ir/PP-OCRv6_small_det.bin
    rec_model: .build/ppocrv6_ir/PP-OCRv6_small_rec.xml
    rec_weights: .build/ppocrv6_ir/PP-OCRv6_small_rec.bin
    device: CPU
```

发布配置不要提交 `.build` 路径或临时下载文件；应把经过回归测试的四个 IR 文件放到发布包 `models/` 下，并使用相对路径。OpenVINO 推理本身只读取 IR/BIN 和字符集。上面的 OpenVINO-only 配置不会隐式回退到 CPU，也不需要 MNN 模型；如果要保留 fallback，需显式把 `cpu` 加入 `backend_priority`，并随包提供 MNN 模型。OpenVINO-only 构建使用 `cargo build --release --no-default-features --features ocr-openvino`，这样不会编译 `ocr-rs`/MNN。

## 复现记录

- OpenVINO runtime：2026.2.1 Windows x64（官方包）。
- 转换结果：det XML 约 252 KB/BIN 约 9.8 MB；rec XML 约 239 KB/BIN 约 21.1 MB。
- 本地 smoke test：两张零图均成功 `compile_model` 和 CPU infer；输出类型为 F32，det 输出 `[1,1,96,320]`，rec 输出 `[1,24,18710]`。
- Rust 适配层还提供可选真实截图测试。设置 `OPENVINO_OCR_IR_ROOT`（指向四个 IR 文件目录）并准备好 OpenVINO DLL 后运行：

  ```powershell
  cargo test --no-default-features --features ocr-openvino configured_ir_models_recognize_fixture -- --nocapture
  ```

  未设置该环境变量时测试会跳过，不影响普通 CI；本地 2026.2.1 runtime 已通过该测试。
- 尚未完成：在目标 CPU 上测量 10--30 ms 目标是否可达，以及把真实截图逐行结果与 MNN 做系统性比对。
