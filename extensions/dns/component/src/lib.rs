//! Nonblocking DNS resolution only. This component cannot create TCP sockets.
use std::{
    cell::RefCell,
    sync::atomic::{AtomicUsize, Ordering},
};
use wasi::sockets::{
    instance_network::instance_network,
    ip_name_lookup::{resolve_addresses, ResolveAddressStream},
    network::{ErrorCode, IpAddress, Network},
};
wit_bindgen::generate!({path:"wit",world:"resolver"});
use exports::hibana::dns::api::{Answer, Failure, Guest, GuestQuery, Query};
struct Resolver;
pub struct GuestLookup(RefCell<Option<Lookup>>);
struct Lookup {
    stream: Option<ResolveAddressStream>,
    _network: Network,
}
static QUERIES: AtomicUsize = AtomicUsize::new(0);
impl Drop for Lookup {
    fn drop(&mut self) {
        QUERIES.fetch_sub(1, Ordering::Relaxed);
    }
}
fn fail(code: &str, message: impl ToString) -> Failure {
    Failure {
        code: code.into(),
        message: message.to_string(),
    }
}
fn resolve_error(error: ErrorCode) -> Failure {
    let code = match error {
        ErrorCode::AccessDenied => "EACCES",
        ErrorCode::NameUnresolvable => "ENOTFOUND",
        ErrorCode::TemporaryResolverFailure => "EAI_AGAIN",
        ErrorCode::PermanentResolverFailure => "EAI_FAIL",
        ErrorCode::InvalidArgument => "EINVAL",
        _ => "EIO",
    };
    fail(code, format!("{error:?}"))
}
impl Guest for Resolver {
    type Query = GuestLookup;
    fn lookup(host: String) -> Result<Query, Failure> {
        if host.is_empty() || host.len() > 253 {
            return Err(fail("EINVAL", "Invalid hostname"));
        }
        if QUERIES.load(Ordering::Relaxed) >= 32 {
            return Err(fail("EMFILE", "At most 32 DNS queries per instance"));
        }
        let network = instance_network();
        let stream = resolve_addresses(&network, &host).map_err(resolve_error)?;
        QUERIES.fetch_add(1, Ordering::Relaxed);
        Ok(Query::new(GuestLookup(RefCell::new(Some(Lookup {
            stream: Some(stream),
            _network: network,
        })))))
    }
}
impl GuestQuery for GuestLookup {
    fn next(&self) -> Result<Answer, Failure> {
        let mut slot = self.0.borrow_mut();
        let lookup = slot
            .as_mut()
            .ok_or_else(|| fail("ERR_DNS_CLOSED", "DNS query is closed"))?;
        let Some(stream) = &lookup.stream else {
            return Ok(Answer {
                address: None,
                done: true,
            });
        };
        match stream.resolve_next_address() {
            Ok(Some(ip)) => {
                let address = match ip {
                    IpAddress::Ipv4((a, b, c, d)) => {
                        std::net::Ipv4Addr::new(a, b, c, d).to_string()
                    }
                    IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => {
                        std::net::Ipv6Addr::new(a, b, c, d, e, f, g, h).to_string()
                    }
                };
                Ok(Answer {
                    address: Some(address),
                    done: false,
                })
            }
            Ok(None) => {
                lookup.stream = None;
                Ok(Answer {
                    address: None,
                    done: true,
                })
            }
            Err(ErrorCode::WouldBlock) => Ok(Answer {
                address: None,
                done: false,
            }),
            Err(error) => {
                lookup.stream = None;
                Err(resolve_error(error))
            }
        }
    }
    fn close(&self) {
        self.0.borrow_mut().take();
    }
}
#[cfg(target_arch = "wasm32")]
export!(Resolver);
