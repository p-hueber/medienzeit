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

    let ssid = ssid.expect("wifi.toml is missing `ssid`");
    let psk = psk.expect("wifi.toml is missing `psk`");
    println!("cargo:rustc-env=MEDIENZEIT_SSID={ssid}");
    println!("cargo:rustc-env=MEDIENZEIT_PSK={psk}");
}
