//! Minimal SOAP envelope builder and response scraper for TR-064.
//!
//! TR-064 responses are flat — a single level of `<NewSomething>value</NewSomething>`
//! inside the action response — so a tag scraper is enough and a real XML parser would
//! be several kilobytes of flash for no benefit. Everything here is total: malformed
//! input returns an error, never a panic.

use core::fmt::Write;
use heapless::String;

use crate::Error;

/// A TR-064 action: which service, where to POST it, and what it is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// e.g. `urn:dslforum-org:service:Hosts:1`
    pub service: &'static str,
    /// e.g. `/upnp/control/hosts`
    pub control_url: &'static str,
    /// e.g. `GetSpecificHostEntry`
    pub name: &'static str,
}

impl Action {
    /// The value for the `SOAPAction` HTTP header: `service#name`.
    pub fn soap_action<const N: usize>(&self) -> Result<String<N>, Error> {
        let mut s = String::new();
        write!(s, "{}#{}", self.service, self.name).map_err(|_| Error::BufferFull)?;
        Ok(s)
    }
}

/// Escape the five XML predefined entities.
///
/// MAC addresses and dotted-quad IPs never need this, but arguments are not always
/// going to be MACs and IPs, and an injection hole in a security control would be a
/// poor joke.
fn write_escaped<const N: usize>(out: &mut String<N>, value: &str) -> Result<(), Error> {
    for c in value.chars() {
        let r = match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c).map_err(|_| ()),
        };
        r.map_err(|_| Error::BufferFull)?;
    }
    Ok(())
}

/// Build a complete SOAP envelope for `action` with the given arguments.
pub fn envelope<const N: usize>(
    action: &Action,
    args: &[(&str, &str)],
) -> Result<String<N>, Error> {
    let mut s: String<N> = String::new();
    s.push_str(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
    )
    .map_err(|_| Error::BufferFull)?;
    s.push_str(
        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>"#,
    )
    .map_err(|_| Error::BufferFull)?;

    write!(s, "<u:{} xmlns:u=\"{}\">", action.name, action.service)
        .map_err(|_| Error::BufferFull)?;
    for (k, v) in args {
        write!(s, "<{k}>").map_err(|_| Error::BufferFull)?;
        write_escaped(&mut s, v)?;
        write!(s, "</{k}>").map_err(|_| Error::BufferFull)?;
    }
    write!(s, "</u:{}>", action.name).map_err(|_| Error::BufferFull)?;

    s.push_str("</s:Body></s:Envelope>").map_err(|_| Error::BufferFull)?;
    Ok(s)
}

/// Pull the text content of the first `<tag>…</tag>` out of a response.
///
/// Namespace prefixes are ignored: `<NewIPAddress>` and `<u:NewIPAddress>` both match.
pub fn extract<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut rest = xml;
    loop {
        let open = rest.find('<')?;
        rest = &rest[open + 1..];
        let close = rest.find('>')?;
        let (name_part, after) = rest.split_at(close);
        rest = &after[1..];

        // Skip closing tags, declarations and comments.
        if name_part.starts_with('/') || name_part.starts_with('?') || name_part.starts_with('!') {
            continue;
        }
        // Strip any attributes, then any namespace prefix.
        let name = name_part.split_whitespace().next().unwrap_or(name_part);
        let name = name.strip_suffix('/').unwrap_or(name);
        let local = name.rsplit(':').next().unwrap_or(name);
        if local != tag {
            continue;
        }
        let end = rest.find("</")?;
        return Some(&rest[..end]);
    }
}

/// TR-064 signals failure as a SOAP Fault carrying a UPnP `errorCode`.
pub fn fault_code(xml: &str) -> Option<u16> {
    if !xml.contains("Fault") {
        return None;
    }
    extract(xml, "errorCode")?.trim().parse().ok()
}

/// Parse the `1`/`0` booleans TR-064 uses. Also accepts `true`/`false`, which some
/// firmware versions emit.
pub fn parse_bool(value: &str) -> Result<bool, Error> {
    match value.trim() {
        "1" | "true" | "True" => Ok(true),
        "0" | "false" | "False" => Ok(false),
        _ => Err(Error::InvalidBool),
    }
}

/// Read a required field out of a response, mapping a SOAP Fault to a typed error.
pub fn require<'a>(xml: &'a str, tag: &'static str) -> Result<&'a str, Error> {
    if let Some(code) = fault_code(xml) {
        return Err(Error::SoapFault(code));
    }
    extract(xml, tag).ok_or(Error::MissingField(tag))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Action = Action {
        service: "urn:dslforum-org:service:Hosts:1",
        control_url: "/upnp/control/hosts",
        name: "GetSpecificHostEntry",
    };

    #[test]
    fn soap_action_header() {
        let h: String<128> = A.soap_action().unwrap();
        assert_eq!(h.as_str(), "urn:dslforum-org:service:Hosts:1#GetSpecificHostEntry");
    }

    #[test]
    fn envelope_contains_action_and_args() {
        let e: String<1024> = envelope(&A, &[("NewMACAddress", "AA:BB:CC:DD:EE:FF")]).unwrap();
        assert!(e.contains("<u:GetSpecificHostEntry xmlns:u=\"urn:dslforum-org:service:Hosts:1\">"));
        assert!(e.contains("<NewMACAddress>AA:BB:CC:DD:EE:FF</NewMACAddress>"));
        assert!(e.ends_with("</s:Body></s:Envelope>"));
    }

    #[test]
    fn envelope_escapes_arguments() {
        let e: String<1024> = envelope(&A, &[("X", "a&b<c>\"d'")]).unwrap();
        assert!(e.contains("<X>a&amp;b&lt;c&gt;&quot;d&apos;</X>"));
    }

    #[test]
    fn envelope_reports_overflow_instead_of_truncating() {
        // A silently truncated envelope would be a very confusing bug on the wire.
        let r: Result<String<64>, _> = envelope(&A, &[("NewMACAddress", "AA:BB:CC:DD:EE:FF")]);
        assert_eq!(r.unwrap_err(), Error::BufferFull);
    }

    #[test]
    fn extract_ignores_namespace_prefixes_and_attributes() {
        assert_eq!(extract("<a><NewIPAddress>192.168.1.5</NewIPAddress></a>", "NewIPAddress"),
                   Some("192.168.1.5"));
        assert_eq!(extract("<u:NewIPAddress>10.0.0.1</u:NewIPAddress>", "NewIPAddress"),
                   Some("10.0.0.1"));
        assert_eq!(extract(r#"<NewIPAddress xsi:type="string">1.2.3.4</NewIPAddress>"#, "NewIPAddress"),
                   Some("1.2.3.4"));
    }

    #[test]
    fn extract_skips_the_xml_declaration_and_close_tags() {
        let xml = r#"<?xml version="1.0"?><s:Body><NewActive>1</NewActive></s:Body>"#;
        assert_eq!(extract(xml, "NewActive"), Some("1"));
    }

    #[test]
    fn extract_returns_none_rather_than_panicking_on_junk() {
        for junk in ["", "<", "<<<", "<unclosed", "no tags at all", "<a></a>"] {
            assert_eq!(extract(junk, "NewIPAddress"), None, "junk: {junk:?}");
        }
    }

    #[test]
    fn parse_bool_accepts_both_spellings() {
        assert_eq!(parse_bool("1"), Ok(true));
        assert_eq!(parse_bool("0"), Ok(false));
        assert_eq!(parse_bool("true"), Ok(true));
        assert_eq!(parse_bool(" 0 "), Ok(false));
        assert_eq!(parse_bool("maybe"), Err(Error::InvalidBool));
    }
}
