use delegated::{OutputHandler, OutputManagerState};
use wayland_server::Display;

mod delegated {
    use wayland_server::{
        backend::GlobalId,
        protocol::wl_output::{self, WlOutput},
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    };

    pub trait OutputHandler {
        fn state(&mut self) -> &mut OutputManagerState;
        fn some_callback(&mut self);
    }

    pub struct OutputManagerState {
        global_id: GlobalId,
    }

    impl OutputManagerState {
        pub fn create_delegated_global<D>(dh: &DisplayHandle) -> Self
        where
            D: OutputHandler + 'static,
        {
            let global_id = dh.create_delegated_global::<D, WlOutput, (), Self>(4, ());
            Self { global_id }
        }

        pub fn gloabl_id(&self) -> GlobalId {
            self.global_id.clone()
        }
    }

    impl<D: OutputHandler> GlobalDispatch<WlOutput, (), D> for OutputManagerState {
        fn bind(
            state: &mut D,
            _handle: &DisplayHandle,
            _client: &Client,
            resource: New<WlOutput>,
            _global_data: &(),
            data_init: &mut DataInit<'_, D>,
        ) {
            let _output = data_init.init_delegated::<_, _, Self>(resource, ());

            state.state();
            state.some_callback();
        }
    }

    impl<D> Dispatch<WlOutput, (), D> for OutputManagerState {
        fn request(
            _state: &mut D,
            _client: &Client,
            _resource: &WlOutput,
            _request: wl_output::Request,
            _data: &(),
            _dhandle: &DisplayHandle,
            _data_init: &mut DataInit<'_, D>,
        ) {
        }
    }
}

struct App {
    output_state: OutputManagerState,
}

impl OutputHandler for App {
    fn state(&mut self) -> &mut OutputManagerState {
        &mut self.output_state
    }

    fn some_callback(&mut self) {}
}

fn main() {
    let display = Display::<App>::new().unwrap();

    let output_state = OutputManagerState::create_delegated_global::<App>(&display.handle());

    let app = App { output_state };

    display.handle().remove_global::<App>(app.output_state.gloabl_id());
}
