use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

type Handler = dyn Fn(&str, &str) -> Vec<u8> + Send + Sync;

#[derive(Clone, Copy)]
enum Mode {
    Normal,
    RejectTransport,
    CloseAfterTransport,
}

pub(crate) struct FakeAdb {
    addr: SocketAddr,
    handler: Arc<Handler>,
    thread: Option<JoinHandle<()>>,
}

impl FakeAdb {
    pub(crate) fn start(handler: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
        Self::start_mode(move |_, command| handler(command), Mode::Normal, "")
    }

    pub(crate) fn start_with_devices(
        devices: &'static str,
        handler: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static,
    ) -> Self {
        Self::start_mode(handler, Mode::Normal, devices)
    }

    pub(crate) fn rejecting_transport() -> Self {
        Self::start_mode(|_, _| Vec::new(), Mode::RejectTransport, "")
    }

    pub(crate) fn closing_after_transport() -> Self {
        Self::start_mode(|_, _| Vec::new(), Mode::CloseAfterTransport, "")
    }

    fn start_mode(
        handler: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static,
        mode: Mode,
        devices: &'static str,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handler: Arc<Handler> = Arc::new(handler);
        let shared = handler.clone();
        let thread = thread::spawn(move || loop {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            serve(stream, &shared, mode, devices);
            if Arc::strong_count(&shared) == 1 {
                break;
            }
        });
        Self {
            addr,
            handler,
            thread: Some(thread),
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for FakeAdb {
    fn drop(&mut self) {
        self.handler = Arc::new(|_, _| Vec::new());
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let len = usize::from_str_radix(std::str::from_utf8(&prefix).unwrap(), 16)
        .map_err(|_| std::io::ErrorKind::InvalidData)?;
    let mut request = vec![0; len];
    stream.read_exact(&mut request)?;
    String::from_utf8(request).map_err(|_| std::io::ErrorKind::InvalidData.into())
}

fn serve(mut stream: TcpStream, handler: &Arc<Handler>, mode: Mode, devices: &str) {
    let Ok(service) = read_request(&mut stream) else {
        return;
    };
    if service == "host:track-devices" {
        if stream.write_all(b"OKAY").is_err() {
            return;
        }
        let body = devices.as_bytes();
        let _ = write!(stream, "{:04x}", body.len());
        let _ = stream.write_all(body);
        return;
    }
    if !service.starts_with("host:transport:") {
        return;
    }
    match mode {
        Mode::RejectTransport => {
            let message = b"offline";
            let _ = stream.write_all(b"FAIL");
            let _ = write!(stream, "{:04x}", message.len());
            let _ = stream.write_all(message);
            return;
        }
        Mode::CloseAfterTransport | Mode::Normal => {
            if stream.write_all(b"OKAY").is_err() {
                return;
            }
            if matches!(mode, Mode::CloseAfterTransport) {
                return;
            }
        }
    }
    let serial = service.trim_start_matches("host:transport:");
    let Ok(command) = read_request(&mut stream) else {
        return;
    };
    if !command.starts_with("shell,v2,raw:") {
        return;
    }
    if stream.write_all(b"OKAY").is_err() {
        return;
    }
    let bytes = handler(serial, command.trim_start_matches("shell,v2,raw:"));
    let _ = stream.write_all(&bytes);
}

pub(crate) fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}
