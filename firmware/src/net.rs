//! Wi-Fi association, DHCP, and getting the wall clock from SNTP.
//!
//! The NTP server is the **default gateway** — the FRITZ!Box serves NTP, and using it
//! avoids DNS entirely on a device that already has to talk to that box for
//! enforcement. One less thing to fail at boot.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer};
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface, WifiController};
use medienzeit_core::sntp;

const SSID: &str = env!("MEDIENZEIT_SSID");
const PSK: &str = env!("MEDIENZEIT_PSK");

/// Keeps the association up, reconnecting after drops.
#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("net: wifi task started, target SSID {SSID}");

    let cfg = WifiConfig::Station(
        StationConfig::default().with_ssid(SSID).with_password(PSK.into()),
    );
    if let Err(e) = controller.set_config(&cfg) {
        println!("net: set_config failed: {e:?}");
        return;
    }

    loop {
        if controller.is_connected() {
            let _ = controller.wait_for_disconnect_async().await;
            println!("net: disconnected");
        }

        match controller.connect_async().await {
            Ok(_) => println!("net: associated"),
            Err(e) => {
                println!("net: connect failed: {e:?}, retrying in 5s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}

/// Block until DHCP has given us an address, reporting once.
pub async fn wait_for_dhcp(stack: Stack<'static>) -> embassy_net::StaticConfigV4 {
    loop {
        if let Some(cfg) = stack.config_v4() {
            println!("net: ip {} gw {:?}", cfg.address, cfg.gateway);
            return cfg;
        }
        Timer::after(Duration::from_millis(300)).await;
    }
}

/// One SNTP round trip. Returns a unix timestamp in seconds.
///
/// Deliberately no retry loop: the caller decides what a failure means, because
/// "we still have no idea what time it is" is a state the ledger must not tick in.
pub async fn sntp_once(
    stack: Stack<'static>,
    server: embassy_net::Ipv4Address,
) -> Result<i64, &'static str> {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 128];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_buf = [0u8; 128];

    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    sock.bind(0).map_err(|_| "udp bind failed")?;

    let req = sntp::request();
    let endpoint = embassy_net::IpEndpoint::new(server.into(), sntp::PORT);
    sock.send_to(&req, endpoint).await.map_err(|_| "sntp send failed")?;

    let mut buf = [0u8; 128];
    let recv = embassy_time::with_timeout(Duration::from_secs(5), sock.recv_from(&mut buf))
        .await
        .map_err(|_| "sntp timed out")?
        .map_err(|_| "sntp recv failed")?;

    sntp::unix_seconds(&buf[..recv.0]).map_err(|e| match e {
        sntp::Error::NotSynchronised => "ntp server is not synchronised",
        sntp::Error::NotAServerReply => "not an ntp server reply",
        sntp::Error::NoTimestamp => "ntp reply had no usable timestamp",
        sntp::Error::BadLength => "ntp reply was the wrong length",
    })
}

