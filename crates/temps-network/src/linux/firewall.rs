//! nftables baseline rules.
//!
//! We install one dedicated nftables table named `temps_network` so we can
//! tear our rules down without touching anything else on the host. The
//! table has two chains:
//!
//! * `forward` (priority -100, type filter, hook forward) — accepts
//!   anything that ingresses from or egresses to our bridge. Sits *before*
//!   Docker's default-DROP `forward` chain so it takes effect even when
//!   Docker is installed alongside us.
//! * `postrouting` (priority 100, type nat, hook postrouting) — masquerades
//!   compute CIDR traffic that egresses on a non-bridge interface. This is what
//!   lets containers reach the internet.
//!
//! We shell out to `nft` because it is the canonical tool, every modern
//! distro ships it, and the rule set we need is small enough that an
//! embedded library (`rustables`) would add more complexity than value.

use crate::config::{NetworkConfig, NodeAlloc, Peer, Transport};
use crate::error::NetworkError;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info};
use uuid::Uuid;

const TABLE: &str = "temps_network";

/// Install the baseline rules. Idempotent: the script first deletes the
/// table (ignoring "not found"), then recreates it.
pub async fn install_baseline(
    config: &NetworkConfig,
    alloc: &NodeAlloc,
    peers: &[Peer],
) -> crate::Result<()> {
    let script = render_baseline(config, alloc, peers);
    apply_nft(&script)
        .await
        .map_err(|reason| NetworkError::Nftables {
            op: "install_baseline",
            table: TABLE.into(),
            reason,
        })?;
    info!(table = TABLE, bridge = %config.bridge_name, cidr = %alloc.compute_cidr, "nftables baseline installed");
    Ok(())
}

/// Return whether the owned table contains the marker for the exact desired
/// configuration. This avoids rewriting a live firewall on every peer poll
/// while still repairing `nft flush table inet temps_network` and stale peer
/// allowlists automatically.
pub async fn baseline_is_current(
    config: &NetworkConfig,
    alloc: &NodeAlloc,
    peers: &[Peer],
) -> crate::Result<bool> {
    let marker = baseline_marker(config, alloc, peers);
    let output = Command::new("nft")
        .args(["list", "table", "inet", TABLE])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| NetworkError::Nftables {
            op: "inspect_baseline",
            table: TABLE.into(),
            reason: format!("spawn nft: {error}"),
        })?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout).contains(&marker))
}

/// Remove the baseline rules. Idempotent.
pub async fn remove_baseline(_config: &NetworkConfig) -> crate::Result<()> {
    let script = format!("delete table inet {table}\n", table = TABLE);
    match apply_nft(&script).await {
        Ok(()) => Ok(()),
        Err(reason) if reason.contains("No such file") || reason.contains("does not exist") => {
            debug!(table = TABLE, "nftables table already absent");
            Ok(())
        }
        Err(reason) => Err(NetworkError::Nftables {
            op: "remove_baseline",
            table: TABLE.into(),
            reason,
        }),
    }
}

fn render_baseline(config: &NetworkConfig, alloc: &NodeAlloc, peers: &[Peer]) -> String {
    let bridge = &config.bridge_name;
    let cidr = alloc.compute_cidr;
    let vxlan_ingress = match config.transport {
        Transport::Vxlan { port, .. } => {
            let mut rules = String::new();
            let underlay_device = &config.underlay_dev;
            let local_family = if alloc.underlay_address.is_ipv4() {
                "ip"
            } else {
                "ip6"
            };
            for peer in peers {
                let family = if peer.underlay_address.is_ipv4() {
                    "ip"
                } else {
                    "ip6"
                };
                if family != local_family {
                    continue;
                }
                rules.push_str(&format!(
                    "add rule inet {TABLE} input iifname \"{underlay_device}\" {family} daddr {} {family} saddr {} udp dport {port} accept\n",
                    alloc.underlay_address, peer.underlay_address
                ));
            }
            rules.push_str(&format!(
                "add rule inet {TABLE} input iifname \"{underlay_device}\" {local_family} daddr {} udp dport {port} counter drop\n",
                alloc.underlay_address
            ));
            rules
        }
        Transport::Native => String::new(),
    };
    let marker = baseline_marker(config, alloc, peers);
    format!(
        "
# Idempotent install: drop the table if it exists, recreate from scratch.
add table inet {table}
delete table inet {table}
add table inet {table}

add chain inet {table} forward {{ type filter hook forward priority -100; policy accept; }}
# Cloud-metadata endpoints hand out instance credentials to any local
# caller; containers must never reach them. These sit BEFORE the bridge
# accept rules (this chain runs at priority -100, ahead of Docker's own
# chains, so a later iptables rule could not catch this traffic).
# 169.254/16 = AWS/GCP/Azure/Hetzner/DO/Tencent; 100.100.100.200 = Alibaba.
add rule inet {table} forward ip daddr 169.254.0.0/16 counter reject
add rule inet {table} forward ip daddr 100.100.100.200 counter reject
add rule inet {table} forward ip6 daddr fd00:ec2::254 counter reject
add rule inet {table} forward ip6 daddr fd20:ce::254 counter reject
add rule inet {table} forward iifname \"{bridge}\" accept
add rule inet {table} forward oifname \"{bridge}\" accept

add chain inet {table} input {{ type filter hook input priority -100; policy accept; }}
{vxlan_ingress}
# Marker used by the reconciler to detect a flushed or stale owned table.
add rule inet {table} input counter comment \"{marker}\"

add chain inet {table} postrouting {{ type nat hook postrouting priority 100; policy accept; }}
add rule inet {table} postrouting ip saddr {cidr} oifname != \"{bridge}\" masquerade
",
        table = TABLE,
        bridge = bridge,
        cidr = cidr,
        vxlan_ingress = vxlan_ingress,
        marker = marker,
    )
}

fn baseline_marker(config: &NetworkConfig, alloc: &NodeAlloc, peers: &[Peer]) -> String {
    let mut peers = peers.to_vec();
    peers.sort_by_key(|peer| (peer.compute_cidr, peer.underlay_address, peer.node_id));
    let signature = format!("{config:?}|{alloc:?}|{peers:?}");
    format!(
        "temps-baseline-{}",
        Uuid::new_v5(&Uuid::NAMESPACE_OID, signature.as_bytes())
    )
}

async fn apply_nft(script: &str) -> std::result::Result<(), String> {
    let mut child = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn nft: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(script.as_bytes())
            .await
            .map_err(|e| format!("write nft script: {}", e))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| format!("close nft stdin: {}", e))?;
    }

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| format!("wait nft: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::Ipv4Net;
    use std::net::{IpAddr, Ipv4Addr};
    use std::str::FromStr;
    use uuid::Uuid;

    #[test]
    fn baseline_script_includes_bridge_and_cidr() {
        let cfg = NetworkConfig::default();
        let alloc = NodeAlloc {
            node_id: Uuid::nil(),
            compute_cidr: Ipv4Net::from_str("172.20.5.0/24").unwrap(),
            bridge_address: IpAddr::V4(Ipv4Addr::new(172, 20, 5, 1)),
            underlay_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        let s = render_baseline(&cfg, &alloc, &[]);
        assert!(s.contains("br-temps0"));
        assert!(s.contains("172.20.5.0/24"));
        assert!(s.contains("masquerade"));
        assert!(s.contains("delete table inet temps_network"));
    }

    #[test]
    fn baseline_script_blocks_metadata_before_bridge_accept() {
        let cfg = NetworkConfig::default();
        let alloc = NodeAlloc {
            node_id: Uuid::nil(),
            compute_cidr: Ipv4Net::from_str("172.20.5.0/24").unwrap(),
            bridge_address: IpAddr::V4(Ipv4Addr::new(172, 20, 5, 1)),
            underlay_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        let s = render_baseline(&cfg, &alloc, &[]);
        let aws_block = s
            .find("ip daddr 169.254.0.0/16 counter reject")
            .expect("link-local cloud metadata reject rule present");
        let alibaba_block = s
            .find("ip daddr 100.100.100.200 counter reject")
            .expect("Alibaba metadata reject rule present");
        let aws_ipv6_block = s
            .find("ip6 daddr fd00:ec2::254 counter reject")
            .expect("AWS IPv6 metadata reject rule present");
        let google_ipv6_block = s
            .find("ip6 daddr fd20:ce::254 counter reject")
            .expect("Google IPv6 metadata reject rule present");
        let bridge_accept = s
            .find("forward iifname \"br-temps0\" accept")
            .expect("bridge accept rule present");
        assert!(
            aws_block < bridge_accept
                && alibaba_block < bridge_accept
                && aws_ipv6_block < bridge_accept
                && google_ipv6_block < bridge_accept,
            "metadata rejects must precede the bridge accept rule, \
             or accepted traffic would never reach them"
        );
    }

    #[test]
    fn vxlan_ingress_is_restricted_to_known_peers() {
        let cfg = NetworkConfig::default();
        let alloc = NodeAlloc {
            node_id: Uuid::nil(),
            compute_cidr: Ipv4Net::from_str("172.20.5.0/24").unwrap(),
            bridge_address: IpAddr::V4(Ipv4Addr::new(172, 20, 5, 1)),
            underlay_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        let peer = Peer {
            node_id: Uuid::new_v4(),
            compute_cidr: Ipv4Net::from_str("172.20.6.0/24").unwrap(),
            underlay_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
        };
        let marker = baseline_marker(&cfg, &alloc, std::slice::from_ref(&peer));
        let script = render_baseline(&cfg, &alloc, &[peer]);
        let allow = script
            .find(
                "input iifname \"eth0\" ip daddr 10.0.0.1 ip saddr 10.0.0.2 udp dport 4789 accept",
            )
            .expect("known peer allow rule");
        let drop = script
            .find("input iifname \"eth0\" ip daddr 10.0.0.1 udp dport 4789 counter drop")
            .expect("unknown peer drop rule");
        assert!(allow < drop);
        assert!(script.contains(&marker));
    }

    #[test]
    fn baseline_marker_is_stable_across_peer_order() {
        let cfg = NetworkConfig::default();
        let alloc = NodeAlloc {
            node_id: Uuid::nil(),
            compute_cidr: Ipv4Net::from_str("172.20.5.0/24").unwrap(),
            bridge_address: IpAddr::V4(Ipv4Addr::new(172, 20, 5, 1)),
            underlay_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        };
        let a = Peer {
            node_id: Uuid::from_u128(1),
            compute_cidr: Ipv4Net::from_str("172.20.6.0/24").unwrap(),
            underlay_address: "10.0.0.2".parse().unwrap(),
        };
        let b = Peer {
            node_id: Uuid::from_u128(2),
            compute_cidr: Ipv4Net::from_str("172.20.7.0/24").unwrap(),
            underlay_address: "10.0.0.3".parse().unwrap(),
        };
        assert_eq!(
            baseline_marker(&cfg, &alloc, &[a.clone(), b.clone()]),
            baseline_marker(&cfg, &alloc, &[b, a])
        );
    }
}
