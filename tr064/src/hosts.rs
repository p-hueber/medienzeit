//! `urn:dslforum-org:service:Hosts:1`
//!
//! We use exactly one action from this service, but it earns its place twice over: it
//! maps a MAC to the device's current IP (which `HostFilter` needs, since that service
//! is addressed by IP) *and* reports whether the device is currently associated with
//! the WLAN.

use heapless::String;

use crate::soap::{self, Action};
use crate::Error;

pub const GET_SPECIFIC_HOST_ENTRY: Action = Action {
    service: "urn:dslforum-org:service:Hosts:1",
    control_url: "/upnp/control/hosts",
    name: "GetSpecificHostEntry",
};

/// Longest envelope this module produces, with headroom.
pub const REQUEST_CAPACITY: usize = 640;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    pub ip: String<46>,
    /// Whether the FRITZ!Box currently sees the device on the network.
    pub active: bool,
    pub hostname: String<64>,
}

pub fn get_specific_host_entry(mac: &str) -> Result<String<REQUEST_CAPACITY>, Error> {
    soap::envelope(&GET_SPECIFIC_HOST_ENTRY, &[("NewMACAddress", mac)])
}

pub fn parse_host_entry(xml: &str) -> Result<HostEntry, Error> {
    let ip = soap::require(xml, "NewIPAddress")?;
    let active = soap::parse_bool(soap::require(xml, "NewActive")?)?;
    // Hostname is informational; a device that has never announced one is not an error.
    let hostname = soap::extract(xml, "NewHostName").unwrap_or("");
    Ok(HostEntry {
        ip: String::try_from(ip).map_err(|_| Error::BufferFull)?,
        active,
        hostname: String::try_from(hostname).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = include_str!("../tests/fixtures/get_specific_host_entry.xml");
    const UNKNOWN: &str = include_str!("../tests/fixtures/fault_no_such_entry.xml");

    #[test]
    fn builds_the_request() {
        let r = get_specific_host_entry("3C:22:FB:11:22:33").unwrap();
        assert!(r.contains("<NewMACAddress>3C:22:FB:11:22:33</NewMACAddress>"));
        assert!(r.contains("urn:dslforum-org:service:Hosts:1"));
    }

    #[test]
    fn parses_an_active_host() {
        let h = parse_host_entry(OK).unwrap();
        assert_eq!(h.ip.as_str(), "192.168.178.42");
        assert!(h.active);
        assert_eq!(h.hostname.as_str(), "Pixel-8");
    }

    #[test]
    fn an_unknown_mac_is_a_typed_fault_not_a_parse_error() {
        // 714 is UPnP's "no such entry in array". Distinguishing it from a transport
        // failure matters: it means the device has never been seen, not that the box
        // is unreachable.
        assert_eq!(parse_host_entry(UNKNOWN), Err(Error::SoapFault(714)));
    }

    #[test]
    fn a_truncated_response_is_an_error_not_a_panic() {
        assert_eq!(parse_host_entry("<s:Envelope><s:Body>"), Err(Error::MissingField("NewIPAddress")));
        assert_eq!(parse_host_entry(""), Err(Error::MissingField("NewIPAddress")));
    }

    #[test]
    fn a_missing_hostname_is_tolerated() {
        let xml = "<NewIPAddress>10.0.0.9</NewIPAddress><NewActive>0</NewActive>";
        let h = parse_host_entry(xml).unwrap();
        assert_eq!(h.hostname.as_str(), "");
        assert!(!h.active);
    }
}
