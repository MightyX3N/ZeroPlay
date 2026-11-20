//! Settings, read from environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

/// An address players can reach the server's media ports on, besides the
/// addresses of the machine itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announce {
    /// IP address or host name.
    pub host: String,
    /// Public port for the first media port. The rest follow in order. `None`
    /// means the same port numbers as on the server.
    pub first_port: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Where the web page and the player connection listen.
    pub http_addr: SocketAddr,
    /// Where the Microsoft sign-in is kept.
    pub data_dir: PathBuf,
    /// When set, the web page asks for this password.
    pub password: Option<String>,
    /// UDP ports for the players' video. One per connected player or watcher.
    pub rtc_ports: Vec<u16>,
    pub announce: Vec<Announce>,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let http_addr = var("ZEROPLAY_HTTP")
            .unwrap_or_else(|| "0.0.0.0:8080".to_owned())
            .parse()
            .context("ZEROPLAY_HTTP must look like 0.0.0.0:8080")?;
        let data_dir = PathBuf::from(var("ZEROPLAY_DATA").unwrap_or_else(|| "zeroplay-data".to_owned()));
        let password = var("ZEROPLAY_PASSWORD");
        let rtc_ports = parse_ports(&var("ZEROPLAY_RTC_PORTS").unwrap_or_else(|| "50000-50009".to_owned()))
            .context("ZEROPLAY_RTC_PORTS must look like 50000-50009")?;
        let announce = match var("ZEROPLAY_ANNOUNCE") {
            Some(text) => parse_announce(&text).context("ZEROPLAY_ANNOUNCE must look like 192.168.1.10,example.playit.gg:12000")?,
            None => Vec::new(),
        };
        Ok(Config { http_addr, data_dir, password, rtc_ports, announce })
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

/// `192.168.1.10,example.playit.gg:12000`.
pub fn parse_announce(text: &str) -> Result<Vec<Announce>> {
    text.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|part| match part.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => Ok(Announce {
                host: host.to_owned(),
                first_port: Some(port.parse().map_err(|_| anyhow!("{part}: bad port"))?),
            }),
            _ => Ok(Announce { host: part.to_owned(), first_port: None }),
        })
        .collect()
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

    #[test]
    fn announce_with_and_without_port() {
        assert_eq!(
            parse_announce("192.168.1.10, abc.playit.gg:12000").unwrap(),
            vec![
                Announce { host: "192.168.1.10".to_owned(), first_port: None },
                Announce { host: "abc.playit.gg".to_owned(), first_port: Some(12000) },
            ]
        );
        assert!(parse_announce("host:nope").is_err());
    }
}
