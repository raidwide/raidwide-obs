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
/// Delay before reconnecting, doubled after each failed attempt up to the max
const MIN_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// The OverlayPlugin events forwarded, subscribed to once connected. The
/// cached ones (zone, primary player, party) are sent right away with their
/// current value, so that a recording starting anywhere knows them
const SUBSCRIBE: &str = r#"{"call":"subscribe","events":["LogLine","ChangeZone","ChangePrimaryPlayer","PartyChanged"]}"#;

/// An OverlayPlugin message, with when it was received on OBS's clock
/// (`os_gettime_ns`), the clock of the frames' render times
pub struct Message {
    pub received_ns: u64,
    pub data: Vec<u8>,
}

impl Message {
    fn received_now(data: Vec<u8>) -> Self {
        Self {
            received_ns: unsafe { obs_sys::os_gettime_ns() },
            data,
        }
    }
}

/// A connection to the OverlayPlugin WebSocket, forwarding its messages to a
/// channel from its own thread, and reconnecting when it can't connect or the
/// connection is lost. Dropping it stops and joins the thread
pub struct Connection {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Connection {
    pub fn spawn(url: String, sender: mpsc::Sender<Message>) -> Self {
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

    /// Stop the connection without waiting for its thread to finish
    pub fn stop_in_background(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread = None;
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

fn run(url: &str, sender: &mpsc::Sender<Message>, stop: &AtomicBool) {
    let mut retry_delay = MIN_RETRY_DELAY;
    let mut failed_attempts = 0;

    while !stop.load(Ordering::Relaxed) {
        match connect(url) {
            Ok(mut ws) => {
                obs_info!("Connected to {url}");
                retry_delay = MIN_RETRY_DELAY;
                failed_attempts = 0;
                match ws.send(tungstenite::Message::text(SUBSCRIBE)) {
                    Ok(()) => {
                        if !forward(ws, sender, stop) {
                            return;
                        }
                    }
                    Err(e) => obs_warning!("Failed to subscribe to {url}, reconnecting: {e}"),
                }
            }
            Err(e) if cfg!(feature = "dummy-producer") => {
                obs_warning!(
                    "Failed to connect to {url}, falling back to dummy debug producer: {e}"
                );
                run_dummy(sender, stop);
                return;
            }
            Err(e) => {
                // Only the first failure is a warning, OverlayPlugin may just
                // not be running yet
                if failed_attempts == 0 {
                    obs_warning!("Failed to connect to {url}, retrying until it succeeds: {e}");
                } else {
                    obs_debug!("Failed to connect to {url}, retrying in {retry_delay:?}: {e}");
                }
                failed_attempts += 1;
            }
        }

        if !sleep_unless_stopped(retry_delay, stop) {
            return;
        }
        retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Forward the connection's messages until it is lost, which returns true, or
/// until the connection is stopped or its receiver dropped
fn forward(
    mut ws: WebSocket<TcpStream>,
    sender: &mpsc::Sender<Message>,
    stop: &AtomicBool,
) -> bool {
    let mut lost = false;
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
                obs_warning!("Lost the WebSocket connection, reconnecting: {e}");
                lost = true;
                break;
            }
        };
        // Pings, pongs and close frames aren't OverlayPlugin messages
        if !(msg.is_text() || msg.is_binary()) {
            continue;
        }
        if sender
            .send(Message::received_now(msg.into_data().into()))
            .is_err()
        {
            break;
        }
    }

    let _ = ws.close(None);
    let _ = ws.flush();
    lost
}

/// Returns false if the connection was stopped meanwhile
fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) -> bool {
    let mut slept = Duration::ZERO;
    while slept < duration {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(READ_TIMEOUT);
        slept += READ_TIMEOUT;
    }
    !stop.load(Ordering::Relaxed)
}

fn run_dummy(sender: &mpsc::Sender<Message>, stop: &AtomicBool) {
    while sleep_unless_stopped(Duration::from_secs(1), stop) {
        obs_debug!("Simulating WebSocket data");
        if sender
            .send(Message::received_now(
                br#"{"type":"RaidwideDummy"}"#.to_vec(),
            ))
            .is_err()
        {
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
            assert_eq!(ws.read().unwrap(), tungstenite::Message::text(SUBSCRIBE));
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
            receiver.recv_timeout(timeout).unwrap().data,
            b"{\"type\":\"ChangeZone\"}"
        );
        assert_eq!(receiver.recv_timeout(timeout).unwrap().data, b"{}");

        let start = Instant::now();
        drop(connection);
        assert!(start.elapsed() < READ_TIMEOUT * 5);
        server.join().unwrap();
    }

    #[test]
    fn reconnects_after_losing_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(tungstenite::accept(stream).unwrap());

            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            ws.send(tungstenite::Message::text("{}")).unwrap();
            while ws.read().is_ok() {}
        });

        let (sender, receiver) = mpsc::channel();
        let connection = Connection::spawn(url, sender);
        let timeout = MIN_RETRY_DELAY + Duration::from_secs(5);
        assert_eq!(receiver.recv_timeout(timeout).unwrap().data, b"{}");
        drop(connection);
        server.join().unwrap();
    }

    #[test]
    fn stops_while_waiting_to_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            assert_eq!(ws.read().unwrap(), tungstenite::Message::text(SUBSCRIBE));
            // Disconnect after subscribing so the client enters the retry delay.
        });

        let (sender, _receiver) = mpsc::channel();
        let connection = Connection::spawn(url, sender);
        server.join().unwrap();
        // Allow the client to observe the disconnect before testing shutdown.
        thread::sleep(READ_TIMEOUT * 3);

        let start = Instant::now();
        drop(connection);
        assert!(start.elapsed() < READ_TIMEOUT * 5);
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(connect("wss://127.0.0.1:1").is_err());
        assert!(connect("http://127.0.0.1:1").is_err());
    }
}
