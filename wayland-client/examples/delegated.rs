#![allow(clippy::single_match)]

use wayland_client::{
    protocol::{
        wl_compositor::{self, WlCompositor},
        wl_display::{self, WlDisplay},
        wl_registry::{self, WlRegistry},
    },
    Connection, Dispatch, Proxy, QueueHandle,
};

mod delegated {
    use super::*;

    pub trait RegistryHandler: 'static {
        fn state(&mut self) -> &mut Registry;
        fn new_global(&mut self, name: u32, interface: &str, version: u32);
    }

    pub struct Registry {
        wl_registry: WlRegistry,
    }

    impl Registry {
        pub fn new<D: RegistryHandler>(qh: &QueueHandle<D>, display: &WlDisplay) -> Self {
            let data = qh.make_data::<WlRegistry, _, Self>(());

            let wl_registry =
                display.send_constructor(wl_display::Request::GetRegistry {}, data).unwrap();

            Self { wl_registry }
        }

        pub fn wl_registry(&self) -> WlRegistry {
            self.wl_registry.clone()
        }
    }

    impl<D: RegistryHandler> Dispatch<WlRegistry, (), D> for Registry {
        fn event(
            state: &mut D,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<D>,
        ) {
            if let wl_registry::Event::Global { name, interface, version } = event {
                state.new_global(name, &interface, version);
            }
        }
    }
}

struct AppData {
    registry: delegated::Registry,
    qh: QueueHandle<Self>,
}

impl delegated::RegistryHandler for AppData {
    fn state(&mut self) -> &mut delegated::Registry {
        &mut self.registry
    }

    fn new_global(&mut self, name: u32, interface: &str, version: u32) {
        println!("[{}] {} (v{})", name, interface, version);

        match interface {
            "wl_compositor" => {
                self.registry.wl_registry().bind(name, version, &self.qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for AppData {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

fn main() {
    let conn = Connection::connect_to_env().unwrap();

    let display = conn.display();

    let mut event_queue = conn.new_event_queue::<AppData>();
    let qh = event_queue.handle();

    let registry = delegated::Registry::new(&qh, &display);

    let mut app = AppData { registry, qh: qh.clone() };

    println!("Advertized globals:");
    event_queue.roundtrip(&mut app).unwrap();
}
