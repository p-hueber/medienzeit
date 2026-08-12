//! LAN admin page: live state, and the bonus-minutes override.
//!
//! The override is the point. Without a way to hand back time, the first false
//! negative — a reader glitch, a lease that moved, an evening she genuinely was not
//! using it — turns this device into a source of household argument rather than a
//! rule everyone understands. It needs to be reachable from a phone in ten seconds.
//!
//! Deliberately plain HTTP on the LAN behind Basic auth. TLS here would mean a
//! certificate on a device with no trust anchor and no clock at boot, to protect a
//! form whose worst-case abuse is granting screen time from inside the house.

use core::fmt::Write as _;

use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use core::cell::RefCell;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::Duration;
use embedded_io_async::Write;
use esp_println::println;
use heapless::String;
use medienzeit_core::settings::Settings;
use static_cell::StaticCell;
use medienzeit_core::{Flow, Snapshot};

/// Latest snapshot, published by the control loop after every tick.
///
/// A cell rather than a `Signal`: taking from a signal *consumes* it, so two requests
/// arriving between loop ticks would show the second one stale data. State that is
/// read repeatedly wants a value, not an event.
static STATE: Mutex<CriticalSectionRawMutex, RefCell<Option<Snapshot<2>>>> =
    Mutex::new(RefCell::new(None));

/// The rules currently in force, published by the control loop so the form can show
/// them. Without this the page would have to guess, and a form pre-filled with guesses
/// silently rewrites whatever it got wrong the moment it is submitted.
static CURRENT: Mutex<CriticalSectionRawMutex, RefCell<Option<Settings>>> =
    Mutex::new(RefCell::new(None));

/// A settings change waiting for the control loop to apply and persist it.
static PENDING: Mutex<CriticalSectionRawMutex, RefCell<Option<Settings>>> =
    Mutex::new(RefCell::new(None));

pub fn publish_settings(s: Settings) {
    CURRENT.lock(|c| *c.borrow_mut() = Some(s));
}

/// Take a pending change, if the page submitted one.
pub fn take_settings() -> Option<Settings> {
    PENDING.lock(|c| c.borrow_mut().take())
}

fn current_settings() -> Option<Settings> {
    CURRENT.lock(|c| *c.borrow())
}

pub fn publish(snapshot: Snapshot<2>) {
    STATE.lock(|cell| *cell.borrow_mut() = Some(snapshot));
}

fn latest() -> Option<Snapshot<2>> {
    STATE.lock(|cell| cell.borrow().clone())
}

/// Bonus minutes requested by the admin page, consumed by the control loop.
pub static BONUS: Signal<CriticalSectionRawMutex, u32> = Signal::new();

const USER: &str = env!("MEDIENZEIT_WEB_USER");
const PASS: &str = env!("MEDIENZEIT_WEB_PASS");

/// `Basic ` + base64("user:pass"), precomputed once at first use.
fn expected_auth(out: &mut String<128>) {
    let mut raw: String<96> = String::new();
    let _ = write!(raw, "{USER}:{PASS}");
    let _ = out.push_str("Basic ");
    base64_into(raw.as_bytes(), out);
}

fn base64_into(input: &[u8], out: &mut String<128>) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let mut enc = [0u8; 4];
        enc[0] = T[(n >> 18) as usize & 63];
        enc[1] = T[(n >> 12) as usize & 63];
        enc[2] = T[(n >> 6) as usize & 63];
        enc[3] = T[n as usize & 63];
        if chunk.len() < 3 {
            enc[3] = b'=';
        }
        if chunk.len() < 2 {
            enc[2] = b'=';
        }
        for c in enc {
            let _ = out.push(c as char);
        }
    }
}

#[embassy_executor::task]
pub async fn serve(stack: Stack<'static>) {
    let mut auth: String<128> = String::new();
    expected_auth(&mut auth);
    println!("web: serving on port 80");
    let out = OUT.init(String::new());
    let body = BODY.init(String::new());

    loop {
        let mut rx = [0u8; 1024];
        let mut tx = [0u8; 2048];
        let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
        sock.set_timeout(Some(Duration::from_secs(10)));

        if sock.accept(80).await.is_err() {
            continue;
        }

        let mut req = [0u8; 1024];
        let n = match sock.read(&mut req).await {
            Ok(n) if n > 0 => n,
            _ => {
                sock.close();
                continue;
            }
        };
        let text = core::str::from_utf8(&req[..n]).unwrap_or("");

        let snapshot = latest();
        let response = handle(out, body, text, &auth, snapshot.as_ref());
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        sock.close();
    }
}

/// Response and body buffers.
///
/// Statics rather than locals: the settings form pushed these past 4 KB together, which
/// is more than the web task's stack wants to carry. Only one request is handled at a
/// time, so a single pair is enough.
static OUT: StaticCell<String<4096>> = StaticCell::new();
static BODY: StaticCell<String<3072>> = StaticCell::new();

fn handle<'b>(
    out: &'b mut String<4096>,
    body: &mut String<3072>,
    request: &str,
    expected_auth: &str,
    snap: Option<&Snapshot<2>>,
) -> &'b str {
    out.clear();
    body.clear();

    if !authorized(request, expected_auth) {
        let _ = out.push_str(
            "HTTP/1.1 401 Unauthorized\r\n\
             WWW-Authenticate: Basic realm=\"Medienzeit\"\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return out.as_str();
    }

    // Any grant arrives as a POST; the amount is capped so a stuck finger on the
    // phone cannot hand over the whole evening.
    let mut granted = 0;
    let mut saved = None;
    if request.starts_with("POST") {
        granted = grant_minutes(request).min(60);
        if granted > 0 {
            BONUS.signal(granted * 60);
            println!("web: granted {granted} bonus minutes");
        }
        if let Some(new) = parse_settings(request) {
            // Validated here as well as at the store, so the page can say the change was
            // refused instead of appearing to accept it and quietly doing nothing.
            if new.valid() {
                PENDING.lock(|c| *c.borrow_mut() = Some(new));
                saved = Some(true);
                println!("web: settings change queued");
            } else {
                saved = Some(false);
                println!("web: settings change refused as invalid");
            }
        }
    }

    page(body, snap, granted, saved);

    let _ = write!(
        out,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = out.push_str(body);
    out.as_str()
}

fn authorized(request: &str, expected: &str) -> bool {
    request.lines().any(|l| {
        let Some((k, v)) = l.split_once(':') else {
            return false;
        };
        k.trim().eq_ignore_ascii_case("authorization") && v.trim() == expected
    })
}

/// Pull `grant=N` out of a form body.
fn grant_minutes(request: &str) -> u32 {
    let Some(body) = request.split("\r\n\r\n").nth(1) else {
        return 0;
    };
    for field in body.split('&') {
        if let Some(v) = field.strip_prefix("grant=") {
            return v.trim_end_matches('\0').parse().unwrap_or(0);
        }
    }
    0
}

/// Read one form field as a number.
fn field(body: &str, name: &str) -> Option<u32> {
    body.split('&').find_map(|f| {
        let (k, v) = f.split_once('=')?;
        (k == name).then(|| v.trim_end_matches('\0').parse().ok())?
    })
}

/// Read an `HH:MM` field as minutes from midnight.
///
/// `<input type=time>` posts the colon percent-encoded, which is the only escape this
/// form can produce — so it is decoded here rather than pulling in a URL decoder.
fn time_field(body: &str, name: &str) -> Option<u32> {
    let raw = body.split('&').find_map(|f| {
        let (k, v) = f.split_once('=')?;
        (k == name).then_some(v)
    })?;
    let raw = raw.trim_end_matches('\0');
    let (h, m) = if let Some(i) = raw.find("%3A").or_else(|| raw.find("%3a")) {
        (&raw[..i], &raw[i + 3..])
    } else {
        raw.split_once(':')?
    };
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Build a settings change from a submitted form, starting from what is in force.
///
/// Every field falls back to the current value, so a form that posts a subset — or a
/// browser that omits a disabled input — changes only what it actually carried.
fn parse_settings(request: &str) -> Option<Settings> {
    let body = request.split("\r\n\r\n").nth(1)?;
    if !body.contains("cap=") {
        return None;
    }
    let now = current_settings()?;
    let s = Settings {
        seq: now.seq,
        refill_num: field(body, "rn").unwrap_or(now.refill_num),
        refill_den: field(body, "rd").unwrap_or(now.refill_den),
        cap_secs: field(body, "cap").map_or(now.cap_secs, |m| m * 60),
        floor_secs: field(body, "floor").map_or(now.floor_secs, |m| m * 60),
        prefill_secs: field(body, "pre").map_or(now.prefill_secs, |m| m * 60),
        grace_secs: field(body, "grace").unwrap_or(now.grace_secs),
        night_start_minute: time_field(body, "ns").unwrap_or(now.night_start_minute),
        night_end_minute: time_field(body, "ne").unwrap_or(now.night_end_minute),
    };
    Some(s)
}

fn page(
    out: &mut String<3072>,
    snap: Option<&Snapshot<2>>,
    granted: u32,
    saved: Option<bool>,
) {
    let _ = out.push_str(
        "<!doctype html><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Medienzeit</title>\
         <style>body{font:16px system-ui;margin:2rem;max-width:24rem}\
         .b{font-size:3rem;font-weight:700;margin:.2em 0}\
         table{border-collapse:collapse;width:100%}td{padding:.3em 0}\
         td+td{text-align:right}\
         button{font-size:1.1rem;padding:.6em 1.2em;margin:.2em}</style>",
    );

    match snap {
        None => {
            let _ = out.push_str("<p>Starting up — no state yet.</p>");
        }
        Some(s) => {
            let mins = s.balance_secs / 60;
            let _ = write!(out, "<div class=b>{mins} min</div>");
            let _ = write!(
                out,
                "<table>\
                 <tr><td>Status</td><td>{}</td></tr>\
                 <tr><td>Time</td><td>{:02}:{:02}</td></tr>\
                 <tr><td>Night</td><td>{}</td></tr>\
                 <tr><td>Device 1</td><td>{} / {}</td></tr>\
                 <tr><td>Device 2</td><td>{} / {}</td></tr>\
                 </table>",
                match s.flow {
                    Flow::Filling => "charging",
                    Flow::Draining => "running",
                    Flow::Held => "held",
                },
                s.local.hour,
                s.local.minute,
                yes_no(s.night),
                put_back(s.docked[0]),
                blocked(s.blocked[0]),
                put_back(s.docked[1]),
                blocked(s.blocked[1]),
            );
        }
    }

    // Alerting health. A push channel nobody checks is a push channel that has been
    // broken for a month, so make its state impossible to miss on the page you
    // actually open.
    let h = crate::notify::health();
    if h.failed_in_a_row > 0 {
        let _ = write!(
            out,
            "<p style=\"color:#b00\"><b>Alerts failing</b> — {} in a row, {} sent overall.</p>",
            h.failed_in_a_row, h.sent
        );
    } else if h.last_ok_ms.is_none() {
        let _ = out.push_str("<p style=\"color:#666\">No alert sent yet this session.</p>");
    } else {
        let _ = write!(out, "<p style=\"color:#666\">Alerts OK — {} sent.</p>", h.sent);
    }

    if granted > 0 {
        // The control loop applies this on its next tick, so the number above is
        // still the old one. Say so rather than looking like the button did nothing.
        let _ = write!(out, "<p><b>+{granted} min granted</b> — applies within a second.</p>");
    }

    let _ = out.push_str(
        "<form method=post>\
         <button name=grant value=10>+10 min</button>\
         <button name=grant value=30>+30 min</button>\
         </form>",
    );

    match saved {
        Some(true) => {
            let _ = out.push_str("<p><b>Settings saved.</b></p>");
        }
        Some(false) => {
            let _ = out.push_str(
                "<p style=\"color:#b00\"><b>Settings refused</b> — a refill divisor of \
                 zero, or a time outside the clock.</p>",
            );
        }
        None => {}
    }

    let Some(c) = current_settings() else { return };
    // Times are shown in the local clock the ledger uses, so what is typed here and what
    // the night rule does are the same numbers.
    let _ = write!(
        out,
        "<h3>Rules</h3><form method=post><table>\
         <tr><td>Earn</td><td><input type=number name=rn value={} min=1 max=100 style=width:4em> \
         per <input type=number name=rd value={} min=1 max=100 style=width:4em> min away</td></tr>\
         <tr><td>Most it can hold</td><td><input type=number name=cap value={} min=0 max=1440 style=width:5em> min</td></tr>\
         <tr><td>Debt allowed</td><td><input type=number name=floor value={} min=0 max=1440 style=width:5em> min</td></tr>\
         <tr><td>Fresh start</td><td><input type=number name=pre value={} min=0 max=1440 style=width:5em> min</td></tr>\
         <tr><td>Free pickup</td><td><input type=number name=grace value={} min=0 max=3600 style=width:5em> s</td></tr>\
         <tr><td>Night from</td><td><input type=time name=ns value=\"{:02}:{:02}\"></td></tr>\
         <tr><td>Night until</td><td><input type=time name=ne value=\"{:02}:{:02}\"></td></tr>\
         </table><button type=submit>Save rules</button></form>\
         <p style=\"color:#666\">Changes apply at once and survive a restart. \
         The balance itself is not touched — a smaller cap trims it on the next tick, \
         a bigger one just leaves room.</p>",
        c.refill_num,
        c.refill_den,
        c.cap_secs / 60,
        c.floor_secs / 60,
        c.prefill_secs / 60,
        c.grace_secs,
        c.night_start_minute / 60,
        c.night_start_minute % 60,
        c.night_end_minute / 60,
        c.night_end_minute % 60,
    );
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}
fn put_back(docked: bool) -> &'static str {
    if docked {
        "put back"
    } else {
        "taken away"
    }
}
fn blocked(b: bool) -> &'static str {
    if b {
        "blocked"
    } else {
        "allowed"
    }
}
