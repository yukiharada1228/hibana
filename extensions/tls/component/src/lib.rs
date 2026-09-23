//! TLS protocol state only. This component has no network imports or sockets.
mod config;
use rustls::ClientConnection;
use std::{
    cell::RefCell,
    io::{self, Read, Write},
    sync::atomic::{AtomicUsize, Ordering},
};

wit_bindgen::generate!({ path: "wit", world: "tls-component" });
use exports::hibana::tls::api::{Failure, Guest, GuestSession, Options, Session, State};
const CHUNK: usize = 16 * 1024;
const MAX_SESSIONS: usize = 32;
static SESSIONS: AtomicUsize = AtomicUsize::new(0);

fn fail(code: &str, message: impl ToString) -> Failure {
    Failure {
        code: code.into(),
        message: message.to_string(),
    }
}
fn tls_error(error: impl ToString) -> Failure {
    fail("ERR_TLS_CONNECTION", error)
}
struct Tls;
pub struct GuestTls(RefCell<Option<Connection>>);
struct Connection {
    tls: ClientConnection,
    failure: Option<Failure>,
    ending: bool,
}
impl Drop for Connection {
    fn drop(&mut self) {
        SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Guest for Tls {
    type Session = GuestTls;
    fn create_client(options: Options) -> Result<Session, Failure> {
        if SESSIONS.load(Ordering::Relaxed) >= MAX_SESSIONS {
            return Err(fail("EMFILE", "At most 32 TLS sessions per instance"));
        }
        let tls = config::make_tls(options)?;
        SESSIONS.fetch_add(1, Ordering::Relaxed);
        Ok(Session::new(GuestTls(RefCell::new(Some(Connection {
            tls,
            failure: None,
            ending: false,
        })))))
    }
}
impl GuestTls {
    fn with<T>(
        &self,
        action: impl FnOnce(&mut Connection) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let mut slot = self.0.borrow_mut();
        let connection = slot
            .as_mut()
            .ok_or_else(|| fail("ERR_SOCKET_CLOSED", "TLS session is closed"))?;
        if let Some(error) = &connection.failure {
            return Err(error.clone());
        }
        let result = action(connection);
        if let Err(error) = &result {
            connection.failure = Some(error.clone());
        }
        result
    }
}
impl GuestSession for GuestTls {
    fn status(&self) -> Result<State, Failure> {
        self.with(|c| {
            Ok(State {
                secure: !c.tls.is_handshaking(),
                wants_input: c.tls.wants_read(),
                wants_output: c.tls.wants_write(),
                alpn: c
                    .tls
                    .alpn_protocol()
                    .map(|value| String::from_utf8_lossy(value).into_owned()),
            })
        })
    }
    fn receive(&self, data: Vec<u8>) -> Result<(), Failure> {
        self.with(|c| {
            if data.len() > CHUNK {
                return Err(fail("EINVAL", "TLS input exceeds 16 KiB"));
            }
            let mut source = io::Cursor::new(data);
            while source.position() < source.get_ref().len() as u64 {
                let consumed = c.tls.read_tls(&mut source).map_err(tls_error)?;
                c.tls.process_new_packets().map_err(tls_error)?;
                if consumed == 0 {
                    return Err(tls_error("TLS input made no progress"));
                }
            }
            Ok(())
        })
    }
    fn receive_eof(&self) -> Result<(), Failure> {
        self.with(|c| {
            c.tls.read_tls(&mut io::empty()).map_err(tls_error)?;
            if c.tls.is_handshaking() {
                return Err(fail(
                    "ECONNRESET",
                    "TLS connection closed before handshake completed",
                ));
            }
            Ok(())
        })
    }
    fn take_output(&self) -> Result<Option<Vec<u8>>, Failure> {
        self.with(|c| {
            if !c.tls.wants_write() {
                return Ok(None);
            }
            let mut bytes = vec![0; CHUNK];
            let count = c
                .tls
                .write_tls(&mut io::Cursor::new(bytes.as_mut_slice()))
                .map_err(tls_error)?;
            bytes.truncate(count);
            Ok(Some(bytes))
        })
    }
    fn read(&self) -> Result<Option<Vec<u8>>, Failure> {
        self.with(|c| {
            if c.tls.is_handshaking() {
                return Ok(None);
            }
            let mut bytes = vec![0; CHUNK];
            match c.tls.reader().read(&mut bytes) {
                Ok(count) => {
                    bytes.truncate(count);
                    Ok(Some(bytes))
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(tls_error(e)),
            }
        })
    }
    fn write(&self, data: Vec<u8>) -> Result<u32, Failure> {
        self.with(|c| {
            if c.ending {
                return Err(fail("EPIPE", "TLS write side has ended"));
            }
            if c.tls.is_handshaking() {
                return Ok(0);
            }
            match c.tls.writer().write(&data[..data.len().min(CHUNK)]) {
                Ok(count) => Ok(count as u32),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(tls_error(e)),
            }
        })
    }
    fn end(&self) -> Result<(), Failure> {
        self.with(|c| {
            if !c.ending {
                c.ending = true;
                c.tls.send_close_notify();
            }
            Ok(())
        })
    }
    fn peer_certificate(&self) -> Result<Option<Vec<u8>>, Failure> {
        self.with(|c| {
            Ok((!c.tls.is_handshaking())
                .then(|| {
                    c.tls
                        .peer_certificates()
                        .and_then(|chain| chain.first())
                        .map(|cert| cert.as_ref().to_vec())
                })
                .flatten())
        })
    }
    fn close(&self) {
        self.0.borrow_mut().take();
    }
}
#[cfg(target_arch = "wasm32")]
export!(Tls);

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options {
        Options {
            server_name: "localhost".into(),
            ca_pem: None,
            alpn: vec![],
        }
    }
    fn session() -> GuestTls {
        let tls = config::make_tls(options()).unwrap();
        SESSIONS.fetch_add(1, Ordering::Relaxed);
        GuestTls(RefCell::new(Some(Connection {
            tls,
            failure: None,
            ending: false,
        })))
    }
    #[test]
    fn handshake_output_is_bounded_and_close_is_idempotent() {
        let tls = session();
        assert!(!tls.status().unwrap().secure);
        assert!(tls.peer_certificate().unwrap().is_none());
        assert_eq!(tls.write(vec![1]).unwrap(), 0);
        let hello = tls.take_output().unwrap().unwrap();
        assert!(!hello.is_empty() && hello.len() <= CHUNK);
        assert!(tls.take_output().unwrap().is_none());
        tls.close();
        tls.close();
        assert_eq!(tls.status().unwrap_err().code, "ERR_SOCKET_CLOSED");
    }
    #[test]
    fn eof_before_handshake_fails_and_cannot_resume() {
        let tls = session();
        assert!(tls.take_output().unwrap().is_some());
        assert_eq!(tls.receive_eof().unwrap_err().code, "ECONNRESET");
        assert_eq!(tls.status().unwrap_err().code, "ECONNRESET");
        assert_eq!(tls.read().unwrap_err().code, "ECONNRESET");
        assert_eq!(tls.write(vec![1]).unwrap_err().code, "ECONNRESET");
        assert!(tls.peer_certificate().is_err());
        tls.close();
        tls.close();
    }
    #[test]
    fn invalid_or_oversized_input_poisons_the_session() {
        for bytes in [vec![0; CHUNK + 1], vec![0; 5]] {
            let tls = session();
            let error = tls.receive(bytes).unwrap_err();
            assert_eq!(tls.status().unwrap_err().code, error.code);
            assert!(tls.peer_certificate().is_err());
            assert!(tls.write(vec![1]).is_err());
            tls.close();
        }
    }
    #[test]
    fn invalid_trust_and_names_never_use_default_settings() {
        for ca in [
            "".to_string(),
            "invalid PEM".to_string(),
            "a".repeat(64 * 1024 + 1),
        ] {
            assert!(config::make_tls(Options {
                ca_pem: Some(ca),
                ..options()
            })
            .is_err());
        }
        assert!(config::make_tls(Options {
            server_name: "bad host".into(),
            ..options()
        })
        .is_err());
        assert!(config::make_tls(Options {
            alpn: vec!["".into()],
            ..options()
        })
        .is_err());
    }
}
