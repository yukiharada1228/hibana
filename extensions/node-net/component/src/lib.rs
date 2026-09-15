//! Application-owned, nonblocking WASI TCP + rustls transport.
//! No host extension imports and no authority beyond the host's WASI policy.
use rustls::{pki_types::ServerName, ClientConfig, ClientConnection, RootCertStore};
use std::{
    cell::RefCell,
    io::{self, Read, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use wasi::{
    io::streams::{InputStream, OutputStream, StreamError},
    sockets::{
        instance_network::instance_network,
        ip_name_lookup::{resolve_addresses, ResolveAddressStream},
        network::{
            ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress,
            Ipv6SocketAddress, Network,
        },
        tcp::{ShutdownType, TcpSocket},
        tcp_create_socket::create_tcp_socket,
    },
};

wit_bindgen::generate!({ path: "wit", world: "transport-component" });
use exports::hibana::node_net::transport::{
    Failure, Guest, GuestSocket, Socket, State, TlsOptions,
};

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
fn tls_error(e: impl ToString) -> Failure {
    fail("ERR_TLS_CONNECTION", e)
}

struct Transport;
pub struct GuestConnection(RefCell<Option<Connection>>);

// Children must be dropped before their parent WASI resources.
struct Connection {
    input: Option<InputStream>,
    output: Option<OutputStream>,
    socket: Option<TcpSocket>,
    resolver: Option<ResolveAddressStream>,
    network: Network,
    port: u16,
    literal_address: Option<IpAddress>,
    tls: Option<ClientConnection>,
    ending: bool,
    write_closed: bool,
    eof: bool,
    last_error: Option<Failure>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        SOCKETS.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Guest for Transport {
    type Socket = GuestConnection;
    fn connect(host: String, port: u16) -> Result<Socket, Failure> {
        if host.is_empty() || host.len() > 253 || port == 0 {
            return Err(fail("EINVAL", "Invalid host or port"));
        }
        if SOCKETS.load(Ordering::Relaxed) >= MAX_SOCKETS {
            return Err(fail("EMFILE", "At most 32 sockets per instance"));
        }
        let network = instance_network();
        let literal_address = host.parse::<std::net::IpAddr>().ok().map(|ip| match ip {
            std::net::IpAddr::V4(ip) => {
                let [a, b, c, d] = ip.octets();
                IpAddress::Ipv4((a, b, c, d))
            }
            std::net::IpAddr::V6(ip) => {
                let [a, b, c, d, e, f, g, h] = ip.segments();
                IpAddress::Ipv6((a, b, c, d, e, f, g, h))
            }
        });
        let resolver = if literal_address.is_none() {
            Some(resolve_addresses(&network, &host).map_err(net_error)?)
        } else {
            None
        };
        SOCKETS.fetch_add(1, Ordering::Relaxed);
        Ok(Socket::new(GuestConnection(RefCell::new(Some(
            Connection {
                input: None,
                output: None,
                socket: None,
                resolver,
                network,
                port,
                literal_address,
                tls: None,
                ending: false,
                write_closed: false,
                eof: false,
                last_error: None,
            },
        )))))
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
                secure: c.tls.as_ref().is_some_and(|t| !t.is_handshaking()),
                write_closed: c.write_closed,
                alpn: c
                    .tls
                    .as_ref()
                    .and_then(|t| t.alpn_protocol())
                    .map(|p| String::from_utf8_lossy(p).into_owned()),
            })
        })
    }
    fn read(&self) -> Result<Option<Vec<u8>>, Failure> {
        self.with(|c| {
            let Some(input) = &c.input else {
                return Ok(None);
            };
            if let Some(tls) = &mut c.tls {
                if tls.is_handshaking() {
                    return Ok(None);
                }
                let mut bytes = vec![0; CHUNK];
                return match tls.reader().read(&mut bytes) {
                    Ok(n) => {
                        bytes.truncate(n);
                        Ok(Some(bytes))
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                    Err(e) => Err(tls_error(e)),
                };
            }
            match input.read(CHUNK as u64) {
                Ok(bytes) if bytes.is_empty() => Ok(None),
                Ok(bytes) => Ok(Some(bytes)),
                Err(StreamError::Closed) => {
                    c.eof = true;
                    Ok(Some(vec![]))
                }
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
            let data = &data[..data.len().min(CHUNK)];
            let result = if let Some(tls) = &mut c.tls {
                if tls.is_handshaking() {
                    return Ok(0);
                }
                tls.writer().write(data)
            } else {
                SocketWriter(output).write(data)
            };
            match result {
                Ok(n) => Ok(n as u32),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(fail("EIO", e)),
            }
        })
    }
    fn start_tls(&self, options: TlsOptions) -> Result<(), Failure> {
        self.with(|c| {
            if c.input.is_none() || c.tls.is_some() || c.ending || c.eof {
                return Err(fail(
                    "ERR_TLS_INVALID_STATE",
                    "TLS requires a connected, open plain socket",
                ));
            }
            c.tls = Some(make_tls(options)?);
            Ok(())
        })
    }
    fn end(&self) -> Result<(), Failure> {
        self.with(|c| {
            if !c.ending {
                c.ending = true;
                if let Some(tls) = &mut c.tls {
                    tls.send_close_notify();
                }
            }
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
        self.progress_tls()?;
        self.finish_write()
    }

    fn progress_connect(&mut self) -> Result<bool, Failure> {
        if self.input.is_none() {
            if let Some(socket) = &self.socket {
                match socket.finish_connect() {
                    Ok((input, output)) => {
                        self.input = Some(input);
                        self.output = Some(output);
                        self.resolver = None;
                    }
                    Err(ErrorCode::WouldBlock) => return Ok(false),
                    Err(e) => {
                        self.last_error = Some(net_error(e));
                        self.socket = None;
                    }
                }
            }
            if self.input.is_none() {
                let next_address = if let Some(address) = self.literal_address.take() {
                    Ok(Some(address))
                } else if let Some(resolver) = &self.resolver {
                    resolver.resolve_next_address()
                } else {
                    Ok(None)
                };
                let address = match next_address {
                    Ok(Some(address)) => address,
                    Ok(None) => {
                        return Err(self
                            .last_error
                            .take()
                            .unwrap_or_else(|| fail("ENOTFOUND", "No usable address")))
                    }
                    Err(ErrorCode::WouldBlock) => return Ok(false),
                    Err(e) => return Err(net_error(e)),
                };
                let (family, address) = match address {
                    IpAddress::Ipv4(address) => (
                        IpAddressFamily::Ipv4,
                        IpSocketAddress::Ipv4(Ipv4SocketAddress {
                            address,
                            port: self.port,
                        }),
                    ),
                    IpAddress::Ipv6(address) => (
                        IpAddressFamily::Ipv6,
                        IpSocketAddress::Ipv6(Ipv6SocketAddress {
                            address,
                            port: self.port,
                            flow_info: 0,
                            scope_id: 0,
                        }),
                    ),
                };
                let socket = create_tcp_socket(family).map_err(net_error)?;
                match socket.start_connect(&self.network, address) {
                    Ok(()) => self.socket = Some(socket),
                    Err(e) => self.last_error = Some(net_error(e)),
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn progress_tls(&mut self) -> Result<(), Failure> {
        let input = self.input.as_ref().unwrap();
        let output = self.output.as_ref().unwrap();
        if let Some(tls) = &mut self.tls {
            if tls.wants_write() {
                match tls.write_tls(&mut SocketWriter(output)) {
                    Ok(_) => (),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                    Err(e) => return Err(tls_error(e)),
                }
            }
            if tls.wants_read() && !self.eof {
                match input.read(CHUNK as u64) {
                    Ok(bytes) if bytes.is_empty() => (),
                    Ok(bytes) => {
                        let mut source = io::Cursor::new(bytes);
                        while (source.position() as usize) < source.get_ref().len() {
                            let n = tls.read_tls(&mut source).map_err(tls_error)?;
                            tls.process_new_packets().map_err(tls_error)?;
                            if n == 0 {
                                return Err(tls_error("TLS input made no progress"));
                            }
                        }
                    }
                    Err(StreamError::Closed) => {
                        self.eof = true;
                        tls.read_tls(&mut io::empty()).map_err(tls_error)?;
                    }
                    Err(e) => return Err(stream_error(e)),
                }
            }
        }
        Ok(())
    }

    fn finish_write(&mut self) -> Result<(), Failure> {
        let output = self.output.as_ref().unwrap();
        if self.ending
            && !self.write_closed
            && !self.tls.as_ref().is_some_and(|t| t.wants_write())
            && output.check_write().map_err(stream_error)? > 0
        {
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

struct SocketWriter<'a>(&'a OutputStream);
impl Write for SocketWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let permit = self
            .0
            .check_write()
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        let n = bytes.len().min(permit as usize).min(CHUNK);
        if n == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.0
            .write(&bytes[..n])
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        self.0
            .flush()
            .map_err(|e| io::Error::other(format!("{e:?}")))?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn make_tls(options: TlsOptions) -> Result<ClientConnection, Failure> {
    let mut roots = RootCertStore::empty();
    if let Some(pem) = options.ca_pem {
        if pem.len() > 64 * 1024 {
            return Err(fail("EINVAL", "CA PEM exceeds 64 KiB"));
        }
        for cert in rustls_pemfile::certs(&mut pem.as_bytes()) {
            roots.add(cert.map_err(tls_error)?).map_err(tls_error)?;
        }
        if roots.is_empty() {
            return Err(fail("EINVAL", "CA must contain PEM certificates"));
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    if options.alpn.len() > 16 || options.alpn.iter().any(|p| p.is_empty() || p.len() > 255) {
        return Err(fail("EINVAL", "Invalid ALPN protocols"));
    }
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = options.alpn.into_iter().map(String::into_bytes).collect();
    let name = ServerName::try_from(options.server_name).map_err(tls_error)?;
    let mut connection = ClientConnection::new(Arc::new(config), name).map_err(tls_error)?;
    connection.set_buffer_limit(Some(128 * 1024));
    Ok(connection)
}

export!(Transport);
