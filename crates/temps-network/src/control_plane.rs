//! Control-plane participation in the multi-host overlay.
//!
//! The control plane is deliberately not a schedulable `nodes` row. Its
//! allocation lives in `network_config` and this module reconciles the same
//! kernel/Docker primitives workers use. Both server startup and the operator
//! CLI call the same idempotent entry point.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bollard::Docker;
use sea_orm::{DatabaseConnection, EntityTrait};
use temps_entities::network_config;
use thiserror::Error;
use tracing::{info, warn};

use crate::allocator::{AllocatorError, PostgresAllocator};
use crate::{NetworkConfig, NetworkError, NetworkManager, NodeAlloc, Peer, Transport};

const PEER_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum ControlPlaneSetupError {
    #[error("control-plane underlay address {value:?} is invalid: {reason}")]
    InvalidUnderlayAddress { value: String, reason: String },
    #[error("VXLAN requires a private underlay address; {address} is publicly routable")]
    PublicUnderlayAddress { address: IpAddr },
    #[error("control-plane overlay allocation failed: {0}")]
    Allocation(#[from] AllocatorError),
    #[error("control-plane overlay network failed: {0}")]
    Network(#[from] NetworkError),
    #[error("network_config singleton row is missing")]
    MissingNetworkConfig,
    #[error("network_config transport {value:?} is unsupported")]
    InvalidTransport { value: String },
    #[error("network_config contains an invalid VXLAN value: {reason}")]
    InvalidVxlanConfig { reason: String },
    #[error("database error while loading network_config: {0}")]
    Database(#[from] sea_orm::DbErr),
}

#[derive(Clone)]
pub struct ControlPlaneOverlay {
    pub alloc: NodeAlloc,
    pub config: NetworkConfig,
    manager: NetworkManager,
}

impl ControlPlaneOverlay {
    pub fn spawn_peer_reconciler(&self, db: Arc<DatabaseConnection>) {
        let manager = self.manager.clone();
        tokio::spawn(async move {
            let allocator = PostgresAllocator::new(db);
            loop {
                match allocator.control_plane_peer_list().await {
                    Ok(peers) => {
                        if let Some(peer) = peers.iter().find(|peer| {
                            !crate::allocator::is_private_underlay(peer.underlay_address)
                        }) {
                            warn!(
                                node_id = %peer.node_id,
                                underlay = %peer.underlay_address,
                                "refusing publicly-routable control-plane overlay peer"
                            );
                            tokio::time::sleep(PEER_RECONCILE_INTERVAL).await;
                            continue;
                        }
                        if let Err(error) = manager.reconcile_peers(peers).await {
                            warn!(error = %error, "control-plane overlay peer reconciliation failed");
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, "could not load control-plane overlay peers")
                    }
                }
                tokio::time::sleep(PEER_RECONCILE_INTERVAL).await;
            }
        });
    }
}

pub async fn setup(
    db: Arc<DatabaseConnection>,
    docker: &Docker,
    underlay_address: &str,
    underlay_device: Option<&str>,
) -> Result<ControlPlaneOverlay, ControlPlaneSetupError> {
    let underlay_address: IpAddr =
        underlay_address
            .parse()
            .map_err(|error: std::net::AddrParseError| {
                ControlPlaneSetupError::InvalidUnderlayAddress {
                    value: underlay_address.to_owned(),
                    reason: error.to_string(),
                }
            })?;
    if !crate::allocator::is_private_underlay(underlay_address) {
        return Err(ControlPlaneSetupError::PublicUnderlayAddress {
            address: underlay_address,
        });
    }
    let allocator = PostgresAllocator::new(db.clone());
    let alloc: NodeAlloc = allocator
        .ensure_control_plane_alloc(underlay_address)
        .await?
        .into();
    let peers = allocator.control_plane_peer_list().await?;
    let persisted = network_config::Entity::find_by_id(1)
        .one(db.as_ref())
        .await?
        .ok_or(ControlPlaneSetupError::MissingNetworkConfig)?;

    let underlay_dev = match underlay_device
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => value.to_owned(),
        None => crate::detect_device_for_address(underlay_address).await?,
    };
    let detected_mtu = crate::detect_underlay_mtu(&underlay_dev).await?;
    let configured_mtu = u32::try_from(persisted.underlay_mtu).map_err(|_| {
        ControlPlaneSetupError::InvalidVxlanConfig {
            reason: format!("underlay_mtu {} is negative", persisted.underlay_mtu),
        }
    })?;
    let transport = match persisted.transport.as_str() {
        "vxlan" => Transport::Vxlan {
            vni: u32::try_from(persisted.vxlan_vni).map_err(|_| {
                ControlPlaneSetupError::InvalidVxlanConfig {
                    reason: format!("vxlan_vni {} is negative", persisted.vxlan_vni),
                }
            })?,
            port: u16::try_from(persisted.vxlan_port).map_err(|_| {
                ControlPlaneSetupError::InvalidVxlanConfig {
                    reason: format!("vxlan_port {} is outside 0..=65535", persisted.vxlan_port),
                }
            })?,
        },
        "native" => Transport::Native,
        value => {
            return Err(ControlPlaneSetupError::InvalidTransport {
                value: value.into(),
            })
        }
    };
    if matches!(transport, Transport::Vxlan { .. }) {
        if let Some(peer) = peers
            .iter()
            .find(|peer| !crate::allocator::is_private_underlay(peer.underlay_address))
        {
            return Err(ControlPlaneSetupError::PublicUnderlayAddress {
                address: peer.underlay_address,
            });
        }
    }
    let config = NetworkConfig {
        transport,
        underlay_mtu: detected_mtu.min(configured_mtu),
        underlay_dev,
        ..NetworkConfig::default()
    };
    let manager = NetworkManager::new(config.clone())?;
    if let Err(error) = manager.bootstrap(alloc.clone(), peers.clone()).await {
        allocator.set_control_plane_ready(false).await?;
        return Err(error.into());
    }
    if let Err(error) = crate::docker::ensure_network(docker, &config, &alloc).await {
        allocator.set_control_plane_ready(false).await?;
        return Err(error.into());
    }
    allocator.set_control_plane_ready(true).await?;
    info!(
        cidr = %alloc.compute_cidr,
        bridge = %alloc.bridge_address,
        underlay = %alloc.underlay_address,
        peers = peers.len(),
        "control-plane overlay is ready"
    );
    Ok(ControlPlaneOverlay {
        alloc,
        config,
        manager,
    })
}

pub async fn current_peers(
    db: Arc<DatabaseConnection>,
) -> Result<Vec<Peer>, ControlPlaneSetupError> {
    Ok(PostgresAllocator::new(db).control_plane_peer_list().await?)
}
