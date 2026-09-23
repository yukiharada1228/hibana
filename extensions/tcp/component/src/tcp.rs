//! Numeric-address TCP establishment and bounded WASI writes.
use super::{
    fail, net_error, Connection, Failure, GuestConnection, Socket, CHUNK, MAX_SOCKETS, SOCKETS,
};
use std::{
    cell::RefCell,
    io::{self, Write},
    sync::atomic::Ordering,
};
use wasi::{
    io::streams::OutputStream,
    sockets::{
        instance_network::instance_network,
        network::{
            ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
        },
        tcp_create_socket::create_tcp_socket,
    },
};

pub(super) fn connect(address: String, port: u16) -> Result<Socket, Failure> {
    if port == 0 {
        return Err(fail("EINVAL", "Port must be 1..65535"));
    }
    let ip = address.parse::<std::net::IpAddr>().map_err(|_| {
        fail(
            "EINVAL",
            "TCP requires a numeric IP address; resolve hostnames with @hibana/dns",
        )
    })?;
    if SOCKETS.load(Ordering::Relaxed) >= MAX_SOCKETS {
        return Err(fail("EMFILE", "At most 32 sockets per instance"));
    }
    let (family, address) = match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            (
                IpAddressFamily::Ipv4,
                IpSocketAddress::Ipv4(Ipv4SocketAddress {
                    address: (a, b, c, d),
                    port,
                }),
            )
        }
        std::net::IpAddr::V6(ip) => {
            let [a, b, c, d, e, f, g, h] = ip.segments();
            (
                IpAddressFamily::Ipv6,
                IpSocketAddress::Ipv6(Ipv6SocketAddress {
                    address: (a, b, c, d, e, f, g, h),
                    port,
                    flow_info: 0,
                    scope_id: 0,
                }),
            )
        }
    };
    let network = instance_network();
    let socket = create_tcp_socket(family).map_err(net_error)?;
    socket.start_connect(&network, address).map_err(net_error)?;
    SOCKETS.fetch_add(1, Ordering::Relaxed);
    Ok(Socket::new(GuestConnection(RefCell::new(Some(
        Connection {
            input: None,
            output: None,
            socket: Some(socket),
            _network: network,
            ending: false,
            write_closed: false,
        },
    )))))
}
impl Connection {
    pub(super) fn progress_connect(&mut self) -> Result<bool, Failure> {
        if self.input.is_none() {
            match self.socket.as_ref().unwrap().finish_connect() {
                Ok((input, output)) => {
                    self.input = Some(input);
                    self.output = Some(output);
                }
                Err(ErrorCode::WouldBlock) => return Ok(false),
                Err(error) => return Err(net_error(error)),
            }
        }
        Ok(true)
    }
}

pub(super) struct SocketWriter<'a>(pub(super) &'a OutputStream);
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
