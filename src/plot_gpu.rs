//! Native GPU trace renderer. Decoded window data is uploaded only when it changes;
//! viewport navigation updates a small uniform buffer in the paint callback.
use crate::plot_trace::series_color;
use eframe::{egui_wgpu, wgpu};
use egui::{Painter, Rect};
use egui_plot::{PlotPoint, PlotTransform};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use wgpu::util::DeviceExt;

const MAX_SEGMENTS: usize = 2_000_000;
static TARGET_FORMAT: OnceLock<wgpu::TextureFormat> = OnceLock::new();
static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(1);

pub fn configure(format: wgpu::TextureFormat) { let _ = TARGET_FORMAT.set(format); }

#[repr(C)]
#[derive(Clone, Copy)]
struct Segment {
    coords: [f32; 4],
    color: u32,
    pad: [u32; 3],
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
    x_origin: f64,
    y_origin: f64,
    generation: u64,
}
impl GpuPlot {
    pub fn available() -> bool { TARGET_FORMAT.get().is_some() }
    pub fn invalidate(&mut self) { self.key = None; }
    pub fn update(&mut self, points: &BTreeMap<String, Vec<PlotPoint>>,
        hidden: &HashSet<String>, key: u64) -> bool {
        if !Self::available() { return false; }
        if self.key == Some(key) { return !self.segments.is_empty(); }
        let Some(first) = points.iter().filter(|(name, _)| !hidden.contains(*name))
            .flat_map(|(_, values)| values.iter()).find(|p| p.x.is_finite() && p.y.is_finite()) else {
            self.segments = Arc::new(Vec::new()); self.key = Some(key); return false;
        };
        let x_origin = first.x;
        let y_origin = first.y;
        let mut segments = Vec::new();
        for (name, values) in points {
            if hidden.contains(name) { continue; }
            let [r, g, b, _] = series_color(name).to_array();
            let color = u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16 | 180 << 24;
            for pair in values.windows(2) {
                let [a, b] = [pair[0], pair[1]];
                if ![a.x, a.y, b.x, b.y].iter().all(|v| v.is_finite()) || a.x >= b.x { continue; }
                if segments.len() >= MAX_SEGMENTS {
                    self.segments = Arc::new(Vec::new()); self.key = Some(key); return false;
                }
                segments.push(Segment {
                    coords: [(a.x - x_origin) as f32, (a.y - y_origin) as f32,
                        (b.x - x_origin) as f32, (b.y - y_origin) as f32],
                    color, pad: [0; 3],
                });
            }
        }
        self.x_origin = x_origin;
        self.y_origin = y_origin;
        self.segments = Arc::new(segments);
        self.key = Some(key);
        self.generation = NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed);
        !self.segments.is_empty()
    }
    pub fn paint(&self, painter: &Painter, transform: &PlotTransform) {
        if self.segments.is_empty() { return; }
        let frame = *transform.frame();
        let bounds = transform.bounds();
        let callback = TraceCallback {
            segments: self.segments.clone(), generation: self.generation,
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

struct TraceCallback {
    segments: Arc<Vec<Segment>>,
    generation: u64,
    view: ViewUniform,
}
struct Resources {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    segments: wgpu::Buffer,
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
        let bind_group = Self::bind_group(device, &pipeline, &segments, &uniform);
        Self { pipeline, uniform, bind_group, segments, generation: u64::MAX, count: 0 }
    }
    fn bind_group(device: &wgpu::Device, pipeline: &wgpu::RenderPipeline,
        segments: &wgpu::Buffer, uniform: &wgpu::Buffer) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("plot-trace-data"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: segments.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
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
        if resource.generation != self.generation {
            resource.segments = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("plot-segments"), contents: bytemuck::cast_slice(&self.segments),
                usage: wgpu::BufferUsages::STORAGE,
            });
            resource.bind_group = Resources::bind_group(device, &resource.pipeline,
                &resource.segments, &resource.uniform);
            resource.generation = self.generation;
            resource.count = self.segments.len() as u32;
        }
        queue.write_buffer(&resource.uniform, 0, bytemuck::bytes_of(&self.view));
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

#[cfg(test)]
mod tests {
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
        let segment = Segment { coords: [0., 0., 10., 10.], color: 0xff00ffff, pad: [0; 3] };
        resources.segments = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None, contents: bytemuck::bytes_of(&segment), usage: wgpu::BufferUsages::STORAGE });
        resources.bind_group = Resources::bind_group(&device, &resources.pipeline,
            &resources.segments, &resources.uniform);
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
