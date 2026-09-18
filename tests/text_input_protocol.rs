//! Real Wayland request/event regression tests for Halley's patched Smithay.
use smithay::{
    delegate_compositor, delegate_input_method_manager, delegate_seat, delegate_text_input_manager,
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::wayland_server::{self as server, Display, protocol::wl_surface::WlSurface},
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        input_method::{InputMethodHandler, InputMethodManagerState, PopupSurface},
        text_input::TextInputManagerState,
    },
};
use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_compositor, wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3 as tim, zwp_text_input_v3 as ti,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2 as imm, zwp_input_method_v2 as im,
    zwp_input_popup_surface_v2 as popup,
};

struct Server {
    compositor: CompositorState,
    seats: SeatState<Self>,
    seat: Seat<Self>,
}
#[derive(Default)]
struct ClientData(CompositorClientState);
impl server::backend::ClientData for ClientData {
    fn initialized(&self, _: server::backend::ClientId) {}
    fn disconnected(&self, _: server::backend::ClientId, _: server::backend::DisconnectReason) {}
}
impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a server::Client) -> &'a CompositorClientState {
        &client.get_data::<ClientData>().unwrap().0
    }
    fn commit(&mut self, surface: &WlSurface) {
        // Test-only focus control, exercised over the same socket as requests.
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(surface.clone()), SERIAL_COUNTER.next_serial());
    }
}
impl SeatHandler for Server {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
    fn focus_changed(&mut self, _: &Seat<Self>, _: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _: &Seat<Self>, _: CursorImageStatus) {}
}
impl InputMethodHandler for Server {
    fn new_popup(&mut self, _: PopupSurface) {}
    fn dismiss_popup(&mut self, _: PopupSurface) {}
    fn popup_repositioned(&mut self, _: PopupSurface) {}
    fn parent_geometry(&self, _: &WlSurface) -> Rectangle<i32, Logical> {
        Rectangle::default()
    }
}
delegate_compositor!(Server);
delegate_seat!(Server);
delegate_text_input_manager!(Server);
delegate_input_method_manager!(Server);

#[derive(Default)]
struct Client {
    globals: HashMap<String, (u32, u32)>,
    text: Vec<(u32, ti::Event)>,
    ime: Vec<(u32, im::Event)>,
    popup_rectangles: Vec<(i32, i32, i32, i32)>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.insert(interface, (name, version));
        }
    }
}
impl Dispatch<ti::ZwpTextInputV3, ()> for Client {
    fn event(
        state: &mut Self,
        proxy: &ti::ZwpTextInputV3,
        event: ti::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.text.push((proxy.id().protocol_id(), event));
    }
}
impl Dispatch<im::ZwpInputMethodV2, ()> for Client {
    fn event(
        state: &mut Self,
        proxy: &im::ZwpInputMethodV2,
        event: im::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.ime.push((proxy.id().protocol_id(), event));
    }
}
impl Dispatch<popup::ZwpInputPopupSurfaceV2, ()> for Client {
    fn event(
        state: &mut Self,
        _: &popup::ZwpInputPopupSurfaceV2,
        event: popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let popup::Event::TextInputRectangle {
            x,
            y,
            width,
            height,
        } = event
        {
            state.popup_rectangles.push((x, y, width, height));
        }
    }
}
delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_seat::WlSeat);
delegate_noop!(Client: ignore tim::ZwpTextInputManagerV3);
delegate_noop!(Client: ignore imm::ZwpInputMethodManagerV2);

struct Fixture {
    state: Client,
    queue: EventQueue<Client>,
    compositor: wl_compositor::WlCompositor,
    seat: wl_seat::WlSeat,
    manager: tim::ZwpTextInputManagerV3,
    ime_manager: imm::ZwpInputMethodManagerV2,
    input: ti::ZwpTextInputV3,
    ime: im::ZwpInputMethodV2,
    surface: wl_surface::WlSurface,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let (client_socket, server_socket) = UnixStream::pair().unwrap();
        let mut display = Display::<Server>::new().unwrap();
        let mut dh = display.handle();
        let compositor = CompositorState::new::<Server>(&dh);
        let mut seats = SeatState::new();
        let mut seat = seats.new_wl_seat(&dh, "test");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        let _text = TextInputManagerState::new::<Server>(&dh);
        let _ime = InputMethodManagerState::new::<Server, _>(&dh, |_| true);
        dh.insert_client(server_socket, Arc::new(ClientData::default()))
            .unwrap();
        let mut server = Server {
            compositor,
            seats,
            seat,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                display.dispatch_clients(&mut server).unwrap();
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut state = Client::default();
        queue.roundtrip(&mut state).unwrap();
        let bind = |name: &str| state.globals[name].0;
        let compositor: wl_compositor::WlCompositor =
            registry.bind(bind("wl_compositor"), 4, &qh, ());
        let seat = registry.bind(bind("wl_seat"), 7, &qh, ());
        let manager: tim::ZwpTextInputManagerV3 =
            registry.bind(bind("zwp_text_input_manager_v3"), 1, &qh, ());
        let ime_manager: imm::ZwpInputMethodManagerV2 =
            registry.bind(bind("zwp_input_method_manager_v2"), 1, &qh, ());
        let input = manager.get_text_input(&seat, &qh, ());
        let ime = ime_manager.get_input_method(&seat, &qh, ());
        let surface = compositor.create_surface(&qh, ());
        surface.commit();
        queue.roundtrip(&mut state).unwrap();
        let mut fixture = Self {
            state,
            queue,
            compositor,
            seat,
            manager,
            ime_manager,
            input,
            ime,
            surface,
            stop,
            worker: Some(worker),
        };
        fixture.clear();
        fixture
    }
    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }
    fn clear(&mut self) {
        self.state.text.clear();
        self.state.ime.clear();
    }
    fn enable(&mut self) {
        self.input.enable();
        self.input.commit();
        self.sync();
        self.clear();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn new_text_input_does_not_reenter_existing_objects() {
    let mut f = Fixture::new();
    let second = f.manager.get_text_input(&f.seat, &f.queue.handle(), ());
    f.sync();
    assert_eq!(f.state.text.len(), 1, "{:?}", f.state.text);
    assert_eq!(f.state.text[0].0, second.id().protocol_id());
    assert!(matches!(f.state.text[0].1, ti::Event::Enter { .. }));
}

#[test]
fn destroying_active_object_allows_another_object_to_enable() {
    let mut f = Fixture::new();
    f.enable();
    let second = f.manager.get_text_input(&f.seat, &f.queue.handle(), ());
    f.input.destroy();
    second.enable();
    second.commit();
    f.sync();
    assert!(
        f.state
            .ime
            .iter()
            .any(|(_, e)| matches!(e, im::Event::Deactivate))
    );
    assert!(
        f.state
            .ime
            .iter()
            .any(|(_, e)| matches!(e, im::Event::Activate))
    );
    f.clear();
    f.ime.commit_string("replacement".into());
    f.ime.commit(0);
    f.sync();
    assert!(f.state.text.iter().any(|(id, e)| *id == second.id().protocol_id() && matches!(e, ti::Event::CommitString { text: Some(text) } if text == "replacement")));
}

#[test]
fn enable_resets_pending_state_and_change_cause_defaults_each_commit() {
    let mut f = Fixture::new();
    f.input.set_surrounding_text("stale".into(), 5, 5);
    f.input
        .set_content_type(ti::ContentHint::SensitiveData, ti::ContentPurpose::Password);
    f.input.set_text_change_cause(ti::ChangeCause::Other);
    f.input.enable();
    f.input.commit();
    f.sync();
    assert!(!f.state.ime.iter().any(|(_, e)| matches!(
        e,
        im::Event::SurroundingText { .. } | im::Event::ContentType { .. }
    )));
    assert!(f.state.ime.iter().any(|(_, e)| matches!(e, im::Event::TextChangeCause { cause } if *cause == wayland_client::WEnum::Value(ti::ChangeCause::InputMethod))));
    f.input.set_text_change_cause(ti::ChangeCause::Other);
    f.input.commit();
    f.sync();
    f.clear();
    f.input.set_surrounding_text("new".into(), 3, 3);
    f.input.commit();
    f.sync();
    assert!(f.state.ime.iter().any(|(_, e)| matches!(e, im::Event::TextChangeCause { cause } if *cause == wayland_client::WEnum::Value(ti::ChangeCause::InputMethod))));
}

#[test]
fn ime_edits_are_buffered_last_write_wins_and_done_uses_commit_count() {
    let mut f = Fixture::new();
    f.enable();
    f.ime.commit_string("old".into());
    f.ime.commit_string("é🦀".into());
    f.ime.set_preedit_string("候補".into(), 0, 6);
    f.ime.delete_surrounding_text(2, 0);
    f.sync();
    assert!(f.state.text.is_empty(), "edits escaped before IME commit");
    f.ime.commit(0);
    f.sync();
    assert_eq!(f.state.text.len(), 4, "{:?}", f.state.text);
    assert!(
        matches!(&f.state.text[1].1, ti::Event::CommitString { text: Some(text) } if text == "é🦀")
    );
    assert!(matches!(f.state.text[3].1, ti::Event::Done { serial: 1 }));
    f.clear();
    f.ime.commit(0);
    f.sync();
    assert_eq!(f.state.text.len(), 1);
    assert!(matches!(f.state.text[0].1, ti::Event::Done { serial: 1 }));
}

#[test]
fn focus_change_discards_uncommitted_app_and_ime_state() {
    let mut f = Fixture::new();
    f.enable();
    f.input.set_surrounding_text("old field".into(), 0, 0);
    f.ime.commit_string("old composition".into());
    let other = f.compositor.create_surface(&f.queue.handle(), ());
    other.commit();
    f.sync();
    f.clear();
    f.input.enable();
    f.input.commit();
    f.ime.commit(0);
    f.sync();
    assert!(
        !f.state
            .ime
            .iter()
            .any(|(_, e)| matches!(e, im::Event::SurroundingText { .. }))
    );
    assert!(
        !f.state
            .text
            .iter()
            .any(|(_, e)| matches!(e, ti::Event::CommitString { .. }))
    );
    f.surface.commit();
    f.sync();
}

#[test]
fn rejected_second_ime_cannot_replace_or_disconnect_the_first() {
    let mut f = Fixture::new();
    f.enable();
    let second = f
        .ime_manager
        .get_input_method(&f.seat, &f.queue.handle(), ());
    f.sync();
    assert_eq!(f.state.ime.len(), 1, "{:?}", f.state.ime);
    assert_eq!(f.state.ime[0].0, second.id().protocol_id());
    assert!(matches!(f.state.ime[0].1, im::Event::Unavailable));
    assert!(
        f.state.text.is_empty(),
        "existing text inputs were reentered"
    );
    second.commit_string("intruder".into());
    second.commit(0);
    second.destroy();
    f.sync();
    f.clear();
    f.ime.commit_string("original".into());
    f.ime.commit(0);
    f.sync();
    assert!(f.state.text.iter().any(
        |(_, e)| matches!(e, ti::Event::CommitString { text: Some(text) } if text == "original")
    ));
}

#[test]
fn focus_leave_clears_pending_enable_without_resetting_commit_counter() {
    let mut f = Fixture::new();
    f.input.enable(); // never committed on the old surface
    let other = f.compositor.create_surface(&f.queue.handle(), ());
    other.commit();
    f.sync();
    f.clear();
    f.input.commit();
    f.sync();
    assert!(
        !f.state
            .ime
            .iter()
            .any(|(_, e)| matches!(e, im::Event::Activate))
    );
    f.enable();
    f.ime.commit_string("new field".into());
    f.ime.commit(0);
    f.sync();
    assert!(
        f.state
            .text
            .iter()
            .any(|(_, e)| matches!(e, ti::Event::Done { serial: 2 }))
    );
}

#[test]
fn disabled_and_inactive_objects_do_not_receive_composition() {
    let mut f = Fixture::new();
    f.enable();
    let second = f.manager.get_text_input(&f.seat, &f.queue.handle(), ());
    second.enable();
    second.commit();
    f.sync();
    f.clear();
    f.ime.commit_string("active only".into());
    f.ime.commit(0);
    f.sync();
    assert!(
        f.state
            .text
            .iter()
            .all(|(id, _)| *id == f.input.id().protocol_id())
    );
    f.input.disable();
    f.input.commit();
    f.sync();
    f.clear();
    f.ime.commit_string("disabled".into());
    f.ime.commit(0);
    f.sync();
    assert!(f.state.text.is_empty());
    // Consecutive disable and enable requests are valid.
    f.input.disable();
    f.input.commit();
    f.input.enable();
    f.input.commit();
    f.input.enable();
    f.input.commit();
    f.sync();
    f.clear();
    f.ime.commit_string("enabled again".into());
    f.ime.commit(0);
    f.sync();
    assert!(
        f.state
            .text
            .iter()
            .any(|(_, e)| matches!(e, ti::Event::Done { serial: 5 }))
    );
}

#[test]
fn ime_reconnect_and_inactive_object_destruction_preserve_focus() {
    let mut f = Fixture::new();
    f.enable();
    let spare = f.manager.get_text_input(&f.seat, &f.queue.handle(), ());
    spare.destroy();
    f.sync();
    assert!(
        !f.state
            .ime
            .iter()
            .any(|(_, e)| matches!(e, im::Event::Deactivate))
    );
    f.ime.destroy();
    f.sync();
    assert!(
        f.state
            .text
            .iter()
            .any(|(_, e)| matches!(e, ti::Event::Leave { .. }))
    );
    f.clear();
    f.ime = f
        .ime_manager
        .get_input_method(&f.seat, &f.queue.handle(), ());
    f.sync();
    assert!(
        f.state
            .text
            .iter()
            .any(|(_, e)| matches!(e, ti::Event::Enter { .. }))
    );
    f.enable();
    f.ime.commit_string("reconnected".into());
    f.ime.commit(1);
    f.sync();
    assert!(
        f.state
            .text
            .iter()
            .any(|(_, e)| matches!(e, ti::Event::Done { serial: 2 }))
    );
}

#[test]
fn reenable_clears_the_previous_fields_popup_rectangle() {
    let mut f = Fixture::new();
    f.enable();
    let surface = f.compositor.create_surface(&f.queue.handle(), ());
    let _popup = f
        .ime
        .get_input_popup_surface(&surface, &f.queue.handle(), ());
    f.input.set_cursor_rectangle(50, 60, 2, 20);
    f.input.commit();
    f.sync();
    assert_eq!(f.state.popup_rectangles.last(), Some(&(50, 60, 2, 20)));
    f.input.enable();
    f.input.commit();
    f.sync();
    assert_eq!(f.state.popup_rectangles.last(), Some(&(0, 0, 0, 0)));
}
