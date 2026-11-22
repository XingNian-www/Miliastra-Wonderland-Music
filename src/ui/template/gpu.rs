//! wgpu 计算着色器 SAD 主路径与 CPU 兜底协作。

use image::GrayImage;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::cpu::{GrayMatch, GrayMatcher, cpu_best_gray_match, cpu_best_gray_match_seeded, sad_at};

/// GPU 侧 SAD 计算着色器（带边界判断，修掉 crate 的行首污染缺陷）。
pub(super) const GPU_SAD_SHADER: &str = r#"
struct Uniforms {
    input_width: u32,
    input_height: u32,
    template_width: u32,
    template_height: u32,
    _reserved_a: u32,
    _reserved_b: u32,
    chunks: u32,
    chunk_rows: u32,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var<storage, read> input_buf: array<u32>;
@group(0) @binding(2) var<storage, read> template_buf: array<u32>;
@group(0) @binding(3) var<storage, read_write> result_buf: array<atomic<u32>>;

@compute @workgroup_size(16, 16, 1)
fn main_sad(@builtin(global_invocation_id) gid: vec3<u32>) {
    let rw = uniforms.input_width - uniforms.template_width + 1u;
    let rh = uniforms.input_height - uniforms.template_height + 1u;
    // 边界判断：缺了它，越界线程会污染下一行的行首条目。
    if (gid.x >= rw || gid.y >= rh) { return; }
    // 把模板行按 gid.z 分片，让每个 (x,y) 由多个线程并行累加：
    // 大模板单个线程要串行 th*tw 次，workgroup 数量太少会让 GPU 严重欠载。
    let row_begin = gid.z * uniforms.chunk_rows;
    let row_end = min(row_begin + uniforms.chunk_rows, uniforms.template_height);
    var total = 0u;
    for (var r = row_begin; r < row_end; r++) {
        let input_base = (gid.y + r) * uniforms.input_width + gid.x;
        let template_base = r * uniforms.template_width;
        for (var c = 0u; c < uniforms.template_width; c++) {
            let a = input_buf[input_base + c];
            let b = template_buf[template_base + c];
            total += max(a, b) - min(a, b);
        }
    }
    atomicAdd(&result_buf[gid.y * rw + gid.x], total);
}
"#;

/// GPU 回读的最长等待时间。
///
/// TDR（驱动重置）、设备丢失或驱动 bug 都可能让 `map_async` 的回调永远不来；
/// 没有上限时，逐帧分类线程会连同 `GPU_STATE` 全局锁一起永久挂起，
/// 值守循环表现为“机器人不再响应”，且没有任何日志（B16-P1-5）。
/// 超时后本次运行改走 CPU（见 [`gpu_best_position`]），不再尝试 GPU。
pub(super) const GPU_READBACK_TIMEOUT: Duration = Duration::from_secs(2);

/// 回读等待期间驱动 `poll` 的间隔：既能让回调尽快被调度，又不空转。
pub(super) const GPU_READBACK_POLL_SLICE: Duration = Duration::from_millis(5);

/// 等待 `map_async` 回调的三种结局。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MapWait<T> {
    /// 回调到达（`T` 是驱动回调的返回值）。
    Received(T),
    /// 超过时限仍未到达。
    TimedOut,
    /// 发送端已断开（回调被丢弃）。
    Disconnected,
}

/// 有界等待一次异步映射回调；每轮先驱动一次 `poll`，再短超时收包。
///
/// 把 `poll` 作为回调传进来，测试可以不建真实 wgpu 设备就验证“不会无限等待”。
pub(super) fn wait_for_map_completion<T>(
    receiver: &Receiver<T>,
    mut poll: impl FnMut(),
    timeout: Duration,
) -> MapWait<T> {
    let deadline = Instant::now() + timeout;
    loop {
        poll();
        match receiver.recv_timeout(GPU_READBACK_POLL_SLICE) {
            Ok(value) => return MapWait::Received(value),
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    return MapWait::TimedOut;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return MapWait::Disconnected,
        }
    }
}

/// 进程内复用的 GPU 匹配器：设备、管线与缓冲只创建一次。
///
/// 与旧实现的关键差别：template-matching crate 每次调用都新建 wgpu 设备
/// （实测固定开销 210–250ms），这里改为复用，每帧只剩提交与回读。
pub(super) struct GpuSearcher {
    pub(super) device: wgpu::Device,
    pub(super) queue: wgpu::Queue,
    pub(super) pipeline: wgpu::ComputePipeline,
    pub(super) layout: wgpu::BindGroupLayout,
    pub(super) uniform: wgpu::Buffer,
    pub(super) input: wgpu::Buffer,
    pub(super) needle: wgpu::Buffer,
    pub(super) result: wgpu::Buffer,
    pub(super) staging: wgpu::Buffer,
    pub(super) input_bytes: u64,
    pub(super) needle_bytes: u64,
    pub(super) result_bytes: u64,
    /// 复用的全零块，用于每次分发前清零原子累加结果图。
    pub(super) zeroes: Vec<u8>,
}

/// 低于该工作量时直接走 CPU：小模板的 SEA 剪枝比 GPU 的固定开销更划算。
pub(super) const GPU_MIN_WORK: u64 = 3_000_000;

/// 期望的并行线程数：大模板的瓶颈是单线程串行 th*tw 次导致的占用率不足，
/// 据此把模板行切成若干片交给不同线程并行累加。
pub(super) const GPU_TARGET_THREADS: u64 = 262_144;

pub(super) enum GpuState {
    Uninitialized,
    Ready(Box<GpuSearcher>),
    Unavailable,
}

pub(super) static GPU_STATE: OnceLock<Mutex<GpuState>> = OnceLock::new();

/// wgpu 的显卡类型转成中文，避免日志里出现 `DiscreteGpu` 这类内部枚举名。
pub(super) fn gpu_device_type_label(device_type: wgpu::DeviceType) -> &'static str {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => "独立显卡",
        wgpu::DeviceType::IntegratedGpu => "集成显卡",
        wgpu::DeviceType::VirtualGpu => "虚拟显卡",
        wgpu::DeviceType::Cpu => "CPU 软件渲染",
        wgpu::DeviceType::Other => "其他设备",
    }
}

/// wgpu 的图形 API 转成业界通用缩写（这些缩写属于保留的英文）。
///
/// 必须穷尽 `wgpu::Backend`：新增变体时编译失败，而不是在日志里留下一个空洞。
pub(super) fn gpu_backend_label(backend: wgpu::Backend) -> &'static str {
    match backend {
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Dx12 => "DirectX 12",
        wgpu::Backend::Dx11 => "DirectX 11",
        wgpu::Backend::Gl => "OpenGL",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        // 占位后端只在测试里出现，不会进入真实匹配路径。
        wgpu::Backend::Empty => "无",
    }
}

pub(super) fn u32_bytes(values: &[u32]) -> Vec<u8> {
    // 一次分配 + 定长切片写入：避免逐元素扩容与边界检查，大模板上是主要开销之一。
    let mut bytes = vec![0u8; values.len() * 4];
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(values) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

impl GpuSearcher {
    pub(super) fn buffer(
        device: &wgpu::Device,
        size: u64,
        usage: wgpu::BufferUsages,
    ) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: size.max(4),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// 创建一次。优先独显，其次集显；软件适配器与虚拟显卡不参与（那类情况交给 CPU 兜底）。
    pub(super) fn create() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            dx12_shader_compiler: Default::default(),
        });
        let candidates: Vec<wgpu::Adapter> = instance
            .enumerate_adapters(wgpu::Backends::all())
            .filter(|adapter| {
                matches!(
                    adapter.get_info().device_type,
                    wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
                )
            })
            .collect();
        // 独显优先，避免多显卡机器上退到集显（实测独显比集显快约 8 倍）。
        let adapter = candidates
            .iter()
            .find(|adapter| adapter.get_info().device_type == wgpu::DeviceType::DiscreteGpu)
            .or_else(|| candidates.first())?;
        let info = adapter.get_info();

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: None,
                features: wgpu::Features::empty(),
                limits: wgpu::Limits::default(),
            },
            None,
        ))
        .ok()?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(GPU_SAD_SHADER)),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: "main_sad",
        });

        // 只在 GPU 匹配器真正创建成功时执行一次（整个进程生命周期），
        // 因此用 info 记录「本次运行实际用的是哪块 GPU」是合适的；
        // 重复失败/回退路径不在这里，不会刷屏。
        log::info!(
            "模板匹配启用 GPU 后端：{}（{}，{}）",
            info.name,
            gpu_device_type_label(info.device_type),
            gpu_backend_label(info.backend)
        );

        Some(Self {
            uniform: Self::buffer(&device, 32, wgpu::BufferUsages::UNIFORM),
            input: Self::buffer(&device, 4, wgpu::BufferUsages::STORAGE),
            needle: Self::buffer(&device, 4, wgpu::BufferUsages::STORAGE),
            result: Self::buffer(
                &device,
                4,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            ),
            staging: Self::buffer(&device, 4, wgpu::BufferUsages::MAP_READ),
            input_bytes: 4,
            needle_bytes: 4,
            result_bytes: 4,
            zeroes: vec![0u8; 4],
            device,
            queue,
            pipeline,
            layout,
        })
    }

    pub(super) fn ensure(&mut self, input_bytes: u64, needle_bytes: u64, result_bytes: u64) {
        if input_bytes > self.input_bytes {
            self.input_bytes = input_bytes.next_power_of_two();
            self.input = Self::buffer(&self.device, self.input_bytes, wgpu::BufferUsages::STORAGE);
        }
        if needle_bytes > self.needle_bytes {
            self.needle_bytes = needle_bytes.next_power_of_two();
            self.needle =
                Self::buffer(&self.device, self.needle_bytes, wgpu::BufferUsages::STORAGE);
        }
        if result_bytes > self.result_bytes {
            self.result_bytes = result_bytes.next_power_of_two();
            self.result = Self::buffer(
                &self.device,
                self.result_bytes,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            );
            self.staging = Self::buffer(
                &self.device,
                self.result_bytes,
                wgpu::BufferUsages::MAP_READ,
            );
            self.zeroes = vec![0u8; self.result_bytes as usize];
        }
    }

    /// 上传阶段：确保缓冲容量、写入 uniform/画面/模板，并建立 bind group。
    pub(super) fn upload(
        &mut self,
        input: &[u32],
        needle: &[u32],
        dims: [u32; 8],
    ) -> wgpu::BindGroup {
        let (iw, ih, tw, th) = (dims[0], dims[1], dims[2], dims[3]);
        let result_bytes = (iw - tw + 1) as u64 * (ih - th + 1) as u64 * 4;
        self.ensure(
            input.len() as u64 * 4,
            needle.len() as u64 * 4,
            result_bytes,
        );

        self.queue.write_buffer(&self.uniform, 0, &u32_bytes(&dims));
        self.queue.write_buffer(&self.input, 0, &u32_bytes(input));
        self.queue.write_buffer(&self.needle, 0, &u32_bytes(needle));
        // 原子累加前清零结果图。
        self.queue
            .write_buffer(&self.result, 0, &self.zeroes[..result_bytes as usize]);

        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.input.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.needle.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.result.as_entire_binding(),
                },
            ],
        })
    }

    /// 计算阶段：提交并等待 GPU，再回读结果图。
    pub(super) fn compute(
        &mut self,
        dims: [u32; 8],
        bind_group: &wgpu::BindGroup,
    ) -> Option<Vec<u32>> {
        let (iw, ih, tw, th) = (dims[0], dims[1], dims[2], dims[3]);
        let chunks = dims[6].max(1);
        let rw = iw - tw + 1;
        let rh = ih - th + 1;
        let result_bytes = rw as u64 * rh as u64 * 4;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.dispatch_workgroups(rw.div_ceil(16), rh.div_ceil(16), chunks);
        }
        encoder.copy_buffer_to_buffer(&self.result, 0, &self.staging, 0, result_bytes);
        self.queue.submit(Some(encoder.finish()));

        let slice = self.staging.slice(0..result_bytes);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        // 有界等待：驱动重置/设备丢失时回调可能永远不来，绝不能无界阻塞。
        match wait_for_map_completion(
            &receiver,
            || {
                let _ = self.device.poll(wgpu::Maintain::Poll);
            },
            GPU_READBACK_TIMEOUT,
        ) {
            MapWait::Received(Ok(())) => {}
            MapWait::Received(Err(error)) => {
                log::warn!("GPU 模板匹配回读失败（原因：{error}），本次运行改用 CPU 计算");
                return None;
            }
            MapWait::TimedOut => {
                log::warn!(
                    "GPU 模板匹配回读超过 {} 秒仍未完成（驱动可能已重置或设备丢失），本次运行改用 CPU 计算",
                    GPU_READBACK_TIMEOUT.as_secs()
                );
                return None;
            }
            MapWait::Disconnected => {
                log::warn!("GPU 模板匹配回读回调被丢弃，本次运行改用 CPU 计算");
                return None;
            }
        }
        let data = slice.get_mapped_range();
        let values: Vec<u32> = data
            .chunks_exact(4)
            .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        drop(data);
        self.staging.unmap();
        Some(values)
    }

    /// 全图扫描：上传 + 计算。
    pub(super) fn search(
        &mut self,
        input: &[u32],
        needle: &[u32],
        dims: [u32; 8],
    ) -> Option<Vec<u32>> {
        let bind_group = self.upload(input, needle, dims);
        self.compute(dims, &bind_group)
    }
}

/// 用 GPU 定位最优点。不可用时返回 None，由 CPU 兜底。
pub(super) fn gpu_best_position(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
) -> Option<(u32, u32)> {
    // GPU 侧忽略 alpha，因此只有完全不透明的模板才走 GPU；带掩码的模板交给 CPU。
    // 可用 MILIASTRA_TEMPLATE_MATCH=cpu 强制走 CPU。
    if matcher.alpha.is_some()
        || matches!(
            std::env::var("MILIASTRA_TEMPLATE_MATCH").as_deref(),
            Ok("cpu")
        )
    {
        return None;
    }
    let (iw, ih) = (haystack.width(), haystack.height());
    let (tw, th) = (matcher.gray.width(), matcher.gray.height());
    if tw == 0 || th == 0 || tw > iw || th > ih {
        return None;
    }
    // 工作量太小时 CPU 的 SEA 剪枝已经够快，GPU 的提交+回读固定开销反而更贵。
    // 实测分界：28×19 的标记类模板 CPU 0.11ms 优于 GPU；46×44 以上 GPU 明显更快。
    let positions = (iw - tw + 1) as u64 * (ih - th + 1) as u64;
    if positions * (tw as u64 * th as u64) < GPU_MIN_WORK {
        return None;
    }

    let state = GPU_STATE.get_or_init(|| Mutex::new(GpuState::Uninitialized));
    let mut guard = state.lock().ok()?;
    if matches!(*guard, GpuState::Uninitialized) {
        *guard = match GpuSearcher::create() {
            Some(searcher) => GpuState::Ready(Box::new(searcher)),
            None => {
                log::warn!(
                    "未找到可用的硬件 GPU 适配器，模板匹配改用 CPU 计算，大区域匹配会比 GPU 明显更慢；本次运行不再重试 GPU"
                );
                GpuState::Unavailable
            }
        };
    }
    let values = {
        let GpuState::Ready(searcher) = &mut *guard else {
            return None;
        };

        let input: Vec<u32> = haystack.as_raw().iter().map(|v| *v as u32).collect();
        let needle: Vec<u32> = matcher.gray.as_raw().iter().map(|v| *v as u32).collect();
        // 按位置数把模板行分片：位置越少，每个位置分到的线程越多，避免 GPU 欠载。
        let chunks = (GPU_TARGET_THREADS / positions.max(1)).clamp(1, 256) as u32;
        let chunk_rows = th.div_ceil(chunks);
        let dims = [iw, ih, tw, th, 0, 0, chunks, chunk_rows];
        searcher.search(&input, &needle, dims)
    };
    let Some(values) = values else {
        // 回读失败/超时：把 GPU 置为不可用并释放设备（含可能仍挂起的映射），
        // 后续帧直接走 CPU，不再让分类线程赌驱动的恢复速度。
        log::warn!(
            "GPU 模板匹配已停用：回读失败或超时，后续改用 CPU 计算（大区域匹配会比 GPU 慢）"
        );
        *guard = GpuState::Unavailable;
        return None;
    };

    let rw = (iw - tw + 1) as usize;
    let mut best = u32::MAX;
    let mut best_index = 0usize;
    for (index, value) in values.iter().enumerate() {
        if *value < best {
            best = *value;
            best_index = index;
        }
    }
    Some(((best_index % rw) as u32, (best_index / rw) as u32))
}

/// 寻找 SAD 最小的位置：优先 GPU（一次全图扫描），CPU 兜底。
///
/// GPU 只负责定位；随后把 GPU 得到的最优值作为初始上界再跑一次 CPU 扫描，
/// 于是既拿到 GPU 的速度，又保持精确的全局最优与并列取栅格序最前的语义。
/// GPU 不可用（无独显/集显、驱动异常、创建失败）或模板带 alpha 掩码时，退回纯 CPU 扫描。
pub(crate) fn best_gray_match(
    haystack: &GrayImage,
    matcher: &GrayMatcher<'_>,
) -> Option<GrayMatch> {
    if let Some((x, y)) = gpu_best_position(haystack, matcher) {
        let sad = sad_at(haystack, matcher, x, y, u64::MAX);
        return cpu_best_gray_match_seeded(haystack, matcher, Some(GrayMatch { x, y, sad }));
    }
    cpu_best_gray_match(haystack, matcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B16-P1-5：回调永远不来时必须按期限返回，不能永久阻塞分类线程。
    #[test]
    fn readback_wait_times_out_instead_of_blocking_forever() {
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        let started = Instant::now();
        let outcome = wait_for_map_completion(&receiver, || {}, Duration::from_millis(40));
        assert_eq!(outcome, MapWait::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "有界等待必须按期限退出，实际耗时 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn readback_wait_receives_the_driver_result() {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(Ok::<(), wgpu::BufferAsyncError>(())).unwrap();
        let outcome = wait_for_map_completion(&receiver, || {}, Duration::from_millis(500));
        assert_eq!(outcome, MapWait::Received(Ok(())));
    }

    /// 设备丢失时回调可能被直接丢弃（发送端断开），也必须立即返回。
    #[test]
    fn readback_wait_reports_a_dropped_callback() {
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        drop(sender);
        let outcome = wait_for_map_completion(&receiver, || {}, Duration::from_millis(500));
        assert_eq!(outcome, MapWait::Disconnected);
    }
}
