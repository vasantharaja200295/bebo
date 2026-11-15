//! GPU renderer. Draws colored rectangles and text rows with wgpu and glyphon.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap, fontdb,
};
use winit::window::Window;

pub struct Quad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [u8; 4],
}

#[derive(Hash)]
pub struct Span {
    pub text: String,
    pub color: [u8; 3],
    pub bold: bool,
    pub italic: bool,
}

pub struct Row {
    pub x: f32,
    pub y: f32,
    /// Clip rectangle: left, top, right, bottom.
    pub clip: [f32; 4],
    pub spans: Vec<Span>,
}

#[derive(Default)]
pub struct Frame {
    /// Rectangles below the text.
    pub quads: Vec<Quad>,
    pub rows: Vec<Row>,
    /// Rectangles above the text.
    pub overlay: Vec<Quad>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    color: [f32; 4],
}

const SHADER: &str = "
struct Out { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> Out {
    var out: Out;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    return out;
}
@fragment fn fs(in: Out) -> @location(0) vec4<f32> { return in.color; }
";

/// The GPU device. `init_device` makes it on a second thread while the window and the shell start.
pub struct Device {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

pub fn init_device(t0: std::time::Instant) -> Device {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    // OpenGL is the default: on the test PC it starts in about 250 ms and uses about 90 MB.
    // DirectX 12 needs about 600 ms and 160 MB, because wgpu examines each adapter. Set WISP_BACKEND=dx12 to use it.
    descriptor.backends = match std::env::var("WISP_BACKEND").as_deref() {
        Ok("dx12") => wgpu::Backends::DX12,
        _ => wgpu::Backends::GL,
    };
    let instance = wgpu::Instance::new(descriptor);
    // The dedicated GPU is the default. Set WISP_GPU=low to use the integrated GPU.
    let power_preference = match std::env::var("WISP_GPU").as_deref() {
        Ok("low") => wgpu::PowerPreference::LowPower,
        _ => wgpu::PowerPreference::HighPerformance,
    };
    let adapter = pollster::block_on(
        instance.request_adapter(&wgpu::RequestAdapterOptions { power_preference, ..Default::default() }),
    )
    .expect("no GPU adapter");
    tick(t0, "adapter (thread)");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("no GPU device");
    tick(t0, "device (thread)");
    Device { instance, adapter, device, queue }
}

pub struct Gpu {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    vertices: wgpu::Buffer,
    vertex_capacity: u64,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    family: String,
    /// Shaped text for each row content. The key is the hash of the spans.
    row_cache: HashMap<u64, (Buffer, u64)>,
    frame_number: u64,

    pub font_px: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub adapter_name: String,

    // The window must drop after the surface.
    window: Arc<Window>,
}

impl Gpu {
    pub fn new(window: Arc<Window>, parts: Device, font_px: f32, t0: std::time::Instant) -> Self {
        let Device { instance, adapter, device, queue } = parts;
        let size = window.inner_size();
        let surface = instance.create_surface(window.clone()).expect("create surface");
        tick(t0, "surface");
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        tick(t0, "surface configured");
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                })],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let vertex_capacity = 64 * 1024;
        let vertices = Self::vertex_buffer(&device, vertex_capacity);

        tick(t0, "pipeline");
        // Load only the fonts that wisp uses. A scan of all system fonts is slow.
        let mut db = fontdb::Database::new();
        for file in ["CascadiaMono.ttf", "consola.ttf", "consolab.ttf", "seguisym.ttf", "seguiemj.ttf"] {
            let _ = db.load_font_file(format!(r"C:\Windows\Fonts\{file}"));
        }
        let family = if db.faces().any(|f| f.families.iter().any(|(n, _)| n == "Cascadia Mono")) {
            "Cascadia Mono"
        } else {
            "Consolas"
        }
        .to_string();
        let font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);

        tick(t0, "fonts");
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer = TextRenderer::new(&mut atlas, &device, Default::default(), None);

        tick(t0, "text renderer");
        let mut gpu = Self {
            adapter_name: adapter.get_info().name,
            instance,
            device,
            queue,
            surface,
            config,
            pipeline,
            vertices,
            vertex_capacity,
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            text_renderer,
            family,
            row_cache: HashMap::new(),
            frame_number: 0,
            font_px,
            cell_w: 1.0,
            cell_h: 1.0,
            window,
        };
        gpu.set_font_px(font_px);
        gpu
    }

    fn vertex_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn set_font_px(&mut self, font_px: f32) {
        self.font_px = font_px.clamp(6.0, 96.0);
        self.cell_h = (self.font_px * 1.3).ceil();
        let mut probe = Buffer::new(&mut self.font_system, Metrics::new(self.font_px, self.cell_h));
        let attrs = Attrs::new().family(Family::Name(&self.family));
        probe.set_text("M", &attrs, Shaping::Advanced, None);
        probe.shape_until_scroll(&mut self.font_system, false);
        let advance = probe.layout_runs().next().and_then(|run| run.glyphs.first().map(|g| g.w));
        self.cell_w = advance.unwrap_or(self.font_px * 0.6).round().max(1.0);
        self.row_cache.clear();
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn size(&self) -> (f32, f32) {
        (self.config.width as f32, self.config.height as f32)
    }

    pub fn render(&mut self, frame: &Frame, background: [u8; 3]) {
        self.frame_number += 1;
        let (width, height) = self.size();
        self.viewport.update(&self.queue, Resolution { width: self.config.width, height: self.config.height });

        // Shape each row that is not in the cache.
        let mut keys = Vec::with_capacity(frame.rows.len());
        for row in &frame.rows {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            row.spans.hash(&mut hasher);
            let key = hasher.finish();
            keys.push(key);
            let entry = self.row_cache.entry(key).or_insert_with(|| {
                let base = Attrs::new().family(Family::Name(&self.family));
                let mut buffer =
                    Buffer::new(&mut self.font_system, Metrics::new(self.font_px, self.cell_h));
                buffer.set_wrap(Wrap::None);
                buffer.set_monospace_width(Some(self.cell_w));
                buffer.set_size(None, Some(self.cell_h));
                let spans = row.spans.iter().map(|s| {
                    let mut attrs = base.clone().color(Color::rgb(s.color[0], s.color[1], s.color[2]));
                    if s.bold {
                        attrs = attrs.weight(Weight::BOLD);
                    }
                    if s.italic {
                        attrs = attrs.style(Style::Italic);
                    }
                    (s.text.as_str(), attrs)
                });
                buffer.set_rich_text(spans, &base, Shaping::Advanced, None);
                buffer.shape_until_scroll(&mut self.font_system, false);
                (buffer, 0)
            });
            entry.1 = self.frame_number;
        }

        let areas = frame.rows.iter().zip(&keys).map(|(row, key)| TextArea {
            buffer: &self.row_cache[key].0,
            left: row.x,
            top: row.y,
            scale: 1.0,
            bounds: TextBounds {
                left: row.clip[0] as i32,
                top: row.clip[1] as i32,
                right: row.clip[2] as i32,
                bottom: row.clip[3] as i32,
            },
            default_color: Color::rgb(255, 255, 255),
            custom_glyphs: &[],
        });
        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
            )
            .expect("prepare text");

        // Rectangles: two triangles for each.
        let mut vertices: Vec<Vertex> = Vec::with_capacity((frame.quads.len() + frame.overlay.len()) * 6);
        for q in frame.quads.iter().chain(&frame.overlay) {
            let color = [linear(q.color[0]), linear(q.color[1]), linear(q.color[2]), q.color[3] as f32 / 255.0];
            let (x0, y0) = (q.x / width * 2.0 - 1.0, 1.0 - q.y / height * 2.0);
            let (x1, y1) = ((q.x + q.w) / width * 2.0 - 1.0, 1.0 - (q.y + q.h) / height * 2.0);
            for pos in [[x0, y0], [x1, y0], [x0, y1], [x0, y1], [x1, y0], [x1, y1]] {
                vertices.push(Vertex { pos, color });
            }
        }
        let bytes: &[u8] = bytemuck::cast_slice(&vertices);
        if bytes.len() as u64 > self.vertex_capacity {
            self.vertex_capacity = (bytes.len() as u64).next_power_of_two();
            self.vertices = Self::vertex_buffer(&self.device, self.vertex_capacity);
        }
        self.queue.write_buffer(&self.vertices, 0, bytes);
        let below = frame.quads.len() as u32 * 6;
        let total = vertices.len() as u32;

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            },
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).expect("create surface");
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            },
            wgpu::CurrentSurfaceTexture::Validation => panic!("surface validation error"),
        };
        let view = surface_texture.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let clear = wgpu::Color {
                r: linear(background[0]) as f64,
                g: linear(background[1]) as f64,
                b: linear(background[2]) as f64,
                a: 1.0,
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_vertex_buffer(0, self.vertices.slice(..));
            pass.draw(0..below, 0..1);
            self.text_renderer.render(&self.atlas, &self.viewport, &mut pass).expect("render text");
            if total > below {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, self.vertices.slice(..));
                pass.draw(below..total, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(surface_texture);

        // Keep only the rows that this frame used.
        let frame_number = self.frame_number;
        self.row_cache.retain(|_, entry| entry.1 == frame_number);
        self.atlas.trim();
    }
}

/// Start-up timing log. `main` writes it to a file if WISP_TIMING is set.
pub static TIMING: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn tick(start: std::time::Instant, label: &str) {
    TIMING.lock().unwrap().push_str(&format!("{:>5} ms  {label}
", start.elapsed().as_millis()));
}

/// sRGB byte to linear value. The surface format is sRGB.
fn linear(c: u8) -> f32 {
    (c as f32 / 255.0).powf(2.2)
}
