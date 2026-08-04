//! TR-064 over embassy-net.
//!
//! Every byte on the wire is built and parsed by `medienzeit-tr064`, the same crate
//! `tr064-cli` uses against the real box — so this module is only sockets, digest
//! bookkeeping, and HTTP framing. If the protocol is wrong it is wrong on the laptop
//! too, where it is tested.
//!
//! The box is addressed by its **IP** (the DHCP gateway), not `fritz.box`, which keeps
//! DNS out of the picture entirely.

use core::fmt::Write as _;

use embassy_net::tcp::TcpSocket;
use embassy_net::{Ipv4Address, Stack};
use embassy_time::Duration;
use embedded_io_async::Write;
use heapless::String;
use medienzeit_tr064::soap::Action;
use medienzeit_tr064::{digest, hostfilter, hosts, DEFAULT_PORT};

const USER: &str = env!("MEDIENZEIT_FB_USER");
const PASS: &str = env!("MEDIENZEIT_FB_PASS");

/// Enough for the largest TR-064 response we ask for, with room to spare.
const RESPONSE_CAPACITY: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Connect,
    Io,
    /// The box answered, but not with something we could parse as HTTP.
    Http,
    /// 401 twice: the credentials or the user's permissions are wrong. Retrying will
    /// not help, so callers should surface this rather than spin.
    Unauthorized,
    Codec(medienzeit_tr064::Error),
}

pub struct Client {
    pub box_ip: Ipv4Address,
    /// Digest nonce-count. Must increase for every request reusing a nonce; we fetch a
    /// fresh challenge each time, but incrementing costs nothing and is correct.
    nc: u32,
}

impl Client {
    pub fn new(box_ip: Ipv4Address) -> Self {
        Self { box_ip, nc: 0 }
    }

    /// Resolve a MAC to its current IP and whether the box can see it on the network.
    pub async fn host_entry(
        &mut self,
        stack: Stack<'_>,
        mac: &str,
    ) -> Result<hosts::HostEntry, Error> {
        let body = hosts::get_specific_host_entry(mac).map_err(Error::Codec)?;
        let mut buf = [0u8; RESPONSE_CAPACITY];
        let xml = self
            .call(stack, &hosts::GET_SPECIFIC_HOST_ENTRY, &body, &mut buf)
            .await?;
        hosts::parse_host_entry(xml).map_err(Error::Codec)
    }

    /// Cut or restore internet access for one IP.
    pub async fn set_blocked(
        &mut self,
        stack: Stack<'_>,
        ip: &str,
        blocked: bool,
    ) -> Result<(), Error> {
        let body = hostfilter::disallow_wan_access_by_ip(ip, blocked).map_err(Error::Codec)?;
        let mut buf = [0u8; RESPONSE_CAPACITY];
        let xml = self
            .call(stack, &hostfilter::DISALLOW_WAN_ACCESS_BY_IP, &body, &mut buf)
            .await?;
        hostfilter::parse_disallow_ack(xml).map_err(Error::Codec)
    }

    /// Read back whether an IP is currently blocked.
    ///
    /// Used to reconcile at startup rather than blindly re-issuing state the box
    /// already has, and to verify a change actually took.
    pub async fn is_blocked(&mut self, stack: Stack<'_>, ip: &str) -> Result<bool, Error> {
        let body = hostfilter::get_wan_access_by_ip(ip).map_err(Error::Codec)?;
        let mut buf = [0u8; RESPONSE_CAPACITY];
        let xml = self
            .call(stack, &hostfilter::GET_WAN_ACCESS_BY_IP, &body, &mut buf)
            .await?;
        hostfilter::parse_wan_access(xml).map_err(Error::Codec)
    }

    /// POST, answer the digest challenge, POST again, return the response body.
    async fn call<'b>(
        &mut self,
        stack: Stack<'_>,
        action: &Action,
        body: &str,
        buf: &'b mut [u8],
    ) -> Result<&'b str, Error> {
        let soap_action: String<128> = action.soap_action().map_err(Error::Codec)?;

        let (status, headers_len, total) =
            self.post(stack, action.control_url, &soap_action, body, None, buf)
                .await?;

        if status != 401 {
            return finish(status, buf, headers_len, total);
        }

        // Copy the challenge out before the buffer is reused for the second response.
        let head = core::str::from_utf8(&buf[..headers_len]).map_err(|_| Error::Http)?;
        let challenge_line = header(head, "www-authenticate").ok_or(Error::Http)?;
        let challenge = digest::Challenge::parse(challenge_line).map_err(Error::Codec)?;

        self.nc = self.nc.wrapping_add(1);
        let mut cnonce: String<16> = String::new();
        let _ = write!(cnonce, "{:08x}", embassy_time::Instant::now().as_micros() as u32);

        let auth: String<512> = challenge
            .authorization(USER, PASS, "POST", action.control_url, self.nc, &cnonce)
            .map_err(Error::Codec)?;

        let (status, headers_len, total) = self
            .post(stack, action.control_url, &soap_action, body, Some(&auth), buf)
            .await?;
        if status == 401 {
            return Err(Error::Unauthorized);
        }
        finish(status, buf, headers_len, total)
    }

    /// One HTTP POST. Returns (status, header length, total bytes read).
    async fn post(
        &self,
        stack: Stack<'_>,
        path: &str,
        soap_action: &str,
        body: &str,
        auth: Option<&str>,
        buf: &mut [u8],
    ) -> Result<(u16, usize, usize), Error> {
        let mut rx = [0u8; 1536];
        let mut tx = [0u8; 1536];
        let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
        sock.set_timeout(Some(Duration::from_secs(10)));

        sock.connect((self.box_ip, DEFAULT_PORT))
            .await
            .map_err(|_| Error::Connect)?;

        let mut head: String<768> = String::new();
        write!(
            head,
            "POST {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             Content-Type: text/xml; charset=\"utf-8\"\r\n\
             SOAPAction: {soap_action}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n",
            self.box_ip,
            body.len()
        )
        .map_err(|_| Error::Io)?;
        if let Some(a) = auth {
            write!(head, "Authorization: {a}\r\n").map_err(|_| Error::Io)?;
        }
        head.push_str("\r\n").map_err(|_| Error::Io)?;

        sock.write_all(head.as_bytes()).await.map_err(|_| Error::Io)?;
        sock.write_all(body.as_bytes()).await.map_err(|_| Error::Io)?;
        sock.flush().await.map_err(|_| Error::Io)?;

        // `Connection: close` means read-to-EOF is correct and chunked encoding never
        // appears, which removes a whole class of parsing from this module.
        let mut total = 0usize;
        loop {
            if total == buf.len() {
                break;
            }
            match sock.read(&mut buf[total..]).await {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        sock.close();

        let text = core::str::from_utf8(&buf[..total]).map_err(|_| Error::Http)?;
        let sep = text.find("\r\n\r\n").ok_or(Error::Http)?;
        let status = text
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .ok_or(Error::Http)?;

        Ok((status, sep + 4, total))
    }
}

/// A SOAP Fault arrives as HTTP 500 with a parseable body, so hand 500 onward and let
/// the codec turn it into a typed error.
fn finish(status: u16, buf: &[u8], head_len: usize, total: usize) -> Result<&str, Error> {
    if status != 200 && status != 500 {
        return Err(Error::Http);
    }
    core::str::from_utf8(&buf[head_len..total]).map_err(|_| Error::Http)
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}
