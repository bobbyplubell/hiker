//! Custom instanced wgpu paint path for the graph view: GPU-instances the node
//! fills (one unit-quad per node, expanded to `±radius` in the vertex shader, AA
//! circle/box in the fragment shader) and the edge segments (one instance per
//! polyline segment, expanded to a width-quad in the vertex shader), bypassing
//! egui's CPU tessellation for the two highest-volume primitives.
//!
//! ## Two position spaces + a view-transform uniform (the pan-lag fix)
//!
//! The batch stores positions in one of two spaces, chosen by the pane:
//! - **Affine** (lens identity): positions are in **world** space and the node
//!   radius is the *base* (un-zoomed) radius. The per-pane uniform carries the
//!   affine view map as `view_scale` + `view_offset`, and the shader applies
//!   `p = pos * view_scale + view_offset` (and `radius * view_scale`) before the
//!   point→NDC map. A pure pan/zoom therefore leaves every instance untouched —
//!   only the small uniform changes — so the per-pane instance/edge buffers are
//!   **not rebuilt or re-uploaded** while panning (see the `layout_epoch` cache
//!   on `State` + `GraphPaintCallback::affine_cache`). This is what kills the
//!   pan lag: edge upload was the bottleneck.
//! - **Non-affine** (Poincaré / Fisheye): positions are *final egui
//!   screen-points* baked in CPU-side as before (the lens moves every node per
//!   frame, so there's nothing to cache); `view_scale = 1`, `view_offset = 0`.
//!
//! Labels, the hovered-node stroke ring, the disk boundary, tooltips and the
//! preview card all stay on the egui Painter (drawn after the callback, so they
//! layer on top) and still run every frame — only the GPU *fills* are cached.
//!
//! Coordinate transform matches egui's own `egui.wgsl`: positions map → NDC
//! relative to the pane rect (egui-wgpu sets the GPU viewport to the callback
//! rect). Colours are packed **premultiplied** so the standard egui
//! premultiplied alpha blend (`src = One`, `dst = OneMinusSrcAlpha`) composites
//! them exactly like every other egui shape.

use egui_wgpu::wgpu;
use egui_wgpu::{CallbackResources, CallbackTrait, ScreenDescriptor};
use wgpu::util::DeviceExt;

/// One instanced node: a screen-point centre, screen-space radius, a shape
/// discriminant (`0.0` = circle, `1.0` = square) and a premultiplied RGBA fill.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct NodeInstance {
    pub center: [f32; 2],
    pub radius: f32,
    pub shape: f32,
    /// Premultiplied straight-gamma RGBA in `0..1` (alpha last), so the egui
    /// premultiplied blend composites it like any other egui shape.
    pub color: [f32; 4],
}

/// One instanced edge segment: its two endpoints (`a`, `b`) and a premultiplied
/// RGBA colour. The vertex shader expands each into a width-quad (6 verts) using
/// `Uniform.edge_width`; one instance per polyline segment (vs. the old 6
/// vertices per segment — a ~6× cut in edge upload).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct EdgeInstance {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub color: [f32; 4],
}

/// The per-frame batch a pane fills instead of calling the egui Painter, then
/// hands to a [`GraphPaintCallback`]. Pushed into by `draw_nodes`/`draw_edges`.
///
/// `world_space` selects what the pushed positions/radii mean (see the module
/// docs): `true` for the cacheable Affine path (world positions + base radii;
/// the shader applies the view transform), `false` for the lens path (final
/// screen-points + final radii). The pane sets it at construction so the deep
/// draw loop just records the right space.
#[derive(Default)]
pub(super) struct GpuBatch {
    pub nodes: Vec<NodeInstance>,
    pub edges: Vec<EdgeInstance>,
    /// When `true`, callers push *world* node centres + *base* radii (Affine,
    /// cacheable); when `false`, *final screen* centres + radii (lens path).
    pub world_space: bool,
    /// Affine **cache hit**: the pane's GPU buffers already hold this geometry,
    /// so the fill pushes are dropped (the draw loop still runs to build the
    /// per-frame labels + hover affordances). The callback then carries the
    /// matching cache key so `prepare` reuses the uploaded buffers and only
    /// rewrites the view-transform uniform — the pan-lag fix.
    pub cached: bool,
}

impl GpuBatch {
    /// A fresh world-space batch (Affine cacheable path): stores world positions
    /// and base radii. `cached` drops the fill pushes when the GPU buffers
    /// already hold this geometry (labels/hover still build).
    pub(super) fn world(cached: bool) -> Self {
        Self { world_space: true, cached, ..Self::default() }
    }
}

impl GpuBatch {
    /// Pack an egui colour into a premultiplied straight-gamma RGBA float. The
    /// `Color32` bytes are already premultiplied sRGB, exactly the form egui's
    /// own vertex colours take, so this matches the egui blend.
    pub(super) fn pack_color(c: egui::Color32) -> [f32; 4] {
        let p = c.to_array(); // premultiplied [r, g, b, a]
        [
            p[0] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[2] as f32 / 255.0,
            p[3] as f32 / 255.0,
        ]
    }

    /// Push one node fill (circle/square) at a final screen position + radius.
    pub(super) fn push_node(
        &mut self,
        center: egui::Pos2,
        radius: f32,
        square: bool,
        color: egui::Color32,
    ) {
        // Affine cache hit: the GPU already holds this fill — drop the push (the
        // caller still ran the loop for the per-frame labels/hover).
        if self.cached {
            return;
        }
        self.nodes.push(NodeInstance {
            center: [center.x, center.y],
            radius,
            shape: if square { 1.0 } else { 0.0 },
            color: Self::pack_color(color),
        });
    }

    /// Push a polyline (≥ 2 points) as one [`EdgeInstance`] per segment. The
    /// vertex shader expands each into a width-quad using `Uniform.edge_width`
    /// (a screen-px width that does NOT scale with the affine view), so the GPU
    /// edges honour the edge-width control. Joins are unmitred (each segment is
    /// independent) — fine for the thin, finely-sampled edges here. Positions are
    /// world-space under Affine and final screen-points otherwise; the caller
    /// sets the view transform on the callback accordingly.
    pub(super) fn push_polyline(&mut self, pts: &[egui::Pos2], color: egui::Color32) {
        if self.cached {
            return;
        }
        let c = Self::pack_color(color);
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if (b - a).length() < 1e-4 {
                continue;
            }
            self.edges.push(EdgeInstance { a: [a.x, a.y], b: [b.x, b.y], color: c });
        }
    }
}

/// Viewport-transform uniform: the pane rect (in points) the callback draws into,
/// as `origin` + `size`. egui-wgpu sets the GPU **viewport to the callback's
/// rect** (not the whole window — see egui_wgpu::renderer, `set_viewport`), so
/// NDC `[-1,1]` spans the *pane*, not the screen. The shaders therefore map a
/// screen-point `p` via `2·(p − origin)/size − 1`. This MUST be per-pane: the
/// main view and the corner minimap have different rects, and both `prepare`
/// before either `paint`, so a single shared uniform would render one pane with
/// the other's transform.
///
/// It also carries the affine **view transform** (`view_scale` + `view_offset`)
/// the shader applies to each stored position *before* the point→NDC map, plus
/// the screen-px `edge_width`. Under Affine these encode `view.zoom` + the pane
/// translate, so a pan/zoom only rewrites this uniform (positions stay world-
/// space and cached); under a lens they're `1` / `0` (positions are already
/// final screen-points).
///
/// The tail carries the **edge-flow** animation clock + look controls: `time` (a
/// monotonic seconds clock fed from `ui.input(|i| i.time)`), `flow` (`0` = off,
/// `1` = on), and the user-pickable dot appearance — `flow_color` (a vivid,
/// high-contrast hue that reads on both light and dark backgrounds; NOT the edge
/// colour, which washes out over the white code-graph background), `flow_size`
/// (screen-px dot radius), `flow_alpha` (dot opacity), `flow_speed` (cycles/sec)
/// and `flow_density` (dots emitted per edge). These are rewritten every frame
/// just like the view transform, so the flow-dot pipeline animates over the
/// *cached* affine edge buffer with NO geometry re-upload — the dots advance
/// purely because `time` moves in the uniform, and changing a look control only
/// rewrites this small uniform (no instance/edge re-upload). The fields pack to a
/// clean 80 bytes (a multiple of 16, so no implicit tail padding desyncs
/// `bytes_of` from the WGSL `Locals`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniform {
    origin_pts: [f32; 2],
    size_pts: [f32; 2],
    view_offset: [f32; 2],
    view_scale: f32,
    edge_width: f32,
    /// Monotonic seconds clock driving the flow-dot phase (only read when
    /// `flow > 0.5`).
    time: f32,
    /// `0` = no flow dots (byte-identical to the pre-flow path; the flow
    /// pipeline simply isn't drawn), `1` = draw the tracer dots.
    flow: f32,
    /// Screen-px dot radius (NOT view-scaled), from the Size control.
    flow_size: f32,
    /// Dot opacity in `0..1`, from the Opacity control. Multiplies `flow_color`
    /// (which is supplied straight/unpremultiplied; the shader premultiplies).
    flow_alpha: f32,
    /// Dot cycles/second, from the Speed control (replaces the old fixed SPEED).
    flow_speed: f32,
    /// Dots emitted per edge, from the Density control. Stored as `f32` for a
    /// uniform-friendly layout; the shader rounds + clamps `>= 1`.
    flow_density: f32,
    /// Pads to a 16-byte boundary before `flow_color` so `bytes_of` matches the
    /// std140-ish WGSL `Locals` layout with no implicit tail padding.
    _pad: [f32; 2],
    /// User-picked dot hue, STRAIGHT (un-premultiplied) sRGB `0..1`; the shader
    /// applies `flow_alpha` and premultiplies for the egui blend.
    flow_color: [f32; 4],
}

/// A growable GPU buffer: tracks capacity so we `write_buffer` when the data
/// fits and reallocate (recreating any dependent bind group) when it grows.
struct GrowBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
    usage: wgpu::BufferUsages,
}

impl GrowBuffer {
    fn new(device: &wgpu::Device, usage: wgpu::BufferUsages, capacity: u64) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("graph-gpu-buffer"),
            size: capacity.max(16),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { buffer, capacity: capacity.max(16), usage }
    }

    /// Upload `bytes`, reallocating (doubling) when they exceed capacity.
    /// Returns `true` if the underlying buffer was replaced.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) -> bool {
        let needed = bytes.len() as u64;
        if needed == 0 {
            return false;
        }
        let mut grew = false;
        if needed > self.capacity {
            let mut cap = self.capacity.max(16);
            while cap < needed {
                cap *= 2;
            }
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("graph-gpu-buffer"),
                size: cap,
                usage: self.usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = cap;
            grew = true;
        }
        queue.write_buffer(&self.buffer, 0, bytes);
        grew
    }
}

/// Per-callback (per-pane) GPU buffers + draw counts. Keyed by the callback's
/// stable id so the main pane and the minimap — each of which emits its own
/// callback whose `prepare` runs before either `paint` — don't clobber each
/// other's instance data through a single shared buffer.
#[derive(Default)]
struct PaneBuffers {
    instance_buffer: Option<GrowBuffer>,
    edge_buffer: Option<GrowBuffer>,
    /// This pane's own viewport-transform uniform + its bind group (created lazily
    /// on first use). Per-pane so each callback maps points against its own rect.
    uniform_buffer: Option<wgpu::Buffer>,
    bind_group: Option<wgpu::BindGroup>,
    node_count: u32,
    edge_instance_count: u32,
    /// The content key the currently-uploaded instance/edge buffers were built
    /// for. When the incoming callback carries the same key (an affine pan/zoom
    /// at an unchanged layout), `prepare` skips the instance/edge re-upload and
    /// only rewrites the uniform. `None` until the first upload. See
    /// [`GpuCacheKey`].
    cache_key: Option<GpuCacheKey>,
}

/// Identifies the geometry currently in a pane's GPU buffers, so an affine
/// pan/zoom (same key) can skip the rebuild + re-upload. Built only for the
/// affine, world-space path; the lens path passes `None` (rebuild every frame).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct GpuCacheKey {
    /// Bumped whenever the layout geometry changes (relayout / settle frame).
    pub layout_epoch: u64,
    /// Distinguishes content that shares an epoch but differs in what's emitted
    /// (node count, edge-toggle, LOD-ish): a cheap structural fingerprint.
    pub content: u64,
}

/// Persistent GPU resources for the graph paint path, stored in egui's callback
/// type-map and reused across frames + panes. The pipelines, uniform + its bind
/// group are shared; the vertex/instance buffers are per-pane (see
/// [`PaneBuffers`]).
pub(super) struct GraphRenderResources {
    node_pipeline: wgpu::RenderPipeline,
    edge_pipeline: wgpu::RenderPipeline,
    /// Tracer-dot pipeline: instanced over the SAME per-pane edge buffer (reuses
    /// [`EdgeInstance`]; no extra upload). Drawn after the edges when `flow > 0.5`.
    flow_pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    panes: std::collections::HashMap<u64, PaneBuffers>,
}

impl GraphRenderResources {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("graph-gpu-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("graph-gpu-bind-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("graph-gpu-pipeline-layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let node_pipeline = build_node_pipeline(device, &shader, &pipeline_layout, format);
        let edge_pipeline = build_edge_pipeline(device, &shader, &pipeline_layout, format);
        let flow_pipeline = build_flow_pipeline(device, &shader, &pipeline_layout, format);

        Self {
            node_pipeline,
            edge_pipeline,
            flow_pipeline,
            bind_layout,
            panes: std::collections::HashMap::new(),
        }
    }
}

/// The premultiplied-alpha blend egui uses, so our shapes composite identically.
const fn egui_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::OneMinusDstAlpha,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

const fn color_target(format: wgpu::TextureFormat) -> wgpu::ColorTargetState {
    wgpu::ColorTargetState {
        format,
        blend: Some(egui_blend()),
        write_mask: wgpu::ColorWrites::ALL,
    }
}

fn build_node_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    // Per-instance attributes: center(0), radius(1), shape(2), color(3).
    let instance_attrs = wgpu::vertex_attr_array![
        0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32x4
    ];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("graph-gpu-node-pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_node"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<NodeInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &instance_attrs,
            }],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_node"),
            targets: &[Some(color_target(format))],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview: None,
        cache: None,
    })
}

fn build_edge_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    // Per-instance attributes: endpoint a(0), endpoint b(1), color(2). The
    // vertex shader expands a unit segment-quad from `vertex_index`.
    let edge_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("graph-gpu-edge-pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_edge"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<EdgeInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &edge_attrs,
            }],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        primitive: wgpu::PrimitiveState {
            // Width-expanded quads (see `vs_edge`); no culling so either winding
            // shows.
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_edge"),
            targets: &[Some(color_target(format))],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview: None,
        cache: None,
    })
}

fn build_flow_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    // Same per-instance layout as the edge pipeline — the flow pipeline reuses
    // the pane's edge buffer verbatim (one dot per edge instance), so no extra
    // upload is needed. The vertex shader expands a screen-px dot quad at a point
    // animated along the segment by the uniform clock.
    let edge_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("graph-gpu-flow-pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_flow"),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<EdgeInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &edge_attrs,
            }],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_flow"),
            targets: &[Some(color_target(format))],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview: None,
        cache: None,
    })
}

/// Monotonic source of per-pane callback ids, so each `GraphPaintCallback`
/// owns a private slot in [`GraphRenderResources::panes`] (two panes per frame
/// must not share one instance buffer).
static NEXT_CALLBACK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One pane's GPU paint callback: the node instances + edge line vertices for
/// that pane, drawn within the callback's clip rect (egui-wgpu sets the scissor
/// to the clip for us, so no manual clipping is needed).
pub(super) struct GraphPaintCallback {
    /// Stable per-pane slot id (assigned by the caller via [`Self::next_id`]),
    /// reused frame-to-frame so a pane's buffers persist + grow in place.
    id: u64,
    /// The pane rect (in points) this callback draws into — egui-wgpu sets the
    /// GPU viewport to it, so the shader maps points → NDC relative to this rect.
    viewport: egui::Rect,
    /// The affine view transform applied in-shader before the point→NDC map:
    /// `p = pos * view_scale + view_offset`. Under Affine these are `view.zoom`
    /// and the pane translate (so positions stay world-space + cacheable); under
    /// a lens they're `1` / `(0,0)` (positions are already final screen-points).
    view_scale: f32,
    view_offset: [f32; 2],
    /// Screen-px edge width (does NOT scale with `view_scale`); the edge vertex
    /// shader uses it as the quad half-width.
    edge_width: f32,
    /// Edge-flow clock (seconds) + enable + look controls. Written into the
    /// uniform every frame (even on an affine cache hit) so the dots animate —
    /// and any look-control edit takes effect — with no buffer rebuild.
    time: f32,
    flow: f32,
    flow_color: [f32; 4],
    flow_size: f32,
    flow_alpha: f32,
    flow_speed: f32,
    flow_density: f32,
    /// `Some` on the affine path: the geometry key these instances were built
    /// for. When the pane already holds this exact key, `prepare` skips the
    /// instance/edge re-upload. `None` on the lens path (always rebuild).
    cache_key: Option<GpuCacheKey>,
    /// Empty when the callback only refreshes the uniform (affine cache hit):
    /// the pane's existing buffers are reused.
    pub nodes: Vec<NodeInstance>,
    pub edges: Vec<EdgeInstance>,
}

/// The view transform + cache key a pane hands its callback. Bundled so the
/// callback constructor stays a couple of arguments.
#[derive(Clone, Copy)]
pub(super) struct ViewXform {
    pub scale: f32,
    pub offset: [f32; 2],
    pub edge_width: f32,
    /// Edge-flow clock (seconds) + enable (0/1) + look controls. Drive the dots
    /// without touching geometry — see [`FlowParams`].
    pub time: f32,
    pub flow: f32,
    pub flow_color: [f32; 4],
    pub flow_size: f32,
    pub flow_alpha: f32,
    pub flow_speed: f32,
    pub flow_density: f32,
    pub cache_key: Option<GpuCacheKey>,
}

/// The edge-flow animation inputs a pane threads onto its callback: the seconds
/// clock, whether flow is enabled, and the look controls (colour/size/opacity/
/// speed/density). Bundled so the various `ViewXform` constructors take one small
/// argument instead of many loose floats.
#[derive(Clone, Copy)]
pub(super) struct FlowParams {
    pub time: f32,
    pub flow: bool,
    /// Straight (un-premultiplied) sRGB dot hue `0..1`.
    pub color: [f32; 4],
    pub size: f32,
    pub alpha: f32,
    pub speed: f32,
    pub density: f32,
}

impl Default for FlowParams {
    /// Flow off, with the default look (amber, ~3px, 0.9 alpha, density 3, speed
    /// 0.35) so an unset/inert flow still carries sane controls.
    fn default() -> Self {
        Self {
            time: 0.0,
            flow: false,
            color: [1.0, 0.549, 0.102, 1.0], // ~#ff8c1a amber
            size: 3.0,
            alpha: 0.9,
            speed: 0.35,
            density: 3.0,
        }
    }
}

impl ViewXform {
    /// The identity transform for the lens / screen-space path: positions are
    /// already final screen-points, so `scale = 1`, `offset = 0`, and there's no
    /// cache key (the lens moves every node per frame — always rebuild). Edge
    /// width is still applied in-shader.
    pub(super) const fn screen(edge_width: f32, flow: FlowParams) -> Self {
        Self {
            scale: 1.0,
            offset: [0.0, 0.0],
            edge_width,
            time: flow.time,
            flow: if flow.flow { 1.0 } else { 0.0 },
            flow_color: flow.color,
            flow_size: flow.size,
            flow_alpha: flow.alpha,
            flow_speed: flow.speed,
            flow_density: flow.density,
            cache_key: None,
        }
    }
}

impl GraphPaintCallback {
    /// Allocate a fresh, process-unique pane slot id. A pane allocates this once
    /// (cached on its `State`) and reuses it every frame.
    pub(super) fn next_id() -> u64 {
        NEXT_CALLBACK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Build a callback for a known pane slot drawing into `viewport` (points).
    /// `nodes`/`edges` may be empty on an affine cache hit (the pane reuses its
    /// uploaded buffers); `xform` carries the view transform + cache key.
    pub(super) const fn new(
        id: u64,
        viewport: egui::Rect,
        xform: ViewXform,
        nodes: Vec<NodeInstance>,
        edges: Vec<EdgeInstance>,
    ) -> Self {
        Self {
            id,
            viewport,
            view_scale: xform.scale,
            view_offset: xform.offset,
            edge_width: xform.edge_width,
            time: xform.time,
            flow: xform.flow,
            flow_color: xform.flow_color,
            flow_size: xform.flow_size,
            flow_alpha: xform.flow_alpha,
            flow_speed: xform.flow_speed,
            flow_density: xform.flow_density,
            cache_key: xform.cache_key,
            nodes,
            edges,
        }
    }
}

impl CallbackTrait for GraphPaintCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        super::profile_scope!("graph_view::gpu::prepare (upload)");
        let format = resources
            .get::<wgpu::TextureFormat>()
            .copied()
            .unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
        let res = resources
            .entry::<GraphRenderResources>()
            .or_insert_with(|| GraphRenderResources::new(device, format));

        // The transform maps screen-points → NDC relative to THIS pane's rect,
        // because egui-wgpu sets the GPU viewport to the callback rect. Sizes are
        // ppp-independent (egui scales the viewport itself), so points suffice.
        // `view_scale`/`view_offset` apply the affine pan/zoom in-shader so the
        // world-space instances need no rebuild on pan (identity under a lens).
        let uniform = Uniform {
            origin_pts: [self.viewport.min.x, self.viewport.min.y],
            size_pts: [
                self.viewport.width().max(f32::EPSILON),
                self.viewport.height().max(f32::EPSILON),
            ],
            view_offset: self.view_offset,
            view_scale: self.view_scale,
            edge_width: self.edge_width,
            // Rewritten every frame (even on a cache hit below) so the flow dots
            // animate over the cached edge buffer with no geometry re-upload; a
            // look-control edit lands here too (still no instance/edge re-upload).
            time: self.time,
            flow: self.flow,
            flow_size: self.flow_size,
            flow_alpha: self.flow_alpha,
            flow_speed: self.flow_speed,
            flow_density: self.flow_density,
            _pad: [0.0, 0.0],
            flow_color: self.flow_color,
        };
        // Lazily create this pane's uniform buffer + bind group against the shared
        // layout (sequential borrows: the `&res.bind_layout` read ends before the
        // `res.panes` mutation), then refresh the transform every frame.
        let initialized = res.panes.get(&self.id).is_some_and(|p| p.uniform_buffer.is_some());
        if !initialized {
            let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("graph-gpu-uniform"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("graph-gpu-bind"),
                layout: &res.bind_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
            });
            let pane = res.panes.entry(self.id).or_default();
            pane.uniform_buffer = Some(buf);
            pane.bind_group = Some(bind_group);
        }
        let pane = res.panes.entry(self.id).or_default();
        // The uniform (pane rect + view transform) is ALWAYS refreshed — it's the
        // cheap per-frame write that makes the affine pan work without touching
        // the instance/edge buffers.
        queue.write_buffer(pane.uniform_buffer.as_ref().unwrap(), 0, bytemuck::bytes_of(&uniform));

        // Affine cache hit: same geometry key already uploaded — skip the
        // instance/edge rebuild + re-upload entirely. This is the pan-lag fix:
        // panning rewrites only the 32-byte uniform above. (The caller hands an
        // empty `nodes`/`edges` here, but we gate on the key to be explicit.)
        let cache_hit = self.cache_key.is_some()
            && pane.cache_key == self.cache_key
            && pane.instance_buffer.is_some();
        if cache_hit {
            // Affine pan/zoom at an unchanged layout: the heavy instance/edge
            // re-upload is skipped — only the uniform above changed. This span's
            // presence (with no child upload work) in a capture is the proof.
            super::profile_scope!("graph_view::gpu::prepare CACHE HIT (no upload)");
            return Vec::new();
        }

        super::profile_scope!("graph_view::gpu::prepare upload (rebuild)");
        if !self.nodes.is_empty() {
            let buf = pane.instance_buffer.get_or_insert_with(|| {
                GrowBuffer::new(device, wgpu::BufferUsages::VERTEX, 16)
            });
            buf.upload(device, queue, bytemuck::cast_slice(&self.nodes));
        }
        if !self.edges.is_empty() {
            let buf = pane.edge_buffer.get_or_insert_with(|| {
                GrowBuffer::new(device, wgpu::BufferUsages::VERTEX, 16)
            });
            buf.upload(device, queue, bytemuck::cast_slice(&self.edges));
        }
        pane.node_count = self.nodes.len() as u32;
        pane.edge_instance_count = self.edges.len() as u32;
        pane.cache_key = self.cache_key;
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(res) = resources.get::<GraphRenderResources>() else {
            return;
        };
        let Some(pane) = res.panes.get(&self.id) else {
            return;
        };
        let Some(bind_group) = pane.bind_group.as_ref() else {
            return;
        };
        // egui-wgpu already set the scissor + viewport to this callback's rect.
        if let (true, Some(buf)) = (pane.edge_instance_count > 0, &pane.edge_buffer) {
            render_pass.set_pipeline(&res.edge_pipeline);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.set_vertex_buffer(0, buf.buffer.slice(..));
            // 6 verts (the width-quad) per edge instance.
            render_pass.draw(0..6, 0..pane.edge_instance_count);
            // Edge-flow tracer dots: one dot per edge instance, animated along
            // the segment by the uniform clock. Reuses the SAME edge buffer (no
            // extra upload). Drawn over the edges so the dots read as moving
            // highlights. Skipped entirely when flow is off — then this path is
            // byte-identical to the pre-flow render.
            if self.flow > 0.5 {
                render_pass.set_pipeline(&res.flow_pipeline);
                render_pass.set_bind_group(0, bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.buffer.slice(..));
                // `dot_count` dots per edge instance, each a 6-vert quad — widen
                // the vertex range instead of a storage buffer. `vs_flow` derives
                // `dot = vid / 6` and `corner = vid % 6`, spacing the dots evenly
                // along the edge so one is always on-screen at any zoom (Bug 1).
                let dot_count = (self.flow_density.round() as u32).clamp(1, 8);
                render_pass.draw(0..(6 * dot_count), 0..pane.edge_instance_count);
            }
        }
        if let (true, Some(buf)) = (pane.node_count > 0, &pane.instance_buffer) {
            render_pass.set_pipeline(&res.node_pipeline);
            render_pass.set_bind_group(0, bind_group, &[]);
            render_pass.set_vertex_buffer(0, buf.buffer.slice(..));
            render_pass.draw(0..6, 0..pane.node_count);
        }
    }
}

/// The WGSL for both pipelines. The node vertex shader expands a per-instance
/// point into a `±radius`-padded quad (a 1px feather margin for AA) via a
/// `vertex_index`-driven unit corner; the fragment shader does a soft circle /
/// box SDF and outputs the premultiplied colour scaled by AA coverage. The edge
/// shaders are a plain pass-through line.
const SHADER: &str = r#"
struct Locals {
    origin_pts: vec2<f32>,
    size_pts: vec2<f32>,
    view_offset: vec2<f32>,
    view_scale: f32,
    edge_width: f32,
    time: f32,
    flow: f32,
    flow_size: f32,
    flow_alpha: f32,
    flow_speed: f32,
    flow_density: f32,
    pad: vec2<f32>,
    flow_color: vec4<f32>,
};
@group(0) @binding(0) var<uniform> r: Locals;

// Apply the affine view transform to a stored position: world→screen under
// Affine (view_scale = zoom, view_offset = pane translate), identity under a
// lens (positions are already final screen-points; scale 1, offset 0).
fn view(pos: vec2<f32>) -> vec2<f32> {
    return pos * r.view_scale + r.view_offset;
}

// Map a screen-point to NDC relative to the pane rect: egui-wgpu sets the GPU
// viewport to the callback rect, so `[-1,1]` spans the pane, not the window.
fn to_ndc(p: vec2<f32>) -> vec4<f32> {
    let rel = (p - r.origin_pts) / r.size_pts;
    return vec4<f32>(2.0 * rel.x - 1.0, 1.0 - 2.0 * rel.y, 0.0, 1.0);
}

struct NodeOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,   // quad corner in [-1,1] padded units
    @location(1) color: vec4<f32>,   // premultiplied
    @location(2) shape: f32,
    @location(3) radius: f32,
};

@vertex
fn vs_node(
    @builtin(vertex_index) vid: u32,
    @location(0) center: vec2<f32>,
    @location(1) radius: f32,
    @location(2) shape: f32,
    @location(3) color: vec4<f32>,
) -> NodeOut {
    // Two triangles forming a unit quad over corners (-1,-1)..(1,1).
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[vid];
    // The stored centre is in the position space (world under Affine); map it
    // through the view transform. The stored radius is the BASE radius under
    // Affine, so scale it by view_scale to grow node size with zoom (identity
    // under a lens, where the radius is already final). Edge width is a separate
    // screen-px uniform and is NOT view-scaled.
    let screen_center = view(center);
    // Node size scales with zoom but never shrinks below 0.4× (matching the CPU
    // `zoom.max(0.4)` floor) so a far-out view keeps nodes hit-visible. Under a
    // lens view_scale == 1, so this is the identity and the stored radius (already
    // final) is used verbatim.
    let r_screen = radius * max(r.view_scale, 0.4);
    // Pad the quad by 1px (in points) so the AA feather has room outside r.
    let pad = (r_screen + 1.0) / max(r_screen, 0.0001);
    let local = c * pad;
    let screen = screen_center + local * r_screen;
    var out: NodeOut;
    out.position = to_ndc(screen);
    out.local = local;
    out.color = color;
    out.shape = shape;
    out.radius = r_screen;
    return out;
}

@fragment
fn fs_node(in: NodeOut) -> @location(0) vec4<f32> {
    // `local` is in units of `radius`; the feather is ~1px wide.
    let feather = 1.0 / max(in.radius, 0.0001);
    var coverage: f32;
    if (in.shape > 0.5) {
        // Square: in-bounds box, AA on each edge.
        let d = vec2<f32>(1.0) - abs(in.local);
        let m = min(d.x, d.y);
        coverage = smoothstep(0.0, feather, m);
    } else {
        // Circle: distance from centre in radius units, edge at 1.0.
        let dist = length(in.local);
        coverage = 1.0 - smoothstep(1.0 - feather, 1.0, dist);
    }
    if (coverage <= 0.0) { discard; }
    // `color` is premultiplied; scale the whole premultiplied value by coverage.
    return in.color * coverage;
}

struct EdgeOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_edge(
    @builtin(vertex_index) vid: u32,
    @location(0) a: vec2<f32>,
    @location(1) b: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> EdgeOut {
    // Endpoints through the view transform, then expand a screen-space width-quad
    // (edge_width is screen px, NOT view-scaled). Guard a degenerate segment.
    let pa = view(a);
    let pb = view(b);
    let d = pb - pa;
    let len = max(length(d), 1e-4);
    let dir = d / len;
    let perp = vec2<f32>(-dir.y, dir.x) * (r.edge_width * 0.5);
    // Two triangles over the quad corners: a+perp, a-perp, b+perp / a-perp, b-perp, b+perp.
    var corners = array<vec2<f32>, 6>(
        pa + perp, pa - perp, pb + perp,
        pa - perp, pb - perp, pb + perp,
    );
    var out: EdgeOut;
    out.position = to_ndc(corners[vid]);
    out.color = color;
    return out;
}

@fragment
fn fs_edge(in: EdgeOut) -> @location(0) vec4<f32> {
    return in.color;
}

// --- Edge-flow tracer dots ------------------------------------------------
// `dot_count` evenly-spaced dots PER edge instance, instanced over the SAME edge
// buffer (a=caller/from, b=callee/to). The caller draws `6 * dot_count` vertices
// per instance; `vs_flow` derives `dot = vid / 6` (which dot) and `corner = vid
// % 6` (which quad corner), and offsets each dot's phase by `dot/dot_count` so
// they're evenly spread along the edge — guaranteeing one lands in the on-screen
// portion at any zoom (Bug 1: a single dot per edge usually fell off-screen when
// zoomed in). All dots ride a→b as their phase sweeps 0→1, so the caller→callee
// direction still reads. A golden-ratio per-instance offset decorrelates edges so
// dots don't march in lockstep. The dot is a small screen-px quad (NOT
// view-scaled, like edge width) expanded here and AA-circled in the fragment
// shader. Colour is the USER-PICKED `flow_color` × `flow_alpha` (NOT the edge
// colour, which washes out over the white code-graph background — Bug 2).

struct FlowOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,   // quad corner in [-1,1] padded units
    @location(1) color: vec4<f32>,   // premultiplied
    @location(2) radius: f32,        // screen-px dot radius
};

@vertex
fn vs_flow(
    @builtin(vertex_index) vid: u32,
    @location(0) a: vec2<f32>,
    @location(1) b: vec2<f32>,
    @location(2) color: vec4<f32>,
    @builtin(instance_index) i: u32,
) -> FlowOut {
    // One edge instance emits `dot_count` dots, each a 6-vert quad. Split the
    // widened vertex index into "which dot" and "which corner".
    let dot_count = max(u32(round(r.flow_density)), 1u);
    let dot = vid / 6u;
    let corner = vid % 6u;

    // Phase: animate forward (a→b). A golden-ratio per-instance offset spreads
    // edges; `dot/dot_count` spaces this edge's own dots evenly so one is always
    // in the on-screen span. `flow_speed` is cycles/second (the Speed control).
    let edge_phase = fract(f32(i) * 0.6180339887);
    let dot_phase = f32(dot) / f32(dot_count);
    let t = fract(r.time * r.flow_speed + edge_phase + dot_phase);
    // Centre interpolates in the STORED space (world under Affine), then through
    // the view transform — so the dot tracks the edge under pan/zoom for free.
    let center = view(mix(a, b, t));

    // Dot radius in screen px (like edge width: NOT view-scaled), from the Size
    // control. Min 1px so it never collapses.
    let r_dot = max(r.flow_size, 1.0);

    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let c = corners[corner];
    // Pad by 1px (in points) so the AA feather has room outside r.
    let pad = (r_dot + 1.0) / max(r_dot, 0.0001);
    let local = c * pad;
    let screen = center + local * r_dot;

    // The dot colour is the user-picked hue (straight sRGB) at `flow_alpha`,
    // premultiplied for the egui blend — NOT the edge colour. A saturated default
    // (amber) reads on both light and dark backgrounds.
    let a_out = clamp(r.flow_alpha, 0.0, 1.0);
    let out_color = vec4<f32>(r.flow_color.rgb * a_out, a_out);

    var out: FlowOut;
    out.position = to_ndc(screen);
    out.local = local;
    out.color = out_color;
    out.radius = r_dot;
    return out;
}

@fragment
fn fs_flow(in: FlowOut) -> @location(0) vec4<f32> {
    // AA circle SDF (same logic as the node circle): distance in radius units,
    // edge at 1.0, ~1px feather.
    let feather = 1.0 / max(in.radius, 0.0001);
    let dist = length(in.local);
    let coverage = 1.0 - smoothstep(1.0 - feather, 1.0, dist);
    if (coverage <= 0.0) { discard; }
    return in.color * coverage;
}
"#;

/// Insert the surface colour format into egui's callback type-map so the lazy
/// [`GraphRenderResources::new`] builds its pipelines against the live target.
/// Call once at startup from the wgpu render state (see `app/src/main.rs`).
/// Without it the path falls back to `Rgba8Unorm`, which is correct for the
/// kittest wgpu backend the snapshot example uses.
pub fn register_target_format(resources: &mut CallbackResources, format: wgpu::TextureFormat) {
    resources.insert(format);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushing N node fills yields N instances; a K-point polyline yields
    /// `K-1` edge *instances* (one per segment — the vertex shader expands each
    /// to a 6-vert width-quad). Guards the batch-building independent of a live
    /// GPU (the headless fallback proof for the paint path).
    #[test]
    fn batch_counts_match_pushed_geometry() {
        let mut b = GpuBatch::default();
        for i in 0..5 {
            b.push_node(egui::pos2(i as f32, 0.0), 3.0, i % 2 == 0, egui::Color32::RED);
        }
        assert_eq!(b.nodes.len(), 5, "5 nodes pushed -> 5 instances");
        // Shape discriminant round-trips: even indices are squares (1.0).
        assert_eq!(b.nodes[0].shape, 1.0);
        assert_eq!(b.nodes[1].shape, 0.0);

        let pts = [egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0), egui::pos2(2.0, 0.0)];
        b.push_polyline(&pts, egui::Color32::BLUE);
        assert_eq!(b.edges.len(), 2, "3-point polyline -> 2 segments -> 2 edge instances");
        // The segment endpoints round-trip into the instance.
        assert_eq!(b.edges[0].a, [0.0, 0.0]);
        assert_eq!(b.edges[0].b, [1.0, 1.0]);
    }

    /// The uniform packs to a 16-byte-aligned size (no implicit tail padding that
    /// would desync the `bytes_of` upload from the WGSL `Locals` layout).
    #[test]
    fn uniform_is_16_byte_aligned() {
        assert_eq!(std::mem::size_of::<Uniform>() % 16, 0);
        // 16 (origin+size) + 16 (view_offset+scale+edge_width) + 16
        // (time+flow+flow_size+flow_alpha) + 16 (flow_speed+flow_density+pad) +
        // 16 (flow_color) = 80 bytes.
        assert_eq!(std::mem::size_of::<Uniform>(), 80);
    }

    /// A `cached` batch drops every fill push (the GPU buffers are reused on an
    /// affine pan), so the caller can run the draw loop for labels/hover without
    /// re-emitting geometry. A non-cached batch records the pushes as usual.
    #[test]
    fn cached_batch_drops_fill_pushes() {
        let mut hit = GpuBatch::world(true);
        hit.push_node(egui::pos2(0.0, 0.0), 3.0, false, egui::Color32::RED);
        hit.push_polyline(&[egui::pos2(0.0, 0.0), egui::pos2(1.0, 0.0)], egui::Color32::RED);
        assert!(hit.nodes.is_empty() && hit.edges.is_empty(), "cache hit drops fills");
        assert!(hit.world_space);

        let mut miss = GpuBatch::world(false);
        miss.push_node(egui::pos2(0.0, 0.0), 3.0, false, egui::Color32::RED);
        assert_eq!(miss.nodes.len(), 1, "cache miss records the fill");
    }

    /// The geometry cache key only matches when BOTH the layout epoch and the
    /// content fingerprint agree — so a relayout (epoch bump) or a structural
    /// change (content) forces a rebuild, while a pure pan (both unchanged) hits.
    #[test]
    fn cache_key_matches_only_on_identical_geometry() {
        let a = GpuCacheKey { layout_epoch: 7, content: 42 };
        assert_eq!(a, GpuCacheKey { layout_epoch: 7, content: 42 });
        assert_ne!(a, GpuCacheKey { layout_epoch: 8, content: 42 });
        assert_ne!(a, GpuCacheKey { layout_epoch: 7, content: 43 });
    }

    /// Colours pack premultiplied in `0..1` (alpha last), matching the egui blend.
    #[test]
    fn pack_color_is_premultiplied_unit_range() {
        let c = egui::Color32::from_rgba_unmultiplied(255, 0, 0, 128);
        let p = GpuBatch::pack_color(c);
        // Premultiplied red at ~50% alpha: r ≈ a ≈ 0.5, g = b = 0.
        assert!((p[3] - 128.0 / 255.0).abs() < 1e-6);
        assert!(p[0] <= p[3] + 1e-6 && p[0] > 0.0);
        assert_eq!(p[1], 0.0);
        assert_eq!(p[2], 0.0);
    }
}
