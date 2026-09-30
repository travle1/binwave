// Min/max columns for many channels in one instanced draw. Each channel column is a
// vertical bar from its min to its max, joined to the next column through their
// midpoints so flat stretches still show. A column whose min is above its max has
// no data yet and draws nothing.
struct View {
    // Visible x (sample) and y bounds, relative to the upload's origins.
    bounds: vec4<f32>,
    size: vec2<f32>,
    // Sample distance between column centres; column 0 is at x = 0.
    x_step: f32,
    columns: u32,
    channels: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
};
@group(0) @binding(0) var<storage, read> values: array<vec2<f32>>;
@group(0) @binding(1) var<uniform> view: View;
// Per-channel display scale and offset, applied to the stored Y values.
@group(0) @binding(2) var<storage, read> placements: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> colors: array<u32>;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn to_pixel(x: f32, y: f32) -> vec2<f32> {
    let span = max(view.bounds.y - view.bounds.x, 0.000001);
    let rise = max(view.bounds.w - view.bounds.z, 0.000001);
    return vec2<f32>((x - view.bounds.x) / span * view.size.x, (view.bounds.w - y) / rise * view.size.y);
}

@vertex fn vs_main(@builtin(vertex_index) vertex: u32,
                   @builtin(instance_index) instance: u32) -> VertexOutput {
    var output: VertexOutput;
    let per_channel = view.columns * 2u;
    let channel = instance / per_channel;
    let column = (instance % per_channel) / 2u;
    let joint = (instance % 2u) == 1u;
    let value = values[channel * view.columns + column];
    let placement = placements[channel];
    var p0 = vec2<f32>(0.0);
    var p1 = vec2<f32>(0.0);
    var draw = value.x <= value.y;
    let x = f32(column) * view.x_step;
    if (joint) {
        draw = draw && column + 1u < view.columns;
        let next = values[channel * view.columns + min(column + 1u, view.columns - 1u)];
        draw = draw && next.x <= next.y;
        p0 = to_pixel(x, (value.x + value.y) * 0.5 * placement.x + placement.y);
        p1 = to_pixel(x + view.x_step, (next.x + next.y) * 0.5 * placement.x + placement.y);
    } else {
        p0 = to_pixel(x, value.x * placement.x + placement.y);
        p1 = to_pixel(x, value.y * placement.x + placement.y);
        // A flat column still shows as one pixel.
        if (abs(p1.y - p0.y) < 1.0) {
            p0.y = p0.y + 0.5;
            p1.y = p1.y - 0.5;
        }
    }
    let direction = p1 - p0;
    let normal = vec2<f32>(-direction.y, direction.x) / max(length(direction), 0.001) * 0.65;
    let start = vertex == 0u || vertex == 1u || vertex == 4u;
    let plus = vertex == 0u || vertex == 2u || vertex == 3u;
    let pixel = select(p1, p0, start) + select(-normal, normal, plus);
    // Columns without data collapse outside the view.
    let clip = select(vec2<f32>(-10.0), vec2<f32>(pixel.x / view.size.x * 2.0 - 1.0,
        1.0 - pixel.y / view.size.y * 2.0), draw);
    output.position = vec4<f32>(clip, 0.0, 1.0);
    let packed = colors[channel];
    output.color = vec4<f32>(f32(packed & 255u), f32((packed >> 8u) & 255u),
        f32((packed >> 16u) & 255u), f32((packed >> 24u) & 255u)) / 255.0;
    return output;
}
@fragment fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> { return input.color; }
