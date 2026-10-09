use std::net::IpAddr;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub server: ServerConfig,
}

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub max_connections: usize,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_address: String,
    pub request_timeout: u64,
    pub rate_limit: u32,
    pub libretranslate_url: String,
    /// Peers whose `X-Real-IP` / `X-Forwarded-For` are believed (the reverse proxy). Anyone
    /// else is rate-limited by the TCP peer address, whatever headers they send.
    pub trusted_proxies: Vec<IpNet>,
}

/// Loopback plus the private ranges: nginx reaches the webserver over the docker network.
pub const DEFAULT_TRUSTED_PROXIES: &str = "127.0.0.0/8,::1/128,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,fc00::/7";

/// An address range (`10.0.0.0/8`, `fc00::/7`, or a bare address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, p)) => (ip, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = ip.parse().map_err(|_| format!("not an IP address: {s}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max).ok_or(format!("bad prefix length: {s}"))?,
            None => max,
        };
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 peer can arrive as ::ffff:a.b.c.d on a dual-stack socket.
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// A comma-separated list of ranges; empty trusts no proxy.
pub fn parse_trusted_proxies(list: &str) -> Result<Vec<IpNet>, String> {
    list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(IpNet::parse).collect()
}

impl AppConfig {
    pub fn from_args(args: &crate::Args, worker_threads: usize) -> Result<Self, String> {
        // Calculate default db connections as worker_threads * 3, with a minimum of 10
        let default_db_connections = std::cmp::max(worker_threads * 3, 10);
        let max_connections = args.db_max_connections.unwrap_or(default_db_connections);

        Ok(Self {
            database: DatabaseConfig {
                host: args.db_host.clone(),
                port: args.db_port,
                database: args.db_name.clone(),
                username: args.db_user.clone(),
                password: args.db_password.clone(),
                max_connections,
            },
            server: ServerConfig {
                bind_address: args.bind_address.clone(),
                request_timeout: args.request_timeout,
                rate_limit: args.rate_limit,
                libretranslate_url: args.libretranslate_url.clone(),
                trusted_proxies: parse_trusted_proxies(&args.trusted_proxies)?,
            },
        })
    }

    pub fn connection_string(&self) -> String {
        format!(
            "postgresql://{}:{}@{}:{}/{}",
            self.database.username,
            self.database.password,
            self.database.host,
            self.database.port,
            self.database.database
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trusted_proxies_cover_loopback_and_private_ranges() {
        let nets = parse_trusted_proxies(DEFAULT_TRUSTED_PROXIES).unwrap();
        let trusted = |ip: &str| nets.iter().any(|n| n.contains(ip.parse().unwrap()));
        for ip in ["127.0.0.1", "::1", "10.1.2.3", "172.18.0.5", "192.168.1.1", "fd00::1", "::ffff:172.18.0.5"] {
            assert!(trusted(ip), "{ip} is trusted");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!trusted(ip), "{ip} is not trusted");
        }
    }

    #[test]
    fn parses_bare_addresses_and_rejects_junk() {
        assert!(IpNet::parse("1.2.3.4").unwrap().contains("1.2.3.4".parse().unwrap()));
        assert!(!IpNet::parse("1.2.3.4").unwrap().contains("1.2.3.5".parse().unwrap()));
        assert!(IpNet::parse("0.0.0.0/0").unwrap().contains("9.9.9.9".parse().unwrap()));
        assert!(IpNet::parse("10.0.0.0/33").is_err());
        assert!(IpNet::parse("nope").is_err());
        assert_eq!(parse_trusted_proxies(" ").unwrap(), vec![]);
    }
}
