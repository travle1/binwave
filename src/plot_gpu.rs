//! Shared native/WebGPU trace renderer. Decoded window data is uploaded only when it changes;
//! viewport navigation updates a small uniform buffer in the paint callback.
use crate::plot_trace::series_color;
use eframe::{egui_wgpu, wgpu};
use egui::{Context, Painter, Rect};
use egui_plot::{PlotPoint, PlotTransform};
use std::collections::{BTreeMap, HashSet};
use std::sync::{mpsc::{Receiver, TryRecvError}, Arc, OnceLock};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use wgpu::util::DeviceExt;

const MAX_SEGMENTS: usize = 1_000_000;
static TARGET_FORMAT: OnceLock<wgpu::TextureFormat> = OnceLock::new();
static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(1);

pub fn configure(format: wgpu::TextureFormat) { let _ = TARGET_FORMAT.set(format); }
/// The surface format set by `configure`, once the GPU is available.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop column plots only.
pub fn target_format() -> Option<wgpu::TextureFormat> { TARGET_FORMAT.get().copied() }

#[repr(C)]
#[derive(Clone, Copy)]
struct Segment {
    coords: [f32; 4],
    color: u32,
    /// Index into the per-series placements applied at paint time.
    series: u32,
    pad: [u32; 2],
}
unsafe impl bytemuck::Zeroable for Segment {}
unsafe impl bytemuck::Pod for Segment {}

#[repr(C)]
#[derive(Clone, Copy)]
struct ViewUniform {
    bounds: [f32; 4],
    size: [f32; 2],
    pad: [f32; 2],
}
unsafe impl bytemuck::Zeroable for ViewUniform {}
unsafe impl bytemuck::Pod for ViewUniform {}

#[derive(Default)]
pub struct GpuPlot {
    key: Option<u64>,
    segments: Arc<Vec<Segment>>,
    series: Arc<Vec<String>>,
    x_origin: f64,
    y_origin: f64,
    generation: u64,
    pending: Option<PendingUpload>,
}
struct PendingUpload {
    key: u64,
    receiver: Receiver<PreparedSegments>,
    cancel: Arc<AtomicBool>,
}
struct PreparedSegments {
    segments: Vec<Segment>,
    series: Vec<String>,
    x_origin: f64,
    y_origin: f64,
}
impl GpuPlot {
    pub fn available() -> bool { TARGET_FORMAT.get().is_some() }
    pub fn invalidate(&mut self) {
        if let Some(pending) = self.pending.take() { pending.cancel.store(true, Ordering::Relaxed); }
        self.key = None;
        self.segments = Arc::new(Vec::new());
        self.series = Arc::new(Vec::new());
    }
    pub fn update(&mut self, ctx: &Context, points: &BTreeMap<String, Arc<Vec<PlotPoint>>>,
        hidden: &HashSet<String>, key: u64) -> bool {
        if !Self::available() { return false; }
        if self.pending.as_ref().is_some_and(|pending| pending.key != key) {
            self.pending.take().unwrap().cancel.store(true, Ordering::Relaxed);
        }
        if let Some(pending) = self.pending.take() {
            match pending.receiver.try_recv() {
                Ok(prepared) => {
                    self.x_origin = prepared.x_origin;
                    self.y_origin = prepared.y_origin;
                    self.segments = Arc::new(prepared.segments);
                    self.series = Arc::new(prepared.series);
                    self.key = Some(pending.key);
                    self.generation = NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed);
                }
                Err(TryRecvError::Disconnected) => {}
                Err(TryRecvError::Empty) => self.pending = Some(pending),
            }
        }
        if self.key == Some(key) { return !self.segments.is_empty(); }
        if self.pending.is_some() { return false; }
        // Cloning Arc handles is cheap even when the decoded arrays are large.
        // Segment reduction runs entirely off the UI thread.
        let visible: Vec<_> = points.iter().filter(|(name, _)| !hidden.contains(*name))
            .map(|(name, values)| (name.clone(), Arc::clone(values))).collect();
        if visible.is_empty() { self.segments = Arc::new(Vec::new()); self.key = Some(key); return false; }
        #[cfg(target_arch = "wasm32")]
        {
            // Browser queries already reduce points off-thread. Prepare the
            // retained upload once per result, never per navigation frame.
            let _ = ctx;
            if let Some(prepared) = prepare_segments(&visible, &AtomicBool::new(false)) {
                self.x_origin = prepared.x_origin;
                self.y_origin = prepared.y_origin;
                self.segments = Arc::new(prepared.segments);
                self.series = Arc::new(prepared.series);
                self.key = Some(key);
                self.generation = NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed);
            }
            return !self.segments.is_empty();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (sender, receiver) = mpsc::sync_channel(1);
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                if let Some(prepared) = prepare_segments(&visible, &worker_cancel) {
                    let _ = sender.send(prepared);
                }
                repaint.request_repaint();
            });
            self.pending = Some(PendingUpload { key, receiver, cancel });
            false
        }
    }
    /// Series offset and scale are applied by the shader, so panning or zooming
    /// a trace vertically only updates this small per-series buffer.
    pub fn paint(&self, painter: &Painter, transform: &PlotTransform,
        presentation: &crate::plot_presentation::Presentation) {
        if self.segments.is_empty() { return; }
        let frame = *transform.frame();
        let bounds = transform.bounds();
        let placements = self.series.iter()
            .map(|name| series_placement(self.y_origin, presentation.placement(name)))
            .collect();
        let callback = TraceCallback {
            segments: self.segments.clone(), generation: self.generation, placements,
            view: ViewUniform {
                bounds: [(bounds.min()[0] - self.x_origin) as f32,
                    (bounds.max()[0] - self.x_origin) as f32,
                    (bounds.min()[1] - self.y_origin) as f32,
                    (bounds.max()[1] - self.y_origin) as f32],
                size: [frame.width(), frame.height()], pad: [0.; 2],
            },
        };
        painter.with_clip_rect(frame).add(egui::Shape::Callback(
            egui_wgpu::Callback::new_paint_callback(Rect::from_min_max(frame.min, frame.max), callback)));
    }
}

/// Scale and offset for Y values stored relative to `y_origin`, so the shader's
/// `y * scale + offset` is the displayed value relative to the same origin.
fn series_placement(y_origin: f64, placement: crate::plot_presentation::Placement) -> [f32; 2] {
    [placement.scale as f32, (y_origin * (placement.scale - 1.) + placement.offset) as f32]
}

fn prepare_segments(visible: &[(String, Arc<Vec<PlotPoint>>)], cancel: &AtomicBool)
    -> Option<PreparedSegments> {
    let first = visible.iter().flat_map(|(_, values)| values.iter())
        .find(|p| p.x.is_finite() && p.y.is_finite())?;
    let x_origin = first.x;
    let y_origin = first.y;
    let per_series = (MAX_SEGMENTS / visible.len().max(1)).max(1);
    let mut segments = Vec::new();
    let mut series = Vec::new();
    for (name, values) in visible {
        if cancel.load(Ordering::Relaxed) { return None; }
        if segments.len() >= MAX_SEGMENTS { break; }
        let [r, g, b, _] = series_color(name).to_array();
        let color = u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16 | 180 << 24;
        let budget = per_series.min(MAX_SEGMENTS - segments.len());
        let index = series.len() as u32;
        series.push(name.clone());
        append_series_segments(&mut segments, values, budget, color, index, x_origin, y_origin);
    }
    Some(PreparedSegments { segments, series, x_origin, y_origin })
}

/// Limit the total upload without dropping an entire plot back to egui's
/// per-trace mesh path. Every reduced group retains its extrema and endpoints.
fn append_series_segments(out: &mut Vec<Segment>, values: &[PlotPoint], budget: usize,
    color: u32, series: u32, x_origin: f64, y_origin: f64) {
    if values.len() < 2 || budget == 0 { return; }
    let mut runs = Vec::new();
    let mut start = 0;
    for index in 0..values.len() {
        let point = values[index];
        if !point.x.is_finite() || !point.y.is_finite()
            || (index > start && point.x < values[index - 1].x) {
            if index.saturating_sub(start) >= 2 { runs.push(start..index); }
            start = index + usize::from(!point.x.is_finite() || !point.y.is_finite());
        }
    }
    if values.len().saturating_sub(start) >= 2 { runs.push(start..values.len()); }
    for (index, run) in runs.iter().enumerate() {
        let share = budget / runs.len() + usize::from(index < budget % runs.len());
        append_run_segments(out, &values[run.clone()], share, color, series, x_origin, y_origin);
    }
}

fn append_run_segments(out: &mut Vec<Segment>, values: &[PlotPoint], budget: usize,
    color: u32, series: u32, x_origin: f64, y_origin: f64) {
    if values.len() < 2 || budget == 0 { return; }
    let push = |out: &mut Vec<Segment>, a: PlotPoint, b: PlotPoint| {
        if a.x > b.x { return; }
        out.push(Segment { coords: [(a.x - x_origin) as f32, (a.y - y_origin) as f32,
            (b.x - x_origin) as f32, (b.y - y_origin) as f32], color, series, pad: [0; 2] });
    };
    if values.len() - 1 <= budget {
        for pair in values.windows(2) { push(out, pair[0], pair[1]); }
        return;
    }
    if budget < 4 {
        push(out, values[0], values[values.len() - 1]);
        return;
    }
    let groups = (budget / 4).max(1);
    let chunk_size = values.len().div_ceil(groups);
    let mut previous = None;
    let start_len = out.len();
    for chunk in values.chunks(chunk_size) {
        let mut extrema = [chunk[0], chunk[0]];
        for &point in chunk {
            if point.y < extrema[0].y { extrema[0] = point; }
            if point.y > extrema[1].y { extrema[1] = point; }
        }
        let mut retained = [chunk[0], extrema[0], extrema[1], chunk[chunk.len() - 1]];
        retained.sort_by(|a, b| a.x.total_cmp(&b.x));
        for point in retained {
            if previous.is_some_and(|last: PlotPoint| last.x == point.x && last.y == point.y) {
                continue;
            }
            if let Some(last) = previous { push(out, last, point); }
            previous = Some(point);
        }
    }
    debug_assert!(out.len() - start_len <= budget);
}

struct TraceCallback {
    segments: Arc<Vec<Segment>>,
    generation: u64,
    placements: Vec<[f32; 2]>,
    view: ViewUniform,
}
struct Resources {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    segments: wgpu::Buffer,
    placements: wgpu::Buffer,
    generation: u64,
    count: u32,
}
impl Resources {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("plot-traces"), source: wgpu::ShaderSource::Wgsl(include_str!("plot_gpu.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("plot-traces"), layout: None,
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"),
                buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState { format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default() }),
            primitive: wgpu::PrimitiveState::default(), depth_stencil: None,
            multisample: wgpu::MultisampleState::default(), multiview_mask: None, cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor { label: Some("plot-view"),
            size: std::mem::size_of::<ViewUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false });
        let segments = device.create_buffer(&wgpu::BufferDescriptor { label: Some("plot-empty-segments"),
            size: std::mem::size_of::<Segment>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false });
        let placements = Self::placement_buffer(device, 1);
        let bind_group = Self::bind_group(device, &pipeline, &segments, &uniform, &placements);
        Self { pipeline, uniform, bind_group, segments, placements, generation: u64::MAX, count: 0 }
    }
    fn placement_buffer(device: &wgpu::Device, series: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor { label: Some("plot-placements"),
            size: (series.max(1) * std::mem::size_of::<[f32; 2]>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false })
    }
    fn bind_group(device: &wgpu::Device, pipeline: &wgpu::RenderPipeline,
        segments: &wgpu::Buffer, uniform: &wgpu::Buffer, placements: &wgpu::Buffer) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("plot-trace-data"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: segments.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: placements.as_entire_binding() },
            ] })
    }
}
impl egui_wgpu::CallbackTrait for TraceCallback {
    fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor, _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources) -> Vec<wgpu::CommandBuffer> {
        if resources.get::<Resources>().is_none() {
            resources.insert(Resources::new(device, *TARGET_FORMAT.get().unwrap()));
        }
        let resource = resources.get_mut::<Resources>().unwrap();
        let placement_bytes = std::mem::size_of_val(self.placements.as_slice()) as u64;
        let mut rebind = false;
        if resource.placements.size() < placement_bytes {
            resource.placements = Resources::placement_buffer(device, self.placements.len());
            rebind = true;
        }
        if resource.generation != self.generation {
            resource.segments = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("plot-segments"), contents: bytemuck::cast_slice(&self.segments),
                usage: wgpu::BufferUsages::STORAGE,
            });
            resource.generation = self.generation;
            resource.count = self.segments.len() as u32;
            rebind = true;
        }
        if rebind {
            resource.bind_group = Resources::bind_group(device, &resource.pipeline,
                &resource.segments, &resource.uniform, &resource.placements);
        }
        queue.write_buffer(&resource.uniform, 0, bytemuck::bytes_of(&self.view));
        if !self.placements.is_empty() {
            queue.write_buffer(&resource.placements, 0, bytemuck::cast_slice(&self.placements));
        }
        Vec::new()
    }
    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources) {
        let Some(resource) = resources.get::<Resources>() else { return; };
        pass.set_pipeline(&resource.pipeline);
        pass.set_bind_group(0, &resource.bind_group, &[]);
        pass.draw(0..6, 0..resource.count);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    #[test]
    fn crowded_plot_prepares_off_ui_thread_without_falling_back() {
        use super::*;
        configure(wgpu::TextureFormat::Bgra8Unorm);
        let ctx = Context::default();
        let samples = Arc::new((0..10_000).map(|index|
            PlotPoint::new(index as f64, (index % 17) as f64)).collect::<Vec<_>>());
        let points: BTreeMap<_, _> = (0..128).map(|index|
            (format!("trace {index}"), Arc::clone(&samples))).collect();
        let mut gpu = GpuPlot::default();
        assert!(!gpu.update(&ctx, &points, &HashSet::new(), 1),
            "vector fallback should paint until the new GPU geometry is ready");
        assert!(gpu.pending.is_some());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while gpu.pending.is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            gpu.update(&ctx, &points, &HashSet::new(), 1);
        }
        assert!(gpu.pending.is_none(), "GPU geometry worker did not finish");
        assert!(!gpu.segments.is_empty());
        assert!(gpu.segments.len() <= MAX_SEGMENTS);
    }
    #[test]
    fn crowded_plot_keeps_gpu_segments_bounded_and_preserves_peak() {
        use super::*;
        let values: Vec<_> = (0..20_000).map(|index| PlotPoint::new(index as f64,
            if index == 12_345 { 1000. } else { 1. })).collect();
        let mut segments = Vec::new();
        append_series_segments(&mut segments, &values, 400, 0, 0, 0., 0.);
        assert!(!segments.is_empty());
        assert!(segments.len() <= 400);
        assert!(segments.iter().any(|segment| segment.coords[1] == 1000.
            || segment.coords[3] == 1000.));
        assert_eq!(segments.first().unwrap().coords[0], 0.);
        assert_eq!(segments.last().unwrap().coords[2], 19_999.);
    }
    #[test]
    fn reduced_gpu_trace_does_not_join_across_invalid_gap() {
        use super::*;
        let mut values: Vec<_> = (0..1000).map(|index| PlotPoint::new(index as f64, 1.)).collect();
        values[500].y = f64::NAN;
        let mut segments = Vec::new();
        append_series_segments(&mut segments, &values, 80, 0, 0, 0., 0.);
        assert!(!segments.is_empty());
        assert!(segments.iter().all(|segment| segment.coords[0] <= segment.coords[2]));
        assert!(segments.iter().all(|segment| segment.coords[0] < 500.
            && segment.coords[2] < 500. || segment.coords[0] > 500.
            && segment.coords[2] > 500.));
    }
    #[test]
    fn series_placement_matches_display_values_relative_to_origin() {
        use super::*;
        let placement = crate::plot_presentation::Placement { scale: 2., offset: 100. };
        let (origin, raw) = (18_000., 18_000.5);
        let [scale, offset] = series_placement(origin, placement);
        let stored = (raw - origin) as f32;
        assert_eq!(stored * scale + offset, (placement.display(raw) - origin) as f32);
    }
    #[test]
    fn prepared_segments_record_their_series() {
        use super::*;
        let line = Arc::new(vec![PlotPoint::new(0., 1.), PlotPoint::new(1., 2.)]);
        let visible = vec![("a".to_owned(), Arc::clone(&line)), ("b".to_owned(), line)];
        let prepared = prepare_segments(&visible, &AtomicBool::new(false)).unwrap();
        assert_eq!(prepared.series, ["a", "b"]);
        assert_eq!(prepared.segments.iter().map(|segment| segment.series).collect::<Vec<_>>(), [0, 1]);
    }
    #[test]
    fn trace_shader_parses_and_validates() {
        let module = naga::front::wgsl::parse_str(include_str!("plot_gpu.wgsl"))
            .unwrap_or_else(|error| panic!("{}", error.emit_to_string(include_str!("plot_gpu.wgsl"))));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all()).validate(&module).unwrap();
    }
    #[test]
    #[ignore = "requires an available GPU adapter"]
    fn pipeline_creates_on_gpu() {
        use super::*;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let mut resources = Resources::new(&device, wgpu::TextureFormat::Bgra8Unorm);
        let segment = Segment { coords: [0., 0., 10., 10.], color: 0xff00ffff, series: 0, pad: [0; 2] };
        resources.segments = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None, contents: bytemuck::bytes_of(&segment), usage: wgpu::BufferUsages::STORAGE });
        resources.bind_group = Resources::bind_group(&device, &resources.pipeline,
            &resources.segments, &resources.uniform, &resources.placements);
        queue.write_buffer(&resources.placements, 0, bytemuck::cast_slice(&[[1f32, 0.]]));
        let view = ViewUniform { bounds: [0., 10., 0., 10.], size: [64., 64.], pad: [0.; 2] };
        queue.write_buffer(&resources.uniform, 0, bytemuck::bytes_of(&view));
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None, size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None,
            size: 64 * 64 * 4, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store },
                })], ..Default::default()
            });
            pass.set_pipeline(&resources.pipeline);
            pass.set_bind_group(0, &resources.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        encoder.copy_texture_to_buffer(texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0,
                    bytes_per_row: Some(64 * 4), rows_per_image: None } },
            wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 });
        queue.submit(Some(encoder.finish()));
        let (sender, receiver) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| sender.send(result).unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        receiver.recv().unwrap().unwrap();
        let pixels = readback.slice(..).get_mapped_range().unwrap();
        assert!(pixels.chunks_exact(4).any(|pixel| pixel[0..3] != [0, 0, 0]));
    }
}
