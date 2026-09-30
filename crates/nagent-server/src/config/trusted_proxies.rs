//! `trusted_proxies` — `[server].trusted_proxies` CIDR list (security plan #5).

use crate::config::file::TomlTrustedProxiesConfig;
use crate::config::{env_opt, resolve_primitive, ConfigError};

/// Trusted-proxy configuration (security plan #5).
///
/// The rate-limit resolver uses `cidr` to decide whether
/// `X-Forwarded-For` is trustworthy for a given TCP connection:
/// the peer IP must fall inside one of the listed CIDR ranges
/// for the header to influence the bucket key. Direct
/// connections from outside the list are always keying on the
/// peer IP, so a public client cannot forge a header to push
/// themselves into a different bucket.
///
/// `loopback_bypass` controls whether loopback IPs skip the
/// bucket entirely. Defaults to `true` (the historical dev
/// default); boot emits a `WARN` when this is `true` and the
/// bind address is non-loopback, which is almost always a
/// misconfiguration.
#[derive(Debug, Clone)]
pub struct TrustedProxiesConfig {
    /// Parsed CIDR list. Empty when no proxy is configured.
    pub cidrs: Vec<ipnet::IpNet>,
    /// When `true`, requests whose resolved peer IP is loopback
    /// (`127.0.0.0/8` or `::1`) bypass the limiter entirely.
    pub loopback_bypass: bool,
}

impl Default for TrustedProxiesConfig {
    fn default() -> Self {
        Self {
            cidrs: Vec::new(),
            loopback_bypass: true,
        }
    }
}

impl TrustedProxiesConfig {
    /// `true` when the peer IP falls in any of the configured
    /// CIDR ranges. Used by the rate-limit resolver to decide
    /// whether to honour `X-Forwarded-For`.
    pub fn is_trusted(&self, ip: std::net::IpAddr) -> bool {
        self.cidrs.iter().any(|net| net.contains(&ip))
    }

    /// Convenience for `loopback_bypass && ip.is_loopback()`.
    pub fn loopback_is_bypassed(&self, ip: std::net::IpAddr) -> bool {
        self.loopback_bypass && ip.is_loopback()
    }

    /// Read the trusted-proxy config from the env var / TOML pair.
    /// Env `NAGENT_TRUSTED_PROXIES` is a comma-separated CIDR list.
    /// Env `NAGENT_TRUSTED_PROXIES_LOOPBACK_BYPASS` (bool) overrides
    /// the TOML `loopback_bypass`. Invalid CIDR strings fail boot.
    pub fn from_env_with_toml(
        toml: Option<&TomlTrustedProxiesConfig>,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();

        // Env wins over TOML when both are set (consistent with the
        // rest of the config layer).
        let cidr_raw = env_opt("NAGENT_TRUSTED_PROXIES")
            .or_else(|| toml.cidr.clone())
            .unwrap_or_default();
        let mut cidrs = Vec::new();
        for entry in cidr_raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let net: ipnet::IpNet = entry.parse().map_err(|e: ipnet::AddrParseError| {
                ConfigError::InvalidEnv(
                    "NAGENT_TRUSTED_PROXIES".into(),
                    format!("invalid CIDR `{entry}`: {e}"),
                )
            })?;
            cidrs.push(net);
        }

        let loopback_bypass = resolve_primitive(
            env_opt("NAGENT_TRUSTED_PROXIES_LOOPBACK_BYPASS").as_deref(),
            toml.loopback_bypass,
            defaults.loopback_bypass,
            "NAGENT_TRUSTED_PROXIES_LOOPBACK_BYPASS",
        )?;

        Ok(Self {
            cidrs,
            loopback_bypass,
        })
    }
}
