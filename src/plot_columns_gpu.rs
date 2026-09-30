//! Draws a pyramid's min/max columns for many channels in one instanced draw.
//! Columns are uploaded when they change; panning and Y placement only update
//! small uniform and per-channel buffers.
use crate::plot_pyramid::Columns;
use eframe::{egui_wgpu, wgpu};
use egui::{Color32, Painter, Rect};
use egui_plot::PlotTransform;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use wgpu::util::DeviceExt;

static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewUniform {
    bounds: [f32; 4],
    size: [f32; 2],
    x_step: f32,
    columns: u32,
    channels: u32,
    pad: [u32; 3],
}
unsafe impl bytemuck::Zeroable for ViewUniform {}
unsafe impl bytemuck::Pod for ViewUniform {}

/// Columns ready for the GPU: channel-major min/max pairs relative to `y_origin`,
/// with column 0 centred at sample `x_origin`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnUpload {
    values: Vec<[f32; 2]>,
    columns: u32,
    channels: u32,
    x_origin: f64,
    x_step: f64,
    y_origin: f64,
    colors: Vec<u32>,
}

impl ColumnUpload {
    /// Pack the chosen channels of `columns`; `channels` lists them with colours.
    pub fn new(columns: &Columns, channels: &[(usize, Color32)]) -> Self {
        let count = columns.values.first().map_or(0, Vec::len);
        let x_step = columns.range.len() as f64 / count.max(1) as f64;
        let x_origin = columns.range.start as f64 + x_step / 2.;
        // Stored relative to a nearby value so f32 keeps its precision.
        let y_origin = channels.iter().flat_map(|&(channel, _)| columns.values[channel].iter().flatten())
            .map(|[low, _]| *low).next().unwrap_or(0.);
        let mut values = Vec::with_capacity(count * channels.len());
        for &(channel, _) in channels {
            values.extend(columns.values[channel].iter().map(|value| match value {
                Some([low, high]) => [(low - y_origin) as f32, (high - y_origin) as f32],
                None => [f32::INFINITY, f32::NEG_INFINITY],
            }));
        }
        let colors = channels.iter().map(|&(_, color)| u32::from_le_bytes(color.to_array())).collect();
        Self { values, columns: count as u32, channels: channels.len() as u32, x_origin, x_step, y_origin, colors }
    }
}

/// Keeps the last upload and paints it for the current view.
#[derive(Default)]
pub struct GpuColumns {
    upload: Option<(u64, Arc<ColumnUpload>)>,
}

impl GpuColumns {
    pub fn set(&mut self, upload: ColumnUpload) {
        self.upload = Some((NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed), Arc::new(upload)));
    }

    /// `placements` gives each uploaded channel's display scale and offset.
    pub fn paint(&self, painter: &Painter, transform: &PlotTransform, placements: &[[f32; 2]]) {
        let Some((generation, upload)) = &self.upload else { return };
        if upload.values.is_empty() { return; }
        let frame = *transform.frame();
        let view = view_uniform(upload, transform);
        // Placements apply to values relative to the upload's Y origin.
        let placements = placements.iter().map(|&[scale, offset]|
            [scale, (upload.y_origin * (scale as f64 - 1.)) as f32 + offset]).collect();
        let callback = ColumnCallback { upload: upload.clone(), generation: *generation, placements, view };
        painter.with_clip_rect(frame).add(egui::Shape::Callback(
            egui_wgpu::Callback::new_paint_callback(Rect::from_min_max(frame.min, frame.max), callback)));
    }
}

fn view_uniform(upload: &ColumnUpload, transform: &PlotTransform) -> ViewUniform {
    let bounds = transform.bounds();
    let frame = transform.frame();
    ViewUniform {
        bounds: [(bounds.min()[0] - upload.x_origin) as f32, (bounds.max()[0] - upload.x_origin) as f32,
            (bounds.min()[1] - upload.y_origin) as f32, (bounds.max()[1] - upload.y_origin) as f32],
        size: [frame.width(), frame.height()],
        x_step: upload.x_step as f32,
        columns: upload.columns,
        channels: upload.channels,
        pad: [0; 3],
    }
}

struct ColumnCallback {
    upload: Arc<ColumnUpload>,
    generation: u64,
    placements: Vec<[f32; 2]>,
    view: ViewUniform,
}

struct Resources {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    values: wgpu::Buffer,
    placements: wgpu::Buffer,
    colors: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    generation: u64,
    instances: u32,
}

impl Resources {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("plot-columns"), source: wgpu::ShaderSource::Wgsl(include_str!("plot_columns.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("plot-columns"), layout: None,
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"),
                buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState { format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default() }),
            primitive: wgpu::PrimitiveState::default(), depth_stencil: None,
            multisample: wgpu::MultisampleState::default(), multiview_mask: None, cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor { label: Some("plot-columns-view"),
            size: std::mem::size_of::<ViewUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let values = storage(device, "plot-columns-values", 8);
        let placements = storage(device, "plot-columns-placements", 8);
        let colors = storage(device, "plot-columns-colors", 4);
        let bind_group = bind_group(device, &pipeline, &values, &uniform, &placements, &colors);
        Self { pipeline, uniform, values, placements, colors, bind_group, generation: u64::MAX, instances: 0 }
    }
}

fn storage(device: &wgpu::Device, label: &str, size: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(4) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
}

fn bind_group(device: &wgpu::Device, pipeline: &wgpu::RenderPipeline, values: &wgpu::Buffer,
    uniform: &wgpu::Buffer, placements: &wgpu::Buffer, colors: &wgpu::Buffer) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("plot-columns"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: values.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: placements.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: colors.as_entire_binding() },
        ] })
}

impl egui_wgpu::CallbackTrait for ColumnCallback {
    fn prepare(&self, device: &wgpu::Device, queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor, _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources) -> Vec<wgpu::CommandBuffer> {
        if resources.get::<Resources>().is_none() {
            let Some(format) = crate::plot_gpu::target_format() else { return Vec::new() };
            resources.insert(Resources::new(device, format));
        }
        let resource = resources.get_mut::<Resources>().unwrap();
        let mut rebind = false;
        let placement_bytes = std::mem::size_of_val(self.placements.as_slice());
        if (resource.placements.size() as usize) < placement_bytes {
            resource.placements = storage(device, "plot-columns-placements", placement_bytes);
            rebind = true;
        }
        if resource.generation != self.generation {
            let upload = &self.upload;
            resource.values = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("plot-columns-values"), contents: bytemuck::cast_slice(&upload.values),
                usage: wgpu::BufferUsages::STORAGE });
            resource.colors = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("plot-columns-colors"), contents: bytemuck::cast_slice(&upload.colors),
                usage: wgpu::BufferUsages::STORAGE });
            resource.generation = self.generation;
            resource.instances = upload.channels * upload.columns * 2;
            rebind = true;
        }
        if rebind {
            resource.bind_group = bind_group(device, &resource.pipeline, &resource.values,
                &resource.uniform, &resource.placements, &resource.colors);
        }
        queue.write_buffer(&resource.uniform, 0, bytemuck::bytes_of(&self.view));
        if !self.placements.is_empty() {
            queue.write_buffer(&resource.placements, 0, bytemuck::cast_slice(&self.placements));
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources) {
        let Some(resource) = resources.get::<Resources>() else { return };
        pass.set_pipeline(&resource.pipeline);
        pass.set_bind_group(0, &resource.bind_group, &[]);
        pass.draw(0..6, 0..resource.instances);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_shader_parses_and_validates() {
        let source = include_str!("plot_columns.wgsl");
        let module = naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|error| panic!("{}", error.emit_to_string(source)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module).unwrap();
    }

    #[test]
    fn uploads_pack_chosen_channels_relative_to_their_origins() {
        let columns = Columns {
            range: 1_000_000..1_000_400,
            bin_samples: 256,
            values: vec![
                vec![Some([10., 12.]), None, Some([11., 30.]), Some([9., 9.])],
                vec![Some([-1., 1.]); 4],
                vec![Some([100_000., 100_002.]), Some([100_001., 100_001.]), None, None],
            ],
        };
        let upload = ColumnUpload::new(&columns, &[(2, Color32::RED), (0, Color32::BLUE)]);
        assert_eq!((upload.columns, upload.channels), (4, 2));
        assert_eq!(upload.x_step, 100.);
        assert_eq!(upload.x_origin, 1_000_050.);
        assert_eq!(upload.y_origin, 100_000.);
        assert_eq!(&upload.values[..4], [[0., 2.], [1., 1.], [f32::INFINITY, f32::NEG_INFINITY],
            [f32::INFINITY, f32::NEG_INFINITY]], "missing columns are inverted pairs");
        assert_eq!(upload.values[4], [-99_990., -99_988.]);
        assert_eq!(upload.colors, [u32::from_le_bytes(Color32::RED.to_array()), u32::from_le_bytes(Color32::BLUE.to_array())]);
    }

    #[test]
    fn the_view_uniform_places_samples_relative_to_the_upload() {
        let columns = Columns { range: 5_000..5_100, bin_samples: 256, values: vec![vec![Some([0., 1.]); 10]] };
        let upload = ColumnUpload::new(&columns, &[(0, Color32::WHITE)]);
        let transform = PlotTransform::new(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200., 100.)),
            egui_plot::PlotBounds::from_min_max([5_000., -1.], [5_100., 3.]), false);
        let view = view_uniform(&upload, &transform);
        assert_eq!(view.bounds, [-5., 95., -1., 3.]);
        assert_eq!((view.size, view.x_step, view.columns, view.channels), ([200., 100.], 10., 10, 1));
        assert_eq!(std::mem::size_of::<ViewUniform>() % 16, 0, "uniform size is 16-byte aligned");
    }

    #[test]
    #[ignore = "requires an available GPU adapter"]
    fn columns_render_on_gpu() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut resources = Resources::new(&device, format);
        // Two channels: a band in the top half, a flat line lower down; a gap at column 2.
        let columns = Columns { range: 0..8, bin_samples: 256, values: vec![
            vec![Some([6., 9.]); 8],
            vec![Some([2., 2.]), Some([2., 2.]), None, Some([2., 2.]), Some([2., 2.]), Some([2., 2.]), Some([2., 2.]), Some([2., 2.])],
        ] };
        let upload = ColumnUpload::new(&columns, &[(0, Color32::RED), (1, Color32::GREEN)]);
        resources.values = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None,
            contents: bytemuck::cast_slice(&upload.values), usage: wgpu::BufferUsages::STORAGE });
        resources.colors = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None,
            contents: bytemuck::cast_slice(&upload.colors), usage: wgpu::BufferUsages::STORAGE });
        resources.placements = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None,
            contents: bytemuck::cast_slice(&[[1f32, 0.], [1., 0.]]), usage: wgpu::BufferUsages::STORAGE });
        resources.bind_group = bind_group(&device, &resources.pipeline, &resources.values, &resources.uniform,
            &resources.placements, &resources.colors);
        let transform = PlotTransform::new(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(64., 64.)),
            egui_plot::PlotBounds::from_min_max([0., 0.], [8., 10.]), false);
        queue.write_buffer(&resources.uniform, 0, bytemuck::bytes_of(&view_uniform(&upload, &transform)));
        let texture = device.create_texture(&wgpu::TextureDescriptor { label: None,
            size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2, format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
        let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 64 * 64 * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &target, depth_slice: None,
                    resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store } })], ..Default::default() });
            pass.set_pipeline(&resources.pipeline);
            pass.set_bind_group(0, &resources.bind_group, &[]);
            pass.draw(0..6, 0..upload.channels * upload.columns * 2);
        }
        encoder.copy_texture_to_buffer(texture.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(64 * 4), rows_per_image: None } },
            wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 });
        queue.submit(Some(encoder.finish()));
        let (sender, receiver) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| sender.send(result).unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        receiver.recv().unwrap().unwrap();
        let pixels = readback.slice(..).get_mapped_range().unwrap();
        let at = |x: usize, y: usize| { let i = (y * 64 + x) * 4; [pixels[i], pixels[i + 1], pixels[i + 2]] };
        let red_rows = (0..64).filter(|&y| (0..64).any(|x| at(x, y) == [255, 0, 0])).collect::<Vec<_>>();
        assert!(!red_rows.is_empty() && *red_rows.first().unwrap() >= 5 && *red_rows.last().unwrap() <= 27,
            "the band spans y 6..9 (rows about 6.4..25.6): {red_rows:?}");
        let green_row = 64 - 2 * 64 / 10;
        let green_columns: Vec<_> = (0..64).filter(|&x| (green_row - 3..green_row + 3).any(|y| at(x, y) == [0, 255, 0])).collect();
        assert!(green_columns.len() > 30, "the flat channel draws across: {green_columns:?}");
        assert!(!(18..=21).all(|x| green_columns.contains(&x)) || green_columns.len() < 64,
            "the gap column has no data");
    }
}
