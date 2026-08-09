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
    println!("cargo:rerun-if-changed=ntfy.toml");
    println!("cargo:rerun-if-changed=tags.toml");
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
    // ntfy endpoint. Plain HTTP so a self-hosted instance works today; ntfy.sh is
    // HTTPS-only and needs the TLS path.
    let ntfy = read_kv(
        "ntfy.toml",
        r#"    host  = "192.168.178.20"   # IP; the device does no DNS
    port  = "80"
    header = "ntfy.example.lan"  # Host: header, for reverse proxies
    topic = "medienzeit-something-long-and-random"
    tls   = "false"           # "true" for ntfy.sh; certificates are not verified
"#,
    );
    for key in ["host", "port", "header", "topic", "tls"] {
        let v = require(&ntfy, "ntfy.toml", key);
        println!("cargo:rustc-env=MEDIENZEIT_NTFY_{}={v}", key.to_uppercase());
    }

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

    // Tag UIDs. Optional, and the only config file that is: the tags are a physical
    // thing that can arrive after the firmware works, and an empty value falls back to
    // the BOOT button so the spend path stays exercisable without them.
    let tags = std::fs::read_to_string("tags.toml").unwrap_or_default();
    let mut tag_uid = ["".to_string(), "".to_string()];
    let mut card_uid = ["".to_string(), "".to_string()];
    let mut protocol = "iso15693".to_string();
    for line in tags.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        match k.trim() {
            "device1_uid" => tag_uid[0] = v,
            "device2_uid" => tag_uid[1] = v,
            "device1_card_uid" => card_uid[0] = v,
            "device2_card_uid" => card_uid[1] = v,
            "protocol" => protocol = v,
            _ => {}
        }
    }
    // One protocol at a time, chosen at build time. The PN5180 can do both, but not
    // without reconfiguring the front end between polls, which cycles the field and
    // wedges the transceiver. Nothing here needs both at once.
    if protocol != "iso15693" && protocol != "iso14443a" {
        panic!("firmware/tags.toml: protocol must be \"iso15693\" or \"iso14443a\", got {protocol:?}");
    }
    println!("cargo:rustc-env=MEDIENZEIT_PROTOCOL={protocol}");
    println!("cargo:rustc-env=MEDIENZEIT_TAG_DEVICE1={}", tag_uid[0]);
    println!("cargo:rustc-env=MEDIENZEIT_TAG_DEVICE2={}", tag_uid[1]);
    println!("cargo:rustc-env=MEDIENZEIT_CARD_DEVICE1={}", card_uid[0]);
    println!("cargo:rustc-env=MEDIENZEIT_CARD_DEVICE2={}", card_uid[1]);

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
