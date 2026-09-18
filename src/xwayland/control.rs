use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, AutoRepeatMode, ChangeKeyboardControlAux, ConnectionExt as _, CreateWindowAux,
    GetGeometryReply, InputFocus, PropMode, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_FROM_PARENT, CURRENT_TIME, NONE};

fn is_destroyed_window(error: &ReplyError) -> bool {
    matches!(error, ReplyError::X11Error(error) if error.error_kind == ErrorKind::Window)
}

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        WM_S0,
        WM_STATE,
        _NET_SUPPORTED,
        _NET_SUPPORTING_WM_CHECK,
        _NET_WM_NAME,
        _NET_ACTIVE_WINDOW,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_NUMBER_OF_DESKTOPS,
        _NET_CURRENT_DESKTOP,
        _NET_DESKTOP_GEOMETRY,
        _NET_DESKTOP_VIEWPORT,
        _NET_WORKAREA,
        _NET_WM_MOVERESIZE,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_DEMANDS_ATTENTION,
        _NET_WM_STATE_FOCUSED,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_ALLOWED_ACTIONS,
        _NET_FRAME_EXTENTS,
        _NET_WM_ACTION_MOVE,
        _NET_WM_ACTION_RESIZE,
        _NET_WM_ACTION_MINIMIZE,
        _NET_WM_ACTION_MAXIMIZE_HORZ,
        _NET_WM_ACTION_MAXIMIZE_VERT,
        _NET_WM_ACTION_FULLSCREEN,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcccmState {
    Normal = 1,
    Iconic = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct AllowedActions {
    pub move_: bool,
    pub resize: bool,
    pub minimize: bool,
    pub maximize: bool,
    pub fullscreen: bool,
}

impl Default for AllowedActions {
    fn default() -> Self {
        Self {
            move_: true,
            resize: true,
            minimize: true,
            maximize: true,
            fullscreen: true,
        }
    }
}

/// An auxiliary control connection to the X server managed by Smithay's XWM.
///
/// Smithay remains the sole owner of `WM_S0`, substructure redirection, and the
/// X11 event stream. This connection fills protocol state and focus behavior
/// that Smithay's public API does not currently expose, keeping that
/// compatibility policy in Halley without modifying the pinned dependency.
pub struct X11Control {
    connection: RustConnection,
    atoms: Atoms,
    root: Window,
    focus_sink: Window,
    active_window: Cell<Option<Window>>,
    root_pointer_available: bool,
    root_pointer_drag: Cell<Option<Window>>,
    desktop_geometry: Cell<Option<PublishedDesktopGeometry>>,
    frame_extents: RefCell<HashMap<Window, (i32, i32, i32, i32)>>,
}

/// The last values written to the root window, kept so repeated publishes on an
/// unchanged layout are free.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PublishedDesktopGeometry {
    desktop: (u32, u32),
    work_area: Option<(i32, i32, u32, u32)>,
}

impl X11Control {
    pub fn connect(display_number: u32) -> Result<Self, Box<dyn Error>> {
        let display = format!(":{display_number}");
        let (connection, screen_number) = RustConnection::connect(Some(&display))?;
        let screen = &connection.setup().roots[screen_number];
        let root = screen.root;
        let root_depth = screen.root_depth;
        let atoms = Atoms::new(&connection)?.reply()?;

        let owner = connection.get_selection_owner(atoms.WM_S0)?.reply()?.owner;
        if owner == NONE {
            return Err("Smithay XWM does not own WM_S0".into());
        }
        let support_window = supporting_wm_window(&connection, root, &atoms)?;
        let focus_sink = create_focus_sink(&connection, root, root_depth)?;
        let root_geometry = connection.get_geometry(root)?.reply()?;
        publish_ewmh(&connection, root, support_window, root_geometry, &atoms)?;
        connection.flush()?;

        // EI redirects XTEST back to a compositor. Never use this delivery
        // path when such a backend is configured; Halley does not enable the
        // optional Xwayland EI portal either.
        let root_pointer_available = std::env::var_os("LIBEI_SOCKET").is_none()
            && connection
                .xtest_get_version(2, 2)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some();
        Ok(Self {
            connection,
            atoms,
            root,
            focus_sink,
            active_window: Cell::new(None),
            root_pointer_available,
            root_pointer_drag: Cell::new(None),
            desktop_geometry: Cell::new(None),
            frame_extents: RefCell::new(HashMap::new()),
        })
    }

    /// Deliver pointer motion in the X server's root coordinate system.
    /// XTEST constructs root/local event coordinates together, so a client
    /// moving its own window cannot invalidate a Wayland-local motion sample.
    pub fn popup_pointer_motion(&self, window: Window, position: (f64, f64)) -> bool {
        let Some((x, y)) = root_pointer_coordinates(position) else {
            return false;
        };
        if !self.root_pointer_available {
            return false;
        }
        if self.root_pointer_drag.get() != Some(window) {
            // The button travels over Wayland; don't overtake its press on
            // this independent X11 connection. Retry on the next motion if
            // Xwayland has not processed the press yet.
            let Ok(cookie) = self.connection.query_pointer(self.root) else {
                return false;
            };
            let Ok(pointer) = cookie.reply() else {
                return false;
            };
            if !pointer
                .mask
                .contains(x11rb::protocol::xproto::KeyButMask::BUTTON1)
            {
                return false;
            }
        }
        let Ok(cookie) = self.connection.xtest_fake_input(
            x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
            0,
            CURRENT_TIME,
            self.root,
            x,
            y,
            0,
        ) else {
            return false;
        };
        cookie.ignore_error();
        if self.connection.flush().is_err() {
            return false;
        }
        self.root_pointer_drag.set(Some(window));
        true
    }

    pub fn popup_pointer_active(&self) -> bool {
        self.root_pointer_drag.get().is_some()
    }

    /// Ensure the last X11 motion has been processed before a Wayland button
    /// release is sent. Only a drag handoff pays for this round trip.
    pub fn finish_popup_pointer(&self) {
        if self.root_pointer_drag.take().is_some() {
            if let Ok(cookie) = self.connection.get_input_focus() {
                let _ = cookie.reply();
            }
        }
    }

    /// Publishes the decoration Halley draws around a client window.
    ///
    /// The X window a client sees is the client area only; the border and
    /// titlebar live in the compositor's scene, so without this the client's
    /// own root-coordinate arithmetic is off by the frame. `extents` is
    /// `(left, right, top, bottom)`.
    pub fn set_frame_extents(
        &self,
        window: Window,
        extents: (i32, i32, i32, i32),
    ) -> Result<(), Box<dyn Error>> {
        if self.frame_extents.borrow().get(&window) == Some(&extents) {
            return Ok(());
        }
        let result = self
            .connection
            .change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_FRAME_EXTENTS,
                AtomEnum::CARDINAL,
                &[
                    extents.0 as u32,
                    extents.1 as u32,
                    extents.2 as u32,
                    extents.3 as u32,
                ],
            )?
            .check();
        if let Err(err) = result {
            if !is_destroyed_window(&err) {
                return Err(err.into());
            }
            return Ok(());
        }
        self.frame_extents.borrow_mut().insert(window, extents);
        Ok(())
    }

    /// Drops the per-window property memo for a window Halley no longer
    /// manages, so a reused XID cannot inherit its predecessor's extents.
    pub fn forget_window(&self, window: Window) {
        self.frame_extents.borrow_mut().remove(&window);
    }

    pub fn set_wm_state(&self, window: Window, state: IcccmState) -> Result<(), Box<dyn Error>> {
        self.connection
            .change_property32(
                PropMode::REPLACE,
                window,
                self.atoms.WM_STATE,
                self.atoms.WM_STATE,
                &[state as u32, NONE],
            )?
            .check()?;
        Ok(())
    }

    pub fn withdraw(&self, window: Window) -> Result<(), Box<dyn Error>> {
        let result = self
            .connection
            .delete_property(window, self.atoms.WM_STATE)?
            .check();
        if let Err(err) = result
            && !is_destroyed_window(&err)
        {
            return Err(err.into());
        }
        Ok(())
    }

    /// Republishes the root desktop geometry after an output layout change.
    ///
    /// `publish_ewmh` writes these once at connect, so without this they keep
    /// describing the layout Halley booted with. X11 clients that self-place
    /// from `_NET_WORKAREA` (GTK dialogs, Java, Steam) would size and position
    /// against a screen that no longer exists.
    ///
    /// `work_area` is `None` for a multi-output layout: a single rectangle
    /// cannot describe it, and EWMH offers no per-monitor form. Deleting the
    /// property says "unknown", which clients handle, where a stale or invented
    /// rectangle silently sends them off-screen. Hyprland bails the same way.
    pub fn publish_desktop_geometry(
        &self,
        desktop: (u32, u32),
        work_area: Option<(i32, i32, u32, u32)>,
    ) -> Result<(), Box<dyn Error>> {
        let published = PublishedDesktopGeometry { desktop, work_area };
        if self.desktop_geometry.get() == Some(published) {
            return Ok(());
        }
        self.connection.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms._NET_DESKTOP_GEOMETRY,
            AtomEnum::CARDINAL,
            &[desktop.0, desktop.1],
        )?;
        match work_area {
            Some((x, y, width, height)) => {
                self.connection.change_property32(
                    PropMode::REPLACE,
                    self.root,
                    self.atoms._NET_WORKAREA,
                    AtomEnum::CARDINAL,
                    &[x as u32, y as u32, width, height],
                )?;
            }
            None => {
                self.connection
                    .delete_property(self.root, self.atoms._NET_WORKAREA)?;
            }
        }
        self.connection.flush()?;
        self.desktop_geometry.set(Some(published));
        Ok(())
    }

    pub fn set_active_window(
        &self,
        window: Option<Window>,
        has_keyboard_focus: bool,
    ) -> Result<(), Box<dyn Error>> {
        let published_window =
            active_window_property_value(window, has_keyboard_focus, self.focus_sink);
        if self.active_window.get() != Some(published_window) {
            self.connection.change_property32(
                PropMode::REPLACE,
                self.root,
                self.atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW,
                &[published_window],
            )?;
            self.active_window.set(Some(published_window));
        }

        // Native Wayland surfaces have no XID to receive core X focus. Leaving
        // X focus at `None` is observably different from transferring it to a
        // different X11 application: Wine/Proton clients can continue treating
        // themselves as the foreground application. Park core focus on the WM's
        // mapped off-screen support window. Mutter uses the same no-focus-window
        // pattern and also publishes that XID as `_NET_ACTIVE_WINDOW` while a
        // native Wayland surface is focused, distinguishing it from a transient
        // state where no surface has focus.
        if let Some(focus_sink) = focus_sink_for_active_window(window, self.focus_sink) {
            // Queue focus without a checked round trip. Xwayland may be
            // servicing a client grab during popup movement; waiting for its
            // confirmation here stalls the compositor that must deliver input.
            self.connection
                .set_input_focus(InputFocus::NONE, focus_sink, CURRENT_TIME)?;
        }
        self.connection.flush()?;
        Ok(())
    }

    /// Gives a globally-active client deterministic core X focus.
    ///
    /// Smithay sends `WM_TAKE_FOCUS` for these clients but otherwise waits for
    /// them to focus themselves. Queue `SetInputFocus` explicitly, preserving
    /// X11 request ordering without waiting for server confirmation on the
    /// compositor thread.
    pub fn focus_window(&self, window: Window) -> Result<(), Box<dyn Error>> {
        self.connection
            .set_input_focus(InputFocus::NONE, window, CURRENT_TIME)?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn set_allowed_actions(
        &self,
        window: Window,
        actions: AllowedActions,
    ) -> Result<(), Box<dyn Error>> {
        let mut atoms = Vec::with_capacity(8);
        if actions.move_ {
            atoms.push(self.atoms._NET_WM_ACTION_MOVE);
        }
        if actions.resize {
            atoms.push(self.atoms._NET_WM_ACTION_RESIZE);
        }
        if actions.minimize {
            atoms.push(self.atoms._NET_WM_ACTION_MINIMIZE);
        }
        if actions.maximize {
            atoms.extend([
                self.atoms._NET_WM_ACTION_MAXIMIZE_HORZ,
                self.atoms._NET_WM_ACTION_MAXIMIZE_VERT,
            ]);
        }
        if actions.fullscreen {
            atoms.push(self.atoms._NET_WM_ACTION_FULLSCREEN);
        }
        let result = self
            .connection
            .change_property32(
                PropMode::REPLACE,
                window,
                self.atoms._NET_WM_ALLOWED_ACTIONS,
                AtomEnum::ATOM,
                &atoms,
            )?
            .check();
        if let Err(err) = result
            && !is_destroyed_window(&err)
        {
            return Err(err.into());
        }
        Ok(())
    }

    pub fn configure_key_repeat(&self, delay: i32, rate: i32) -> Result<(), Box<dyn Error>> {
        let enabled = rate > 0;
        self.connection
            .change_keyboard_control(&ChangeKeyboardControlAux::new().auto_repeat_mode(
                if enabled {
                    AutoRepeatMode::ON
                } else {
                    AutoRepeatMode::OFF
                },
            ))?
            .check()?;
        if !enabled {
            self.connection.flush()?;
            return Ok(());
        }

        let extension = self.connection.xkb_use_extension(1, 0)?.reply()?;
        if !extension.supported {
            return Err("XWayland does not support XKB 1.0 keyboard controls".into());
        }
        let device = u16::from(xkb::ID::USE_CORE_KBD);
        let current = self.connection.xkb_get_controls(device)?.reply()?;
        let repeat_delay = u16::try_from(delay.clamp(1, i32::from(u16::MAX)))?;
        let repeat_interval = repeat_interval_ms(rate);
        self.connection
            .xkb_set_controls(
                device,
                current.internal_mods_mask,
                current.internal_mods_real_mods,
                current.ignore_lock_mods_mask,
                current.ignore_lock_mods_real_mods,
                current.internal_mods_vmods,
                current.internal_mods_vmods,
                current.ignore_lock_mods_vmods,
                current.ignore_lock_mods_vmods,
                current.mouse_keys_dflt_btn,
                current.groups_wrap,
                current.access_x_option,
                xkb::BoolCtrl::REPEAT_KEYS,
                xkb::BoolCtrl::REPEAT_KEYS,
                u32::from(xkb::BoolCtrl::REPEAT_KEYS).into(),
                repeat_delay,
                repeat_interval,
                current.slow_keys_delay,
                current.debounce_delay,
                current.mouse_keys_delay,
                current.mouse_keys_interval,
                current.mouse_keys_time_to_max,
                current.mouse_keys_max_speed,
                current.mouse_keys_curve,
                current.access_x_timeout,
                current.access_x_timeout_mask,
                current.access_x_timeout_values,
                current.access_x_timeout_options_mask,
                current.access_x_timeout_options_values,
                &current.per_key_repeat,
            )?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }
}

fn repeat_interval_ms(rate: i32) -> u16 {
    (1000.0 / rate.max(1) as f32)
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16
}

fn supporting_wm_window(
    connection: &RustConnection,
    root: Window,
    atoms: &Atoms,
) -> Result<Window, Box<dyn Error>> {
    let reply = connection
        .get_property(
            false,
            root,
            atoms._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            0,
            1,
        )?
        .reply()?;
    reply
        .value32()
        .and_then(|mut values| values.next())
        .filter(|window| *window != NONE)
        .ok_or_else(|| "Smithay XWM did not publish _NET_SUPPORTING_WM_CHECK".into())
}

fn create_focus_sink(
    connection: &RustConnection,
    root: Window,
    root_depth: u8,
) -> Result<Window, Box<dyn Error>> {
    // Unlike Smithay's pre-existing WM support window, this window is observed
    // by the XWM. Focusing it therefore produces an ordered FocusOut(game),
    // FocusIn(sink) pair and lets the XWM's own `_NET_ACTIVE_WINDOW` handler
    // settle on the sink instead of racing our property update back to `None`.
    let focus_sink = connection.generate_id()?;
    connection
        .create_window(
            root_depth,
            focus_sink,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().override_redirect(1),
        )?
        .check()?;
    connection.map_window(focus_sink)?.check()?;
    Ok(focus_sink)
}

fn focus_sink_for_active_window(
    active_window: Option<Window>,
    focus_sink: Window,
) -> Option<Window> {
    active_window.is_none().then_some(focus_sink)
}

fn active_window_property_value(
    active_window: Option<Window>,
    has_keyboard_focus: bool,
    focus_sink: Window,
) -> Window {
    active_window.unwrap_or(if has_keyboard_focus { focus_sink } else { NONE })
}

fn publish_ewmh(
    connection: &RustConnection,
    root: Window,
    support_window: Window,
    geometry: GetGeometryReply,
    atoms: &Atoms,
) -> Result<(), Box<dyn Error>> {
    connection.change_property8(
        PropMode::REPLACE,
        support_window,
        atoms._NET_WM_NAME,
        atoms.UTF8_STRING,
        b"Halley",
    )?;

    // Only advertise behavior Halley actually implements. In particular,
    // ABOVE/BELOW, SHADED, and STICKY stay absent until their compositor-side
    // policy exists rather than inheriting Smithay's broader default list.
    let supported = [
        atoms._NET_SUPPORTED,
        atoms._NET_SUPPORTING_WM_CHECK,
        atoms._NET_WM_NAME,
        atoms._NET_ACTIVE_WINDOW,
        atoms._NET_CLIENT_LIST,
        atoms._NET_CLIENT_LIST_STACKING,
        atoms._NET_NUMBER_OF_DESKTOPS,
        atoms._NET_CURRENT_DESKTOP,
        atoms._NET_DESKTOP_GEOMETRY,
        atoms._NET_DESKTOP_VIEWPORT,
        atoms._NET_WORKAREA,
        atoms._NET_WM_MOVERESIZE,
        atoms._NET_WM_STATE,
        atoms._NET_WM_STATE_MAXIMIZED_VERT,
        atoms._NET_WM_STATE_MAXIMIZED_HORZ,
        atoms._NET_WM_STATE_HIDDEN,
        atoms._NET_WM_STATE_FULLSCREEN,
        atoms._NET_WM_STATE_DEMANDS_ATTENTION,
        atoms._NET_WM_STATE_FOCUSED,
        atoms._NET_WM_STATE_SKIP_TASKBAR,
        atoms._NET_WM_STATE_SKIP_PAGER,
        atoms._NET_WM_ALLOWED_ACTIONS,
        atoms._NET_FRAME_EXTENTS,
        atoms._NET_WM_ACTION_MOVE,
        atoms._NET_WM_ACTION_RESIZE,
        atoms._NET_WM_ACTION_MINIMIZE,
        atoms._NET_WM_ACTION_MAXIMIZE_HORZ,
        atoms._NET_WM_ACTION_MAXIMIZE_VERT,
        atoms._NET_WM_ACTION_FULLSCREEN,
    ];
    connection.change_property32(
        PropMode::REPLACE,
        root,
        atoms._NET_SUPPORTED,
        AtomEnum::ATOM,
        &supported,
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        root,
        atoms._NET_NUMBER_OF_DESKTOPS,
        AtomEnum::CARDINAL,
        &[1],
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        root,
        atoms._NET_CURRENT_DESKTOP,
        AtomEnum::CARDINAL,
        &[0],
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        root,
        atoms._NET_DESKTOP_GEOMETRY,
        AtomEnum::CARDINAL,
        &[u32::from(geometry.width), u32::from(geometry.height)],
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        root,
        atoms._NET_DESKTOP_VIEWPORT,
        AtomEnum::CARDINAL,
        &[0, 0],
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        root,
        atoms._NET_WORKAREA,
        AtomEnum::CARDINAL,
        &[0, 0, u32::from(geometry.width), u32::from(geometry.height)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{active_window_property_value, focus_sink_for_active_window, repeat_interval_ms};

    #[test]
    fn repeat_rate_converts_to_xkb_interval() {
        assert_eq!(repeat_interval_ms(20), 50);
        assert_eq!(repeat_interval_ms(30), 33);
        assert_eq!(repeat_interval_ms(45), 22);
    }

    #[test]
    fn native_wayland_focus_uses_the_x11_focus_sink() {
        assert_eq!(focus_sink_for_active_window(None, 41), Some(41));
        assert_eq!(focus_sink_for_active_window(Some(73), 41), None);
    }

    #[test]
    fn native_wayland_focus_publishes_the_sink_as_active() {
        assert_eq!(active_window_property_value(None, true, 41), 41);
        assert_eq!(active_window_property_value(None, false, 41), 0);
        assert_eq!(active_window_property_value(Some(73), true, 41), 73);
    }
}

/// Core XTEST coordinates are signed 16-bit root-screen pixels. Falling back
/// is preferable to wrapping a large desktop coordinate onto another output.
fn root_pointer_coordinates((x, y): (f64, f64)) -> Option<(i16, i16)> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let (x, y) = (x.round(), y.round());
    if x < i16::MIN as f64 || x > i16::MAX as f64 || y < i16::MIN as f64 || y > i16::MAX as f64 {
        return None;
    }
    Some((x as i16, y as i16))
}

#[cfg(test)]
mod popup_pointer_tests {
    use super::*;

    #[test]
    fn root_pointer_coordinates_do_not_wrap_or_accept_nonfinite_values() {
        assert_eq!(
            root_pointer_coordinates((1972.503, 1190.397)),
            Some((1973, 1190))
        );
        for point in [
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
            (32768.0, 0.0),
            (-32769.0, 0.0),
        ] {
            assert_eq!(root_pointer_coordinates(point), None);
        }
    }

    #[test]
    #[ignore = "requires HALLEY_TEST_XVFB pointing to Xvfb; uses an isolated X server"]
    fn moving_popout_delivers_root_coordinates_without_origin_feedback() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        use x11rb::protocol::xproto::{ConfigureWindowAux, EventMask, KeyButMask};
        struct Server(std::process::Child);
        impl Drop for Server {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let executable = std::env::var_os("HALLEY_TEST_XVFB").expect("set HALLEY_TEST_XVFB");
        let mut server = Server(
            Command::new(executable)
                .args([
                    "-displayfd",
                    "1",
                    "-screen",
                    "0",
                    "4096x3072x24",
                    "-nolisten",
                    "tcp",
                    "-noreset",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut display = String::new();
        std::io::BufReader::new(server.0.stdout.take().unwrap())
            .read_line(&mut display)
            .unwrap();
        let number: u32 = display.trim().parse().unwrap();
        let (app, _) = RustConnection::connect(Some(&format!(":{number}"))).unwrap();
        let atoms = Atoms::new(&app).unwrap().reply().unwrap();
        let root = app.setup().roots[0].root;
        app.set_selection_owner(root, atoms.WM_S0, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        app.change_property32(
            PropMode::REPLACE,
            root,
            atoms._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            &[root],
        )
        .unwrap()
        .check()
        .unwrap();
        let control = X11Control::connect(number).unwrap();
        let window = app.generate_id().unwrap();
        app.create_window(
            COPY_FROM_PARENT as u8,
            window,
            control.root,
            1283,
            -730,
            772,
            2849,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            ),
        )
        .unwrap()
        .check()
        .unwrap();
        app.map_window(window).unwrap().check().unwrap();
        // Without a processed left-button press, keep the Wayland path.
        assert!(!control.popup_pointer_motion(window, (1677.0, 711.0)));
        control
            .connection
            .xtest_fake_input(
                x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
                0,
                0,
                control.root,
                1677,
                711,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        control
            .connection
            .xtest_fake_input(
                x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                1,
                0,
                control.root,
                0,
                0,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        assert!(
            app.query_pointer(window)
                .unwrap()
                .reply()
                .unwrap()
                .mask
                .contains(KeyButMask::BUTTON1)
        );
        while app.poll_for_event().unwrap().is_some() {}
        // Replay the captured origin change, then additional moving origins.
        for (index, (x, y)) in [(3068, -293), (1995, -53), (2500, -365)]
            .into_iter()
            .enumerate()
        {
            app.configure_window(window, &ConfigureWindowAux::new().x(x).y(y))
                .unwrap()
                .check()
                .unwrap();
            let expected_x = 1973 + index as i16;
            assert!(control.popup_pointer_motion(window, (f64::from(expected_x), 1190.0)));
            // The release barrier processes motion before the next input stream.
            control.finish_popup_pointer();
            let pointer = app.query_pointer(window).unwrap().reply().unwrap();
            assert_eq!((pointer.root_x, pointer.root_y), (expected_x, 1190));
            assert_eq!(i32::from(pointer.win_x) + x, i32::from(expected_x));
            assert_eq!(i32::from(pointer.win_y) + y, 1190);
            let mut saw_motion = false;
            while let Some(event) = app.poll_for_event().unwrap() {
                if let x11rb::protocol::Event::MotionNotify(event) = event {
                    saw_motion = true;
                    assert_eq!(event.event, window);
                    assert_eq!((event.root_x, event.root_y), (expected_x, 1190));
                    assert_eq!(i32::from(event.event_x) + x, i32::from(expected_x));
                }
            }
            assert!(
                saw_motion,
                "held grab must receive the root-coordinate motion"
            );
        }
        control
            .connection
            .xtest_fake_input(
                x11rb::protocol::xproto::BUTTON_RELEASE_EVENT,
                1,
                0,
                control.root,
                0,
                0,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        assert!(!control.popup_pointer_motion(window, (1973.0, 1190.0)));
        assert!(!control.popup_pointer_active());
    }
}
