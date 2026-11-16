//! GPU renderer. Draws rounded rectangles and text rows with wgpu and glyphon.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap, fontdb,
};
use winit::window::Window;

#[derive(Clone, Copy)]
pub struct Quad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [u8; 4],
    /// Corner radius in pixels. 0 is a plain rectangle.
    pub radius: f32,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Default)]
pub enum Font {
    /// Terminal font, fixed width.
    #[default]
    Mono,
    /// Interface font.
    Ui,
    UiBold,
    /// Lucide icons.
    Icon,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Align {
    #[default]
    Left,
    Right,
    Center,
}

#[derive(Hash)]
pub struct Span {
    pub text: String,
    pub color: [u8; 3],
    pub bold: bool,
    pub italic: bool,
}

#[derive(Default)]
pub struct Row {
    /// Anchor point. `align` tells which side of the text is at `x`.
    pub x: f32,
    pub y: f32,
    /// Clip rectangle: left, top, right, bottom.
    pub clip: [f32; 4],
    pub spans: Vec<Span>,
    pub font: Font,
    /// Font size in pixels. 0 selects the terminal font size.
    pub size: f32,
    pub align: Align,
    /// Transparency. 0 is opaque and 255 is invisible.
    pub fade: u8,
}

#[derive(Default)]
pub struct Frame {
    /// Rectangles below the terminal text.
    pub quads: Vec<Quad>,
    pub rows: Vec<Row>,
    /// Rectangles above the terminal text (popups, bars).
    pub overlay: Vec<Quad>,
    /// Text above the overlay rectangles.
    pub top_rows: Vec<Row>,
    /// Rectangles and text of popups. They are above all other layers.
    pub popup: Vec<Quad>,
    pub popup_rows: Vec<Row>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 2],
    color: [f32; 4],
    /// Pixel position from the rectangle center.
    local: [f32; 2],
    half: [f32; 2],
    radius: f32,
}

const SHADER: &str = "
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
    @location(2) half: vec2<f32>,
    @location(3) radius: f32,
};
@vertex fn vs(
    @location(0) pos: vec2<f32>, @location(1) color: vec4<f32>, @location(2) local: vec2<f32>,
    @location(3) half: vec2<f32>, @location(4) radius: f32,
) -> Out {
    var out: Out;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    out.local = local;
    out.half = half;
    out.radius = radius;
    return out;
}
@fragment fn fs(in: Out) -> @location(0) vec4<f32> {
    if (in.radius <= 0.0) { return in.color; }
    // Signed distance to a rounded box. The edge is smooth for one pixel.
    let q = abs(in.local) - in.half + vec2<f32>(in.radius);
    let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - in.radius;
    return vec4<f32>(in.color.rgb, in.color.a * clamp(0.5 - d, 0.0, 1.0));
}
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
    // DirectX 12 needs about 600 ms and 160 MB, because wgpu examines each adapter. Set BEBO_BACKEND=dx12 to use it.
    descriptor.backends = match std::env::var("BEBO_BACKEND").as_deref() {
        Ok("dx12") => wgpu::Backends::DX12,
        _ => wgpu::Backends::GL,
    };
    let instance = wgpu::Instance::new(descriptor);
    // The dedicated GPU is the default. Set BEBO_GPU=low to use the integrated GPU.
    let power_preference = match std::env::var("BEBO_GPU").as_deref() {
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
    /// One text renderer for each layer: terminal text, interface text, popup text.
    text: [TextRenderer; 3],
    mono: String,
    /// All installed font families: (name, fixed width). Empty until `families` reads the system fonts.
    families: Vec<(String, bool)>,
    ui: String,
    /// Shaped text for each row content. The key is the hash of the row.
    row_cache: HashMap<u64, (Buffer, f32, u64)>,
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
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x4, 2 => Float32x2, 3 => Float32x2, 4 => Float32
                    ],
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
        tick(t0, "pipeline");
        let vertex_capacity = 256 * 1024;
        let vertices = Self::vertex_buffer(&device, vertex_capacity);

        // Load only the fonts that bebo uses. A scan of all system fonts is slow.
        let mut db = fontdb::Database::new();
        let user_fonts = std::env::var("LOCALAPPDATA").unwrap_or_default() + r"\Microsoft\Windows\Fonts\";
        // Inter is the interface font. If it is not installed, Segoe UI is the interface font.
        for file in ["JetBrainsMonoNL-Regular.ttf", "JetBrainsMonoNL-Bold.ttf", "JetBrainsMonoNL-Italic.ttf", "Inter.ttc"] {
            if db.load_font_file(format!("{user_fonts}{file}")).is_err() {
                let _ = db.load_font_file(format!(r"C:\Windows\Fonts\{file}"));
            }
        }
        let system = [
            "CascadiaMono.ttf", "consola.ttf", "consolab.ttf", "segoeui.ttf", "seguisb.ttf", "seguisym.ttf",
            "seguiemj.ttf",
        ];
        for file in system {
            let _ = db.load_font_file(format!(r"C:\Windows\Fonts\{file}"));
        }
        db.load_font_source(fontdb::Source::Binary(Arc::new(lucide_icons::LUCIDE_FONT_BYTES)));
        let has = |name: &str| db.faces().any(|f| f.families.iter().any(|(n, _)| n == name));
        let mono = ["JetBrains Mono NL", "Cascadia Mono", "Consolas"].into_iter().find(|n| has(n)).unwrap_or("Consolas").to_string();
        let ui = if has("Inter") { "Inter" } else { "Segoe UI" }.to_string();
        let font_system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        tick(t0, "fonts");

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text = [(); 3].map(|_| TextRenderer::new(&mut atlas, &device, Default::default(), None));
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
            text,
            mono,
            families: Vec::new(),
            ui,
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

    pub fn mono(&self) -> &str {
        &self.mono
    }

    /// All installed font families: (name, fixed width). Fixed-width fonts are first.
    /// The first call reads the system font folders. That is too slow for the start of bebo.
    pub fn families(&mut self) -> &[(String, bool)] {
        if self.families.is_empty() {
            let db = self.font_system.db_mut();
            db.load_system_fonts();
            let mut list: Vec<(String, bool)> =
                db.faces().filter_map(|f| f.families.first().map(|(n, _)| (n.clone(), f.monospaced))).collect();
            list.retain(|(n, _)| n != "lucide");
            list.sort_by_key(|(n, fixed)| (!fixed, n.to_lowercase()));
            list.dedup_by(|a, b| a.0 == b.0);
            self.families = list;
        }
        &self.families
    }

    /// Selects the terminal font. A name that is not installed changes nothing.
    pub fn set_mono(&mut self, name: &str) {
        let has = |db: &fontdb::Database| db.faces().any(|f| f.families.iter().any(|(n, _)| n == name));
        if name.is_empty() || name == self.mono {
            return;
        }
        if !has(self.font_system.db()) {
            self.families();
        }
        if has(self.font_system.db()) {
            self.mono = name.to_string();
            self.set_font_px(self.font_px);
        }
    }

    pub fn set_font_px(&mut self, font_px: f32) {
        self.font_px = font_px.clamp(6.0, 96.0);
        self.cell_h = (self.font_px * 1.25).ceil();
        let mut probe = Buffer::new(&mut self.font_system, Metrics::new(self.font_px, self.cell_h));
        let attrs = Attrs::new().family(Family::Name(&self.mono));
        probe.set_text("M", &attrs, Shaping::Advanced, None);
        probe.shape_until_scroll(&mut self.font_system, false);
        let advance = probe.layout_runs().next().and_then(|run| run.glyphs.first().map(|g| g.w));
        // The cell width is the exact advance of the font. A rounded width moves the cursor away from the text.
        // ponytail: a font that is not fixed-width, and wide characters from a fallback font, do not stay on the cell grid.
        // Fix: draw each cell as its own text area.
        self.cell_w = advance.unwrap_or(self.font_px * 0.6).max(1.0);
        self.row_cache.clear();
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        // Windows can report a wrong, very large size during a resize. A size above the limit stops wgpu.
        let max = self.device.limits().max_texture_dimension_2d;
        self.config.width = width.clamp(1, max);
        self.config.height = height.clamp(1, max);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn size(&self) -> (f32, f32) {
        (self.config.width as f32, self.config.height as f32)
    }

    /// Shapes the row if it is not in the cache. Returns the cache key and the text width.
    fn shape(&mut self, row: &Row) -> (u64, f32) {
        let size = if row.size > 0.0 { row.size } else { self.font_px };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        row.spans.hash(&mut hasher);
        row.font.hash(&mut hasher);
        size.to_bits().hash(&mut hasher);
        row.fade.hash(&mut hasher);
        let key = hasher.finish();
        let frame_number = self.frame_number;
        let entry = self.row_cache.entry(key).or_insert_with(|| {
            let mono = row.font == Font::Mono;
            let family = match row.font {
                Font::Mono => self.mono.as_str(),
                Font::Ui | Font::UiBold => self.ui.as_str(),
                Font::Icon => "lucide",
            };
            let mut base = Attrs::new().family(Family::Name(family));
            if row.font == Font::UiBold {
                base = base.weight(Weight::SEMIBOLD);
            }
            let line_height = if mono && row.size == 0.0 { self.cell_h } else { (size * 1.4).ceil() };
            let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(size, line_height));
            buffer.set_wrap(Wrap::None);
            buffer.set_size(None, Some(line_height));
            let spans = row.spans.iter().map(|s| {
                let mut attrs = base.clone().color(Color::rgba(s.color[0], s.color[1], s.color[2], 255 - row.fade));
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
            let width = buffer.layout_runs().next().map_or(0.0, |run| run.line_w);
            (buffer, width, 0)
        });
        entry.2 = frame_number;
        (key, entry.1)
    }

    /// Width in pixels of `text` in the interface font.
    pub fn text_width(&mut self, text: &str, font: Font, size: f32) -> f32 {
        let span = Span { text: text.to_string(), color: [0; 3], bold: false, italic: false };
        self.shape(&Row { spans: vec![span], font, size, ..Default::default() }).1
    }

    pub fn render(&mut self, frame: &Frame, background: [u8; 3]) {
        self.frame_number += 1;
        let (width, height) = self.size();
        self.viewport.update(&self.queue, Resolution { width: self.config.width, height: self.config.height });

        for (layer, rows) in [&frame.rows, &frame.top_rows, &frame.popup_rows].into_iter().enumerate() {
            let shaped: Vec<(u64, f32)> = rows.iter().map(|row| self.shape(row)).collect();
            let areas = rows.iter().zip(&shaped).map(|(row, (key, w))| TextArea {
                buffer: &self.row_cache[key].0,
                left: match row.align {
                    Align::Left => row.x,
                    Align::Right => row.x - w,
                    Align::Center => row.x - w / 2.0,
                },
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
            self.text[layer]
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
        }

        // Rectangles: two triangles for each.
        let mut vertices: Vec<Vertex> = Vec::with_capacity((frame.quads.len() + frame.overlay.len() + frame.popup.len()) * 6);
        for q in frame.quads.iter().chain(&frame.overlay).chain(&frame.popup) {
            let color = [linear(q.color[0]), linear(q.color[1]), linear(q.color[2]), q.color[3] as f32 / 255.0];
            let (x0, y0) = (q.x / width * 2.0 - 1.0, 1.0 - q.y / height * 2.0);
            let (x1, y1) = ((q.x + q.w) / width * 2.0 - 1.0, 1.0 - (q.y + q.h) / height * 2.0);
            let half = [q.w / 2.0, q.h / 2.0];
            let radius = q.radius.min(half[0]).min(half[1]);
            let corners = [(x0, y0, -1.0, -1.0), (x1, y0, 1.0, -1.0), (x0, y1, -1.0, 1.0), (x1, y1, 1.0, 1.0)];
            for i in [0, 1, 2, 2, 1, 3] {
                let (x, y, sx, sy) = corners[i];
                vertices.push(Vertex { pos: [x, y], color, local: [sx * half[0], sy * half[1]], half, radius });
            }
        }
        let bytes: &[u8] = bytemuck::cast_slice(&vertices);
        if bytes.len() as u64 > self.vertex_capacity {
            self.vertex_capacity = (bytes.len() as u64).next_power_of_two();
            self.vertices = Self::vertex_buffer(&self.device, self.vertex_capacity);
        }
        self.queue.write_buffer(&self.vertices, 0, bytes);
        // The vertex ranges of the three layers.
        let below = frame.quads.len() as u32 * 6;
        let middle = below + frame.overlay.len() as u32 * 6;
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
            // Order: rectangles, terminal text, overlay rectangles, overlay text, popup rectangles, popup text.
            for (layer, range) in [0..below, below..middle, middle..total].into_iter().enumerate() {
                if !range.is_empty() {
                    pass.set_pipeline(&self.pipeline);
                    pass.set_vertex_buffer(0, self.vertices.slice(..));
                    pass.draw(range, 0..1);
                }
                self.text[layer].render(&self.atlas, &self.viewport, &mut pass).expect("render text");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(surface_texture);

        // Keep only the rows that this frame used.
        let frame_number = self.frame_number;
        self.row_cache.retain(|_, entry| entry.2 + 1 >= frame_number);
        self.atlas.trim();
    }
}

/// Start-up timing log. `main` writes it to a file if BEBO_TIMING is set.
pub static TIMING: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn tick(start: std::time::Instant, label: &str) {
    TIMING.lock().unwrap().push_str(&format!("{:>5} ms  {label}\n", start.elapsed().as_millis()));
}

/// sRGB byte to linear value. The surface format is sRGB.
fn linear(c: u8) -> f32 {
    (c as f32 / 255.0).powf(2.2)
}
