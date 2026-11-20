//! Settings, read from environment variables. All have defaults; none need
//! setting for a normal NAS install.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    /// The page for the home network.
    pub http_addr: SocketAddr,
    /// The page for visitors coming through the playit tunnel. Only on this
    /// machine; the tunnel delivers here.
    pub remote_addr: SocketAddr,
    /// Where the Microsoft sign-in, the password and the playit key are kept.
    pub data_dir: PathBuf,
    /// UDP ports for the players' video. One per connected player or watcher.
    pub rtc_ports: Vec<u16>,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let http_addr = var("ZEROPLAY_HTTP")
            .unwrap_or_else(|| "0.0.0.0:8080".to_owned())
            .parse()
            .context("ZEROPLAY_HTTP must look like 0.0.0.0:8080")?;
        let remote_addr = var("ZEROPLAY_REMOTE_HTTP")
            .unwrap_or_else(|| "127.0.0.1:8081".to_owned())
            .parse()
            .context("ZEROPLAY_REMOTE_HTTP must look like 127.0.0.1:8081")?;
        let data_dir = PathBuf::from(var("ZEROPLAY_DATA").unwrap_or_else(|| "zeroplay-data".to_owned()));
        let rtc_ports = parse_ports(&var("ZEROPLAY_RTC_PORTS").unwrap_or_else(|| "50000-50009".to_owned()))
            .context("ZEROPLAY_RTC_PORTS must look like 50000-50009")?;
        Ok(Config { http_addr, remote_addr, data_dir, rtc_ports })
    }
}

/// `50000-50009` or `50000,50002,50004`.
pub fn parse_ports(text: &str) -> Result<Vec<u16>> {
    let mut ports = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((from, to)) => {
                let from: u16 = from.trim().parse()?;
                let to: u16 = to.trim().parse()?;
                if to < from {
                    bail!("{part}: the range goes backwards");
                }
                ports.extend(from..=to);
            }
            None => ports.push(part.parse()?),
        }
    }
    if ports.is_empty() {
        bail!("no ports given");
    }
    if ports.contains(&0) {
        bail!("port 0 is not allowed");
    }
    ports.dedup();
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_ranges_and_lists() {
        assert_eq!(parse_ports("50000-50002").unwrap(), vec![50000, 50001, 50002]);
        assert_eq!(parse_ports("50000, 50004").unwrap(), vec![50000, 50004]);
        assert!(parse_ports("50002-50000").is_err());
        assert!(parse_ports("").is_err());
        assert!(parse_ports("0").is_err());
    }
}
