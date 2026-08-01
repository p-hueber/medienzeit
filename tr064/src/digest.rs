//! HTTP Digest authentication (RFC 2617, MD5 + qop=auth).
//!
//! The FRITZ!Box answers the first unauthenticated POST with a 401 and a
//! `WWW-Authenticate: Digest …` challenge, then accepts the retry. MD5 is not a choice
//! we get to make — it is what the box offers.
//!
//! `cnonce` is passed in rather than generated here so the crate stays free of any
//! RNG dependency and the tests can be deterministic. The firmware supplies bytes from
//! the ESP32's hardware RNG.

use core::fmt::Write;
use heapless::String;
use md5::{Digest, Md5};

use crate::Error;

pub const MAX_FIELD: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String<MAX_FIELD>,
    pub nonce: String<MAX_FIELD>,
    /// True when the server offered `qop="auth"`, which changes the response formula.
    pub qop_auth: bool,
    pub opaque: Option<String<MAX_FIELD>>,
}

fn hex(bytes: &[u8; 16]) -> String<32> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::new();
    for b in bytes {
        // Both pushes fit: 16 bytes -> 32 chars, exactly the capacity.
        let _ = out.push(HEX[(b >> 4) as usize] as char);
        let _ = out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn md5_hex(parts: &[&str]) -> String<32> {
    let mut h = Md5::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            h.update(b":");
        }
        h.update(p.as_bytes());
    }
    let out: [u8; 16] = h.finalize().into();
    hex(&out)
}

/// Pull `key=value` / `key="value"` pairs out of a challenge header.
fn param<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = header;
    while let Some(pos) = rest.find(key) {
        let before_ok = pos == 0
            || matches!(rest.as_bytes()[pos - 1], b' ' | b',')
            || rest[..pos].ends_with("Digest ");
        let after = &rest[pos + key.len()..];
        let after_trimmed = after.trim_start();
        if before_ok && after_trimmed.starts_with('=') {
            let v = after_trimmed[1..].trim_start();
            return Some(if let Some(q) = v.strip_prefix('"') {
                let end = q.find('"')?;
                &q[..end]
            } else {
                let end = v.find(',').unwrap_or(v.len());
                v[..end].trim()
            });
        }
        rest = &rest[pos + key.len()..];
    }
    None
}

fn field(value: &str) -> Result<String<MAX_FIELD>, Error> {
    String::try_from(value).map_err(|_| Error::BufferFull)
}

impl Challenge {
    /// Parse the value of a `WWW-Authenticate` header.
    pub fn parse(header_value: &str) -> Result<Self, Error> {
        let h = header_value.trim();
        if h.len() < 6 || !h[..6].eq_ignore_ascii_case("digest") {
            return Err(Error::BadChallenge);
        }
        let realm = param(h, "realm").ok_or(Error::BadChallenge)?;
        let nonce = param(h, "nonce").ok_or(Error::BadChallenge)?;
        let qop_auth = param(h, "qop").is_some_and(|q| q.split(',').any(|v| v.trim() == "auth"));
        let opaque = match param(h, "opaque") {
            Some(o) => Some(field(o)?),
            None => None,
        };
        Ok(Self { realm: field(realm)?, nonce: field(nonce)?, qop_auth, opaque })
    }

    /// Build the `Authorization` header value for one request.
    ///
    /// `nc` must increase for every request reusing the same nonce; reusing a
    /// (nonce, nc) pair is exactly what replay protection exists to reject.
    pub fn authorization<const N: usize>(
        &self,
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce: &str,
    ) -> Result<String<N>, Error> {
        let ha1 = md5_hex(&[username, &self.realm, password]);
        let ha2 = md5_hex(&[method, uri]);

        let mut nc_hex: String<8> = String::new();
        write!(nc_hex, "{nc:08x}").map_err(|_| Error::BufferFull)?;

        let response = if self.qop_auth {
            md5_hex(&[&ha1, &self.nonce, &nc_hex, cnonce, "auth", &ha2])
        } else {
            md5_hex(&[&ha1, &self.nonce, &ha2])
        };

        let mut out: String<N> = String::new();
        write!(
            out,
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", algorithm=MD5, response=\"{}\"",
            username, self.realm, self.nonce, uri, response
        )
        .map_err(|_| Error::BufferFull)?;
        if self.qop_auth {
            write!(out, ", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\"")
                .map_err(|_| Error::BufferFull)?;
        }
        if let Some(o) = &self.opaque {
            write!(out, ", opaque=\"{o}\"").map_err(|_| Error::BufferFull)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a FRITZ!Box actually sends back on the first POST.
    const FRITZ: &str = r#"Digest realm="F!Box SOAP-Auth", nonce="3E1D5C7B9A0F2846", qop="auth", charset="utf-8", algorithm="MD5""#;

    #[test]
    fn parses_a_fritzbox_challenge() {
        let c = Challenge::parse(FRITZ).unwrap();
        assert_eq!(c.realm.as_str(), "F!Box SOAP-Auth");
        assert_eq!(c.nonce.as_str(), "3E1D5C7B9A0F2846");
        assert!(c.qop_auth);
        assert_eq!(c.opaque, None);
    }

    #[test]
    fn parses_unquoted_and_opaque_variants() {
        let c = Challenge::parse(r#"Digest realm="r", nonce="n", qop=auth, opaque="xyz""#).unwrap();
        assert!(c.qop_auth);
        assert_eq!(c.opaque.as_deref(), Some("xyz"));
    }

    #[test]
    fn rejects_non_digest_and_incomplete_challenges() {
        assert_eq!(Challenge::parse("Basic realm=\"x\""), Err(Error::BadChallenge));
        assert_eq!(Challenge::parse("Digest realm=\"r\""), Err(Error::BadChallenge));
        assert_eq!(Challenge::parse(""), Err(Error::BadChallenge));
    }

    /// The RFC 2617 section 3.5 worked example, which pins the whole MD5 chain.
    #[test]
    fn matches_the_rfc2617_reference_vector() {
        let c = Challenge::parse(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        let auth: String<512> = c
            .authorization("Mufasa", "Circle Of Life", "GET", "/dir/index.html", 1, "0a4f113b")
            .unwrap();
        assert!(
            auth.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "got: {auth}"
        );
        assert!(auth.contains("nc=00000001"));
        assert!(auth.contains("qop=auth"));
    }

    #[test]
    fn without_qop_the_short_formula_is_used() {
        let c = Challenge::parse(r#"Digest realm="r", nonce="n""#).unwrap();
        let auth: String<512> = c.authorization("u", "p", "POST", "/x", 1, "cn").unwrap();
        assert!(!auth.contains("qop"));
        assert!(!auth.contains("cnonce"));
        // HA1=md5(u:r:p), HA2=md5(POST:/x), response=md5(HA1:n:HA2)
        let ha1 = md5_hex(&["u", "r", "p"]);
        let ha2 = md5_hex(&["POST", "/x"]);
        let expect = md5_hex(&[&ha1, "n", &ha2]);
        assert!(auth.contains(expect.as_str()));
    }

    #[test]
    fn nonce_count_is_zero_padded_to_eight_digits() {
        let c = Challenge::parse(FRITZ).unwrap();
        let auth: String<512> = c.authorization("u", "p", "POST", "/x", 42, "cn").unwrap();
        assert!(auth.contains("nc=0000002a"), "got: {auth}");
    }

    #[test]
    fn overflow_is_reported_not_truncated() {
        let c = Challenge::parse(FRITZ).unwrap();
        let r: Result<String<32>, _> = c.authorization("u", "p", "POST", "/x", 1, "cn");
        assert_eq!(r.unwrap_err(), Error::BufferFull);
    }
}
