//! TR-064 codec for the FRITZ!Box — request building, digest auth, response parsing.
//!
//! Deliberately **does no I/O**. It turns arguments into bytes to send and bytes
//! received into typed values, and nothing else. That is what lets the identical code
//! run under `reqwless` on the ESP32 and under `std::net` in `medienzeit-tr064-cli`,
//! and it is why every line here is testable on a laptop against recorded XML.
//!
//! TR-064 speaks plain HTTP on port 49000, so none of this needs TLS.
//!
//! # Talking to a box
//!
//! 1. POST the envelope from e.g. [`hosts::get_specific_host_entry`] to the action's
//!    `control_url`, with `SOAPAction` set from [`soap::Action::soap_action`].
//! 2. The box answers `401` with a `WWW-Authenticate` header. Feed it to
//!    [`digest::Challenge::parse`].
//! 3. Re-POST with the `Authorization` header from
//!    [`digest::Challenge::authorization`].
//! 4. Parse the body with the matching `parse_*` function.
//!
//! # Prerequisites on the box
//!
//! *Heimnetz → Netzwerk → Netzwerkeinstellungen → Zugriff für Anwendungen zulassen*
//! must be on, and the user needs box-settings permission. Both devices want static
//! DHCP leases, because [`hostfilter`] addresses rules by IP.

#![no_std]

pub mod digest;
pub mod hostfilter;
pub mod hosts;
pub mod soap;

/// TR-064's plain-HTTP port. There is a TLS listener on 49443, which we do not need.
pub const DEFAULT_PORT: u16 = 49000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A fixed-capacity buffer was too small. Never a truncated result.
    BufferFull,
    /// The response parsed, but a field we require was absent.
    MissingField(&'static str),
    /// The box returned a SOAP Fault carrying this UPnP `errorCode`.
    ///
    /// Worth distinguishing: `401` is a permissions problem with the TR-064 user,
    /// `714` means the MAC has never been seen, and both are configuration issues
    /// rather than transient failures — retrying will not help.
    SoapFault(u16),
    /// A `WWW-Authenticate` header we could not make sense of.
    BadChallenge,
    /// A boolean field held something other than 0/1/true/false.
    InvalidBool,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::BufferFull => write!(f, "buffer too small"),
            Error::MissingField(t) => write!(f, "response missing <{t}>"),
            Error::SoapFault(401) => write!(f, "SOAP fault 401: TR-064 user lacks permission"),
            Error::SoapFault(714) => write!(f, "SOAP fault 714: no such device known to the box"),
            Error::SoapFault(c) => write!(f, "SOAP fault {c}"),
            Error::BadChallenge => write!(f, "unparseable WWW-Authenticate challenge"),
            Error::InvalidBool => write!(f, "expected 0/1 boolean"),
        }
    }
}
