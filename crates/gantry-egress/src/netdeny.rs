//! 拒绝网段（`docs/modules/egress.md` §7.7）：对 DNS 解析结果与 IP 字面量都适用。
//!
//! IPv4 映射（`::ffff:0:0/96`）与 NAT64（`64:ff9b::/96`）按内嵌的 IPv4 再判；另可追加
//! Egress 自己的监听地址与沙盒网段（引导配置给出）。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

const DENIED_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

const DENIED_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

const NAT64_PREFIX: [u16; 6] = [0x0064, 0xff9b, 0, 0, 0, 0];

/// 一个 CIDR 网段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid CIDR: {0}")]
pub struct CidrError(String);

impl Cidr {
    pub fn new(addr: IpAddr, prefix: u8) -> Result<Self, CidrError> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(CidrError(format!("{addr}/{prefix}")));
        }
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

impl FromStr for Cidr {
    type Err = CidrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = value
            .split_once('/')
            .ok_or_else(|| CidrError(value.to_owned()))?;
        let addr: IpAddr = addr.parse().map_err(|_| CidrError(value.to_owned()))?;
        let prefix: u8 = prefix.parse().map_err(|_| CidrError(value.to_owned()))?;
        Self::new(addr, prefix)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn prefix_eq(net: &[u8], ip: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    let rest = prefix % 8;
    if net[..full] != ip[..full] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    net[full] & mask == ip[full] & mask
}

/// 落点拒绝表：内置网段 + 追加网段（Egress 自身监听地址、沙盒网段）。
#[derive(Clone, Debug, Default)]
pub struct DenyNets {
    extra: Vec<Cidr>,
}

impl DenyNets {
    pub fn new(extra: Vec<Cidr>) -> Self {
        Self { extra }
    }

    /// `ip` 是否落在拒绝网段（`hard.private_ip`）。
    pub fn is_denied(&self, ip: IpAddr) -> bool {
        if let IpAddr::V6(v6) = ip {
            if let Some(v4) = embedded_v4(v6) {
                return self.is_denied(IpAddr::V4(v4));
            }
        }
        let builtin = match ip {
            IpAddr::V4(v4) => DENIED_V4
                .iter()
                .any(|(net, prefix)| prefix_eq(&net.octets(), &v4.octets(), *prefix)),
            IpAddr::V6(v6) => DENIED_V6
                .iter()
                .any(|(net, prefix)| prefix_eq(&net.octets(), &v6.octets(), *prefix)),
        };
        builtin || self.extra.iter().any(|cidr| cidr.contains(ip))
    }
}

/// IPv4 映射与 NAT64 内嵌的 IPv4。
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    let segments = v6.segments();
    if segments[..6] == NAT64_PREFIX {
        let [.., hi, lo] = segments;
        return Some(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
    }
    None
}
