#![warn(missing_docs)]

//! The Rust implementation of the Pinnacle API.

mod input;
mod msg;
mod output;
mod process;
mod tag;
mod window;

use input::libinput::Libinput;
use input::Input;
use output::Output;
use tag::Tag;
use window::rules::WindowRules;
use window::Window;

/// The xkbcommon crate, re-exported for your convenience.
pub use xkbcommon;

/// The prelude for the Pinnacle API.
///
/// This contains useful imports that you will likely need.
/// To that end, you can do `use pinnacle_api::prelude::*` to
/// prevent your config file from being cluttered with imports.
pub mod prelude {
    pub use crate::input::Modifier;
    pub use crate::input::MouseButton;
    pub use crate::input::MouseEdge;
    pub use crate::output::AlignmentHorizontal;
    pub use crate::output::AlignmentVertical;
    pub use crate::tag::Layout;
    pub use crate::window::rules::WindowRule;
    pub use crate::window::rules::WindowRuleCondition;
    pub use crate::window::FloatingOrTiled;
    pub use crate::window::FullscreenOrMaximized;
}

/// Re-exports of every config struct.
///
/// Usually you can just use the [`Pinnacle`][crate::Pinnacle] struct passed into
/// the `setup` function, but if you need access to these elsewhere, here they are.
pub mod modules {
    pub use crate::input::libinput::Libinput;
    pub use crate::input::Input;
    pub use crate::output::Output;
    pub use crate::process::Process;
    pub use crate::tag::Tag;
    pub use crate::window::rules::WindowRules;
    pub use crate::window::Window;
}

use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{atomic::AtomicU32, Mutex, OnceLock},
};

use msg::{Args, CallbackId, IncomingMsg, Msg, Request, RequestResponse};
use process::Process;

use crate::msg::RequestId;

static STREAM: OnceLock<Mutex<UnixStream>> = OnceLock::new();
#[allow(clippy::type_complexity)]
static CALLBACK_VEC: Mutex<Vec<Box<dyn FnMut(Option<Args>) + Send>>> = Mutex::new(Vec::new());
lazy_static::lazy_static! {
    static ref UNREAD_CALLBACK_MSGS: Mutex<HashMap<CallbackId, IncomingMsg>> = Mutex::new(HashMap::new());
    static ref UNREAD_REQUEST_MSGS: Mutex<HashMap<RequestId, IncomingMsg>> = Mutex::new(HashMap::new());
}

static REQUEST_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Setup Pinnacle.
pub fn setup(config_func: impl FnOnce(Pinnacle)) -> anyhow::Result<()> {
    STREAM
        .set(Mutex::new(UnixStream::connect(PathBuf::from(
            std::env::var("PINNACLE_SOCKET").unwrap_or("/tmp/pinnacle_socket".to_string()),
        ))?))
        .unwrap();

    let pinnacle = Pinnacle {
        process: Process,
        input: Input { libinput: Libinput },
        window: Window { rules: WindowRules },
        output: Output,
        tag: Tag,
    };

    config_func(pinnacle);

    loop {
        let mut unread_callback_msgs = UNREAD_CALLBACK_MSGS.lock().unwrap();
        let mut callback_vec = CALLBACK_VEC.lock().unwrap();
        let mut to_remove = vec![];
        for (cb_id, incoming_msg) in unread_callback_msgs.iter() {
            let IncomingMsg::CallCallback { callback_id, args } = incoming_msg else {
                continue;
            };
            let Some(f) = callback_vec.get_mut(callback_id.0 as usize) else {
                continue;
            };
            f(args.clone());
            to_remove.push(*cb_id);
        }
        for id in to_remove {
            unread_callback_msgs.remove(&id);
        }

        let incoming_msg = read_msg(None);

        assert!(matches!(incoming_msg, IncomingMsg::CallCallback { .. }));

        let IncomingMsg::CallCallback { callback_id, args } = incoming_msg else {
            unreachable!()
        };

        let Some(f) = callback_vec.get_mut(callback_id.0 as usize) else {
            continue;
        };

        f(args);
    }
}

fn send_msg(msg: Msg) -> anyhow::Result<()> {
    let mut msg = rmp_serde::encode::to_vec_named(&msg)?;
    let mut msg_len = (msg.len() as u32).to_ne_bytes();

    let mut stream = STREAM.get().unwrap().lock().unwrap();

    stream.write_all(msg_len.as_mut_slice())?;
    stream.write_all(msg.as_mut_slice())?;

    Ok(())
}

fn read_msg(request_id: Option<RequestId>) -> IncomingMsg {
    loop {
        if let Some(request_id) = request_id {
            if let Some(msg) = UNREAD_REQUEST_MSGS.lock().unwrap().remove(&request_id) {
                return msg;
            }
        }

        let mut stream = STREAM.get().unwrap().lock().unwrap();
        let mut msg_len_bytes = [0u8; 4];
        stream.read_exact(msg_len_bytes.as_mut_slice()).unwrap();

        let msg_len = u32::from_ne_bytes(msg_len_bytes);
        let mut msg_bytes = vec![0u8; msg_len as usize];
        stream.read_exact(msg_bytes.as_mut_slice()).unwrap();

        let incoming_msg: IncomingMsg = rmp_serde::from_slice(msg_bytes.as_slice()).unwrap();

        if let Some(request_id) = request_id {
            match &incoming_msg {
                IncomingMsg::CallCallback {
                    callback_id,
                    args: _,
                } => {
                    UNREAD_CALLBACK_MSGS
                        .lock()
                        .unwrap()
                        .insert(*callback_id, incoming_msg);
                }
                IncomingMsg::RequestResponse {
                    request_id: req_id,
                    response: _,
                } => {
                    if req_id != &request_id {
                        UNREAD_REQUEST_MSGS
                            .lock()
                            .unwrap()
                            .insert(*req_id, incoming_msg);
                    } else {
                        return incoming_msg;
                    }
                }
            }
        } else {
            return incoming_msg;
        }
    }
}

fn request(request: Request) -> RequestResponse {
    use std::sync::atomic::Ordering;
    let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);

    let msg = Msg::Request {
        request_id: RequestId(request_id),
        request,
    };
    send_msg(msg).unwrap(); // TODO: propogate

    let IncomingMsg::RequestResponse {
        request_id: _,
        response,
    } = read_msg(Some(RequestId(request_id)))
    else {
        unreachable!()
    };

    response
}

/// The entry to configuration.
///
/// This struct houses every submodule you'll need to configure Pinnacle.
#[derive(Clone, Copy)]
pub struct Pinnacle {
    /// Process management.
    pub process: Process,
    /// Window management.
    pub window: Window,
    /// Input management.
    pub input: Input,
    /// Output management.
    pub output: Output,
    /// Tag management.
    pub tag: Tag,
}

impl Pinnacle {
    pub fn quit(&self) {
        send_msg(Msg::Quit).unwrap();
    }
}
