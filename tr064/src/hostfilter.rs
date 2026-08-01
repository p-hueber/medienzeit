//! `urn:dslforum-org:service:X_AVM-DE_HostFilter:1` — the actual enforcement.
//!
//! `DisallowWANAccessByIP` is what cuts a device off the internet. It is addressed by
//! **IP**, not MAC, so a device needs a static DHCP lease on the FRITZ!Box or the rule
//! silently stops matching when the lease moves. Resolve the current IP via
//! [`crate::hosts`] before calling this, and treat a changed IP as an event worth
//! logging.

use heapless::String;

use crate::soap::{self, Action};
use crate::Error;

pub const DISALLOW_WAN_ACCESS_BY_IP: Action = Action {
    service: "urn:dslforum-org:service:X_AVM-DE_HostFilter:1",
    control_url: "/upnp/control/x_hostfilter",
    name: "DisallowWANAccessByIP",
};

pub const GET_WAN_ACCESS_BY_IP: Action = Action {
    service: "urn:dslforum-org:service:X_AVM-DE_HostFilter:1",
    control_url: "/upnp/control/x_hostfilter",
    name: "GetWANAccessByIP",
};

pub const REQUEST_CAPACITY: usize = 640;

/// Block (`disallow = true`) or restore (`false`) internet access for one IP.
pub fn disallow_wan_access_by_ip(
    ip: &str,
    disallow: bool,
) -> Result<String<REQUEST_CAPACITY>, Error> {
    soap::envelope(
        &DISALLOW_WAN_ACCESS_BY_IP,
        &[
            ("NewIPv4Address", ip),
            ("NewDisallow", if disallow { "1" } else { "0" }),
        ],
    )
}

pub fn get_wan_access_by_ip(ip: &str) -> Result<String<REQUEST_CAPACITY>, Error> {
    soap::envelope(&GET_WAN_ACCESS_BY_IP, &[("NewIPv4Address", ip)])
}

/// `DisallowWANAccessByIP` returns an empty body on success — there is nothing to read
/// back, so the only thing to check is that it is not a Fault.
pub fn parse_disallow_ack(xml: &str) -> Result<(), Error> {
    match soap::fault_code(xml) {
        Some(code) => Err(Error::SoapFault(code)),
        None => Ok(()),
    }
}

/// Read back whether an IP is currently blocked. Used to reconcile after a reboot
/// rather than blindly re-issuing a block the box already has.
pub fn parse_wan_access(xml: &str) -> Result<bool, Error> {
    let v = soap::require(xml, "NewDisallow")?;
    soap::parse_bool(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACK: &str = include_str!("../tests/fixtures/disallow_wan_access_ack.xml");
    const STATUS: &str = include_str!("../tests/fixtures/get_wan_access_blocked.xml");
    const DENIED: &str = include_str!("../tests/fixtures/fault_unauthorized.xml");

    #[test]
    fn block_and_unblock_differ_only_in_the_flag() {
        let block = disallow_wan_access_by_ip("192.168.178.42", true).unwrap();
        let allow = disallow_wan_access_by_ip("192.168.178.42", false).unwrap();
        assert!(block.contains("<NewDisallow>1</NewDisallow>"));
        assert!(allow.contains("<NewDisallow>0</NewDisallow>"));
        assert!(block.contains("<NewIPv4Address>192.168.178.42</NewIPv4Address>"));
        assert!(block.contains("X_AVM-DE_HostFilter:1"));
    }

    #[test]
    fn control_url_is_the_hostfilter_endpoint() {
        // Posting HostFilter actions to /upnp/control/hosts silently 401s or 500s,
        // which is a miserable thing to debug. Pin it.
        assert_eq!(DISALLOW_WAN_ACCESS_BY_IP.control_url, "/upnp/control/x_hostfilter");
        assert_eq!(GET_WAN_ACCESS_BY_IP.control_url, "/upnp/control/x_hostfilter");
    }

    #[test]
    fn an_empty_ack_is_success() {
        assert_eq!(parse_disallow_ack(ACK), Ok(()));
    }

    #[test]
    fn unauthorized_surfaces_as_a_fault() {
        // 401 here means the TR-064 user lacks box-settings permission — a
        // configuration mistake, and worth an alert rather than a silent retry.
        assert_eq!(parse_disallow_ack(DENIED), Err(Error::SoapFault(401)));
    }

    #[test]
    fn reads_back_the_blocked_state() {
        assert_eq!(parse_wan_access(STATUS), Ok(true));
    }
}
