//! The native window: winit events in, egui frames out, drawn on the CPU.
//!
//! This replaces eframe, which can only draw through the GPU. Loading a GPU
//! driver costs far more memory than the editor itself (on Mesa/radv: ~15 MiB
//! of driver plus ~34 MiB of LLVM mapped in, and its own heap), and an editor's
//! frames — flat rectangles and text — are cheap to rasterize in software.
//! Frames are drawn straight into the window system's shared-memory buffer
//! (softbuffer), so there is no intermediate canvas either.
//!
//! Only what rsedit needs is implemented: one window, no child viewports.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{ViewportCommand, ViewportId, ViewportInfo};
use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::window::{Window, WindowId};

use crate::app::EditorApp;
use crate::damage::{Damage, PxRect};

/// After this long without a frame, the window's pixel buffers are handed back:
/// the compositor keeps showing the last frame, and an idle editor shouldn't
/// hold two copies of the screen (~30 MiB for a maximized 1440p window).
const RELEASE_BUFFERS_AFTER: Duration = Duration::from_secs(2);

/// Frame pacing when the monitor's refresh rate is unknown.
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
/// Frames follow the monitor's refresh rate, but no faster than 100 per
/// second: on a 144/240 Hz screen more frames cost CPU without helping an editor.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(10);

/// Largest texture egui may create (its font atlas grows on demand).
const MAX_TEXTURE_SIDE: usize = 8192;

pub enum UserEvent {
    /// egui wants a repaint at `when`; `pass` is the egui pass that asked.
    Repaint { when: Instant, pass: u64 },
    #[cfg(feature = "accessibility")]
    AccessKit(egui_winit::accesskit_winit::Event),
}

#[cfg(feature = "accessibility")]
impl From<egui_winit::accesskit_winit::Event> for UserEvent {
    fn from(e: egui_winit::accesskit_winit::Event) -> Self {
        Self::AccessKit(e)
    }
}

/// Opens the editor window and runs until it is closed.
pub fn run(viewport: egui::ViewportBuilder, arg: Option<PathBuf>) -> Result<(), String> {
    let mut event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| e.to_string())?;
    // Declared after the event loop, so it is dropped first even when a panic
    // unwinds: the window and clipboard must never outlive the display connection.
    let mut runner = Runner {
        proxy: event_loop.create_proxy(),
        viewport,
        arg,
        ctx: egui::Context::default(),
        start: Instant::now(),
        win: None,
        next_repaint: None,
        last_paint: Instant::now(),
        frame_interval: DEFAULT_FRAME_INTERVAL,
        error: None,
    };
    event_loop.run_app_on_demand(&mut runner).map_err(|e| e.to_string())?;
    runner.error.map_or(Ok(()), Err)
}

struct Runner {
    proxy: EventLoopProxy<UserEvent>,
    viewport: egui::ViewportBuilder,
    arg: Option<PathBuf>,
    ctx: egui::Context,
    start: Instant,
    win: Option<Win>,
    /// When egui next wants a frame; `None` = only on input.
    next_repaint: Option<Instant>,
    last_paint: Instant,
    /// Frames come no faster than this: the monitor's refresh interval, capped
    /// at [`MIN_FRAME_INTERVAL`].
    frame_interval: Duration,
    error: Option<String>,
}

/// Everything that exists once the window does.
struct Win {
    window: Arc<Window>,
    sb_context: softbuffer::Context<Arc<Window>>,
    /// Dropped while idle (see [`RELEASE_BUFFERS_AFTER`]), recreated on the next frame.
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    renderer: EguiSoftwareRender,
    /// What changed since the frame in each pixel buffer.
    damage: Damage,
    state: egui_winit::State,
    info: ViewportInfo,
    app: EditorApp,
    /// Clipboard actions requested by viewport commands, run after the frame.
    actions: Vec<egui_winit::ActionRequested>,
}

impl Runner {
    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let window = egui_winit::create_window(&self.ctx, event_loop, &self.viewport)
            .map_err(|e| e.to_string())?;
        let window = Arc::new(window);
        let sb_context = softbuffer::Context::new(window.clone()).map_err(|e| e.to_string())?;

        #[allow(unused_mut)]
        let mut state = egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(MAX_TEXTURE_SIDE),
        );
        #[cfg(feature = "accessibility")]
        state.init_accesskit(event_loop, &window, self.proxy.clone());

        let proxy = self.proxy.clone();
        self.ctx.set_request_repaint_callback(move |info| {
            let _ = proxy.send_event(UserEvent::Repaint {
                when: Instant::now() + info.delay,
                pass: info.current_cumulative_pass_nr,
            });
        });

        let mut info = ViewportInfo::default();
        egui_winit::update_viewport_info(&mut info, &self.ctx, &window, true);
        let app = EditorApp::new(&self.ctx, self.arg.take());
        window.request_redraw();
        self.win = Some(Win {
            window,
            sb_context,
            surface: None,
            // Without caching: the cache is a second full-window canvas plus a
            // copy of every primitive's pixels. Unchanged pixels are skipped
            // by damage tracking instead.
            renderer: EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false),
            damage: Damage::default(),
            state,
            info,
            app,
            actions: Vec::new(),
        });
        Ok(())
    }

    /// Runs one egui pass and draws it. Returns false once the app should quit.
    fn paint(&mut self) -> Result<bool, String> {
        let Some(w) = self.win.as_mut() else {
            return Ok(true);
        };
        self.next_repaint = None;
        self.last_paint = Instant::now();
        let window = w.window.clone();
        self.frame_interval = window
            .current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .filter(|&mhz| mhz >= 1000)
            .map_or(DEFAULT_FRAME_INTERVAL, |mhz| Duration::from_secs(1000) / mhz)
            .max(MIN_FRAME_INTERVAL);

        egui_winit::update_viewport_info(&mut w.info, &self.ctx, &window, false);
        let mut raw = w.state.take_egui_input(&window);
        raw.time = Some(self.start.elapsed().as_secs_f64());
        raw.viewports = std::iter::once((ViewportId::ROOT, w.info.clone())).collect();
        w.app.raw_input_hook(&mut raw);
        let close_requested = raw.viewport().close_requested();

        let app = &mut w.app;
        let mut out = self.ctx.run_ui(raw, |ui| app.frame(ui));
        w.info.events.clear(); // they have been seen by now
        // egui insists (in debug builds) that texture updates are explicitly
        // cleared once applied; dropping them unhandled panics.
        let mut textures = std::mem::take(&mut out.textures_delta);

        w.state.handle_platform_output(&window, std::mem::take(&mut out.platform_output));
        let (commands, repaint_delay) = out
            .viewport_output
            .remove(&ViewportId::ROOT)
            .map_or((Vec::new(), Duration::MAX), |vp| (vp.commands, vp.repaint_delay));
        if close_requested && !commands.contains(&ViewportCommand::CancelClose) {
            textures.clear(); // nothing will be drawn anymore
            w.app.on_exit();
            return Ok(false);
        }
        egui_winit::process_viewport_commands(
            &self.ctx,
            &mut w.info,
            commands,
            &window,
            &mut w.actions,
        );

        let drawn = draw(&self.ctx, w, out.shapes, out.pixels_per_point, &textures);
        textures.clear(); // applied (or lost along with the frame on error)
        drawn?;

        for action in w.actions.drain(..) {
            let input = &mut w.state;
            match action {
                egui_winit::ActionRequested::Cut => {
                    input.egui_input_mut().events.push(egui::Event::Cut)
                }
                egui_winit::ActionRequested::Copy => {
                    input.egui_input_mut().events.push(egui::Event::Copy)
                }
                egui_winit::ActionRequested::Paste => {
                    if let Some(text) = input.clipboard_text() {
                        let text = text.replace("\r\n", "\n");
                        if !text.is_empty() {
                            input.egui_input_mut().events.push(egui::Event::Paste(text));
                        }
                    }
                }
                _ => {}
            }
        }

        // A close command or clipboard action is handled in the next frame.
        let pending_input = !w.info.events.is_empty() || !w.state.egui_input().events.is_empty();
        if pending_input || repaint_delay.is_zero() {
            self.redraw_soon();
        } else if repaint_delay != Duration::MAX {
            self.schedule(Instant::now() + repaint_delay);
        }
        Ok(true)
    }

    /// Asks for a frame as soon as the frame rate allows.
    ///
    /// Without pacing, every input event (a 1000 Hz mouse) and every frame of
    /// an animation would run a full egui pass. Presented frames are throttled
    /// by the compositor on Wayland, but frames that change no pixels are
    /// never presented (see [`draw`]), and X11 doesn't throttle at all.
    fn redraw_soon(&mut self) {
        let earliest = self.last_paint + self.frame_interval;
        match &self.win {
            Some(w) if Instant::now() >= earliest => w.window.request_redraw(),
            _ => self.schedule(earliest),
        }
    }

    fn schedule(&mut self, when: Instant) {
        let when = when.max(self.last_paint + self.frame_interval);
        self.next_repaint = Some(self.next_repaint.map_or(when, |t| t.min(when)));
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        if let Some(w) = self.win.as_mut() {
            w.app.on_error_exit();
        }
        self.error = Some(error);
        event_loop.exit();
    }
}

/// Rasterizes one frame into the window, redrawing only what changed (see
/// [`crate::damage`]), or, while minimized, only applies the texture updates
/// so none are lost.
fn draw(
    ctx: &egui::Context,
    w: &mut Win,
    shapes: Vec<egui::epaint::ClippedShape>,
    pixels_per_point: f32,
    textures: &egui::TexturesDelta,
) -> Result<(), String> {
    let size = w.window.inner_size();
    let minimized = w.window.is_minimized().unwrap_or(false);
    let (Some(width), Some(height), false) =
        (NonZeroU32::new(size.width), NonZeroU32::new(size.height), minimized)
    else {
        w.renderer.apply_textures(textures);
        return Ok(());
    };
    let prims = ctx.tessellate(unbatched(shapes), pixels_per_point);
    // The panels cover the whole window; this only shows through gaps.
    let bg = ctx.global_style().visuals.panel_fill;
    let bg = u32::from_be_bytes([0, bg.r(), bg.g(), bg.b()]);
    let damage = w.damage.diff(&prims, pixels_per_point, width.get(), height.get(), bg, textures);
    if damage.is_empty() {
        // Looks exactly like the frame on screen: nothing to draw or present.
        w.renderer.apply_textures(textures);
        return Ok(());
    }

    let surface = match &mut w.surface {
        Some(s) => s,
        None => w.surface.insert(
            softbuffer::Surface::new(&w.sb_context, w.window.clone())
                .map_err(|e| e.to_string())?,
        ),
    };
    surface.resize(width, height).map_err(|e| e.to_string())?;
    let mut buffer = surface.buffer_mut().map_err(|e| e.to_string())?;
    let (width, height) = (width.get(), height.get());
    let full = PxRect { x0: 0, y0: 0, x1: width, y1: height };
    let regions = w.damage.repaint_regions(&damage, buffer.age()).unwrap_or_else(|| vec![full]);
    for r in &regions {
        for y in r.y0..r.y1 {
            let row = (y * width) as usize;
            buffer[row + r.x0 as usize..row + r.x1 as usize].fill(bg);
        }
    }
    let pixels: &mut [[u8; 4]] = bytemuck::cast_slice_mut(&mut buffer);
    let mut target = BufferMutRef::new(pixels, width as usize, height as usize);
    let bounds: Vec<[usize; 4]> = regions
        .iter()
        .map(|r| [r.x0 as usize, r.y0 as usize, r.x1 as usize, r.y1 as usize])
        .collect();
    w.renderer.render_regions(&mut target, &prims, textures, pixels_per_point, &bounds);
    let rects: Vec<softbuffer::Rect> = regions
        .iter()
        .filter_map(|r| {
            Some(softbuffer::Rect {
                x: r.x0,
                y: r.y0,
                width: NonZeroU32::new(r.x1 - r.x0)?,
                height: NonZeroU32::new(r.y1 - r.y0)?,
            })
        })
        .collect();
    w.window.pre_present_notify();
    buffer.present_with_damage(&rects).map_err(|e| e.to_string())?;
    w.damage.presented(damage);
    Ok(())
}

/// Keeps egui from merging consecutive shapes into one mesh, so that each
/// shape (a line of text, a button) stays its own primitive and a change to it
/// damages only its own pixels. A no-op paint callback between two shapes
/// stops the merge; the rasterizer skips callbacks.
fn unbatched(shapes: Vec<egui::epaint::ClippedShape>) -> Vec<egui::epaint::ClippedShape> {
    use egui::epaint::{ClippedShape, PaintCallback, Shape};
    fn flatten(cs: ClippedShape, out: &mut Vec<ClippedShape>, sep: &PaintCallback) {
        if let Shape::Vec(inner) = cs.shape {
            for shape in inner {
                flatten(ClippedShape { shape, ..cs }, out, sep);
            }
            return;
        }
        if out.last().is_some() {
            out.push(ClippedShape { shape: Shape::Callback(sep.clone()), ..cs });
        }
        out.push(cs);
    }
    let sep = PaintCallback { rect: egui::Rect::NOTHING, callback: Arc::new(()) };
    let mut out = Vec::with_capacity(shapes.len() * 2);
    for cs in shapes {
        flatten(cs, &mut out, &sep);
    }
    out
}

impl ApplicationHandler<UserEvent> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.win.is_none()
            && let Err(e) = self.create_window(event_loop)
        {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    /// Everything that talks to the display server (window, pixel buffers, the
    /// clipboard thread) must go while the event loop still owns the connection:
    /// dropped after `run_app` returns, the Wayland clipboard segfaults.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.win = None;
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, _cause: StartCause) {
        if let (Some(w), Some(when)) = (&self.win, self.next_repaint)
            && when <= Instant::now()
        {
            self.next_repaint = None;
            w.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        if self.win.is_none() {
            return;
        }
        match event {
            UserEvent::Repaint { when, pass } => {
                // Requests from before the last frame have been served already.
                let current = self.ctx.cumulative_pass_nr();
                if current == pass || current == pass + 1 {
                    self.schedule(when);
                }
            }
            #[cfg(feature = "accessibility")]
            UserEvent::AccessKit(e) => {
                use egui_winit::accesskit_winit::WindowEvent as Ak;
                let Some(w) = self.win.as_mut() else { return };
                match e.window_event {
                    Ak::InitialTreeRequested => {
                        self.ctx.enable_accesskit();
                        w.window.request_redraw();
                    }
                    Ak::ActionRequested(request) => {
                        w.state.on_accesskit_action_request(request);
                        w.window.request_redraw();
                    }
                    Ak::AccessibilityDeactivated => self.ctx.disable_accesskit(),
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(w) = self.win.as_mut() else { return };
        match &event {
            WindowEvent::RedrawRequested => {
                match self.paint() {
                    Ok(true) => {}
                    Ok(false) => event_loop.exit(),
                    Err(e) => self.fail(event_loop, format!("drawing the window failed: {e}")),
                }
                return;
            }
            WindowEvent::CloseRequested => {
                // Ask the app (it may have unsaved files) in the next frame.
                w.info.events.push(egui::ViewportEvent::Close);
            }
            WindowEvent::Occluded(occluded) => w.info.occluded = Some(*occluded),
            _ => {}
        }
        let response = w.state.on_window_event(&w.window, &event);
        if response.repaint || matches!(event, WindowEvent::CloseRequested) {
            self.redraw_soon();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let release_at = self.last_paint + RELEASE_BUFFERS_AFTER;
        if let Some(w) = self.win.as_mut()
            && w.surface.is_some()
            && now >= release_at
        {
            w.surface = None;
            w.damage.forget_buffers();
        }
        let holding_buffers = self.win.as_ref().is_some_and(|w| w.surface.is_some());
        let wake = match (self.next_repaint, holding_buffers) {
            (Some(t), true) => Some(t.min(release_at)),
            (Some(t), false) => Some(t),
            (None, true) => Some(release_at),
            (None, false) => None,
        };
        event_loop.set_control_flow(match wake {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}
