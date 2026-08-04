//! Reads Wi-Fi credentials from `wifi.toml` and hands them to the compiler as env
//! vars. The file is gitignored: the PSK belongs on this machine, not in the repo.

use std::collections::HashMap;
use std::path::Path;

/// Parse a flat `key = "value"` file. Deliberately not a TOML dependency: these files
/// have no structure worth a parser.
fn read_kv(path: &str, template: &str) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(Path::new(path)) else {
        panic!("\n\n  firmware/{path} is missing. Create it with:\n\n{template}\n  It is gitignored.\n");
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    out
}

fn require(map: &HashMap<String, String>, file: &str, key: &str) -> String {
    map.get(key)
        .unwrap_or_else(|| panic!("firmware/{file} is missing `{key}`"))
        .clone()
}

fn main() {
    println!("cargo:rerun-if-changed=wifi.toml");
    println!("cargo:rerun-if-changed=fritzbox.toml");
    println!("cargo:rerun-if-changed=web.toml");
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

    let fb = read_kv(
        "fritzbox.toml",
        r#"    user = "medienzeit"
    pass = "the dedicated TR-064 user's password"
    device1_name = "Handy"
    device1_mac  = "aa:bb:cc:dd:ee:ff"
    device2_name = "Tablet"
    device2_mac  = "11:22:33:44:55:66"
"#,
    );
    // Admin page credentials. Separate from the TR-064 user on purpose: this one only
    // grants screen time, so it does not want the FRITZ!Box password behind it.
    let web = read_kv(
        "web.toml",
        r#"    user = "papa"
    pass = "something only you know"
"#,
    );
    for key in ["user", "pass"] {
        let v = require(&web, "web.toml", key);
        println!("cargo:rustc-env=MEDIENZEIT_WEB_{}={v}", key.to_uppercase());
    }

    for key in [
        "user",
        "pass",
        "device1_name",
        "device1_mac",
        "device2_name",
        "device2_mac",
    ] {
        let v = require(&fb, "fritzbox.toml", key);
        println!("cargo:rustc-env=MEDIENZEIT_FB_{}={v}", key.to_uppercase());
    }
}
