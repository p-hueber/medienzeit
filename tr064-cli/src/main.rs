//! Exercise the TR-064 codec against a real FRITZ!Box.
//!
//! Deliberately dependency-free: TR-064 is plain HTTP on port 49000, so `std::net` is
//! enough. Every byte on the wire is produced and consumed by `medienzeit-tr064`, the
//! same crate the firmware will use — so a green run here means the codec is correct
//! against real hardware, months before there is any firmware to test it in.
//!
//!     export FRITZBOX_USER=medienzeit FRITZBOX_PASS=...
//!     cargo run -p medienzeit-tr064-cli -- host 3C:22:FB:11:22:33
//!     cargo run -p medienzeit-tr064-cli -- block 192.168.178.42
//!     cargo run -p medienzeit-tr064-cli -- status 192.168.178.42
//!     cargo run -p medienzeit-tr064-cli -- unblock 192.168.178.42

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use medienzeit_tr064::soap::Action;
use medienzeit_tr064::{digest, hostfilter, hosts, DEFAULT_PORT};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Config {
    host: String,
    user: String,
    pass: String,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn usage() -> String {
    concat!(
        "usage: tr064-cli <command>\n\n",
        "  host <MAC>       resolve a MAC to its IP and show whether it is online\n",
        "  block <IP>       cut the device off the internet\n",
        "  unblock <IP>     restore internet access\n",
        "  status <IP>      report whether the device is currently blocked\n\n",
        "env: FRITZBOX_HOST (default fritz.box), FRITZBOX_USER, FRITZBOX_PASS"
    )
    .to_string()
}

fn run() -> Result<(), String> {
    let cfg = Config {
        host: std::env::var("FRITZBOX_HOST").unwrap_or_else(|_| "fritz.box".into()),
        user: std::env::var("FRITZBOX_USER").map_err(|_| "FRITZBOX_USER is not set")?,
        pass: std::env::var("FRITZBOX_PASS").map_err(|_| "FRITZBOX_PASS is not set")?,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, arg) = match args.as_slice() {
        [c, a] => (c.as_str(), a.as_str()),
        _ => return Err(usage()),
    };

    match cmd {
        "host" => {
            let body = hosts::get_specific_host_entry(arg).map_err(|e| e.to_string())?;
            let xml = call(&cfg, &hosts::GET_SPECIFIC_HOST_ENTRY, &body)?;
            let h = hosts::parse_host_entry(&xml).map_err(|e| e.to_string())?;
            println!("ip       {}", h.ip);
            println!("online   {}", if h.active { "yes" } else { "no" });
            println!("hostname {}", if h.hostname.is_empty() { "-" } else { &h.hostname });
        }
        "block" | "unblock" => {
            let disallow = cmd == "block";
            let body =
                hostfilter::disallow_wan_access_by_ip(arg, disallow).map_err(|e| e.to_string())?;
            let xml = call(&cfg, &hostfilter::DISALLOW_WAN_ACCESS_BY_IP, &body)?;
            hostfilter::parse_disallow_ack(&xml).map_err(|e| e.to_string())?;
            println!("{arg} {}", if disallow { "blocked" } else { "unblocked" });
            // Read back rather than trusting the ack: this is the whole point of the
            // exercise, and a silent no-op would be the worst possible outcome.
            let check = hostfilter::get_wan_access_by_ip(arg).map_err(|e| e.to_string())?;
            let xml = call(&cfg, &hostfilter::GET_WAN_ACCESS_BY_IP, &check)?;
            let now = hostfilter::parse_wan_access(&xml).map_err(|e| e.to_string())?;
            println!("readback {}", if now { "blocked" } else { "allowed" });
            if now != disallow {
                return Err("the box did not apply the change — check user permissions".into());
            }
        }
        "status" => {
            let body = hostfilter::get_wan_access_by_ip(arg).map_err(|e| e.to_string())?;
            let xml = call(&cfg, &hostfilter::GET_WAN_ACCESS_BY_IP, &body)?;
            let blocked = hostfilter::parse_wan_access(&xml).map_err(|e| e.to_string())?;
            println!("{arg} {}", if blocked { "blocked" } else { "allowed" });
        }
        _ => return Err(usage()),
    }
    Ok(())
}

/// POST a SOAP body, answer the digest challenge, return the response body.
fn call(cfg: &Config, action: &Action, body: &str) -> Result<String, String> {
    let soap_action: heapless::String<128> = action.soap_action().map_err(|e| e.to_string())?;

    let (status, headers, response) = post(cfg, action.control_url, &soap_action, body, None)?;
    if status != 401 {
        return finish(status, response);
    }

    let challenge_header = header(&headers, "www-authenticate")
        .ok_or("FRITZ!Box returned 401 without a WWW-Authenticate header")?;
    let challenge = digest::Challenge::parse(challenge_header).map_err(|e| e.to_string())?;
    let auth: heapless::String<512> = challenge
        .authorization(&cfg.user, &cfg.pass, "POST", action.control_url, 1, &cnonce())
        .map_err(|e| e.to_string())?;

    let (status, _, response) = post(cfg, action.control_url, &soap_action, body, Some(&auth))?;
    if status == 401 {
        return Err("authentication rejected — check FRITZBOX_USER / FRITZBOX_PASS, and that \
                    the user has box-settings permission"
            .into());
    }
    finish(status, response)
}

fn finish(status: u16, response: String) -> Result<String, String> {
    // A SOAP Fault arrives as HTTP 500 with a parseable body, so hand 500 onward and
    // let the codec turn it into a typed error.
    if status == 200 || status == 500 {
        Ok(response)
    } else {
        Err(format!("unexpected HTTP {status}"))
    }
}

/// HTTP status, lower-cased headers, body.
type Response = (u16, Vec<(String, String)>, String);

fn post(
    cfg: &Config,
    path: &str,
    soap_action: &str,
    body: &str,
    auth: Option<&str>,
) -> Result<Response, String> {
    let addr = format!("{}:{}", cfg.host, DEFAULT_PORT);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("cannot reach {addr}: {e}. Is TR-064 enabled on the box?"))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPAction: {soap_action}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        cfg.host,
        body.len()
    );
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);

    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;

    // `Connection: close` means read-to-EOF is correct and we never have to deal with
    // chunked transfer encoding.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed HTTP response")?;
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or("malformed HTTP status line")?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    Ok((status, headers, body.to_string()))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Any value that does not repeat is fine; the server only requires freshness.
fn cnonce() -> String {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{n:016x}")
}
