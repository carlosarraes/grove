//! How an instance is reached: loopback-only by default, or deliberately exposed.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, UdpSocket};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Exposure {
    #[default]
    Local,
    Exposed {
        public_host: String,
    },
}

impl Exposure {
    pub fn local() -> Self {
        Self::Local
    }

    pub fn explicit(host: &str) -> Result<Self> {
        validate_public_host(host)?;
        Ok(Self::Exposed {
            public_host: host.to_string(),
        })
    }

    pub fn detect() -> Result<Self> {
        detect_with(default_route_ipv4)
    }

    pub fn is_exposed(&self) -> bool {
        matches!(self, Self::Exposed { .. })
    }

    pub fn public_host(&self) -> &str {
        match self {
            Self::Local => "localhost",
            Self::Exposed { public_host } => public_host,
        }
    }

    pub fn bind_host(&self) -> &str {
        match self {
            Self::Local => "127.0.0.1",
            Self::Exposed { .. } => "0.0.0.0",
        }
    }
}

fn detect_with(route: impl FnOnce() -> std::io::Result<Ipv4Addr>) -> Result<Exposure> {
    let address = route().with_context(|| {
        "detecting the default-route IPv4 address; pass `--expose-host <IPv4-or-hostname>` to choose it explicitly"
    })?;
    if address.is_loopback() || address.is_unspecified() {
        bail!(
            "default-route address {address} is not reachable from another machine; pass `--expose-host <IPv4-or-hostname>` explicitly"
        );
    }
    Exposure::explicit(&address.to_string())
}

/// Ask the kernel which IPv4 source address it would use for the default route. UDP
/// `connect` records a peer and selects a route without sending a packet.
fn default_route_ipv4() -> std::io::Result<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80))?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(address) => Ok(address),
        IpAddr::V6(_) => unreachable!("an IPv4 socket selected an IPv6 source address"),
    }
}

fn validate_public_host(host: &str) -> Result<()> {
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        if address.is_loopback() || address.is_unspecified() {
            bail!("exposure host {host:?} is not reachable from another machine");
        }
        return Ok(());
    }

    let hostname = host.strip_suffix('.').unwrap_or(host);
    let valid = !hostname.is_empty()
        && hostname.len() <= 253
        && !hostname.eq_ignore_ascii_case("localhost")
        && !hostname.to_ascii_lowercase().ends_with(".localhost")
        && hostname.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        });
    if !valid {
        bail!(
            "exposure host {host:?} must be a non-loopback IPv4 address or an ASCII hostname, without a scheme, port, or path"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Exposure, detect_with};
    use std::io;
    use std::net::Ipv4Addr;

    #[test]
    fn automatic_exposure_uses_the_default_routes_source_ipv4() {
        let exposure = detect_with(|| Ok(Ipv4Addr::new(192, 168, 50, 12))).expect("detect");

        assert_eq!(
            exposure,
            Exposure::Exposed {
                public_host: "192.168.50.12".to_string()
            }
        );
    }

    #[test]
    fn automatic_exposure_explains_how_to_override_detection_failures() {
        let error = detect_with(|| {
            Err(io::Error::new(
                io::ErrorKind::NetworkUnreachable,
                "no route",
            ))
        })
        .expect_err("no default route should fail");

        assert!(error.to_string().contains("--expose-host"), "{error:#}");
    }

    #[test]
    fn automatic_exposure_rejects_a_loopback_route() {
        let error = detect_with(|| Ok(Ipv4Addr::LOCALHOST))
            .expect_err("loopback is not remotely reachable");

        assert!(error.to_string().contains("--expose-host"), "{error:#}");
    }
}
