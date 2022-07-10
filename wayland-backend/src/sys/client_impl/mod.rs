//! Client-side implementation of a Wayland protocol backend using `libwayland`

use std::{
    collections::HashSet,
    ffi::CStr,
    os::raw::{c_int, c_void},
    os::unix::{io::RawFd, net::UnixStream, prelude::IntoRawFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

use crate::{
    core_interfaces::WL_DISPLAY_INTERFACE,
    protocol::{
        check_for_signature, same_interface, AllowNull, Argument, ArgumentType, Interface, Message,
        ObjectInfo, ProtocolError, ANONYMOUS_INTERFACE,
    },
};
use scoped_tls::scoped_thread_local;
use smallvec::SmallVec;

use wayland_sys::{client::*, common::*, ffi_dispatch};

pub use crate::types::client::{InvalidId, NoWaylandLib, WaylandError};

use super::{free_arrays, RUST_MANAGED};

use super::client::*;

scoped_thread_local!(static BACKEND: Backend);

/// An ID representing a Wayland object
#[derive(Clone)]
pub struct InnerObjectId {
    id: u32,
    ptr: *mut wl_proxy,
    alive: Option<Arc<AtomicBool>>,
    interface: &'static Interface,
}

unsafe impl Send for InnerObjectId {}
unsafe impl Sync for InnerObjectId {}

impl std::cmp::PartialEq for InnerObjectId {
    fn eq(&self, other: &Self) -> bool {
        match (&self.alive, &other.alive) {
            (Some(ref a), Some(ref b)) => {
                // this is an object we manage
                Arc::ptr_eq(a, b)
            }
            (None, None) => {
                // this is an external (un-managed) object
                self.ptr == other.ptr
                    && self.id == other.id
                    && same_interface(self.interface, other.interface)
            }
            _ => false,
        }
    }
}

impl std::cmp::Eq for InnerObjectId {}

impl std::hash::Hash for InnerObjectId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.ptr.hash(state);
        self.alive
            .as_ref()
            .map(|arc| &**arc as *const AtomicBool)
            .unwrap_or(std::ptr::null())
            .hash(state);
    }
}

impl InnerObjectId {
    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    pub fn interface(&self) -> &'static Interface {
        self.interface
    }

    pub fn protocol_id(&self) -> u32 {
        self.id
    }

    pub unsafe fn from_ptr(
        interface: &'static Interface,
        ptr: *mut wl_proxy,
    ) -> Result<Self, InvalidId> {
        // Safety: the provided pointer must be a valid wayland object
        let ptr_iface_name = unsafe {
            CStr::from_ptr(ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_class, ptr))
        };
        // Safety: the code generated by wayland-scanner is valid
        let provided_iface_name = unsafe {
            CStr::from_ptr(
                interface
                    .c_ptr
                    .expect("[wayland-backend-sys] Cannot use Interface without c_ptr!")
                    .name,
            )
        };
        if ptr_iface_name != provided_iface_name {
            return Err(InvalidId);
        }

        let id = ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_id, ptr);

        // Test if the proxy is managed by us.
        let is_rust_managed = ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_listener, ptr)
            == &RUST_MANAGED as *const u8 as *const _;

        let alive = if is_rust_managed {
            // Safety: the object is rust_managed, so its user-data pointer must be valid
            let udata = unsafe {
                &*(ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_user_data, ptr)
                    as *mut ProxyUserData)
            };
            Some(udata.alive.clone())
        } else {
            None
        };

        Ok(Self { id, ptr, alive, interface })
    }

    pub fn as_ptr(&self) -> *mut wl_proxy {
        if self.alive.as_ref().map(|alive| alive.load(Ordering::Acquire)).unwrap_or(true) {
            self.ptr
        } else {
            std::ptr::null_mut()
        }
    }
}

impl std::fmt::Display for InnerObjectId {
    #[cfg_attr(coverage, no_coverage)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.interface.name, self.id)
    }
}

impl std::fmt::Debug for InnerObjectId {
    #[cfg_attr(coverage, no_coverage)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ObjectId({})", self)
    }
}

struct ProxyUserData {
    alive: Arc<AtomicBool>,
    data: Arc<dyn ObjectData>,
    interface: &'static Interface,
}

#[derive(Debug)]
struct ConnectionState {
    display: *mut wl_display,
    evq: *mut wl_event_queue,
    display_id: InnerObjectId,
    last_error: Option<WaylandError>,
    known_proxies: HashSet<*mut wl_proxy>,
}

unsafe impl Send for ConnectionState {}

#[derive(Debug)]
struct Dispatcher;

#[derive(Debug)]
struct Inner {
    state: Mutex<ConnectionState>,
    dispatch_lock: Mutex<Dispatcher>,
}

#[derive(Clone, Debug)]
pub struct InnerBackend {
    inner: Arc<Inner>,
}

#[derive(Clone, Debug)]
pub struct WeakInnerBackend {
    inner: Weak<Inner>,
}

impl InnerBackend {
    fn lock_state(&self) -> MutexGuard<ConnectionState> {
        self.inner.state.lock().unwrap()
    }

    pub fn downgrade(&self) -> WeakInnerBackend {
        WeakInnerBackend { inner: Arc::downgrade(&self.inner) }
    }

    pub fn display_ptr(&self) -> *mut wl_display {
        self.inner.state.lock().unwrap().display
    }
}

impl WeakInnerBackend {
    pub fn upgrade(&self) -> Option<InnerBackend> {
        Weak::upgrade(&self.inner).map(|inner| InnerBackend { inner })
    }
}

unsafe impl Send for InnerBackend {}
unsafe impl Sync for InnerBackend {}

impl InnerBackend {
    pub fn connect(stream: UnixStream) -> Result<Self, NoWaylandLib> {
        if !is_lib_available() {
            return Err(NoWaylandLib);
        }
        let display = unsafe {
            ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_connect_to_fd, stream.into_raw_fd())
        };
        if display.is_null() {
            panic!("[wayland-backend-sys] libwayland reported an allocation failure.");
        }
        // set the log trampoline
        #[cfg(feature = "log")]
        unsafe {
            ffi_dispatch!(
                WAYLAND_CLIENT_HANDLE,
                wl_log_set_handler_client,
                wl_log_trampoline_to_rust_client
            );
        }
        let display_alive = Arc::new(AtomicBool::new(true));
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(ConnectionState {
                    display,
                    evq: std::ptr::null_mut(),
                    display_id: InnerObjectId {
                        id: 1,
                        ptr: display as *mut wl_proxy,
                        alive: Some(display_alive),
                        interface: &WL_DISPLAY_INTERFACE,
                    },
                    last_error: None,
                    known_proxies: HashSet::new(),
                }),
                dispatch_lock: Mutex::new(Dispatcher),
            }),
        })
    }

    pub unsafe fn from_foreign_display(display: *mut wl_display) -> Self {
        let evq = unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_create_queue, display) };
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(ConnectionState {
                    display,
                    evq,
                    display_id: InnerObjectId {
                        id: 1,
                        ptr: display as *mut wl_proxy,
                        alive: None,
                        interface: &WL_DISPLAY_INTERFACE,
                    },
                    last_error: None,
                    known_proxies: HashSet::new(),
                }),
                dispatch_lock: Mutex::new(Dispatcher),
            }),
        }
    }

    pub fn flush(&self) -> Result<(), WaylandError> {
        let mut guard = self.lock_state();
        guard.no_last_error()?;
        let ret = unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_flush, guard.display) };
        if ret < 0 {
            Err(guard.store_if_not_wouldblock_and_return_error(std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    pub fn dispatch_inner_queue(&self) -> Result<usize, WaylandError> {
        self.inner.dispatch_lock.lock().unwrap().dispatch_pending(self.inner.clone())
    }
}

impl ConnectionState {
    #[inline]
    fn no_last_error(&self) -> Result<(), WaylandError> {
        if let Some(ref err) = self.last_error {
            Err(err.clone())
        } else {
            Ok(())
        }
    }

    #[inline]
    fn store_and_return_error(&mut self, err: std::io::Error) -> WaylandError {
        // check if it was actually a protocol error
        let err = if err.raw_os_error() == Some(nix::errno::Errno::EPROTO as i32) {
            let mut object_id = 0;
            let mut interface = std::ptr::null();
            let code = unsafe {
                ffi_dispatch!(
                    WAYLAND_CLIENT_HANDLE,
                    wl_display_get_protocol_error,
                    self.display,
                    &mut interface,
                    &mut object_id
                )
            };
            let object_interface = unsafe {
                if interface.is_null() {
                    String::new()
                } else {
                    let cstr = std::ffi::CStr::from_ptr((*interface).name);
                    cstr.to_string_lossy().into()
                }
            };
            WaylandError::Protocol(ProtocolError {
                code,
                object_id,
                object_interface,
                message: String::new(),
            })
        } else {
            WaylandError::Io(err)
        };
        crate::log_error!("{}", err);
        self.last_error = Some(err.clone());
        err
    }

    #[inline]
    fn store_if_not_wouldblock_and_return_error(&mut self, e: std::io::Error) -> WaylandError {
        if e.kind() != std::io::ErrorKind::WouldBlock {
            self.store_and_return_error(e)
        } else {
            e.into()
        }
    }
}

impl Dispatcher {
    fn dispatch_pending(&self, inner: Arc<Inner>) -> Result<usize, WaylandError> {
        let (display, evq) = {
            let guard = inner.state.lock().unwrap();
            (guard.display, guard.evq)
        };
        let backend = Backend { backend: InnerBackend { inner } };

        // We erase the lifetime of the Handle to be able to store it in the tls,
        // it's safe as it'll only last until the end of this function call anyway
        let ret = BACKEND.set(&backend, || unsafe {
            if evq.is_null() {
                ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_dispatch_pending, display)
            } else {
                ffi_dispatch!(
                    WAYLAND_CLIENT_HANDLE,
                    wl_display_dispatch_queue_pending,
                    display,
                    evq
                )
            }
        });
        if ret < 0 {
            Err(backend
                .backend
                .inner
                .state
                .lock()
                .unwrap()
                .store_if_not_wouldblock_and_return_error(std::io::Error::last_os_error()))
        } else {
            Ok(ret as usize)
        }
    }
}

#[derive(Debug)]
pub struct InnerReadEventsGuard {
    inner: Arc<Inner>,
    display: *mut wl_display,
    done: bool,
}

impl InnerReadEventsGuard {
    pub fn try_new(backend: InnerBackend) -> Result<Self, WaylandError> {
        let (display, evq) = {
            let guard = backend.lock_state();
            (guard.display, guard.evq)
        };
        let dispatcher = backend.inner.dispatch_lock.lock().unwrap();
        // do the prepare_read() and dispatch as necessary
        loop {
            let ret = unsafe {
                if evq.is_null() {
                    ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_prepare_read, display)
                } else {
                    ffi_dispatch!(
                        WAYLAND_CLIENT_HANDLE,
                        wl_display_prepare_read_queue,
                        display,
                        evq
                    )
                }
            };
            if ret < 0 {
                dispatcher.dispatch_pending(backend.inner.clone())?;
            } else {
                break;
            }
        }
        std::mem::drop(dispatcher);

        // prepare_read is done, we are ready
        Ok(Self { inner: backend.inner, display, done: false })
    }

    pub fn connection_fd(&self) -> RawFd {
        unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_get_fd, self.display) }
    }

    pub fn read(mut self) -> Result<usize, WaylandError> {
        self.done = true;
        let ret =
            unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_read_events, self.display) };
        if ret < 0 {
            // we have done the reading, and there is an error
            Err(self
                .inner
                .state
                .lock()
                .unwrap()
                .store_if_not_wouldblock_and_return_error(std::io::Error::last_os_error()))
        } else {
            // the read occured, dispatch pending events
            self.inner.dispatch_lock.lock().unwrap().dispatch_pending(self.inner.clone())
        }
    }
}

impl Drop for InnerReadEventsGuard {
    fn drop(&mut self) {
        if !self.done {
            unsafe {
                ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_cancel_read, self.display);
            }
        }
    }
}

impl InnerBackend {
    pub fn display_id(&self) -> ObjectId {
        ObjectId { id: self.lock_state().display_id.clone() }
    }

    pub fn last_error(&self) -> Option<WaylandError> {
        self.lock_state().last_error.clone()
    }

    pub fn info(&self, ObjectId { id }: ObjectId) -> Result<ObjectInfo, InvalidId> {
        if !id.alive.as_ref().map(|a| a.load(Ordering::Acquire)).unwrap_or(true) || id.ptr.is_null()
        {
            return Err(InvalidId);
        }

        let version = if id.id == 1 {
            // special case the display, because libwayland returns a version of 0 for it
            1
        } else {
            unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_version, id.ptr) }
        };

        Ok(ObjectInfo { id: id.id, interface: id.interface, version })
    }

    pub fn null_id() -> ObjectId {
        ObjectId {
            id: InnerObjectId {
                ptr: std::ptr::null_mut(),
                interface: &ANONYMOUS_INTERFACE,
                id: 0,
                alive: None,
            },
        }
    }

    pub fn send_request(
        &self,
        Message { sender_id: ObjectId { id }, opcode, args }: Message<ObjectId>,
        data: Option<Arc<dyn ObjectData>>,
        child_spec: Option<(&'static Interface, u32)>,
    ) -> Result<ObjectId, InvalidId> {
        let mut guard = self.lock_state();
        if !id.alive.as_ref().map(|a| a.load(Ordering::Acquire)).unwrap_or(true) || id.ptr.is_null()
        {
            return Err(InvalidId);
        }
        let parent_version = if id.id == 1 {
            1
        } else {
            unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_version, id.ptr) }
        };
        // check that the argument list is valid
        let message_desc = match id.interface.requests.get(opcode as usize) {
            Some(msg) => msg,
            None => {
                panic!("Unknown opcode {} for object {}@{}.", opcode, id.interface.name, id.id);
            }
        };
        if !check_for_signature(message_desc.signature, &args) {
            panic!(
                "Unexpected signature for request {}@{}.{}: expected {:?}, got {:?}.",
                id.interface.name, id.id, message_desc.name, message_desc.signature, args
            );
        }

        // Prepare the child object data
        let child_spec = if message_desc
            .signature
            .iter()
            .any(|arg| matches!(arg, ArgumentType::NewId(_)))
        {
            if let Some((iface, version)) = child_spec {
                if let Some(child_interface) = message_desc.child_interface {
                    if !same_interface(child_interface, iface) {
                        panic!(
                            "Wrong placeholder used when sending request {}@{}.{}: expected interface {} but got {}",
                            id.interface.name,
                            id.id,
                            message_desc.name,
                            child_interface.name,
                            iface.name
                        );
                    }
                    if version != parent_version {
                        panic!(
                            "Wrong placeholder used when sending request {}@{}.{}: expected version {} but got {}",
                            id.interface.name,
                            id.id,
                            message_desc.name,
                            parent_version,
                            version
                        );
                    }
                }
                Some((iface, version))
            } else if let Some(child_interface) = message_desc.child_interface {
                Some((child_interface, parent_version))
            } else {
                panic!(
                    "Wrong placeholder used when sending request {}@{}.{}: target interface must be specified for a generic constructor.",
                    id.interface.name,
                    id.id,
                    message_desc.name
                );
            }
        } else {
            None
        };

        let child_interface_ptr = child_spec
            .as_ref()
            .map(|(i, _)| {
                i.c_ptr.expect("[wayland-backend-sys] Cannot use Interface without c_ptr!")
                    as *const _
            })
            .unwrap_or(std::ptr::null());
        let child_version = child_spec.as_ref().map(|(_, v)| *v).unwrap_or(parent_version);

        // check that all input objects are valid and create the [wl_argument]
        let mut argument_list = SmallVec::<[wl_argument; 4]>::with_capacity(args.len());
        let mut arg_interfaces = message_desc.arg_interfaces.iter();
        for (i, arg) in args.iter().enumerate() {
            match *arg {
                Argument::Uint(u) => argument_list.push(wl_argument { u }),
                Argument::Int(i) => argument_list.push(wl_argument { i }),
                Argument::Fixed(f) => argument_list.push(wl_argument { f }),
                Argument::Fd(h) => argument_list.push(wl_argument { h }),
                Argument::Array(ref a) => {
                    let a = Box::new(wl_array {
                        size: a.len(),
                        alloc: a.len(),
                        data: a.as_ptr() as *mut _,
                    });
                    argument_list.push(wl_argument { a: Box::into_raw(a) })
                }
                Argument::Str(ref s) => argument_list.push(wl_argument { s: s.as_ptr() }),
                Argument::Object(ref o) => {
                    let next_interface = arg_interfaces.next().unwrap();
                    if !o.id.ptr.is_null() {
                        if !id.alive.as_ref().map(|a| a.load(Ordering::Acquire)).unwrap_or(true) {
                            unsafe { free_arrays(message_desc.signature, &argument_list) };
                            return Err(InvalidId);
                        }
                        if !same_interface(next_interface, o.id.interface) {
                            panic!("Request {}@{}.{} expects an argument of interface {} but {} was provided instead.", id.interface.name, id.id, message_desc.name, next_interface.name, o.id.interface.name);
                        }
                    } else if !matches!(
                        message_desc.signature[i],
                        ArgumentType::Object(AllowNull::Yes)
                    ) {
                        panic!(
                            "Request {}@{}.{} expects an non-null object argument.",
                            id.interface.name, id.id, message_desc.name
                        );
                    }
                    argument_list.push(wl_argument { o: o.id.ptr as *const _ })
                }
                Argument::NewId(_) => argument_list.push(wl_argument { n: 0 }),
            }
        }

        let ret = if guard.evq.is_null() || child_spec.is_none() {
            unsafe {
                ffi_dispatch!(
                    WAYLAND_CLIENT_HANDLE,
                    wl_proxy_marshal_array_constructor_versioned,
                    id.ptr,
                    opcode as u32,
                    argument_list.as_mut_ptr(),
                    child_interface_ptr,
                    child_version
                )
            }
        } else {
            // We are a guest Backend, need to use a wrapper
            unsafe {
                let wrapped_ptr =
                    ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_create_wrapper, id.ptr);
                ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_set_queue, wrapped_ptr, guard.evq);
                let ret = ffi_dispatch!(
                    WAYLAND_CLIENT_HANDLE,
                    wl_proxy_marshal_array_constructor_versioned,
                    wrapped_ptr,
                    opcode as u32,
                    argument_list.as_mut_ptr(),
                    child_interface_ptr,
                    child_version
                );
                ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_wrapper_destroy, wrapped_ptr);
                ret
            }
        };

        unsafe {
            free_arrays(message_desc.signature, &argument_list);
        }

        if ret.is_null() && child_spec.is_some() {
            panic!("[wayland-backend-sys] libwayland reported an allocation failure.");
        }

        // initialize the proxy
        let child_id = if let Some((child_interface, _)) = child_spec {
            let child_alive = Arc::new(AtomicBool::new(true));
            let child_id = ObjectId {
                id: InnerObjectId {
                    ptr: ret,
                    alive: Some(child_alive.clone()),
                    id: unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_id, ret) },
                    interface: child_interface,
                },
            };
            let child_udata = match data {
                Some(data) => {
                    Box::new(ProxyUserData { alive: child_alive, data, interface: child_interface })
                }
                None => {
                    // we destroy this proxy before panicking to avoid a leak, as it cannot be destroyed by the
                    // main destructor given it does not yet have a proper user-data
                    unsafe {
                        ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_destroy, ret);
                    }
                    panic!(
                        "Sending a request creating an object without providing an object data."
                    );
                }
            };
            guard.known_proxies.insert(ret);
            unsafe {
                ffi_dispatch!(
                    WAYLAND_CLIENT_HANDLE,
                    wl_proxy_add_dispatcher,
                    ret,
                    dispatcher_func,
                    &RUST_MANAGED as *const u8 as *const c_void,
                    Box::into_raw(child_udata) as *mut c_void
                );
            }
            child_id
        } else {
            Self::null_id()
        };

        if message_desc.is_destructor {
            if let Some(ref alive) = id.alive {
                let udata = unsafe {
                    Box::from_raw(ffi_dispatch!(
                        WAYLAND_CLIENT_HANDLE,
                        wl_proxy_get_user_data,
                        id.ptr
                    ) as *mut ProxyUserData)
                };
                unsafe {
                    ffi_dispatch!(
                        WAYLAND_CLIENT_HANDLE,
                        wl_proxy_set_user_data,
                        id.ptr,
                        std::ptr::null_mut()
                    );
                }
                alive.store(false, Ordering::Release);
                udata.data.destroyed(ObjectId { id: id.clone() });
            }
            guard.known_proxies.remove(&id.ptr);
            unsafe {
                ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_destroy, id.ptr);
            }
        }

        Ok(child_id)
    }

    pub fn get_data(&self, ObjectId { id }: ObjectId) -> Result<Arc<dyn ObjectData>, InvalidId> {
        if !id.alive.as_ref().map(|a| a.load(Ordering::Acquire)).unwrap_or(false) {
            return Err(InvalidId);
        }

        if id.id == 1 {
            // special case the display whose object data is not accessible
            return Ok(Arc::new(DumbObjectData));
        }

        let udata = unsafe {
            &*(ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_user_data, id.ptr)
                as *mut ProxyUserData)
        };
        Ok(udata.data.clone())
    }

    pub fn set_data(
        &self,
        ObjectId { id }: ObjectId,
        data: Arc<dyn ObjectData>,
    ) -> Result<(), InvalidId> {
        if !id.alive.as_ref().map(|a| a.load(Ordering::Acquire)).unwrap_or(false) {
            return Err(InvalidId);
        }

        // Cannot touch the user_data of the display
        if id.id == 1 {
            return Err(InvalidId);
        }

        let udata = unsafe {
            &mut *(ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_user_data, id.ptr)
                as *mut ProxyUserData)
        };

        udata.data = data;

        Ok(())
    }
}

unsafe extern "C" fn dispatcher_func(
    _: *const c_void,
    proxy: *mut c_void,
    opcode: u32,
    _: *const wl_message,
    args: *const wl_argument,
) -> c_int {
    let proxy = proxy as *mut wl_proxy;

    // Safety: if our dispatcher fun is called, then the associated proxy must be rust_managed and have a valid user_data
    let udata_ptr = unsafe {
        ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_user_data, proxy) as *mut ProxyUserData
    };
    let udata = unsafe { &mut *udata_ptr };

    let interface = udata.interface;
    let message_desc = match interface.events.get(opcode as usize) {
        Some(desc) => desc,
        None => {
            crate::log_error!("Unknown event opcode {} for interface {}.", opcode, interface.name);
            return -1;
        }
    };

    let mut parsed_args =
        SmallVec::<[Argument<ObjectId>; 4]>::with_capacity(message_desc.signature.len());
    let mut arg_interfaces = message_desc.arg_interfaces.iter().copied();
    let mut created = None;
    // Safety (args deference): the args array provided by libwayland is well-formed
    for (i, typ) in message_desc.signature.iter().enumerate() {
        match typ {
            ArgumentType::Uint => parsed_args.push(Argument::Uint(unsafe { (*args.add(i)).u })),
            ArgumentType::Int => parsed_args.push(Argument::Int(unsafe { (*args.add(i)).i })),
            ArgumentType::Fixed => parsed_args.push(Argument::Fixed(unsafe { (*args.add(i)).f })),
            ArgumentType::Fd => parsed_args.push(Argument::Fd(unsafe { (*args.add(i)).h })),
            ArgumentType::Array(_) => {
                let array = unsafe { &*((*args.add(i)).a) };
                // Safety: the array provided by libwayland must be valid
                let content =
                    unsafe { std::slice::from_raw_parts(array.data as *mut u8, array.size) };
                parsed_args.push(Argument::Array(Box::new(content.into())));
            }
            ArgumentType::Str(_) => {
                let ptr = unsafe { (*args.add(i)).s };
                // Safety: the c-string provided by libwayland must be valid
                let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
                parsed_args.push(Argument::Str(Box::new(cstr.into())));
            }
            ArgumentType::Object(_) => {
                let obj = unsafe { (*args.add(i)).o as *mut wl_proxy };
                if !obj.is_null() {
                    // retrieve the object relevant info
                    let obj_id = ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_id, obj);
                    // check if this is a local or distant proxy
                    let next_interface = arg_interfaces.next().unwrap_or(&ANONYMOUS_INTERFACE);
                    let listener = ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_listener, obj);
                    if listener == &RUST_MANAGED as *const u8 as *const c_void {
                        // Safety: the object is rust-managed, its user-data must be valid
                        let obj_udata = unsafe {
                            &*(ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_user_data, obj)
                                as *mut ProxyUserData)
                        };
                        if !same_interface(next_interface, obj_udata.interface) {
                            crate::log_error!(
                                "Received object {}@{} in {}.{} but expected interface {}.",
                                obj_udata.interface.name,
                                obj_id,
                                interface.name,
                                message_desc.name,
                                next_interface.name,
                            );
                            return -1;
                        }
                        parsed_args.push(Argument::Object(ObjectId {
                            id: InnerObjectId {
                                alive: Some(obj_udata.alive.clone()),
                                ptr: obj,
                                id: obj_id,
                                interface: obj_udata.interface,
                            },
                        }));
                    } else {
                        parsed_args.push(Argument::Object(ObjectId {
                            id: InnerObjectId {
                                alive: None,
                                id: obj_id,
                                ptr: obj,
                                interface: next_interface,
                            },
                        }));
                    }
                } else {
                    // libwayland-client.so checks nulls for us
                    parsed_args.push(Argument::Object(ObjectId {
                        id: InnerObjectId {
                            alive: None,
                            id: 0,
                            ptr: std::ptr::null_mut(),
                            interface: &ANONYMOUS_INTERFACE,
                        },
                    }))
                }
            }
            ArgumentType::NewId(_) => {
                let obj = unsafe { (*args.add(i)).o as *mut wl_proxy };
                // this is a newid, it needs to be initialized
                if !obj.is_null() {
                    let child_interface = message_desc.child_interface.unwrap_or_else(|| {
                        crate::log_warn!(
                            "Event {}.{} creates an anonymous object.",
                            interface.name,
                            opcode
                        );
                        &ANONYMOUS_INTERFACE
                    });
                    let child_alive = Arc::new(AtomicBool::new(true));
                    let child_id = InnerObjectId {
                        ptr: obj,
                        alive: Some(child_alive.clone()),
                        id: ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_id, obj),
                        interface: child_interface,
                    };
                    let child_udata = Box::into_raw(Box::new(ProxyUserData {
                        alive: child_alive,
                        data: Arc::new(UninitObjectData),
                        interface: child_interface,
                    }));
                    created = Some((child_id.clone(), child_udata));
                    ffi_dispatch!(
                        WAYLAND_CLIENT_HANDLE,
                        wl_proxy_add_dispatcher,
                        obj,
                        dispatcher_func,
                        &RUST_MANAGED as *const u8 as *const c_void,
                        child_udata as *mut c_void
                    );
                    parsed_args.push(Argument::NewId(ObjectId { id: child_id }));
                } else {
                    parsed_args.push(Argument::NewId(ObjectId {
                        id: InnerObjectId {
                            id: 0,
                            ptr: std::ptr::null_mut(),
                            alive: None,
                            interface: &ANONYMOUS_INTERFACE,
                        },
                    }))
                }
            }
        }
    }

    let proxy_id = ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_get_id, proxy);
    let id = ObjectId {
        id: InnerObjectId {
            alive: Some(udata.alive.clone()),
            ptr: proxy,
            id: proxy_id,
            interface: udata.interface,
        },
    };

    let ret = BACKEND.with(|backend| {
        let mut guard = backend.backend.lock_state();
        if let Some((ref new_id, _)) = created {
            guard.known_proxies.insert(new_id.ptr);
        }
        if message_desc.is_destructor {
            guard.known_proxies.remove(&proxy);
        }
        std::mem::drop(guard);
        udata.data.clone().event(
            backend,
            Message { sender_id: id.clone(), opcode: opcode as u16, args: parsed_args },
        )
    });

    if message_desc.is_destructor {
        // Safety: the udata_ptr must be valid as we are in a rust-managed object, and we are done with using udata
        let udata = unsafe { Box::from_raw(udata_ptr) };
        ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_set_user_data, proxy, std::ptr::null_mut());
        udata.alive.store(false, Ordering::Release);
        udata.data.destroyed(id);
        ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_destroy, proxy);
    }

    match (created, ret) {
        (Some((_, child_udata_ptr)), Some(child_data)) => {
            // Safety: child_udata_ptr is valid, we created it earlier
            unsafe {
                (*child_udata_ptr).data = child_data;
            }
        }
        (Some((child_id, _)), None) => {
            panic!("Callback creating object {} did not provide any object data.", child_id);
        }
        (None, Some(_)) => {
            panic!("An object data was returned from a callback not creating any object");
        }
        (None, None) => {}
    }

    0
}

#[cfg(feature = "log")]
extern "C" {
    fn wl_log_trampoline_to_rust_client(fmt: *const std::os::raw::c_char, list: *const c_void);
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        // Cleanup the objects we know about, libwayland will discard any future message
        // they receive.
        for proxy_ptr in self.known_proxies.drain() {
            let _ = unsafe {
                Box::from_raw(ffi_dispatch!(
                    WAYLAND_CLIENT_HANDLE,
                    wl_proxy_get_user_data,
                    proxy_ptr
                ) as *mut ProxyUserData)
            };
            unsafe {
                ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_proxy_destroy, proxy_ptr);
            }
        }
        if self.evq.is_null() {
            // we own the connection, close it
            unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_display_disconnect, self.display) }
        } else {
            // we don't own the connecton, just destroy the event queue
            unsafe { ffi_dispatch!(WAYLAND_CLIENT_HANDLE, wl_event_queue_destroy, self.evq) }
        }
    }
}
