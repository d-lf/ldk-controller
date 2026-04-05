use ldk_node::bitcoin::Network;
use ldk_node::bitcoin::FeeRate;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use ldk_node::payment::{ConfirmationStatus, PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::{Builder, Node};
use serde::Serialize;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

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
    InvalidInvoice(String),
    InvalidInvoiceRequest(String),
    InvalidPubkey(String),
    InvalidAmount(u64),
    ChannelFailed(String),
    PeerFailed(String),
    PaymentFailed(String),
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
            Self::InvalidInvoice(msg) => write!(f, "invalid invoice: {msg}"),
            Self::InvalidInvoiceRequest(msg) => write!(f, "invalid invoice request: {msg}"),
            Self::InvalidPubkey(msg) => write!(f, "invalid pubkey: {msg}"),
            Self::InvalidAmount(amount) => write!(f, "invalid amount: {amount}"),
            Self::ChannelFailed(msg) => write!(f, "channel operation failed: {msg}"),
            Self::PeerFailed(msg) => write!(f, "peer operation failed: {msg}"),
            Self::PaymentFailed(msg) => write!(f, "payment failed: {msg}"),
            Self::StopFailed(msg) => write!(f, "ldk node stop failed: {msg}"),
        }
    }
}

impl std::error::Error for LdkServiceError {}

pub struct LdkService {
    node: Arc<Node>,
    network: Network,
}

pub struct LdkPaymentResult {
    pub preimage: String,
    pub fees_paid_msat: Option<u64>,
}

pub struct LdkInvoiceResult {
    pub invoice: String,
    pub payment_hash: Option<String>,
    pub amount_msat: Option<u64>,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LdkChannelInfo {
    pub channel_id: String,
    pub counterparty_pubkey: String,
    pub is_channel_ready: bool,
    pub is_usable: bool,
    pub is_announced: bool,
    pub is_outbound: bool,
    pub capacity: u64,
    pub local_balance: u64,
    pub remote_balance: u64,
    pub base_fee_msat: u32,
    pub fee_rate_ppm: u32,
    pub cltv_expiry_delta: u16,
    pub short_channel_id: Option<u64>,
    pub confirmations: Option<u32>,
    pub inbound_htlc_minimum_msat: u64,
    pub inbound_htlc_maximum_msat: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LdkPeerInfo {
    pub node_id: String,
    pub address: String,
    pub is_persisted: bool,
    pub is_connected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceInfo {
    pub onchain_balance_sat: u64,
    pub channel_balance_sat: u64,
    pub pending_balance_sat: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnchainTxInfo {
    pub txid: String,
    pub tx_type: String,
    pub amount_sat: u64,
    pub fee_sat: Option<u64>,
    pub confirmed: bool,
    pub block_height: Option<u32>,
    pub block_time: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedInvoiceInfo {
    pub amount: u64,
    pub description: String,
    pub destination: String,
    pub payment_hash: String,
    pub expiry: u64,
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

    pub fn get_detailed_balance(&self) -> BalanceInfo {
        let b = self.node.list_balances();
        BalanceInfo {
            onchain_balance_sat: b.spendable_onchain_balance_sats,
            channel_balance_sat: b.total_lightning_balance_sats,
            pending_balance_sat: b.total_onchain_balance_sats.saturating_sub(b.spendable_onchain_balance_sats),
        }
    }

    pub fn new_onchain_address(&self) -> Result<String, LdkServiceError> {
        self.node
            .onchain_payment()
            .new_address()
            .map(|a| a.to_string())
            .map_err(|e| LdkServiceError::AddressGenerationFailed(e.to_string()))
    }

    pub fn make_invoice(
        &self,
        amount_msat: u64,
        description: Option<&str>,
        description_hash: Option<&str>,
        expiry_secs: Option<u64>,
    ) -> Result<LdkInvoiceResult, LdkServiceError> {
        if amount_msat == 0 {
            return Err(LdkServiceError::InvalidAmount(amount_msat));
        }
        if description_hash.is_some() {
            return Err(LdkServiceError::InvalidInvoiceRequest(
                "description_hash is not supported yet".to_string(),
            ));
        }

        let description_value = description.unwrap_or("nwc invoice").to_string();
        let desc = Description::new(description_value)
            .map_err(|e| LdkServiceError::InvalidInvoiceRequest(e.to_string()))?;
        let invoice_desc = Bolt11InvoiceDescription::Direct(desc);
        let expiry_u32 = expiry_secs
            .map(u32::try_from)
            .transpose()
            .map_err(|_| {
                LdkServiceError::InvalidInvoiceRequest("expiry exceeds u32::MAX".to_string())
            })?
            .unwrap_or(3600);

        let invoice = self
            .node
            .bolt11_payment()
            .receive(amount_msat, &invoice_desc, expiry_u32)
            .map_err(|e| LdkServiceError::InvalidInvoiceRequest(e.to_string()))?;

        let payment_hash = Some(invoice.payment_hash().to_string());
        let expires_at = invoice.expires_at().map(|ts| ts.as_secs());

        Ok(LdkInvoiceResult {
            invoice: invoice.to_string(),
            payment_hash,
            amount_msat: invoice.amount_milli_satoshis(),
            expires_at,
        })
    }

    pub fn pay_invoice(
        &self,
        invoice_str: &str,
        amount_msat: Option<u64>,
    ) -> Result<LdkPaymentResult, LdkServiceError> {
        let invoice = Bolt11Invoice::from_str(invoice_str)
            .map_err(|e| LdkServiceError::InvalidInvoice(e.to_string()))?;
        let payment_id = if let Some(amount) = amount_msat {
            self.node
                .bolt11_payment()
                .send_using_amount(&invoice, amount, None)
                .map_err(|e| LdkServiceError::PaymentFailed(e.to_string()))?
        } else {
            self.node
                .bolt11_payment()
                .send(&invoice, None)
                .map_err(|e| LdkServiceError::PaymentFailed(e.to_string()))?
        };

        self.wait_for_outbound_payment(payment_id)
    }

    pub fn pay_keysend(
        &self,
        dest_pubkey: &str,
        amount_msat: u64,
    ) -> Result<LdkPaymentResult, LdkServiceError> {
        if amount_msat == 0 {
            return Err(LdkServiceError::InvalidAmount(amount_msat));
        }
        let node_id = PublicKey::from_str(dest_pubkey)
            .map_err(|e| LdkServiceError::InvalidPubkey(e.to_string()))?;
        let payment_id = self
            .node
            .spontaneous_payment()
            .send(amount_msat, node_id, None)
            .map_err(|e| LdkServiceError::PaymentFailed(e.to_string()))?;

        self.wait_for_outbound_payment(payment_id)
    }

    pub fn open_channel(
        &self,
        counterparty_pubkey: &str,
        counterparty_addr: &str,
        channel_amount_sats: u64,
        push_to_counterparty_msat: Option<u64>,
    ) -> Result<(), LdkServiceError> {
        let node_id = PublicKey::from_str(counterparty_pubkey)
            .map_err(|e| LdkServiceError::InvalidPubkey(e.to_string()))?;
        let addr = SocketAddress::from_str(counterparty_addr)
            .map_err(|e| LdkServiceError::ChannelFailed(e.to_string()))?;
        self.node
            .open_channel(
                node_id,
                addr,
                channel_amount_sats,
                push_to_counterparty_msat,
                None,
            )
            .map_err(|e| LdkServiceError::ChannelFailed(e.to_string()))?;
        Ok(())
    }

    pub fn connect_peer(
        &self,
        counterparty_pubkey: &str,
        counterparty_addr: &str,
    ) -> Result<(), LdkServiceError> {
        let node_id = PublicKey::from_str(counterparty_pubkey)
            .map_err(|e| LdkServiceError::InvalidPubkey(e.to_string()))?;
        let addr = SocketAddress::from_str(counterparty_addr)
            .map_err(|e| LdkServiceError::PeerFailed(e.to_string()))?;
        self.node
            .connect(node_id, addr, true)
            .map_err(|e| LdkServiceError::PeerFailed(e.to_string()))?;
        Ok(())
    }

    pub fn disconnect_peer(&self, counterparty_pubkey: &str) -> Result<(), LdkServiceError> {
        let node_id = PublicKey::from_str(counterparty_pubkey)
            .map_err(|e| LdkServiceError::InvalidPubkey(e.to_string()))?;
        self.node
            .disconnect(node_id)
            .map_err(|e| LdkServiceError::PeerFailed(e.to_string()))?;
        Ok(())
    }

    pub fn update_channel_fees(
        &self,
        channel_id: &str,
        base_fee_msat: Option<u32>,
        fee_rate_ppm: Option<u32>,
    ) -> Result<(), LdkServiceError> {
        let details = self
            .node
            .list_channels()
            .into_iter()
            .find(|c| c.channel_id.to_string() == channel_id)
            .ok_or_else(|| {
                LdkServiceError::ChannelFailed(format!("channel not found: {channel_id}"))
            })?;

        let mut config = details.config.clone();
        if let Some(base) = base_fee_msat {
            config.forwarding_fee_base_msat = base;
        }
        if let Some(rate) = fee_rate_ppm {
            config.forwarding_fee_proportional_millionths = rate;
        }

        self.node
            .update_channel_config(
                &details.user_channel_id,
                details.counterparty_node_id,
                config,
            )
            .map_err(|e| LdkServiceError::ChannelFailed(e.to_string()))
    }

    pub fn send_to_address(
        &self,
        address: &str,
        amount_sats: u64,
        fee_rate_sat_per_vbyte: Option<u64>,
    ) -> Result<String, LdkServiceError> {
        let addr = address
            .parse::<ldk_node::bitcoin::Address<_>>()
            .map_err(|e| LdkServiceError::PaymentFailed(format!("invalid address: {e}")))?
            .assume_checked();

        let fee_rate = fee_rate_sat_per_vbyte.map(|r| FeeRate::from_sat_per_kwu(r * 250));

        let txid = self
            .node
            .onchain_payment()
            .send_to_address(&addr, amount_sats, fee_rate)
            .map_err(|e| LdkServiceError::PaymentFailed(format!("send_to_address failed: {e}")))?;

        Ok(txid.to_string())
    }

    pub fn decode_invoice_str(&self, invoice_str: &str) -> Result<DecodedInvoiceInfo, LdkServiceError> {
        let invoice = Bolt11Invoice::from_str(invoice_str)
            .map_err(|e| LdkServiceError::InvalidInvoice(e.to_string()))?;

        let amount = invoice.amount_milli_satoshis().unwrap_or(0);
        let description = invoice.description().to_string();
        let destination = invoice.payee_pub_key().map(|k| k.to_string())
            .unwrap_or_else(|| invoice.recover_payee_pub_key().to_string());
        let payment_hash = invoice.payment_hash().to_string();
        let expiry = invoice.expiry_time().as_secs();

        Ok(DecodedInvoiceInfo {
            amount,
            description,
            destination,
            payment_hash,
            expiry,
        })
    }

    pub fn list_onchain_transactions(&self) -> Vec<OnchainTxInfo> {
        self.node
            .list_payments()
            .into_iter()
            .filter_map(|p| {
                if let PaymentKind::Onchain { txid, status } = &p.kind {
                    let (confirmed, block_height, block_time) = match status {
                        ConfirmationStatus::Confirmed { height, timestamp, .. } => {
                            (true, Some(*height), Some(*timestamp))
                        }
                        ConfirmationStatus::Unconfirmed => (false, None, None),
                    };
                    let tx_type = match p.direction {
                        PaymentDirection::Outbound => "send",
                        PaymentDirection::Inbound => "receive",
                    };
                    Some(OnchainTxInfo {
                        txid: txid.to_string(),
                        tx_type: tx_type.to_string(),
                        amount_sat: p.amount_msat.unwrap_or(0) / 1000,
                        fee_sat: p.fee_paid_msat.map(|f| f / 1000),
                        confirmed,
                        block_height,
                        block_time,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn next_event_async(&self) -> ldk_node::Event {
        self.node.next_event_async().await
    }

    pub fn event_handled(&self) -> Result<(), LdkServiceError> {
        self.node
            .event_handled()
            .map_err(|e| LdkServiceError::StopFailed(e.to_string()))
    }

    pub fn stop(&self) -> Result<(), LdkServiceError> {
        self.node
            .stop()
            .map_err(|e| LdkServiceError::StopFailed(e.to_string()))
    }

    pub fn has_ready_channel_with(&self, counterparty_pubkey: &str) -> bool {
        let Ok(counterparty) = PublicKey::from_str(counterparty_pubkey) else {
            return false;
        };
        self.node
            .list_channels()
            .iter()
            .any(|c| c.counterparty_node_id == counterparty && c.is_channel_ready)
    }

    pub fn has_channel_with(&self, counterparty_pubkey: &str) -> bool {
        let Ok(counterparty) = PublicKey::from_str(counterparty_pubkey) else {
            return false;
        };
        self.node
            .list_channels()
            .iter()
            .any(|c| c.counterparty_node_id == counterparty)
    }

    fn channel_to_info(channel: &ldk_node::ChannelDetails) -> LdkChannelInfo {
        let local_balance = channel.outbound_capacity_msat / 1000;
        let remote_balance = channel.inbound_capacity_msat / 1000;
        LdkChannelInfo {
            channel_id: channel.channel_id.to_string(),
            counterparty_pubkey: channel.counterparty_node_id.to_string(),
            is_channel_ready: channel.is_channel_ready,
            is_usable: channel.is_usable,
            is_announced: channel.is_announced,
            is_outbound: channel.is_outbound,
            capacity: channel.channel_value_sats,
            local_balance,
            remote_balance,
            base_fee_msat: channel.config.forwarding_fee_base_msat,
            fee_rate_ppm: channel.config.forwarding_fee_proportional_millionths,
            cltv_expiry_delta: channel.config.cltv_expiry_delta,
            short_channel_id: channel.short_channel_id,
            confirmations: channel.confirmations,
            inbound_htlc_minimum_msat: channel.inbound_htlc_minimum_msat,
            inbound_htlc_maximum_msat: channel.inbound_htlc_maximum_msat,
        }
    }

    pub fn list_channels(&self) -> Vec<LdkChannelInfo> {
        self.node
            .list_channels()
            .iter()
            .map(Self::channel_to_info)
            .collect()
    }

    pub fn get_channel(&self, channel_id: &str) -> Option<LdkChannelInfo> {
        self.node
            .list_channels()
            .iter()
            .find(|channel| channel.channel_id.to_string() == channel_id)
            .map(Self::channel_to_info)
    }

    pub fn close_channel(&self, channel_id: &str, force: bool) -> Result<(), LdkServiceError> {
        let details = self
            .node
            .list_channels()
            .into_iter()
            .find(|channel| channel.channel_id.to_string() == channel_id)
            .ok_or_else(|| {
                LdkServiceError::ChannelFailed(format!("channel not found: {channel_id}"))
            })?;

        if force {
            self.node
                .force_close_channel(
                    &details.user_channel_id,
                    details.counterparty_node_id,
                    Some("closed via control API".to_string()),
                )
                .map_err(|e| LdkServiceError::ChannelFailed(e.to_string()))?;
        } else {
            self.node
                .close_channel(&details.user_channel_id, details.counterparty_node_id)
                .map_err(|e| LdkServiceError::ChannelFailed(e.to_string()))?;
        }
        Ok(())
    }

    pub fn list_peers(&self) -> Vec<LdkPeerInfo> {
        self.node
            .list_peers()
            .iter()
            .map(|peer| LdkPeerInfo {
                node_id: peer.node_id.to_string(),
                address: peer.address.to_string(),
                is_persisted: peer.is_persisted,
                is_connected: peer.is_connected,
            })
            .collect()
    }

    fn wait_for_outbound_payment(
        &self,
        payment_id: PaymentId,
    ) -> Result<LdkPaymentResult, LdkServiceError> {
        let timeout = Duration::from_secs(60);
        let start = std::time::Instant::now();
        loop {
            if let Some(payment) = self
                .node
                .list_payments()
                .into_iter()
                .find(|p| p.id == payment_id && p.direction == PaymentDirection::Outbound)
            {
                match payment.status {
                    PaymentStatus::Succeeded => {
                        let preimage = match payment.kind {
                            PaymentKind::Bolt11 { preimage, .. } => preimage,
                            PaymentKind::Bolt11Jit { preimage, .. } => preimage,
                            PaymentKind::Spontaneous { preimage, .. } => preimage,
                            _ => None,
                        }
                        .ok_or_else(|| {
                            LdkServiceError::PaymentFailed(
                                "payment succeeded but preimage missing".to_string(),
                            )
                        })?;

                        return Ok(LdkPaymentResult {
                            preimage: hex_string(&preimage.0),
                            fees_paid_msat: payment.fee_paid_msat,
                        });
                    }
                    PaymentStatus::Failed => {
                        return Err(LdkServiceError::PaymentFailed(
                            "payment marked failed".to_string(),
                        ));
                    }
                    PaymentStatus::Pending => {}
                }
            }

            if start.elapsed() > timeout {
                return Err(LdkServiceError::PaymentFailed(
                    "timeout waiting for payment outcome".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn hex_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}
