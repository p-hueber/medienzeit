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

        // The stylesheet needs no auth — it carries nothing private, and requiring it
        // would leave the 401 page unstyled for no gain. Cached hard: it only changes
        // when the firmware does.
        if text.starts_with("GET /s.css") {
            // Checked rather than discarded: `write!` into a heapless String truncates
            // instead of failing, so an over-long header goes out as a malformed
            // response with an empty body and nothing logged anywhere.
            let mut head: String<160> = String::new();
            let built = write!(
                head,
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/css; charset=utf-8\r\n\
                 Cache-Control: public, max-age=31536000, immutable\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                STYLESHEET.len()
            );
            if built.is_err() {
                println!("web: stylesheet header did not fit, refusing to send it");
                sock.close();
                continue;
            }
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(STYLESHEET.as_bytes()).await;
            let _ = sock.flush().await;
            sock.close();
            continue;
        }

        let snapshot = latest();
        let response = handle(out, body, text, &auth, snapshot.as_ref());
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        sock.close();
    }
}

/// The stylesheet, in flash rather than in the page buffer.
///
/// Kept out of the HTML deliberately. The page is assembled into a fixed RAM buffer, so
/// an inline `<style>` would spend the scarce resource; as a separate route it is a
/// `const &str` written straight to the socket, spending the abundant one — 2.3 MB of
/// free app flash against about a kilobyte of spare buffer.
const STYLESHEET: &str = include_str!("s.css");

/// Response and body buffers.
///
/// Statics rather than locals: the settings form pushed these past 4 KB together, which
/// is more than the web task's stack wants to carry. Only one request is handled at a
/// time, so a single pair is enough.
static OUT: StaticCell<String<8192>> = StaticCell::new();
static BODY: StaticCell<String<6144>> = StaticCell::new();

/// Warn before the page silently loses its tail.
///
/// `heapless` truncates rather than failing, and the states that push the page longest —
/// a grant confirmation and a save confirmation together — are exactly the ones a parent
/// is looking at when it matters. The measured page is about 3 KB, so this leaves room
/// for the copy to grow without anyone watching the byte count.
const BODY_WARN_AT: usize = 5 * 6144 / 6;

fn handle<'b>(
    out: &'b mut String<8192>,
    body: &mut String<6144>,
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
    if body.len() >= BODY_WARN_AT {
        println!("web: page is {} bytes, buffer is {}", body.len(), body.capacity());
    }

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

/// The four corner registration marks every framed block carries.
///
/// Static decoration from the design system, with no data in it, so it is emitted as a
/// literal wherever a `.bp` block opens.
const CORNERS: &str =
    "<i class=tl></i><i class=tr></i><i class=bl></i><i class=br></i>";

fn page(
    out: &mut String<6144>,
    snap: Option<&Snapshot<2>>,
    granted: u32,
    saved: Option<bool>,
) {
    let _ = out.push_str(
        "<!doctype html><html lang=de><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Medienzeit</title><link rel=stylesheet href=/s.css></head>\
         <body><main><h1>Medienzeit</h1>",
    );

    // --- balance --------------------------------------------------------------
    let _ = write!(out, "<div class=\"plate bp\">{CORNERS}<p class=kicker>Saldo</p>");
    match snap {
        None => {
            let _ = out.push_str("<p class=balance><strong>—</strong></p>\
                                  <p class=stale-note>Startet — noch kein Stand.</p>");
        }
        Some(s) => {
            // A grant is applied by the control loop on its next tick, so the number
            // here is briefly the old one. Dimming it and saying so beats looking like
            // the button did nothing.
            let _ = write!(
                out,
                "<p class=balance{}><strong>{}</strong><span class=unit>Min</span></p>",
                if granted > 0 { " data-stale=true" } else { "" },
                s.balance_secs / 60
            );
            if granted > 0 {
                let _ = write!(
                    out,
                    "<p class=stale-note>+{granted} Min gewährt — der Saldo oben braucht \
                     einen Moment.</p>"
                );
            }
        }
    }
    let _ = out.push_str("</div>");

    // --- the hurried path -----------------------------------------------------
    let _ = write!(
        out,
        "<form class=bonus-form method=post action=/>\
         <button class=\"bonus-btn bp\" type=submit name=grant value=10>{CORNERS}+10 Min</button>\
         <button class=\"bonus-btn bp\" type=submit name=grant value=30>{CORNERS}+30 Min</button>\
         </form>"
    );

    // --- status ---------------------------------------------------------------
    if let Some(s) = snap {
        let _ = write!(
            out,
            "<div class=\"status-wrap bp\">{CORNERS}<table class=status>\
             <thead><tr><th>Feld</th><th colspan=2>Wert</th></tr></thead><tbody>\
             <tr><th scope=row>Status</th><td colspan=2>{}</td></tr>\
             <tr><th scope=row>Uhrzeit</th><td colspan=2>{:02}:{:02}</td></tr>\
             <tr><th scope=row>Nacht</th><td colspan=2>{}</td></tr>",
            match s.flow {
                Flow::Filling => "füllt sich",
                Flow::Draining => "läuft",
                Flow::Held => "gehalten",
            },
            s.local.hour,
            s.local.minute,
            yes_no(s.night),
        );
        for i in 0..2 {
            let _ = write!(
                out,
                "<tr><th scope=row>{}</th><td>{}</td><td{}>{}</td></tr>",
                crate::DEV_NAMES[i],
                put_back(s.docked[i]),
                if s.blocked[i] { " data-state=blocked" } else { "" },
                blocked(s.blocked[i]),
            );
        }
        let _ = out.push_str("</tbody></table></div>");
    }

    // --- alerting health ------------------------------------------------------
    //
    // On the page deliberately: a push channel nobody checks is a push channel that has
    // been broken for a month, and silence from it looks exactly like good behaviour.
    let h = crate::notify::health();
    if h.failed_in_a_row > 0 {
        let _ = write!(
            out,
            "<p class=\"alert-health is-failing\">Benachrichtigungen: {} Fehlversuche \
             in Folge</p>",
            h.failed_in_a_row
        );
    } else if h.last_ok_ms.is_none() {
        let _ = out.push_str(
            "<p class=alert-health>Benachrichtigungen: noch keine gesendet</p>",
        );
    } else {
        let _ = write!(
            out,
            "<p class=alert-health>Benachrichtigungen: in Ordnung · {} gesendet</p>",
            h.sent
        );
    }

    // --- the deliberate path --------------------------------------------------
    let Some(c) = current_settings() else {
        let _ = out.push_str("</main></body></html>");
        return;
    };
    // Left open after a submission, so the result is visible where the change was made
    // rather than hidden behind a disclosure that has snapped shut.
    let _ = write!(
        out,
        "<div class=\"rules-wrap bp\">{CORNERS}<details class=rules{}>\
         <summary>Regeln</summary><div class=rules-body>",
        if saved.is_some() { " open" } else { "" }
    );
    match saved {
        Some(true) => {
            let _ = out.push_str("<p class=saved-note>Gespeichert.</p>");
        }
        Some(false) => {
            let _ = out.push_str(
                "<p class=\"saved-note is-failing\">Abgelehnt — Teiler 0 oder eine \
                 Uhrzeit außerhalb des Tages.</p>",
            );
        }
        None => {}
    }
    // Times are shown in the local clock the ledger uses, so what is typed here and what
    // the night rule does are the same numbers.
    let _ = write!(
        out,
        "<form method=post action=/>\
         <p class=earn-row>\
         <label class=visually-hidden for=rn>Verdient, Minuten</label>\
         Verdient <input type=number id=rn name=rn value={} min=0 inputmode=numeric> Min je \
         <label class=visually-hidden for=rd>je Minuten weggenommen</label>\
         <input type=number id=rd name=rd value={} min=1 inputmode=numeric> Min weggenommen</p>\
         <div class=rule-row><label for=cap>Höchststand</label>\
         <span><input type=number id=cap name=cap value={} min=0 inputmode=numeric>\
         <span class=unit-hint>Min</span></span></div>\
         <div class=rule-row><label for=floor>Erlaubtes Minus</label>\
         <span><input type=number id=floor name=floor value={} min=0 inputmode=numeric>\
         <span class=unit-hint>Min</span></span></div>\
         <div class=rule-row><label for=pre>Startguthaben</label>\
         <span><input type=number id=pre name=pre value={} min=0 inputmode=numeric>\
         <span class=unit-hint>Min</span></span></div>\
         <div class=rule-row><label for=grace>Karenzzeit</label>\
         <span><input type=number id=grace name=grace value={} min=0 inputmode=numeric>\
         <span class=unit-hint>Sek</span></span></div>\
         <div class=rule-row><label for=ns>Nacht ab</label>\
         <input type=time id=ns name=ns value=\"{:02}:{:02}\"></div>\
         <div class=rule-row><label for=ne>Nacht bis</label>\
         <input type=time id=ne name=ne value=\"{:02}:{:02}\"></div>\
         <button class=save-btn type=submit>Speichern</button></form>\
         <p class=explainer>Änderungen wirken sofort und werden dauerhaft gespeichert.</p>\
         </div></details></div></main></body></html>",
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
        "ja"
    } else {
        "nein"
    }
}

/// "zurückgelegt" is the chosen word for a device at the reader and is not to be
/// substituted — it is the term the household uses.
fn put_back(docked: bool) -> &'static str {
    if docked {
        "zurückgelegt"
    } else {
        "weggenommen"
    }
}

fn blocked(b: bool) -> &'static str {
    if b {
        "blockiert"
    } else {
        "erlaubt"
    }
}
