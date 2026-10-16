use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use tungstenite::WebSocket;
use tungstenite::client::IntoClientRequest;

use crate::{obs_debug, obs_info, obs_warning};

/// Bound on the TCP connection and the WebSocket handshake, so that an
/// unreachable host can't block the caller for the OS's default timeout
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a connection waiting for messages checks whether it must stop
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// A connection to the OverlayPlugin WebSocket, forwarding its messages to a
/// channel from its own thread. Dropping it stops and joins the thread
pub struct Connection {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Connection {
    pub fn spawn(url: String, sender: mpsc::Sender<Vec<u8>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                run(&url, &sender, &stop);
                obs_debug!("WebSocket thread finished");
            }
        });

        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Connect to `url` (`ws://` only), with a bounded connection and handshake
pub fn connect(url: &str) -> Result<WebSocket<TcpStream>, String> {
    let request = url.into_client_request().map_err(|e| e.to_string())?;
    let uri = request.uri();
    if uri.scheme_str() != Some("ws") {
        return Err("only ws:// URLs are supported".to_string());
    }
    let host = uri.host().ok_or("missing host")?;
    // IPv6 literals keep their brackets in the URI
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(80);
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("cannot resolve host")?;

    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let (ws, _response) = tungstenite::client(request, stream).map_err(|e| e.to_string())?;
    ws.get_ref()
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    Ok(ws)
}

fn run(url: &str, sender: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool) {
    let mut ws = match connect(url) {
        Ok(ws) => ws,
        Err(e) => {
            obs_warning!("Failed to connect to {url}, falling back to dummy debug producer: {e}");
            run_dummy(sender, stop);
            return;
        }
    };
    obs_info!("Connected to {url}");

    while !stop.load(Ordering::Relaxed) {
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => {
                obs_warning!("Error reading from WebSocket: {e}");
                return;
            }
        };
        // Pings, pongs and close frames aren't OverlayPlugin messages
        if !(msg.is_text() || msg.is_binary()) {
            continue;
        }
        if sender.send(msg.into_data().into()).is_err() {
            break;
        }
    }

    let _ = ws.close(None);
    let _ = ws.flush();
}

fn run_dummy(sender: &mpsc::Sender<Vec<u8>>, stop: &AtomicBool) {
    let ticks_per_message = Duration::from_secs(1).as_millis() / READ_TIMEOUT.as_millis();
    let mut ticks = 0;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(READ_TIMEOUT);
        ticks += 1;
        if ticks % ticks_per_message != 0 {
            continue;
        }
        obs_debug!("Simulating WebSocket data");
        if sender.send(b"SEI RAIDWIDE".to_vec()).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    #[test]
    fn forwards_messages_and_stops_while_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            ws.send(tungstenite::Message::text("{\"type\":\"ChangeZone\"}"))
                .unwrap();
            ws.send(tungstenite::Message::Ping(Default::default()))
                .unwrap();
            ws.send(tungstenite::Message::binary(b"{}".to_vec()))
                .unwrap();
            // Stay connected and quiet until the client closes
            while ws.read().is_ok() {}
        });

        let (sender, receiver) = mpsc::channel();
        let connection = Connection::spawn(url, sender);
        let timeout = Duration::from_secs(5);
        assert_eq!(
            receiver.recv_timeout(timeout).unwrap(),
            b"{\"type\":\"ChangeZone\"}"
        );
        assert_eq!(receiver.recv_timeout(timeout).unwrap(), b"{}");

        let start = Instant::now();
        drop(connection);
        assert!(start.elapsed() < READ_TIMEOUT * 5);
        server.join().unwrap();
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(connect("wss://127.0.0.1:1").is_err());
        assert!(connect("http://127.0.0.1:1").is_err());
    }
}
