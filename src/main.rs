use std::{os::unix::io::OwnedFd, sync::Arc};

use clap::Parser;

use smithay::{
    backend::{
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
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager, WindowSurfaceType, utils::under_from_surface_tree},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    reexports::{
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
        output::{OutputHandler, OutputManagerState},
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, TraversalAction,
            with_states, with_surface_tree_downward,
        },
        selection::{
            SelectionHandler,
            data_device::{ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler},
        },
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
};

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

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>) {
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
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: smithay::input::pointer::CursorImageStatus) {}
}

const SOCKET_NAME: &str = "wayland-5";

struct App {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    #[allow(dead_code)] // must be kept alive for the xdg-output global
    output_manager_state: OutputManagerState,

    popups: PopupManager,

    seat: Seat<Self>,
    output: Output,

    output_size: Size<i32, Logical>,
}

/// The popups of `toplevel`, topmost first, each with the position of its surface origin in output coordinates.
///
/// Toplevel surfaces are always placed at the output origin.
fn popups_of(toplevel: &WlSurface) -> impl Iterator<Item = (WlSurface, Point<i32, Logical>)> + use<> {
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
        .map(move |(popup, loc)| (popup.wl_surface().clone(), geometry_loc + loc - popup.geometry().loc))
}

impl App {
    /// Size `surface` to fill the output, either fullscreen or windowed.
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
        let size = self.output_size;
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
            let fullscreen = toplevel.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Fullscreen));
            self.set_toplevel_state(toplevel, fullscreen);
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }
    }

    /// The topmost surface under `location` with the position of its origin, searching popups first,
    /// then each toplevel together with its subsurfaces (Chromium draws tooltips and bubbles as subsurfaces).
    fn surface_under(&self, location: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find_map(|toplevel| {
                let surface = toplevel.wl_surface();
                popups_of(surface)
                    .find_map(|(popup, loc)| {
                        under_from_surface_tree(&popup, location, loc, WindowSurfaceType::ALL)
                    })
                    .or_else(|| under_from_surface_tree(surface, location, (0, 0), WindowSurfaceType::ALL))
            })
            .map(|(surface, loc)| (surface, loc.to_f64()))
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Program path to a Chromium-based browser
    app: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    run_winit(args.app)
}

pub fn run_winit(app_name: String) -> Result<(), Box<dyn std::error::Error>> {
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

    let mut state = {
        App {
            compositor_state,
            xdg_shell_state: XdgShellState::new::<App>(&dh),
            shm_state,
            seat_state,
            data_device_state: DataDeviceState::new::<App>(&dh),
            output_manager_state,
            popups: PopupManager::default(),
            seat,
            output,
            output_size: Size::from((0, 0)),
        }
    };

    let listener = ListeningSocket::bind(SOCKET_NAME)?;
    let mut clients = Vec::new();

    let (mut backend, mut winit) = winit::init::<GlesRenderer>()?;

    state.resize_output(backend.window_size());

    let start_time = std::time::Instant::now();

    let keyboard = state.seat.add_keyboard(Default::default(), 200, 200).unwrap();
    // Without a pointer the seat never advertises the wl_pointer capability, so clients
    // never bind one and receive no mouse events at all.
    let pointer = state.seat.add_pointer();

    // Brave is single-instance per profile: if the host Brave is already running with the
    // default profile, a new process just forwards its args over the SingletonSocket and exits,
    // so the window opens on the host compositor. A dedicated profile dir forces a new instance.
    let brave_profile = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ltmi/brave-profile");

    if let Err(err) = std::process::Command::new(app_name)
        .args(["--ozone-platform=wayland", format!("--user-data-dir={}", brave_profile.display()).as_str(), "--no-first-run"])
        .env("WAYLAND_DISPLAY", SOCKET_NAME)
        .env_remove("DISPLAY") // Disable host XWayland
        .spawn()
    {
        tracing::warn!("Failed to spawn the app: {err}");
    }

    loop {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                state.resize_output(size);
            }
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

                    // Keyboard focus stays on the toplevel; Chromium routes keys to its menus and bubbles itself.
                    if let Some(toplevel) = state.xdg_shell_state.toplevel_surfaces().first() {
                        let surface = toplevel.wl_surface().clone();
                        keyboard.set_focus(&mut state, Some(surface), 0.into());
                    }

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
                        let amount = event
                            .amount(axis)
                            .unwrap_or_else(|| v120.unwrap_or(0.0) * 15.0 / 120.0);
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
            _ => (),
        });

        match status {
            PumpStatus::Continue => (),
            PumpStatus::Exit(_) => return Ok(()),
        };

        let size = backend.window_size();
        let damage = Rectangle::from_size(size);
        {
            let (renderer, mut framebuffer) = backend.bind().unwrap();
            // Elements are ordered front to back, so each toplevel's popups go before the toplevel itself.
            let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            for toplevel in state.xdg_shell_state.toplevel_surfaces() {
                let surface = toplevel.wl_surface();
                for (popup, loc) in popups_of(surface) {
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
                    (0, 0),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                ));
            }

            let mut frame = renderer
                .render(&mut framebuffer, size, Transform::Flipped180)
                .unwrap();
            frame.clear(Color32F::new(0.1, 0.0, 0.0, 1.0), &[damage]).unwrap();
            draw_render_elements(&mut frame, 1.0, &elements, &[damage]).unwrap();
            // We rely on the nested compositor to do the sync for us
            let _ = frame.finish().unwrap();

            let time = start_time.elapsed().as_millis() as u32;
            for toplevel in state.xdg_shell_state.toplevel_surfaces() {
                for (popup, _) in popups_of(toplevel.wl_surface()) {
                    send_frames_surface_tree(&popup, time);
                }
                send_frames_surface_tree(toplevel.wl_surface(), time);
            }

            if let Some(stream) = listener.accept()? {
                tracing::info!("Got a client: {:?}", stream);

                let client = display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .unwrap();
                clients.push(client);
            }

            display.dispatch_clients(&mut state)?;
            state.popups.cleanup();
            display.flush_clients()?;
        }

        // It is important that all events on the display have been dispatched and flushed to clients before
        // swapping buffers because this operation may block.
        backend.submit(Some(&[damage])).unwrap();
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
