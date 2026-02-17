use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Node};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct LdkServiceConfig {
    pub network: String,
    pub bitcoind_rpc_host: String,
    pub bitcoind_rpc_port: u16,
    pub bitcoind_rpc_user: String,
    pub bitcoind_rpc_password: String,
    pub ldk_storage_dir: String,
    pub ldk_listen_addr: Option<String>,
}

impl LdkServiceConfig {
    fn parse_network(&self) -> Result<Network, LdkServiceInitError> {
        match self.network.to_lowercase().as_str() {
            "regtest" => Ok(Network::Regtest),
            "testnet" => Ok(Network::Testnet),
            "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
            "signet" => Ok(Network::Signet),
            other => Err(LdkServiceInitError::InvalidNetwork {
                network: other.to_string(),
            }),
        }
    }

    fn validate(&self) -> Result<(), LdkServiceInitError> {
        if self.bitcoind_rpc_host.trim().is_empty() {
            return Err(LdkServiceInitError::InvalidConfig(
                "bitcoind_rpc_host must not be empty".to_string(),
            ));
        }
        if self.bitcoind_rpc_user.trim().is_empty() {
            return Err(LdkServiceInitError::InvalidConfig(
                "bitcoind_rpc_user must not be empty".to_string(),
            ));
        }
        if self.bitcoind_rpc_password.trim().is_empty() {
            return Err(LdkServiceInitError::InvalidConfig(
                "bitcoind_rpc_password must not be empty".to_string(),
            ));
        }
        if self.ldk_storage_dir.trim().is_empty() {
            return Err(LdkServiceInitError::InvalidConfig(
                "ldk_storage_dir must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum LdkServiceInitError {
    InvalidNetwork { network: String },
    InvalidListeningAddress { address: String },
    InvalidConfig(String),
    BuildFailed(String),
    StartFailed(String),
}

impl fmt::Display for LdkServiceInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNetwork { network } => {
                write!(f, "unsupported network for LdkService: {network}")
            }
            Self::InvalidListeningAddress { address } => {
                write!(f, "invalid ldk_listen_addr: {address}")
            }
            Self::InvalidConfig(msg) => write!(f, "invalid LdkService config: {msg}"),
            Self::BuildFailed(msg) => write!(f, "failed to build LdkService node: {msg}"),
            Self::StartFailed(msg) => write!(f, "failed to start LdkService node: {msg}"),
        }
    }
}

impl std::error::Error for LdkServiceInitError {}

#[derive(Debug)]
pub enum LdkServiceError {
    SyncFailed(String),
    AddressGenerationFailed(String),
    BalanceOverflow { sats: u64 },
    StopFailed(String),
}

impl fmt::Display for LdkServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SyncFailed(msg) => write!(f, "ldk wallet sync failed: {msg}"),
            Self::AddressGenerationFailed(msg) => {
                write!(f, "ldk address generation failed: {msg}")
            }
            Self::BalanceOverflow { sats } => {
                write!(f, "balance conversion overflow for sats={sats}")
            }
            Self::StopFailed(msg) => write!(f, "ldk node stop failed: {msg}"),
        }
    }
}

impl std::error::Error for LdkServiceError {}

pub struct LdkService {
    node: Arc<Node>,
    network: Network,
}

impl LdkService {
    pub fn start_from_config(cfg: &LdkServiceConfig) -> Result<Arc<Self>, LdkServiceInitError> {
        cfg.validate()?;
        let network = cfg.parse_network()?;

        let mut builder = Builder::new();
        builder.set_network(network);
        builder.set_chain_source_bitcoind_rpc(
            cfg.bitcoind_rpc_host.clone(),
            cfg.bitcoind_rpc_port,
            cfg.bitcoind_rpc_user.clone(),
            cfg.bitcoind_rpc_password.clone(),
        );
        builder.set_storage_dir_path(cfg.ldk_storage_dir.clone());

        if let Some(listen_addr) = &cfg.ldk_listen_addr {
            let socket = SocketAddress::from_str(listen_addr).map_err(|_| {
                LdkServiceInitError::InvalidListeningAddress {
                    address: listen_addr.clone(),
                }
            })?;
            builder
                .set_listening_addresses(vec![socket])
                .map_err(|e| LdkServiceInitError::BuildFailed(e.to_string()))?;
        }

        let node = builder
            .build()
            .map_err(|e| LdkServiceInitError::BuildFailed(e.to_string()))?;
        node.start()
            .map_err(|e| LdkServiceInitError::StartFailed(e.to_string()))?;

        Ok(Arc::new(Self {
            node: Arc::new(node),
            network,
        }))
    }

    pub fn node_id(&self) -> String {
        self.node.node_id().to_string()
    }

    pub fn network(&self) -> &'static str {
        match self.network {
            Network::Regtest => "regtest",
            Network::Testnet => "testnet",
            Network::Bitcoin => "bitcoin",
            Network::Signet => "signet",
            _ => "unknown",
        }
    }

    pub fn sync_wallets(&self) -> Result<(), LdkServiceError> {
        self.node
            .sync_wallets()
            .map_err(|e| LdkServiceError::SyncFailed(e.to_string()))
    }

    pub fn get_balance_msat(&self) -> Result<u64, LdkServiceError> {
        let sats = self.node.list_balances().spendable_onchain_balance_sats;
        sats.checked_mul(1000)
            .ok_or(LdkServiceError::BalanceOverflow { sats })
    }

    pub fn new_onchain_address(&self) -> Result<String, LdkServiceError> {
        self.node
            .onchain_payment()
            .new_address()
            .map(|a| a.to_string())
            .map_err(|e| LdkServiceError::AddressGenerationFailed(e.to_string()))
    }

    pub fn stop(&self) -> Result<(), LdkServiceError> {
        self.node
            .stop()
            .map_err(|e| LdkServiceError::StopFailed(e.to_string()))
    }
}
