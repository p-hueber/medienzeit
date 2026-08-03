//! Reads Wi-Fi credentials from `wifi.toml` and hands them to the compiler as env
//! vars. The file is gitignored: the PSK belongs on this machine, not in the repo.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=wifi.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let path = Path::new("wifi.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        panic!(
            "\n\n  firmware/wifi.toml is missing. Create it with:\n\n\
             \x20   ssid = \"YourNetwork\"\n\
             \x20   psk  = \"YourPassword\"\n\n\
             It is gitignored.\n"
        );
    };

    let mut ssid = None;
    let mut psk = None;
    for line in text.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "ssid" => ssid = Some(v),
            "psk" => psk = Some(v),
            _ => {}
        }
    }

    // A floor for RTC plausibility. An RTC that has been running since the factory
    // holds a wrong-but-well-formed time with its oscillator-stop flag clear, so
    // nothing in the chip can flag it. Firmware cannot predate itself, so any stored
    // time older than this build is definitionally untrustworthy.
    let build_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs();
    println!("cargo:rustc-env=MEDIENZEIT_BUILD_UNIX={build_unix}");

    let ssid = ssid.expect("wifi.toml is missing `ssid`");
    let psk = psk.expect("wifi.toml is missing `psk`");
    println!("cargo:rustc-env=MEDIENZEIT_SSID={ssid}");
    println!("cargo:rustc-env=MEDIENZEIT_PSK={psk}");
}
