//! Hosting X11 apps through Xwayland, with ltmi as their X11 window manager.

use std::{borrow::Cow, process::Stdio};

use smithay::{
    backend::input::KeyState,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::{
        calloop::{LoopHandle, RegistrationToken},
        wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Rectangle, Serial},
    wayland::{
        seat::WaylandFocus,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, X11Window, XwmId},
    },
};

use crate::App;

/// A running Xwayland. Dropping it stops Xwayland and removes its X11 socket and lock file.
pub struct XServer {
    /// The X display number, as in `DISPLAY=:<n>`.
    pub display: u32,
    handle: LoopHandle<'static, App>,
    token: RegistrationToken,
}

impl Drop for XServer {
    /// The event loop is never freed (Xwayland's and the window manager's sources hold handles to it), so
    /// the source, which owns the lock file, has to be removed explicitly.
    fn drop(&mut self) {
        self.handle.remove(self.token);
    }
}

/// Spawn Xwayland and start the X11 window manager once it is ready.
///
/// Returns `None` if Xwayland could not be started, in which case ltmi runs Wayland-only.
/// The X11 sockets already listen when this returns, so X clients may connect right away; they wait until
/// Xwayland is ready.
pub fn start(dh: &DisplayHandle, handle: &LoopHandle<'static, App>) -> Option<XServer> {
    let spawned = XWayland::spawn(
        dh,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::inherit(),
        |_| (),
    );
    let (xwayland, client) = match spawned {
        Ok(spawned) => spawned,
        Err(err) => {
            tracing::warn!("Failed to start Xwayland, X11 apps won't run: {err}");
            return None;
        }
    };
    let display_number = xwayland.display_number();

    let wm_handle = handle.clone();
    let inserted = handle.insert_source(xwayland, move |event, _, state: &mut App| match event {
        XWaylandEvent::Ready { x11_socket, .. } => {
            match X11Wm::start_wm(wm_handle.clone(), x11_socket, client.clone()) {
                Ok(wm) => {
                    tracing::info!("Xwayland ready on DISPLAY=:{display_number}");
                    state.xwm = Some(wm);
                }
                Err(err) => tracing::warn!("Failed to start the X11 window manager: {err}"),
            }
        }
        XWaylandEvent::Error => tracing::warn!("Xwayland failed to start"),
    });
    match inserted {
        Ok(token) => Some(XServer {
            display: display_number,
            handle: handle.clone(),
            token,
        }),
        Err(err) => {
            tracing::warn!("Failed to watch Xwayland, X11 apps won't run: {}", err.error);
            None
        }
    }
}

/// Keyboard focus target that also covers X11 windows.
///
/// Xwayland only sends keys to the X window holding X input focus, which [`X11Surface`] sets when it gains
/// keyboard focus. Focusing its bare `WlSurface` would not do that.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => surface.alive(),
            Self::X11(window) => window.alive(),
        }
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            Self::X11(window) => window.wl_surface().map(Cow::Owned),
        }
    }
}

impl KeyboardTarget<App> for FocusTarget {
    fn enter(&self, seat: &Seat<App>, data: &mut App, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
            Self::X11(window) => KeyboardTarget::enter(window, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<App>, data: &mut App, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            Self::X11(window) => KeyboardTarget::leave(window, seat, data, serial),
        }
    }

    fn key(&self, seat: &Seat<App>, data: &mut App, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: u32) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::key(surface, seat, data, key, state, serial, time),
            Self::X11(window) => KeyboardTarget::key(window, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(&self, seat: &Seat<App>, data: &mut App, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Wayland(surface) => KeyboardTarget::modifiers(surface, seat, data, modifiers, serial),
            Self::X11(window) => KeyboardTarget::modifiers(window, seat, data, modifiers, serial),
        }
    }
}

impl XWaylandShellHandler for App {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    /// X11 windows get their `wl_surface` only after they are mapped, and can only take keyboard focus then.
    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, _window: X11Surface) {
        self.refocus();
    }
}

/// Kiosk policy: every managed X11 window fills the output; override-redirect windows (menus, tooltips)
/// place themselves.
impl XwmHandler for App {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 event before the window manager started")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!("Failed to map X11 window: {err}");
            return;
        }
        self.configure_x11(&window);
        if let Err(err) = window.set_activated(true) {
            tracing::warn!("Failed to activate X11 window: {err}");
        }
        self.x11_windows.push(window);
        self.refocus();
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_windows.push(window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_windows.retain(|w| w != &window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
        self.refocus();
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_windows.retain(|w| w != &window);
        self.refocus();
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        _w: Option<u32>,
        _h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // The requested geometry is ignored; answering with the full output keeps the window filling it.
        self.configure_x11(&window);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<X11Window>,
    ) {
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {}
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}

impl App {
    /// Size a managed X11 window to fill the output.
    pub fn configure_x11(&self, window: &X11Surface) {
        if let Err(err) = window.configure(Rectangle::from_size(self.output_size)) {
            tracing::warn!("Failed to configure X11 window: {err}");
        }
    }
}
