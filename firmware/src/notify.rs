//! Push alerts to ntfy.
//!
//! Alerts are the enforcement story for everything the router cannot touch — offline
//! use, mobile data, a device taken away at night. So the failure that matters is not
//! a dropped message, it is a *silently* dropped one: a fortnight of quiet that looks
//! exactly like a fortnight of good behaviour.
//!
//! Two consequences shape this module:
//!
//! - Sending happens in its own task behind a queue. A hung socket must never stop the
//!   ledger ticking or the display updating.
//! - The last success and the consecutive failure count are published, so the admin
//!   page can show that alerting is dead rather than leaving it to be noticed.
//!
//! Plain HTTP by default, which works against a self-hosted ntfy. ntfy.sh itself is
//! HTTPS-only; see `TLS` in the notes below.

use core::cell::RefCell;
use core::fmt::Write as _;

use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write;
use esp_println::println;
use heapless::String;

/// Longest alert text. Anything longer is a design problem, not a truncation problem.
pub const MAX_MSG: usize = 160;
pub type Message = String<MAX_MSG>;

/// Queue depth. Deliberately small: if this backs up, alerting is broken and the
/// interesting fact is that it is broken, not the twentieth queued message.
const QUEUE_DEPTH: usize = 8;

static QUEUE: Channel<CriticalSectionRawMutex, Message, QUEUE_DEPTH> = Channel::new();

/// Health, for the admin page. A push system nobody checks is a push system that has
/// been broken for a month.
#[derive(Debug, Clone, Copy, Default)]
pub struct Health {
    pub sent: u32,
    pub failed_in_a_row: u32,
    /// Uptime millis at the last success, or `None` if there has never been one.
    pub last_ok_ms: Option<u64>,
}

static HEALTH: Mutex<CriticalSectionRawMutex, RefCell<Health>> =
    Mutex::new(RefCell::new(Health { sent: 0, failed_in_a_row: 0, last_ok_ms: None }));

pub fn health() -> Health {
    HEALTH.lock(|c| *c.borrow())
}

/// Queue an alert. Never blocks; drops the message if the queue is full.
///
/// Dropping is the right call in the only situation it happens: the sender is stuck,
/// so the queue is stale, and blocking the caller would take the control loop down
/// with it.
pub fn send(text: &str) {
    let mut msg = Message::new();
    let _ = msg.push_str(&text[..text.len().min(MAX_MSG)]);
    if QUEUE.try_send(msg).is_err() {
        println!("notify: queue full, dropped: {text}");
    }
}

pub struct Config {
    pub host: IpAddress,
    pub port: u16,
    /// Host header value — ntfy routes on it, and a self-hosted instance behind a
    /// reverse proxy needs it to match.
    pub host_header: &'static str,
    pub topic: &'static str,
}

#[embassy_executor::task]
pub async fn sender(stack: Stack<'static>, cfg: Config) {
    println!(
        "notify: sender up, posting to {}:{}/{}",
        cfg.host_header, cfg.port, cfg.topic
    );

    loop {
        let msg = QUEUE.receive().await;

        match post(stack, &cfg, &msg).await {
            Ok(()) => HEALTH.lock(|c| {
                let mut h = c.borrow_mut();
                h.sent += 1;
                h.failed_in_a_row = 0;
                h.last_ok_ms = Some(Instant::now().as_millis());
            }),
            Err(e) => {
                HEALTH.lock(|c| c.borrow_mut().failed_in_a_row += 1);
                let n = health().failed_in_a_row;
                println!("notify: send failed ({e}), {n} in a row");
                // Back off a little so a dead endpoint does not spin the radio, but
                // not so much that a transient failure delays a real alert.
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn post(stack: Stack<'static>, cfg: &Config, msg: &str) -> Result<(), &'static str> {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
    sock.set_timeout(Some(Duration::from_secs(10)));

    sock.connect((cfg.host, cfg.port))
        .await
        .map_err(|_| "connect")?;

    let mut head: String<512> = String::new();
    write!(
        head,
        "POST /{} HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Title: Medienzeit\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        cfg.topic,
        cfg.host_header,
        msg.len()
    )
    .map_err(|_| "request too long")?;

    sock.write_all(head.as_bytes()).await.map_err(|_| "write")?;
    sock.write_all(msg.as_bytes()).await.map_err(|_| "write")?;
    sock.flush().await.map_err(|_| "flush")?;

    // Read just enough for the status line. ntfy answers with a JSON body we do not
    // care about, and `Connection: close` means we can stop reading whenever we like.
    let mut buf = [0u8; 128];
    let n = sock.read(&mut buf).await.map_err(|_| "read")?;
    sock.close();

    let text = core::str::from_utf8(&buf[..n]).map_err(|_| "not utf-8")?;
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or("no status line")?;

    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err("http error")
    }
}
