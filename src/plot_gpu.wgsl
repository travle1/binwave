struct Segment { coords: vec4<f32>, color: u32, pad: array<u32, 3> };
struct View { bounds: vec4<f32>, size: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<storage, read> segments: array<Segment>;
@group(0) @binding(1) var<uniform> view: View;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex fn vs_main(@builtin(vertex_index) vertex: u32,
                   @builtin(instance_index) instance: u32) -> VertexOutput {
    let segment = segments[instance];
    let span = max(view.bounds.y - view.bounds.x, 0.000001);
    let rise = max(view.bounds.w - view.bounds.z, 0.000001);
    let p0 = vec2<f32>((segment.coords.x - view.bounds.x) / span * view.size.x,
                       (view.bounds.w - segment.coords.y) / rise * view.size.y);
    let p1 = vec2<f32>((segment.coords.z - view.bounds.x) / span * view.size.x,
                       (view.bounds.w - segment.coords.w) / rise * view.size.y);
    let direction = p1 - p0;
    let normal = vec2<f32>(-direction.y, direction.x) / max(length(direction), 0.001) * 0.65;
    let start = vertex == 0u || vertex == 1u || vertex == 4u;
    let plus = vertex == 0u || vertex == 2u || vertex == 3u;
    let pixel = select(p1, p0, start) + select(-normal, normal, plus);
    var output: VertexOutput;
    output.position = vec4<f32>(pixel.x / view.size.x * 2.0 - 1.0,
        1.0 - pixel.y / view.size.y * 2.0, 0.0, 1.0);
    let packed = segment.color;
    output.color = vec4<f32>(f32(packed & 255u), f32((packed >> 8u) & 255u),
        f32((packed >> 16u) & 255u), f32((packed >> 24u) & 255u)) / 255.0;
    return output;
}
@fragment fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> { return input.color; }
