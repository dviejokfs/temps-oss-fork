//! Cloud-metadata egress block for app containers.
//!
//! Every major cloud provider serves instance credentials from a
//! link-local metadata endpoint (`169.254.169.254` on AWS/GCP/Azure/
//! Hetzner/DigitalOcean, `100.100.100.200` on Alibaba). The endpoint
//! trusts *location*, not credentials: any request originating from the
//! host — including one made by a deployed app container, or by an SSRF
//! bug tricked into fetching an attacker-supplied URL — receives live
//! cloud credentials for the server's IAM role.
//!
//! Deployed workloads have essentially no legitimate reason to call the
//! metadata service, so this module blocks it at the network layer: a
//! dedicated `TEMPS_METADATA_BLOCK` iptables chain, hooked into FORWARD
//! for traffic entering from the app bridge. A URL-validation bug in any
//! current or future feature then becomes a refused connection instead of
//! a credential leak. This mirrors the (broader) `TEMPS_SANDBOX_EGRESS`
//! filter in `temps-agents`; unlike that filter it deliberately does NOT
//! touch RFC-1918 ranges — app containers legitimately talk to linked
//! services on private addresses.
//!
//! The block is **best-effort**, matching the sandbox filter's semantics:
//! on non-Linux hosts (macOS Docker Desktop development) or when iptables
//! is unavailable (rootless Docker, missing CAP_NET_ADMIN) a WARN is
//! logged and deployment continues. Production installs run on Linux
//! where the rules apply.

use bollard::Docker;
use tracing::{info, warn};

const CHAIN: &str = "TEMPS_METADATA_BLOCK";

/// Metadata endpoints blocked for app containers. REJECT (not DROP) so a
/// misconfigured app gets an immediate "connection refused" it can log,
/// instead of a mystery timeout.
const METADATA_ADDRS: &[&str] = &[
    // AWS, GCP, Azure, Hetzner, DigitalOcean, OpenStack, ...
    "169.254.169.254/32",
    // Alibaba Cloud
    "100.100.100.200/32",
];

/// Ensure the metadata-block chain exists and is hooked into FORWARD for
/// the given app network. Idempotent; called from `ensure_network_exists`
/// on every deploy so the rules survive firewall flushes between deploys.
///
/// Never fails the deploy: every problem is logged as a WARN and the
/// function returns, exactly like `apply_sandbox_egress_filter`.
pub async fn apply_metadata_egress_block(docker: &Docker, network_name: &str) {
    // Linux-only: on macOS Docker Desktop there is no host iptables, and
    // the bridge lives inside the Desktop VM anyway.
    if !cfg!(target_os = "linux") {
        return;
    }

    let Some(bridge_name) = resolve_bridge_name(docker, network_name).await else {
        return;
    };

    let cmds = build_iptables_commands(&bridge_name);
    for cmd in &cmds {
        run_iptables(cmd).await;
    }

    info!(
        network = network_name,
        bridge_interface = %bridge_name,
        blocked = ?METADATA_ADDRS,
        "Cloud-metadata egress block applied to app bridge"
    );
}

/// Resolve the host bridge interface backing a Docker network. Custom
/// bridge networks store it under the `com.docker.network.bridge.name`
/// option; absent that, Docker's bridge driver names it `br-<id[..12]>`.
async fn resolve_bridge_name(docker: &Docker, network_name: &str) -> Option<String> {
    match docker
        .inspect_network(
            network_name,
            None::<bollard::query_parameters::InspectNetworkOptions>,
        )
        .await
    {
        Ok(inspect) => {
            let from_option = inspect
                .options
                .as_ref()
                .and_then(|o| o.get("com.docker.network.bridge.name"))
                .cloned();
            let id = inspect.id.unwrap_or_default();
            let fallback = if id.len() >= 12 {
                Some(format!("br-{}", &id[..12]))
            } else {
                None
            };
            let resolved = from_option.or(fallback);
            if resolved.is_none() {
                warn!(
                    network = network_name,
                    "App network has no bridge name option and no usable id; \
                     cloud-metadata egress block not applied. Containers on this \
                     network can reach the cloud metadata endpoint."
                );
            }
            resolved
        }
        Err(e) => {
            warn!(
                network = network_name,
                error = %e,
                "Could not inspect app network to resolve bridge interface; \
                 cloud-metadata egress block not applied. Containers on this \
                 network can reach the cloud metadata endpoint."
            );
            None
        }
    }
}

/// Ordered iptables invocations that install the chain idempotently:
/// create (exists is benign) → flush → per-endpoint REJECT → RETURN →
/// delete FORWARD hook (absent is benign) → insert FORWARD hook.
fn build_iptables_commands(bridge_name: &str) -> Vec<Vec<String>> {
    let mut cmds: Vec<Vec<String>> = Vec::new();
    let s = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();

    cmds.push(s(&["-N", CHAIN]));
    cmds.push(s(&["-F", CHAIN]));
    for addr in METADATA_ADDRS {
        cmds.push(s(&[
            "-A",
            CHAIN,
            "-d",
            addr,
            "-j",
            "REJECT",
            "--reject-with",
            "icmp-port-unreachable",
        ]));
    }
    cmds.push(s(&["-A", CHAIN, "-j", "RETURN"]));
    // Delete-then-insert guarantees exactly one FORWARD hook entry.
    cmds.push(s(&["-D", "FORWARD", "-i", bridge_name, "-j", CHAIN]));
    cmds.push(s(&["-I", "FORWARD", "1", "-i", bridge_name, "-j", CHAIN]));
    cmds
}

/// Run one iptables invocation, downgrading expected failures to silence
/// and everything else to a WARN (same policy as the sandbox filter).
async fn run_iptables(args: &[String]) {
    let status = match tokio::process::Command::new("iptables")
        .args(args)
        .status()
        .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(
                command = format!("iptables {}", args.join(" ")),
                error = %e,
                "iptables could not be executed; cloud-metadata egress block may \
                 be incomplete. Ensure iptables is installed and the temps server \
                 has CAP_NET_ADMIN."
            );
            return;
        }
    };
    if status.success() {
        return;
    }
    let code = status.code().unwrap_or(-1);
    let benign_chain_exists = code == 1 && args.first().map(String::as_str) == Some("-N");
    let benign_missing_hook = args.first().map(String::as_str) == Some("-D");
    let benign_lock_contention = code == 4;
    if !(benign_chain_exists || benign_missing_hook || benign_lock_contention) {
        warn!(
            command = format!("iptables {}", args.join(" ")),
            exit_code = code,
            "iptables command failed; cloud-metadata egress block may be \
             incomplete. If this is a permission error, ensure the temps server \
             process has CAP_NET_ADMIN."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_install_chain_flush_and_hook_in_order() {
        let cmds = build_iptables_commands("br-abc123def456");
        let flat: Vec<String> = cmds.iter().map(|c| c.join(" ")).collect();

        assert_eq!(flat[0], format!("-N {}", CHAIN));
        assert_eq!(flat[1], format!("-F {}", CHAIN));
        // One REJECT per metadata endpoint, all before the RETURN.
        let return_idx = flat
            .iter()
            .position(|c| c == &format!("-A {} -j RETURN", CHAIN))
            .expect("RETURN rule present");
        for addr in METADATA_ADDRS {
            let idx = flat
                .iter()
                .position(|c| c.contains(addr) && c.contains("REJECT"))
                .unwrap_or_else(|| panic!("REJECT rule for {} present", addr));
            assert!(idx < return_idx, "{} must be rejected before RETURN", addr);
        }
        // Hook is delete-then-insert on the given bridge, last two commands.
        assert_eq!(
            flat[flat.len() - 2],
            format!("-D FORWARD -i br-abc123def456 -j {}", CHAIN)
        );
        assert_eq!(
            flat[flat.len() - 1],
            format!("-I FORWARD 1 -i br-abc123def456 -j {}", CHAIN)
        );
    }

    #[test]
    fn blocks_only_metadata_endpoints_never_private_ranges() {
        // App containers must keep reaching RFC-1918 (linked databases,
        // internal services). Only single-host metadata endpoints may
        // appear in the block list.
        for addr in METADATA_ADDRS {
            assert!(
                addr.ends_with("/32"),
                "{} must be a single host, not a range",
                addr
            );
            assert!(
                !addr.starts_with("10.")
                    && !addr.starts_with("172.")
                    && !addr.starts_with("192.168."),
                "{} must not cover RFC-1918 space",
                addr
            );
        }
    }
}
