//! TCP connections to numeric IP addresses. DNS and TLS are separate components.
//! No host extension imports and no authority beyond the host's WASI policy.
mod tcp;

use std::{
    cell::RefCell,
    io::{self, Write},
    sync::atomic::{AtomicUsize, Ordering},
};
use tcp::SocketWriter;
use wasi::{
    io::streams::{InputStream, OutputStream, StreamError},
    sockets::{
        network::{ErrorCode, Network},
        tcp::{ShutdownType, TcpSocket},
    },
};

wit_bindgen::generate!({ path: "wit", world: "transport-component" });
use exports::hibana::tcp::api::{Failure, Guest, GuestSocket, Socket, State};

const CHUNK: usize = 16 * 1024;
const MAX_SOCKETS: usize = 32;
static SOCKETS: AtomicUsize = AtomicUsize::new(0);

fn fail(code: &str, message: impl ToString) -> Failure {
    Failure {
        code: code.into(),
        message: message.to_string(),
    }
}
fn net_error(e: ErrorCode) -> Failure {
    let code = match e {
        ErrorCode::AccessDenied => "EACCES",
        ErrorCode::ConnectionRefused => "ECONNREFUSED",
        ErrorCode::ConnectionReset | ErrorCode::ConnectionAborted => "ECONNRESET",
        ErrorCode::Timeout => "ETIMEDOUT",
        ErrorCode::NameUnresolvable => "ENOTFOUND",
        ErrorCode::PermanentResolverFailure => "EAI_FAIL",
        ErrorCode::TemporaryResolverFailure => "EAI_AGAIN",
        ErrorCode::RemoteUnreachable => "EHOSTUNREACH",
        ErrorCode::InvalidArgument => "EINVAL",
        ErrorCode::WouldBlock => "EAGAIN",
        _ => "EIO",
    };
    fail(code, format!("{e:?}"))
}
fn stream_error(e: StreamError) -> Failure {
    fail("EIO", format!("{e:?}"))
}

struct Transport;
pub struct GuestConnection(RefCell<Option<Connection>>);

// Children must be dropped before their parent WASI resources.
struct Connection {
    input: Option<InputStream>,
    output: Option<OutputStream>,
    socket: Option<TcpSocket>,
    _network: Network,
    ending: bool,
    write_closed: bool,
}
impl Drop for Connection {
    fn drop(&mut self) {
        SOCKETS.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Guest for Transport {
    type Socket = GuestConnection;
    fn connect(host: String, port: u16) -> Result<Socket, Failure> {
        tcp::connect(host, port)
    }

    fn ip_version(address: String) -> u8 {
        match address.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(_)) => 4,
            Ok(std::net::IpAddr::V6(_)) => 6,
            Err(_) => 0,
        }
    }
}

impl GuestConnection {
    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T, Failure>) -> Result<T, Failure> {
        let mut connection = self.0.borrow_mut();
        f(connection
            .as_mut()
            .ok_or_else(|| fail("ERR_SOCKET_CLOSED", "Socket is closed"))?)
    }
}

impl GuestSocket for GuestConnection {
    fn status(&self) -> Result<State, Failure> {
        self.with(|c| {
            c.progress()?;
            Ok(State {
                connected: c.input.is_some(),
                write_closed: c.write_closed,
            })
        })
    }
    fn read(&self) -> Result<Option<Vec<u8>>, Failure> {
        self.with(|c| {
            let Some(input) = &c.input else {
                return Ok(None);
            };
            match input.read(CHUNK as u64) {
                Ok(bytes) if bytes.is_empty() => Ok(None),
                Ok(bytes) => Ok(Some(bytes)),
                Err(StreamError::Closed) => Ok(Some(vec![])),
                Err(e) => Err(stream_error(e)),
            }
        })
    }
    fn write(&self, data: Vec<u8>) -> Result<u32, Failure> {
        self.with(|c| {
            if c.ending {
                return Err(fail("EPIPE", "Socket write side has ended"));
            }
            let Some(output) = &c.output else {
                return Ok(0);
            };
            match SocketWriter(output).write(&data[..data.len().min(CHUNK)]) {
                Ok(n) => Ok(n as u32),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(fail("EIO", e)),
            }
        })
    }

    fn end(&self) -> Result<(), Failure> {
        self.with(|c| {
            c.ending = true;
            Ok(())
        })
    }
    fn keep_alive(&self, enabled: bool, idle_ms: u32) -> Result<(), Failure> {
        self.with(|c| {
            let socket = c
                .socket
                .as_ref()
                .ok_or_else(|| fail("ERR_SOCKET_CLOSED", "Socket is not connected"))?;
            socket.set_keep_alive_enabled(enabled).map_err(net_error)?;
            if enabled && idle_ms > 0 {
                socket
                    .set_keep_alive_idle_time(u64::from(idle_ms) * 1_000_000)
                    .map_err(net_error)?;
            }
            Ok(())
        })
    }
    fn close(&self) {
        self.0.borrow_mut().take();
    }
}

impl Connection {
    // Each phase is nonblocking; the JS scheduler yields between calls.
    fn progress(&mut self) -> Result<(), Failure> {
        if !self.progress_connect()? {
            return Ok(());
        }
        self.finish_write()
    }

    fn finish_write(&mut self) -> Result<(), Failure> {
        let output = self.output.as_ref().unwrap();
        if self.ending && !self.write_closed && output.check_write().map_err(stream_error)? > 0 {
            self.socket
                .as_ref()
                .unwrap()
                .shutdown(ShutdownType::Send)
                .map_err(net_error)?;
            self.write_closed = true;
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
export!(Transport);
