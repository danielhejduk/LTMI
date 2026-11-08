use std::{
    os::unix::io::OwnedFd,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser;

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        egl::display,
        input::{
            AbsolutePositionEvent, Axis, AxisSource, Event, InputEvent, KeyboardKeyEvent, PointerAxisEvent,
            PointerButtonEvent,
        },
        renderer::{
            Color32F, Frame, Renderer,
            element::{
                Kind,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
            utils::{draw_render_elements, on_commit_buffer_handler},
        },
        winit::{self, WinitEvent},
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xwayland_shell,
    desktop::{PopupKind, PopupManager, WindowSurfaceType, utils::under_from_surface_tree},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, ListeningSocket,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{
                wl_buffer, wl_seat,
                wl_surface::{self, WlSurface},
            },
        },
        winit::platform::pump_events::PumpStatus,
    },
    utils::{Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Serial, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, TraversalAction, with_states,
            with_surface_tree_downward,
        },
        dmabuf::{DmabufState, ImportNotifier},
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler},
        },
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Surface, X11Wm, XWaylandClientData},
};

use crate::{timer::Timer, xwayland::FocusTarget};

mod dmabuf;
mod launch;
mod timer;
mod xwayland;

impl BufferHandler for App {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl XdgShellHandler for App {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.set_toplevel_state(&surface, false);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        self.set_toplevel_state(&surface, true);
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.set_toplevel_state(&surface, false);
        surface.send_configure();
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // Windowed toplevels already fill the output; answer with the unchanged state as the protocol requires.
        surface.send_configure();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // The initial popup geometry is already taken from the positioner; the configure is sent on its first commit.
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("Failed to track popup: {err:?}");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Handle popup grab here
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }
}

impl SelectionHandler for App {
    type SelectionUserData = ();
}

impl DataDeviceHandler for App {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for App {}
impl ServerDndGrabHandler for App {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(xwayland) = client.get_data::<XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);

        // Like toplevels, popups may not attach a buffer until they received their first configure.
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(err) = popup.send_configure()
        {
            tracing::warn!("Failed to configure popup: {err:?}");
        }

        if let Some(toplevel) = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find(|t| t.wl_surface() == surface)
            && !toplevel.is_initial_configure_sent()
        {
            toplevel.send_configure();
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for App {}

impl SeatHandler for App {
    type KeyboardFocus = FocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&FocusTarget>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: smithay::input::pointer::CursorImageStatus) {}
}

struct App {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    #[allow(dead_code)] // must be kept alive for the xdg-output global
    output_manager_state: OutputManagerState,
    xwayland_shell_state: XWaylandShellState,
    dmabuf_state: DmabufState,
    /// Client dmabufs waiting for a test import, see [`dmabuf::import_pending`].
    pending_dmabufs: Vec<(Dmabuf, ImportNotifier)>,
    /// Set once Xwayland is ready.
    xwm: Option<X11Wm>,

    popups: PopupManager,
    /// Mapped X11 windows, managed and override-redirect, oldest first.
    x11_windows: Vec<X11Surface>,

    seat: Seat<Self>,
    output: Output,

    output_size: Size<i32, Logical>,
}

const TIMER_BAR_HEIGHT: i32 = 6;
const TIMER_BAR_TRACK: Color32F = Color32F::new(0.15, 0.15, 0.15, 1.0);
const TIMER_BAR_FILL: Color32F = Color32F::new(0.25, 0.55, 0.95, 1.0);
const TIMER_BAR_DONE: Color32F = Color32F::new(0.3, 0.8, 0.4, 1.0);

fn draw_timer_bar(frame: &mut impl Frame, size: Size<i32, Physical>, progress: f64) -> Result<(), Box<dyn std::error::Error>> {
    let bar = Rectangle::from_size((size.w, TIMER_BAR_HEIGHT.min(size.h)).into());
    let filled = Rectangle::from_size(((f64::from(size.w) * progress).round() as i32, bar.size.h).into());
    let fill = if progress >= 1.0 { TIMER_BAR_DONE } else { TIMER_BAR_FILL };

    for (dst, color) in [(bar, TIMER_BAR_TRACK), (filled, fill)] {
        if !dst.is_empty() {
            frame
                .draw_solid(dst, &[Rectangle::from_size(dst.size)], color)
                .map_err(|err| format!("failed to draw the timer bar: {err:?}"))?;
        }
    }
    Ok(())
}

/// The popups of `toplevel`, topmost first, each with the position of its surface origin in output coordinates.
///
/// Toplevel surfaces are always placed at the output origin.
fn popups_of(
    toplevel: &WlSurface,
    origin: Point<i32, Logical>,
) -> impl Iterator<Item = (WlSurface, Point<i32, Logical>)> + use<> {
    // Popups are positioned relative to the parent's window geometry, which may be inset
    // from its surface origin (e.g. client-side shadows).
    let geometry_loc = with_states(toplevel, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map(|geometry| geometry.loc)
            .unwrap_or_default()
    });
    PopupManager::popups_for_surface(toplevel)
        .map(move |(popup, loc)| (popup.wl_surface().clone(), origin + geometry_loc + loc - popup.geometry().loc))
}

impl App {
    pub fn content_area(&self) -> Rectangle<i32, Logical> {
        let bar = TIMER_BAR_HEIGHT.min(self.output_size.h).max(0);
        Rectangle::new((0, bar).into(), (self.output_size.w, self.output_size.h - bar).into())
    }
    /// Size `surface` to fill the content area, either fullscreen or windowed.
    ///
    /// Windowed toplevels are tiled on every edge rather than maximized, so clients keep their title bar
    /// (Chromium's tab strip) but drop client-side shadows and rounded corners that would not fit the output.
    fn set_toplevel_state(&self, surface: &ToplevelSurface, fullscreen: bool) {
        const TILED: [xdg_toplevel::State; 4] = [
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ];
        let size = self.content_area().size;
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.states.unset(xdg_toplevel::State::Maximized);
            if fullscreen {
                state.states.set(xdg_toplevel::State::Fullscreen);
                TILED.into_iter().for_each(|tiled| _ = state.states.unset(tiled));
            } else {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                TILED.into_iter().for_each(|tiled| _ = state.states.set(tiled));
            }
            state.size = Some(size);
        });
    }

    fn resize_output(&mut self, size: Size<i32, Physical>) {
        let mode = OutputMode { size, refresh: 60_000 };
        self.output.change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        let size = size.to_logical(1);
        self.output_size = size;
        for toplevel in self.xdg_shell_state.toplevel_surfaces() {
            let fullscreen =
                toplevel.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Fullscreen));
            self.set_toplevel_state(toplevel, fullscreen);
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }
        for window in self.x11_windows.iter().filter(|w| !w.is_override_redirect()) {
            self.configure_x11(window);
        }
    }

    /// Give keyboard focus to the newest managed X11 window, otherwise to the first Wayland toplevel.
    ///
    /// X11 windows without a `wl_surface` yet are skipped: focusing them would set X input focus but never
    /// send Xwayland the matching `wl_keyboard.enter`.
    fn refocus(&mut self) {
        let focus = self
            .x11_windows
            .iter()
            .rev()
            .find(|w| !w.is_override_redirect() && w.wl_surface().is_some())
            .map(|w| FocusTarget::X11(w.clone()))
            .or_else(|| {
                let toplevel = self.xdg_shell_state.toplevel_surfaces().first()?;
                Some(FocusTarget::Wayland(toplevel.wl_surface().clone()))
            });
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
        }
    }

    /// The topmost surface under `location` with the position of its origin, searching X11 windows first
    /// (newest on top), then the popups and each toplevel together with its subsurfaces (Chromium draws
    /// tooltips and bubbles as subsurfaces).
    fn surface_under(&self, location: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.x11_windows
            .iter()
            .rev()
            .find_map(|window| {
                let surface = window.wl_surface()?;
                under_from_surface_tree(&surface, location, window.geometry().loc, WindowSurfaceType::ALL)
            })
            .or_else(|| {
                let origin = self.content_area().loc;
                self.xdg_shell_state.toplevel_surfaces().iter().find_map(|toplevel| {
                    let surface = toplevel.wl_surface();
                    popups_of(surface, origin)
                        .find_map(|(popup, loc)| under_from_surface_tree(&popup, location, loc, WindowSurfaceType::ALL))
                        .or_else(|| under_from_surface_tree(surface, location, origin, WindowSurfaceType::ALL))
                })
            })
            .map(|(surface, loc)| (surface, loc.to_f64()))
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to a program
    app: String,

    #[arg(short, long, default_value = "30m", value_parser = humantime::parse_duration)]
    time: Duration,

    /// Argument preset to launch the program with
    #[arg(short, long, value_enum, default_value_t)]
    preset: launch::Preset,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    run_winit(args.app, args.time, args.preset)
}

pub fn run_winit(
    app_name: String,
    duration: Duration,
    preset: launch::Preset,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut display: Display<App> = Display::new()?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<App>(&dh);
    let shm_state = ShmState::new::<App>(&dh, vec![]);
    let mut seat_state = SeatState::new();
    let seat = seat_state.new_wl_seat(&dh, "winit");

    // Chromium-based clients (Brave, Chrome, Electron) won't map a window until
    // the compositor advertises at least one wl_output.
    let output_manager_state = OutputManagerState::new_with_xdg_output::<App>(&dh);
    let output = Output::new(
        "winit".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "ltmi".into(),
            model: "winit".into(),
        },
    );
    output.create_global::<App>(&dh);

    // Xwayland and its window manager are calloop event sources; the loop is dispatched once per frame.
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;

    let mut state = {
        App {
            compositor_state,
            xdg_shell_state: XdgShellState::new::<App>(&dh),
            shm_state,
            seat_state,
            data_device_state: DataDeviceState::new::<App>(&dh),
            output_manager_state,
            // Created before Xwayland starts, since Xwayland binds it to pair its surfaces with X windows.
            xwayland_shell_state: XWaylandShellState::new::<App>(&dh),
            dmabuf_state: DmabufState::new(),
            pending_dmabufs: Vec::new(),
            xwm: None,
            popups: PopupManager::default(),
            x11_windows: Vec::new(),
            seat,
            output,
            output_size: Size::from((0, 0)),
        }
    };

    let listener = ListeningSocket::bind_auto("wayland", 1..33)?;
    let mut clients = Vec::new();

    let (mut backend, mut winit) = winit::init::<GlesRenderer>()?;
    // Before Xwayland and the app start, which only look for it when they connect.
    dmabuf::init(&mut state.dmabuf_state, &dh, backend.renderer());
    dmabuf::import_pending(&mut state, backend.renderer());

    {
        let (renderer, _) = backend.bind().unwrap();
        let display = renderer.egl_context().display().get_display_handle();
        unsafe { smithay::backend::egl::ffi::egl::SwapInterval(**display, 0) };
    }

    state.resize_output(backend.window_size());

    let start_time = std::time::Instant::now();

    let keyboard = state.seat.add_keyboard(Default::default(), 200, 200).unwrap();
    // Without a pointer the seat never advertises the wl_pointer capability, so clients
    // never bind one and receive no mouse events at all.
    let pointer = state.seat.add_pointer();

    // Kept alive until ltmi exits, when dropping it cleans up the X display.
    let xserver = xwayland::start(&dh, &event_loop.handle());
    let x_display = xserver.as_ref().map(|x| x.display);

    let socket_name = listener.socket_name().unwrap().to_owned();
    let socket_name = socket_name.to_str().unwrap();
    let launch_args = launch::get_args(preset);
    let mut app = launch::launch(&app_name, &launch_args, socket_name, x_display)?;
    let mut launched_at = Instant::now();

    let relaunch_args = launch::get_relaunch_args(preset);
    // Keeps an app that dies right after starting from being respawned every frame.
    const MIN_RELAUNCH_INTERVAL: Duration = Duration::from_secs(1);

    let timer = Timer::new(Instant::now(), duration);
    let mut unlocked = false;
    let mut kill_at: Option<Instant> = None;
    let mut focused = true;

    loop {
        let now = Instant::now();
        if !unlocked && timer.is_done(now) {
            unlocked = true;
            tracing::info!("timer done, unlocked");
        }

        // The compositor only exists to host the app, so quit once it exits (this also reaps it).
        // While locked, the app closing itself (its own close button, Ctrl+Q, closing the last tab)
        // must not end the session, so it is started again instead.
        if let Some(status) = app.try_wait()? {
            if unlocked {
                tracing::info!("{app_name} exited with {status}, quitting");
                return Ok(());
            }
            if now.duration_since(launched_at) >= MIN_RELAUNCH_INTERVAL {
                let left = Duration::from_secs(timer.remaining(now).as_secs());
                tracing::info!(
                    "{app_name} exited with {status} while locked ({} left), relaunching",
                    humantime::format_duration(left)
                );
                app = launch::launch(&app_name, &relaunch_args, socket_name, x_display)?;
                launched_at = now;
            }
        }

        if kill_at.is_some_and(|t| now >= t) {
            app.kill()?;
            kill_at = None;
        }

        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                state.resize_output(size);
            }
            WinitEvent::Focus(f) => focused = f,
            WinitEvent::Input(event) => match event {
                InputEvent::Keyboard { event } => {
                    keyboard.input::<(), _>(
                        &mut state,
                        event.key_code(),
                        event.state(),
                        0.into(),
                        start_time.elapsed().as_millis() as u32,
                        |_, _, _| {
                            //
                            FilterResult::Forward
                        },
                    );
                }
                InputEvent::PointerMotionAbsolute { event } => {
                    let location = event.position_transformed(state.output_size);

                    // Keyboard focus stays on the app's window; Chromium routes keys to its menus and bubbles itself.
                    state.refocus();

                    let focus = state.surface_under(location);
                    pointer.motion(
                        &mut state,
                        focus,
                        &MotionEvent {
                            location,
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(&mut state);
                }
                InputEvent::PointerButton { event } => {
                    pointer.button(
                        &mut state,
                        &ButtonEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: event.time_msec(),
                            button: event.button_code(),
                            state: event.state(),
                        },
                    );
                    pointer.frame(&mut state);
                }
                InputEvent::PointerAxis { event } => {
                    let source = event.source();
                    let mut frame = AxisFrame::new(event.time_msec()).source(source);
                    for axis in [Axis::Horizontal, Axis::Vertical] {
                        // Wheel events only carry discrete v120 steps; derive a smooth value
                        // from them (15px per notch, like libinput) for clients that want one.
                        let v120 = event.amount_v120(axis);
                        let amount = event.amount(axis).unwrap_or_else(|| v120.unwrap_or(0.0) * 15.0 / 120.0);
                        if amount != 0.0 {
                            frame = frame
                                .relative_direction(axis, event.relative_direction(axis))
                                .value(axis, amount);
                            if let Some(steps) = v120 {
                                frame = frame.v120(axis, steps as i32);
                            }
                        } else if source == AxisSource::Finger {
                            frame = frame.stop(axis);
                        }
                    }
                    pointer.axis(&mut state, frame);
                    pointer.frame(&mut state);
                }
                _ => {}
            },
            WinitEvent::CloseRequested => {
                let now = Instant::now();
                if timer.is_done(now) {
                    for toplevel in state.xdg_shell_state.toplevel_surfaces() {
                        toplevel.send_close();
                    }
                    for window in state.x11_windows.iter().filter(|w| !w.is_override_redirect()) {
                        let _ = window.close();
                    }
                    kill_at.get_or_insert(now + Duration::from_secs(5));
                } else {
                    let left = Duration::from_secs(timer.remaining(now).as_secs());
                    tracing::info!("close ignored: locked {} left", humantime::format_duration(left));
                }
            }
            _ => (),
        });

        match status {
            PumpStatus::Continue => (),
            PumpStatus::Exit(_) => return Ok(()),
        };

        let size = backend.window_size();
        let damage = Rectangle::from_size(size);
        {
            if focused {
                let (renderer, mut framebuffer) = backend.bind().unwrap();
                // Elements are ordered front to back: X11 windows (newest first) above the Wayland toplevels,
                // and each toplevel's popups before the toplevel itself.
                let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
                for window in state.x11_windows.iter().rev() {
                    if let Some(surface) = window.wl_surface() {
                        elements.extend(render_elements_from_surface_tree(
                            renderer,
                            &surface,
                            window.geometry().loc.to_physical(1),
                            1.0,
                            1.0,
                            Kind::Unspecified,
                        ));
                    }
                }
                let origin = state.content_area().loc;
                for toplevel in state.xdg_shell_state.toplevel_surfaces() {
                    let surface = toplevel.wl_surface();
                    for (popup, loc) in popups_of(surface, origin) {
                        elements.extend(render_elements_from_surface_tree(
                            renderer,
                            &popup,
                            loc.to_physical(1),
                            1.0,
                            1.0,
                            Kind::Unspecified,
                        ));
                    }
                    elements.extend(render_elements_from_surface_tree(
                        renderer,
                        surface,
                        origin.to_physical(1),
                        1.0,
                        1.0,
                        Kind::Unspecified,
                    ));
                }

                let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180).unwrap();
                frame.clear(Color32F::new(0.1, 0.0, 0.0, 1.0), &[damage]).unwrap();
                draw_render_elements(&mut frame, 1.0, &elements, &[damage]).unwrap();
                draw_timer_bar(&mut frame, size, timer.progress(now))?;
                // We rely on the nested compositor to do the sync for us
                let _ = frame.finish().unwrap();
            }

            let time = start_time.elapsed().as_millis() as u32;
            for toplevel in state.xdg_shell_state.toplevel_surfaces() {
                for (popup, _) in popups_of(toplevel.wl_surface(), Point::default()) {
                    send_frames_surface_tree(&popup, time);
                }
                send_frames_surface_tree(toplevel.wl_surface(), time);
            }
            for surface in state.x11_windows.iter().filter_map(X11Surface::wl_surface) {
                send_frames_surface_tree(&surface, time);
            }

            if let Some(stream) = listener.accept()? {
                tracing::info!("Got a client: {:?}", stream);

                let client = display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .unwrap();
                clients.push(client);
            }

            event_loop.dispatch(Some(Duration::ZERO), &mut state)?;
            display.dispatch_clients(&mut state)?;
            state.popups.cleanup();
            display.flush_clients()?;
        }

        // It is important that all events on the display have been dispatched and flushed to clients before
        // swapping buffers because this operation may block.
        // Only present when we actually bound and rendered a frame above.
        if focused {
            backend.submit(Some(&[damage])).unwrap();
        }
    }
}

pub fn send_frames_surface_tree(surface: &wl_surface::WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            // the surface may not have any user_data if it is a subsurface and has not
            // yet been committed
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {
        tracing::info!("initialized");
    }

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        tracing::info!("disconnected");
    }
}

// Macros used to delegate protocol handling to types in the app state.
delegate_xdg_shell!(App);
delegate_compositor!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_data_device!(App);
delegate_output!(App);
delegate_xwayland_shell!(App);
delegate_dmabuf!(App);
