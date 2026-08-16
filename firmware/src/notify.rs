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

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embedded_tls::{Aes128GcmSha256, NoVerify, TlsConfig, TlsConnection, TlsContext};
use static_cell::StaticCell;
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
    /// Literal address for a self-hosted instance, or `None` to resolve `host_name`.
    pub host: Option<IpAddress>,
    pub port: u16,
    /// Host header value, and the DNS name and SNI name when TLS is used.
    pub host_header: &'static str,
    pub topic: &'static str,
    /// Wrap the connection in TLS.
    ///
    /// **Server certificates are not verified.** Deliberate: the payload is "she took
    /// the phone at 23:40", nobody's secret, and verification protects against
    /// impersonation rather than against an attacker simply dropping the traffic —
    /// which is the failure that would actually matter here, and which no amount of
    /// certificate checking prevents. Pinning a root would also mean the alerts die
    /// silently the day that root rotates. What does leak to a man in the middle is
    /// the topic name, which ntfy treats as a bearer token for publish and subscribe.
    pub tls: bool,
}

/// TLS record buffers. Statics rather than task locals: 16 KB each would blow the
/// task stack, and only one alert is ever in flight.
static TLS_RX: StaticCell<[u8; 16384]> = StaticCell::new();
static TLS_TX: StaticCell<[u8; 16384]> = StaticCell::new();

/// `embedded-tls` wants a `rand_core` 0.6 CSPRNG; esp-hal's hardware RNG is one.
struct HwRng(esp_hal::rng::Rng);

impl rand_core::RngCore for HwRng {
    fn next_u32(&mut self) -> u32 {
        self.0.random()
    }
    fn next_u64(&mut self) -> u64 {
        ((self.next_u32() as u64) << 32) | self.next_u32() as u64
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let word = self.next_u32().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for HwRng {}

#[embassy_executor::task]
pub async fn sender(stack: Stack<'static>, cfg: Config) {
    // The topic is not logged. ntfy treats it as a bearer token for *both* publishing
    // and subscribing, so anyone who reads it can post alerts to this household or read
    // them. Serial logs get pasted into issues; this repository is public. A short
    // prefix is enough to tell two configurations apart without giving it away.
    println!(
        "notify: sender up, posting to {}{}:{}/{}… ",
        if cfg.tls { "https://" } else { "http://" },
        cfg.host_header,
        cfg.port,
        &cfg.topic[..cfg.topic.len().min(4)]
    );

    let tls_rx = TLS_RX.init([0; 16384]);
    let tls_tx = TLS_TX.init([0; 16384]);

    loop {
        let msg = QUEUE.receive().await;

        match post(stack, &cfg, &msg, tls_rx, tls_tx).await {
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

async fn post(
    stack: Stack<'static>,
    cfg: &Config,
    msg: &str,
    tls_rx: &mut [u8],
    tls_tx: &mut [u8],
) -> Result<(), &'static str> {
    // A literal address wins; otherwise resolve. DNS is only needed for the public
    // service — a self-hosted instance is configured by IP so boot has one less
    // dependency.
    let addr = match cfg.host {
        Some(a) => a,
        None => {
            *stack
            .dns_query(cfg.host_header, DnsQueryType::A)
            .await
                .map_err(|_| "dns")?
                .first()
                .ok_or("dns empty")?
        }
    };

    // 4 KB rather than 1 KB: a TLS handshake flight (certificate chain) is far bigger
    // than anything the plain-HTTP path ever sees.
    let mut rx = [0u8; 4096];
    let mut tx = [0u8; 4096];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
    sock.set_timeout(Some(Duration::from_secs(15)));

    sock.connect((addr, cfg.port)).await.map_err(|_| "connect")?;

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

    let mut buf = [0u8; 128];
    let n = if cfg.tls {
        // RSA is opt-in in embedded-tls, and public CAs still issue RSA chains.
        let tls_config = TlsConfig::new()
            .with_server_name(cfg.host_header)
            .enable_rsa_signatures();
        let mut tls: TlsConnection<'_, TcpSocket, Aes128GcmSha256> =
            TlsConnection::new(sock, tls_rx, tls_tx);
        let mut rng = HwRng(esp_hal::rng::Rng::new());
        tls.open::<HwRng, NoVerify>(TlsContext::new(&tls_config, &mut rng))
            .await
            .map_err(|e| {
                println!("notify: tls error {e:?}");
                "tls handshake"
            })?;

        tls.write(head.as_bytes()).await.map_err(|_| "write")?;
        tls.write(msg.as_bytes()).await.map_err(|_| "write")?;
        tls.flush().await.map_err(|_| "flush")?;
        let n = tls.read(&mut buf).await.map_err(|_| "read")?;
        let _ = tls.close().await;
        n
    } else {
        sock.write_all(head.as_bytes()).await.map_err(|_| "write")?;
        sock.write_all(msg.as_bytes()).await.map_err(|_| "write")?;
        sock.flush().await.map_err(|_| "flush")?;
        // Read just enough for the status line; `Connection: close` means we can stop
        // reading whenever we like, and ntfy's JSON body is of no interest.
        let n = sock.read(&mut buf).await.map_err(|_| "read")?;
        sock.close();
        n
    };

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
