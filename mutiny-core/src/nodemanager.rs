use anyhow::anyhow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::HashMap, ops::Deref, sync::Arc};

use crate::event::{HTLCStatus, PaymentInfo};
use crate::gossip::*;
use crate::labels::LabelStorage;
use crate::logging::LOGGING_KEY;
use crate::redshift::{RedshiftManager, RedshiftStatus, RedshiftStorage};
use crate::storage::{MutinyStorage, KEYCHAIN_STORE_KEY};
use crate::utils::sleep;
use crate::{
    auth::{AuthManager, AuthProfile},
    MutinyWalletConfig,
};
use crate::{
    chain::MutinyChain,
    error::MutinyError,
    esplora::EsploraSyncClient,
    fees::MutinyFeeEstimator,
    gossip, keymanager,
    logging::MutinyLogger,
    lspclient::LspClient,
    node::{Node, ProbScorer, PubkeyConnectionInfo, RapidGossipSync},
    onchain::get_esplora_url,
    onchain::OnChainWallet,
    utils,
};
use bdk::chain::{BlockId, ConfirmationTime};
use bdk::{wallet::AddressIndex, LocalUtxo};
use bdk_esplora::esplora_client::AsyncClient;
use bip39::Mnemonic;
use bitcoin::blockdata::script;
use bitcoin::hashes::hex::{FromHex, ToHex};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{rand, PublicKey};
use bitcoin::util::bip32::ExtendedPrivKey;
use bitcoin::{Address, Network, OutPoint, Transaction, Txid};
use core::time::Duration;
use futures::{future::join_all, lock::Mutex};
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use lightning::chain::channelmonitor::Balance;
use lightning::chain::keysinterface::{NodeSigner, Recipient};
use lightning::chain::Confirm;
use lightning::events::ClosureReason;
use lightning::ln::channelmanager::{ChannelDetails, PhantomRouteHints};
use lightning::ln::PaymentHash;
use lightning::routing::gossip::NodeId;
use lightning::util::logger::*;
use lightning::{log_debug, log_error, log_info, log_warn};
use lightning_invoice::{Invoice, InvoiceDescription};
use lnurl::lnurl::LnUrl;
use lnurl::{AsyncClient as LnUrlClient, LnUrlResponse, Response};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

const BITCOIN_PRICE_CACHE_SEC: u64 = 300;

// This is the NodeStorage object saved to the DB
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct NodeStorage {
    pub nodes: HashMap<String, NodeIndex>,
}

// This is the NodeIndex reference that is saved to the DB
#[derive(Serialize, Deserialize, Clone)]
pub struct NodeIndex {
    pub child_index: u32,
    pub lsp: Option<String>,
    pub archived: Option<bool>,
}

impl NodeIndex {
    pub fn is_archived(&self) -> bool {
        self.archived.unwrap_or(false)
    }
}

// This is the NodeIdentity that refer to a specific node
// Used for public facing identification.
pub struct NodeIdentity {
    pub uuid: String,
    pub pubkey: PublicKey,
}

#[derive(Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct MutinyBip21RawMaterials {
    pub address: Address,
    pub invoice: Invoice,
    pub btc_amount: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct MutinyInvoice {
    pub bolt11: Option<Invoice>,
    pub description: Option<String>,
    pub payment_hash: sha256::Hash,
    pub preimage: Option<String>,
    pub payee_pubkey: Option<PublicKey>,
    pub amount_sats: Option<u64>,
    pub expire: u64,
    pub paid: bool,
    pub fees_paid: Option<u64>,
    pub inbound: bool,
    pub labels: Vec<String>,
    pub last_updated: u64,
}

impl From<Invoice> for MutinyInvoice {
    fn from(value: Invoice) -> Self {
        let description = match value.description() {
            InvoiceDescription::Direct(a) => {
                if a.is_empty() {
                    None
                } else {
                    Some(a.to_string())
                }
            }
            InvoiceDescription::Hash(_) => None,
        };

        let timestamp = value.duration_since_epoch().as_secs();
        let expiry = timestamp + value.expiry_time().as_secs();

        let payment_hash = value.payment_hash().to_owned();
        let payee_pubkey = value.payee_pub_key().map(|p| p.to_owned());
        let amount_sats = value.amount_milli_satoshis().map(|m| m / 1000);

        MutinyInvoice {
            bolt11: Some(value),
            description,
            payment_hash,
            preimage: None,
            payee_pubkey,
            amount_sats,
            expire: expiry,
            paid: false,
            fees_paid: None,
            inbound: true,
            labels: vec![],
            last_updated: timestamp,
        }
    }
}

impl MutinyInvoice {
    pub(crate) fn from(
        i: PaymentInfo,
        payment_hash: PaymentHash,
        inbound: bool,
        labels: Vec<String>,
    ) -> Result<Self, MutinyError> {
        match i.bolt11 {
            Some(invoice) => {
                // Construct an invoice from a bolt11, easy
                let amount_sats = if let Some(inv_amt) = invoice.amount_milli_satoshis() {
                    if inv_amt == 0 {
                        i.amt_msat.0.map(|a| a / 1_000)
                    } else {
                        Some(inv_amt / 1_000)
                    }
                } else {
                    i.amt_msat.0.map(|a| a / 1_000)
                };
                Ok(MutinyInvoice {
                    inbound,
                    last_updated: i.last_update,
                    paid: i.status == HTLCStatus::Succeeded,
                    labels,
                    amount_sats,
                    payee_pubkey: i.payee_pubkey,
                    preimage: i.preimage.map(|p| p.to_hex()),
                    fees_paid: i.fee_paid_msat.map(|f| f / 1_000),
                    ..invoice.into()
                })
            }
            None => {
                let paid = i.status == HTLCStatus::Succeeded;
                let amount_sats: Option<u64> = i.amt_msat.0.map(|s| s / 1_000);
                let fees_paid = i.fee_paid_msat.map(|f| f / 1_000);
                let preimage = i.preimage.map(|p| p.to_hex());
                let payment_hash = sha256::Hash::from_inner(payment_hash.0);
                let invoice = MutinyInvoice {
                    bolt11: None,
                    description: None,
                    payment_hash,
                    preimage,
                    payee_pubkey: i.payee_pubkey,
                    amount_sats,
                    expire: i.last_update,
                    paid,
                    fees_paid,
                    inbound,
                    labels,
                    last_updated: i.last_update,
                };
                Ok(invoice)
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct MutinyPeer {
    pub pubkey: PublicKey,
    pub connection_string: Option<String>,
    pub alias: Option<String>,
    pub color: Option<String>,
    pub label: Option<String>,
    pub is_connected: bool,
}

impl PartialOrd for MutinyPeer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MutinyPeer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.is_connected
            .cmp(&other.is_connected)
            .then_with(|| self.alias.cmp(&other.alias))
            .then_with(|| self.pubkey.cmp(&other.pubkey))
            .then_with(|| self.connection_string.cmp(&other.connection_string))
    }
}

#[derive(Serialize, Deserialize, Clone, Eq, PartialEq)]
pub struct MutinyChannel {
    pub user_chan_id: String,
    pub balance: u64,
    pub size: u64,
    pub reserve: u64,
    pub outpoint: Option<OutPoint>,
    pub peer: PublicKey,
    pub confirmations_required: Option<u32>,
    pub confirmations: u32,
}

impl From<&ChannelDetails> for MutinyChannel {
    fn from(c: &ChannelDetails) -> Self {
        MutinyChannel {
            user_chan_id: c.user_channel_id.to_hex(),
            balance: c.outbound_capacity_msat / 1_000,
            size: c.channel_value_satoshis,
            reserve: c.unspendable_punishment_reserve.unwrap_or(0),
            outpoint: c.funding_txo.map(|f| f.into_bitcoin_outpoint()),
            peer: c.counterparty.node_id,
            confirmations_required: c.confirmations_required,
            confirmations: c.confirmations.unwrap_or(0),
        }
    }
}

/// A wallet transaction
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TransactionDetails {
    /// Optional transaction
    pub transaction: Option<Transaction>,
    /// Transaction id
    pub txid: Txid,
    /// Received value (sats)
    /// Sum of owned outputs of this transaction.
    pub received: u64,
    /// Sent value (sats)
    /// Sum of owned inputs of this transaction.
    pub sent: u64,
    /// Fee value in sats if it was available.
    pub fee: Option<u64>,
    /// If the transaction is confirmed, contains height and Unix timestamp of the block containing the
    /// transaction, unconfirmed transaction contains `None`.
    pub confirmation_time: ConfirmationTime,
    /// Labels associated with this transaction
    pub labels: Vec<String>,
}

impl PartialOrd for TransactionDetails {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TransactionDetails {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.confirmation_time
            .cmp(&other.confirmation_time)
            .then_with(|| self.txid.cmp(&other.txid))
    }
}

impl From<bdk::TransactionDetails> for TransactionDetails {
    fn from(t: bdk::TransactionDetails) -> Self {
        TransactionDetails {
            transaction: t.transaction,
            txid: t.txid,
            received: t.received,
            sent: t.sent,
            fee: t.fee,
            confirmation_time: t.confirmation_time,
            labels: vec![],
        }
    }
}

/// Information about a channel that was closed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelClosure {
    pub user_channel_id: Option<[u8; 16]>,
    pub channel_id: Option<[u8; 32]>,
    pub node_id: Option<PublicKey>,
    pub reason: String,
    pub timestamp: u64,
}

impl ChannelClosure {
    pub fn new(
        user_channel_id: u128,
        channel_id: [u8; 32],
        node_id: Option<PublicKey>,
        reason: ClosureReason,
    ) -> Self {
        Self {
            user_channel_id: Some(user_channel_id.to_be_bytes()),
            channel_id: Some(channel_id),
            node_id,
            reason: reason.to_string(),
            timestamp: utils::now().as_secs(),
        }
    }
}

impl PartialOrd for ChannelClosure {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ChannelClosure {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.timestamp.cmp(&other.timestamp)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ActivityItem {
    OnChain(TransactionDetails),
    Lightning(Box<MutinyInvoice>),
    ChannelClosed(ChannelClosure),
}

impl ActivityItem {
    pub fn last_updated(&self) -> Option<u64> {
        match self {
            ActivityItem::OnChain(t) => match t.confirmation_time {
                ConfirmationTime::Confirmed { time, .. } => Some(time),
                ConfirmationTime::Unconfirmed => None,
            },
            ActivityItem::Lightning(i) => Some(i.last_updated),
            ActivityItem::ChannelClosed(c) => Some(c.timestamp),
        }
    }

    pub fn labels(&self) -> Vec<String> {
        match self {
            ActivityItem::OnChain(t) => t.labels.clone(),
            ActivityItem::Lightning(i) => i.labels.clone(),
            ActivityItem::ChannelClosed(_) => vec![],
        }
    }

    pub fn is_channel_open(&self) -> bool {
        match self {
            ActivityItem::OnChain(onchain) => {
                onchain.labels.iter().any(|l| l.contains("LN Channel:"))
            }
            ActivityItem::Lightning(_) => false,
            ActivityItem::ChannelClosed(_) => false,
        }
    }
}

impl PartialOrd for ActivityItem {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ActivityItem {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // We want None to be greater than Some because those are pending transactions
        // so those should be at the top of the list
        match (self.last_updated(), other.last_updated()) {
            (Some(self_time), Some(other_time)) => self_time.cmp(&other_time),
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (None, None) => core::cmp::Ordering::Equal,
        }
    }
}

pub struct MutinyBalance {
    pub confirmed: u64,
    pub unconfirmed: u64,
    pub lightning: u64,
    pub force_close: u64,
}

pub struct LnUrlParams {
    pub max: u64,
    pub min: u64,
    pub tag: String,
}

/// The [NodeManager] is the main entry point for interacting with the Mutiny Wallet.
/// It is responsible for managing the on-chain wallet and the lightning nodes.
///
/// It can be used to create a new wallet, or to load an existing wallet.
///
/// It can be configured to use all different custom backend services, or to use the default
/// services provided by Mutiny.
pub struct NodeManager<S: MutinyStorage> {
    pub(crate) stop: Arc<AtomicBool>,
    mnemonic: Mnemonic,
    network: Network,
    #[cfg(target_arch = "wasm32")]
    websocket_proxy_addr: String,
    esplora: Arc<AsyncClient>,
    wallet: Arc<OnChainWallet<S>>,
    gossip_sync: Arc<RapidGossipSync>,
    scorer: Arc<utils::Mutex<ProbScorer>>,
    chain: Arc<MutinyChain<S>>,
    fee_estimator: Arc<MutinyFeeEstimator<S>>,
    pub(crate) storage: S,
    pub(crate) node_storage: Mutex<NodeStorage>,
    pub(crate) nodes: Arc<Mutex<HashMap<PublicKey, Arc<Node<S>>>>>,
    auth: AuthManager<S>,
    lnurl_client: LnUrlClient,
    pub(crate) lsp_clients: Vec<LspClient>,
    pub(crate) logger: Arc<MutinyLogger>,
    bitcoin_price_cache: Arc<Mutex<Option<(f32, Duration)>>>,
}

impl<S: MutinyStorage> NodeManager<S> {
    /// Returns if there is a saved wallet in storage.
    /// This is checked by seeing if a mnemonic seed exists in storage.
    pub fn has_node_manager(storage: S) -> bool {
        storage.get_mnemonic().is_ok()
    }

    /// Creates a new [NodeManager] with the given parameters.
    /// The mnemonic seed is read from storage, unless one is provided.
    /// If no mnemonic is provided, a new one is generated and stored.
    pub async fn new(c: MutinyWalletConfig, storage: S) -> Result<NodeManager<S>, MutinyError> {
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(target_arch = "wasm32")]
        let websocket_proxy_addr = c
            .websocket_proxy_addr
            .unwrap_or_else(|| String::from("wss://p.mutinywallet.com"));

        // todo we should eventually have default mainnet
        let network: Network = c.network.unwrap_or(Network::Signet);

        let mnemonic = match c.mnemonic {
            Some(seed) => storage.insert_mnemonic(seed)?,
            None => match storage.get_mnemonic() {
                Ok(mnemonic) => mnemonic,
                Err(_) => {
                    let seed = keymanager::generate_seed(12)?;
                    storage.insert_mnemonic(seed)?
                }
            },
        };

        let logger = Arc::new(MutinyLogger::with_writer(stop.clone(), storage.clone()));

        let esplora_server_url = get_esplora_url(network, c.user_esplora_url);
        let tx_sync = Arc::new(EsploraSyncClient::new(esplora_server_url, logger.clone()));

        let esplora = Arc::new(tx_sync.client().clone());
        let fee_estimator = Arc::new(MutinyFeeEstimator::new(
            storage.clone(),
            esplora.clone(),
            logger.clone(),
        ));

        let wallet = Arc::new(OnChainWallet::new(
            &mnemonic,
            storage.clone(),
            network,
            esplora.clone(),
            fee_estimator.clone(),
            logger.clone(),
        )?);

        let chain = Arc::new(MutinyChain::new(tx_sync, wallet.clone(), logger.clone()));

        let (gossip_sync, scorer) =
            gossip::get_gossip_sync(&storage, c.user_rgs_url, network, logger.clone()).await?;

        let scorer = Arc::new(utils::Mutex::new(scorer));

        let gossip_sync = Arc::new(gossip_sync);

        // load lsp clients, if any
        let lsp_clients: Vec<LspClient> = match c.lsp_url.clone() {
            // check if string is some and not an empty string
            Some(lsp_urls) if !lsp_urls.is_empty() => {
                let urls: Vec<&str> = lsp_urls.split(',').collect();

                let futs = urls.into_iter().map(|url| LspClient::new(url.trim()));

                let results = futures::future::join_all(futs).await;

                results
                    .into_iter()
                    .flat_map(|res| match res {
                        Ok(client) => Some(client),
                        Err(e) => {
                            log_warn!(logger, "Error starting up lsp client: {e}");
                            None
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        let node_storage = storage.get_nodes()?;

        // Remove the archived nodes, we don't need to start them up.
        let unarchived_nodes = node_storage
            .clone()
            .nodes
            .into_iter()
            .filter(|(_, n)| !n.is_archived());

        let mut nodes_map = HashMap::new();

        for node_item in unarchived_nodes {
            let node = Node::new(
                node_item.0,
                &node_item.1,
                stop.clone(),
                &mnemonic,
                storage.clone(),
                gossip_sync.clone(),
                scorer.clone(),
                chain.clone(),
                fee_estimator.clone(),
                wallet.clone(),
                network,
                esplora.clone(),
                &lsp_clients,
                logger.clone(),
                #[cfg(target_arch = "wasm32")]
                websocket_proxy_addr.clone(),
            )
            .await?;

            let id = node
                .keys_manager
                .get_node_id(Recipient::Node)
                .expect("Failed to get node id");

            nodes_map.insert(id, Arc::new(node));
        }

        // when we create the nodes we set the LSP if one is missing
        // we need to save it to local storage after startup in case
        // a LSP was set.
        let updated_nodes: HashMap<String, NodeIndex> = nodes_map
            .values()
            .map(|n| (n._uuid.clone(), n.node_index()))
            .collect();

        log_info!(logger, "inserting updated nodes");

        storage.insert_nodes(NodeStorage {
            nodes: updated_nodes,
        })?;

        log_info!(logger, "inserted updated nodes");

        let nodes = Arc::new(Mutex::new(nodes_map));

        let seed = mnemonic.to_seed("");
        let xprivkey = ExtendedPrivKey::new_master(network, &seed)?;
        let auth = AuthManager::new(xprivkey, storage.clone())?;

        // Create default profile if it doesn't exist
        auth.create_init()?;

        let lnurl_client = lnurl::Builder::default()
            .build_async()
            .expect("failed to make lnurl client");

        let nm = NodeManager {
            stop,
            mnemonic,
            network,
            wallet,
            gossip_sync,
            scorer,
            chain,
            fee_estimator,
            storage,
            node_storage: Mutex::new(node_storage),
            nodes,
            #[cfg(target_arch = "wasm32")]
            websocket_proxy_addr,
            esplora,
            auth,
            lnurl_client,
            lsp_clients,
            logger,
            bitcoin_price_cache: Arc::new(Mutex::new(None)),
        };

        Ok(nm)
    }

    /// Returns the node with the given pubkey
    pub(crate) async fn get_node(&self, pk: &PublicKey) -> Result<Arc<Node<S>>, MutinyError> {
        let nodes = self.nodes.lock().await;
        let node = nodes.get(pk).ok_or(MutinyError::NotFound)?;
        Ok(node.clone())
    }

    /// Stops all of the nodes and background processes.
    /// Returns after node has been stopped.
    pub async fn stop(&self) -> Result<(), MutinyError> {
        self.stop.swap(true, Ordering::Relaxed);
        let mut nodes = self.nodes.lock().await;
        let node_futures = nodes.iter().map(|(_, n)| async {
            match n.stopped().await {
                Ok(_) => {
                    log_debug!(self.logger, "stopped node: {}", n.pubkey.to_hex())
                }
                Err(e) => {
                    log_error!(
                        self.logger,
                        "failed to stop node {}: {e}",
                        n.pubkey.to_hex()
                    )
                }
            }
        });
        log_debug!(self.logger, "stopping all nodes");
        join_all(node_futures).await;
        nodes.clear();
        log_debug!(self.logger, "stopped all nodes");

        // stop the indexeddb object to close db connection
        if self.storage.connected().unwrap_or(false) {
            log_debug!(self.logger, "stopping storage");
            self.storage.stop();
            log_debug!(self.logger, "stopped storage");
        }

        Ok(())
    }

    /// Starts a background tasks to poll redshifts until they are ready and then start attempting payments.
    ///
    /// This function will first find redshifts that are in the [RedshiftStatus::AttemptingPayments] state and start attempting payments
    /// and redshifts that are in the [RedshiftStatus::ClosingChannels] state and finish closing channels.
    /// This is done in case the node manager was shutdown while attempting payments or closing channels.
    pub(crate) fn start_redshifts(nm: Arc<NodeManager<S>>) {
        // find AttemptingPayments redshifts and restart attempting payments
        // find ClosingChannels redshifts and restart closing channels
        // use unwrap_or_default() to handle errors
        let all = nm.storage.get_redshifts().unwrap_or_default();
        for redshift in all {
            match redshift.status {
                RedshiftStatus::AttemptingPayments => {
                    // start attempting payments
                    let nm_clone = nm.clone();
                    utils::spawn(async move {
                        if let Err(e) = nm_clone.attempt_payments(redshift).await {
                            log_error!(nm_clone.logger, "Error attempting redshift payments: {e}");
                        }
                    });
                }
                RedshiftStatus::ClosingChannels => {
                    // finish closing channels
                    let nm_clone = nm.clone();
                    utils::spawn(async move {
                        if let Err(e) = nm_clone.close_channels(redshift).await {
                            log_error!(nm_clone.logger, "Error closing redshift channels: {e}");
                        }
                    });
                }
                _ => {} // ignore other statuses
            }
        }

        utils::spawn(async move {
            loop {
                if nm.stop.load(Ordering::Relaxed) {
                    break;
                }
                // find redshifts with channels ready
                // use unwrap_or_default() to handle errors
                let all = nm.storage.get_redshifts().unwrap_or_default();
                for mut redshift in all {
                    if redshift.status == RedshiftStatus::ChannelOpened {
                        // update status
                        redshift.status = RedshiftStatus::AttemptingPayments;
                        if let Err(e) = nm.storage.persist_redshift(redshift.clone()) {
                            log_error!(nm.logger, "Error persisting redshift status update: {e}");
                        }

                        // start attempting payments
                        let payment_nm = nm.clone();
                        utils::spawn(async move {
                            if let Err(e) = payment_nm.attempt_payments(redshift).await {
                                log_error!(
                                    payment_nm.logger,
                                    "Error attempting redshift payments: {e}"
                                );
                            }
                        });
                    }
                }

                // sleep 10 seconds
                sleep(10_000).await;
            }
        });
    }

    /// Creates a background process that will sync the wallet with the blockchain.
    /// This will also update the fee estimates every 10 minutes.
    pub fn start_sync(nm: Arc<NodeManager<S>>) {
        // If we are stopped, don't sync
        if nm.stop.load(Ordering::Relaxed) {
            return;
        }

        utils::spawn(async move {
            let mut sync_count: u64 = 0;
            loop {
                // If we are stopped, don't sync
                if nm.stop.load(Ordering::Relaxed) {
                    return;
                }

                // we don't need to re-sync fees every time
                // just do it every 10 minutes
                if sync_count % 10 == 0 {
                    if let Err(e) = nm.fee_estimator.update_fee_estimates().await {
                        log_error!(nm.logger, "Failed to update fee estimates: {e}");
                    } else {
                        log_info!(nm.logger, "Updated fee estimates!");
                    }
                }

                if let Err(e) = nm.sync().await {
                    log_error!(nm.logger, "Failed to sync: {e}");
                }

                // if this is the first sync, set the done_first_sync flag
                if sync_count == 0 {
                    let _ = nm.storage.set_done_first_sync();
                }

                // sleep for 1 minute, checking graceful shutdown check each 1s.
                for _ in 0..60 {
                    if nm.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    sleep(1_000).await;
                }

                // increment sync count
                sync_count += 1;
            }
        });
    }

    /// Broadcast a transaction to the network.
    /// The transaction is broadcast through the configured esplora server.
    pub async fn broadcast_transaction(&self, tx: Transaction) -> Result<(), MutinyError> {
        self.wallet.broadcast_transaction(tx).await
    }

    /// Returns the mnemonic seed phrase for the wallet.
    pub fn show_seed(&self) -> Mnemonic {
        self.mnemonic.clone()
    }

    /// Returns the network of the wallet.
    pub fn get_network(&self) -> Network {
        self.network
    }

    /// Gets a new bitcoin address from the wallet.
    /// Will generate a new address on every call.
    ///
    /// It is recommended to create a new address for every transaction.
    pub fn get_new_address(&self, labels: Vec<String>) -> Result<Address, MutinyError> {
        let mut wallet = self.wallet.wallet.try_write()?;
        let address = wallet.get_address(AddressIndex::New).address;
        self.set_address_labels(address.clone(), labels)?;
        Ok(address)
    }

    /// Gets the current balance of the on-chain wallet.
    pub fn get_wallet_balance(&self) -> Result<u64, MutinyError> {
        let wallet = self.wallet.wallet.try_read()?;

        Ok(wallet.get_balance().total())
    }

    /// Creates a BIP 21 invoice. This creates a new address and a lightning invoice.
    pub async fn create_bip21(
        &self,
        amount: Option<u64>,
        labels: Vec<String>,
    ) -> Result<MutinyBip21RawMaterials, MutinyError> {
        let invoice = self.create_invoice(amount, labels.clone()).await?;

        let Ok(address) = self.get_new_address(labels.clone()) else {
            return Err(MutinyError::WalletOperationFailed);
        };

        let Some(bolt11) = invoice.bolt11 else {
            return Err(MutinyError::WalletOperationFailed);
        };

        Ok(MutinyBip21RawMaterials {
            address,
            invoice: bolt11,
            btc_amount: amount.map(|amount| bitcoin::Amount::from_sat(amount).to_btc().to_string()),
            labels,
        })
    }

    /// Sends an on-chain transaction to the given address.
    /// The amount is in satoshis and the fee rate is in sat/vbyte.
    ///
    /// If a fee rate is not provided, one will be used from the fee estimator.
    pub async fn send_to_address(
        &self,
        send_to: Address,
        amount: u64,
        labels: Vec<String>,
        fee_rate: Option<f32>,
    ) -> Result<Txid, MutinyError> {
        if !send_to.is_valid_for_network(self.network) {
            return Err(MutinyError::IncorrectNetwork(send_to.network));
        }

        self.wallet.send(send_to, amount, labels, fee_rate).await
    }

    /// Sweeps all the funds from the wallet to the given address.
    /// The fee rate is in sat/vbyte.
    ///
    /// If a fee rate is not provided, one will be used from the fee estimator.
    pub async fn sweep_wallet(
        &self,
        send_to: Address,
        labels: Vec<String>,
        fee_rate: Option<f32>,
    ) -> Result<Txid, MutinyError> {
        if !send_to.is_valid_for_network(self.network) {
            return Err(MutinyError::IncorrectNetwork(send_to.network));
        }

        self.wallet.sweep(send_to, labels, fee_rate).await
    }

    /// Estimates the onchain fee for a transaction sending to the given address.
    /// The amount is in satoshis and the fee rate is in sat/vbyte.
    pub fn estimate_tx_fee(
        &self,
        destination_address: Address,
        amount: u64,
        fee_rate: Option<f32>,
    ) -> Result<u64, MutinyError> {
        self.wallet
            .estimate_tx_fee(destination_address.script_pubkey(), amount, fee_rate)
    }

    /// Estimates the onchain fee for a opening a lightning channel.
    /// The amount is in satoshis and the fee rate is in sat/vbyte.
    pub fn estimate_channel_open_fee(
        &self,
        amount: u64,
        fee_rate: Option<f32>,
    ) -> Result<u64, MutinyError> {
        // Dummy p2wsh script for the channel output
        let script = script::Builder::new()
            .push_int(0)
            .push_slice(&[0; 32])
            .into_script();
        self.wallet.estimate_tx_fee(script, amount, fee_rate)
    }

    /// Checks if the given address has any transactions.
    /// If it does, it returns the details of the first transaction.
    ///
    /// This should be used to check if a payment has been made to an address.
    pub async fn check_address(
        &self,
        address: &Address,
    ) -> Result<Option<TransactionDetails>, MutinyError> {
        if !address.is_valid_for_network(self.network) {
            return Err(MutinyError::IncorrectNetwork(address.network));
        }

        let script = address.payload.script_pubkey();
        let txs = self.esplora.scripthash_txs(&script, None).await?;

        let details_opt = txs.first().map(|tx| {
            let received: u64 = tx
                .vout
                .iter()
                .filter(|v| v.scriptpubkey == script)
                .map(|v| v.value)
                .sum();

            let confirmation_time = tx
                .confirmation_time()
                .map(|c| ConfirmationTime::Confirmed {
                    height: c.height,
                    time: c.timestamp,
                })
                .unwrap_or(ConfirmationTime::Unconfirmed);

            let address_labels = self.get_address_labels().unwrap_or_default();
            let labels = address_labels
                .get(&address.to_string())
                .cloned()
                .unwrap_or_default();

            let details = TransactionDetails {
                transaction: Some(tx.to_tx()),
                txid: tx.txid,
                received,
                sent: 0,
                fee: None,
                confirmation_time,
                labels,
            };

            let block_id = match tx.status.block_hash {
                Some(hash) => {
                    let height = tx
                        .status
                        .block_height
                        .expect("block height must be present");
                    Some(BlockId { hash, height })
                }
                None => None,
            };

            (details, block_id)
        });

        // if we found a tx we should try to import it into the wallet
        if let Some((details, block_id)) = details_opt.clone() {
            let wallet = self.wallet.clone();
            utils::spawn(async move {
                let tx = details.transaction.expect("tx must be present");
                wallet
                    .insert_tx(tx, details.confirmation_time, block_id)
                    .await
                    .expect("failed to insert tx");
            });
        }

        Ok(details_opt.map(|(d, _)| d))
    }

    /// Returns all the on-chain and lightning activity from the wallet.
    pub async fn get_activity(&self) -> Result<Vec<ActivityItem>, MutinyError> {
        // todo add contacts to the activity
        let (lightning, closures) =
            futures_util::join!(self.list_invoices(), self.list_channel_closures());
        let lightning = lightning?;
        let closures = closures?;
        let onchain = self.list_onchain()?;

        let mut activity = Vec::with_capacity(lightning.len() + onchain.len());
        for ln in lightning {
            // Only show paid invoices
            if ln.paid {
                activity.push(ActivityItem::Lightning(Box::new(ln)));
            }
        }
        for on in onchain {
            activity.push(ActivityItem::OnChain(on));
        }
        for chan in closures {
            activity.push(ActivityItem::ChannelClosed(chan));
        }

        // Newest first
        activity.sort_by(|a, b| b.cmp(a));

        Ok(activity)
    }

    /// Adds labels to the TransactionDetails based on the address labels.
    /// This will panic if the TransactionDetails does not have a transaction.
    /// Make sure you flag `include_raw` when calling `list_transactions` to
    /// ensure that the transaction is included.
    fn add_onchain_labels(
        &self,
        address_labels: &HashMap<String, Vec<String>>,
        tx: bdk::TransactionDetails,
    ) -> TransactionDetails {
        // find the first output address that has a label
        let labels = tx
            .transaction
            .clone()
            .unwrap() // safe because we call with list_transactions(true)
            .output
            .iter()
            .find_map(|o| {
                if let Ok(addr) = Address::from_script(&o.script_pubkey, self.network) {
                    address_labels.get(&addr.to_string()).cloned()
                } else {
                    None
                }
            })
            .unwrap_or_default();

        TransactionDetails {
            labels,
            ..tx.into()
        }
    }

    /// Lists all the on-chain transactions in the wallet.
    /// These are sorted by confirmation time.
    pub fn list_onchain(&self) -> Result<Vec<TransactionDetails>, MutinyError> {
        let mut txs = self.wallet.list_transactions(true)?;
        txs.sort();
        let address_labels = self.get_address_labels()?;
        let txs = txs
            .into_iter()
            .map(|tx| self.add_onchain_labels(&address_labels, tx))
            .collect();

        Ok(txs)
    }

    /// Gets the details of a specific on-chain transaction.
    pub fn get_transaction(&self, txid: Txid) -> Result<Option<TransactionDetails>, MutinyError> {
        match self.wallet.get_transaction(txid, true)? {
            Some(tx) => {
                let address_labels = self.get_address_labels()?;
                let tx_details = self.add_onchain_labels(&address_labels, tx);
                Ok(Some(tx_details))
            }
            None => Ok(None),
        }
    }

    /// Gets the current balance of the wallet.
    /// This includes both on-chain and lightning funds.
    ///
    /// This will not include any funds in an unconfirmed lightning channel.
    pub async fn get_balance(&self) -> Result<MutinyBalance, MutinyError> {
        let onchain = self.wallet.wallet.try_read()?.get_balance();

        let nodes = self.nodes.lock().await;
        let lightning_msats: u64 = nodes
            .iter()
            .flat_map(|(_, n)| n.channel_manager.list_channels())
            .map(|c| c.balance_msat)
            .sum();

        // get the amount in limbo from force closes
        let force_close: u64 = nodes
            .iter()
            .flat_map(|(_, n)| {
                let channels = n.channel_manager.list_channels();
                let ignored_channels: Vec<&ChannelDetails> = channels.iter().collect();
                n.chain_monitor.get_claimable_balances(&ignored_channels)
            })
            .map(|bal| match bal {
                Balance::ClaimableOnChannelClose {
                    claimable_amount_satoshis,
                } => claimable_amount_satoshis,
                Balance::ClaimableAwaitingConfirmations {
                    claimable_amount_satoshis,
                    ..
                } => claimable_amount_satoshis,
                Balance::ContentiousClaimable {
                    claimable_amount_satoshis,
                    ..
                } => claimable_amount_satoshis,
                Balance::MaybeTimeoutClaimableHTLC {
                    claimable_amount_satoshis,
                    ..
                } => claimable_amount_satoshis,
                Balance::MaybePreimageClaimableHTLC {
                    claimable_amount_satoshis,
                    ..
                } => claimable_amount_satoshis,
                Balance::CounterpartyRevokedOutputClaimable {
                    claimable_amount_satoshis,
                    ..
                } => claimable_amount_satoshis,
            })
            .sum();

        Ok(MutinyBalance {
            confirmed: onchain.confirmed + onchain.trusted_pending,
            unconfirmed: onchain.untrusted_pending + onchain.immature,
            lightning: lightning_msats / 1_000,
            force_close,
        })
    }

    /// Lists all the UTXOs in the wallet.
    pub fn list_utxos(&self) -> Result<Vec<LocalUtxo>, MutinyError> {
        self.wallet.list_utxos()
    }

    /// Syncs the lightning wallet with the blockchain.
    /// This will update the wallet with any lightning channels
    /// that have been opened or closed.
    ///
    /// This should be called before syncing the on-chain wallet
    /// to ensure that new on-chain transactions are picked up.
    async fn sync_ldk(&self) -> Result<(), MutinyError> {
        let nodes = self.nodes.lock().await;

        let confirmables: Vec<&(dyn Confirm)> = nodes
            .iter()
            .flat_map(|(_, node)| {
                let vec: Vec<&(dyn Confirm)> =
                    vec![node.channel_manager.deref(), node.chain_monitor.deref()];
                vec
            })
            .collect();

        self.chain
            .tx_sync
            .sync(confirmables)
            .await
            .map_err(|_e| MutinyError::ChainAccessFailed)?;

        Ok(())
    }

    /// Syncs the on-chain wallet and lightning wallet.
    /// This will update the on-chain wallet with any new
    /// transactions and update the lightning wallet with
    /// any channels that have been opened or closed.
    async fn sync(&self) -> Result<(), MutinyError> {
        // If we are stopped, don't sync
        if self.stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Sync ldk first because it may broadcast transactions
        // to addresses that are in our bdk wallet. This way
        // they are found on this iteration of syncing instead
        // of the next one.
        if let Err(e) = self.sync_ldk().await {
            log_error!(self.logger, "Failed to sync ldk: {e}");
            return Err(e);
        }

        // sync bdk wallet
        match self.wallet.sync().await {
            Ok(()) => Ok(log_info!(self.logger, "We are synced!")),
            Err(e) => {
                log_error!(self.logger, "Failed to sync on-chain wallet: {e}");
                Err(e)
            }
        }
    }

    /// Gets a fee estimate for an average priority transaction.
    /// Value is in sat/vbyte.
    pub fn estimate_fee_normal(&self) -> u32 {
        self.fee_estimator
            .get_est_sat_per_1000_weight(ConfirmationTarget::Normal)
    }

    /// Gets a fee estimate for an high priority transaction.
    /// Value is in sat/vbyte.
    pub fn estimate_fee_high(&self) -> u32 {
        self.fee_estimator
            .get_est_sat_per_1000_weight(ConfirmationTarget::HighPriority)
    }

    /// Creates a new lightning node and adds it to the manager.
    pub async fn new_node(&self) -> Result<NodeIdentity, MutinyError> {
        create_new_node_from_node_manager(self).await
    }

    /// Archives a node so it will not be started up next time the node manager is created.
    ///
    /// If the node has any active channels it will fail to archive
    #[allow(dead_code)]
    pub(crate) async fn archive_node(&self, pubkey: PublicKey) -> Result<(), MutinyError> {
        if let Some(node) = self.nodes.lock().await.get(&pubkey) {
            // disallow archiving nodes with active channels or
            // claimable on-chain funds, so we don't lose funds
            if node.channel_manager.list_channels().is_empty()
                && node.chain_monitor.get_claimable_balances(&[]).is_empty()
            {
                self.archive_node_by_uuid(node._uuid.clone()).await
            } else {
                Err(anyhow!("Node has active channels, cannot archive").into())
            }
        } else {
            Err(anyhow!("Could not find node to archive").into())
        }
    }

    /// Archives a node so it will not be started up next time the node manager is created.
    ///
    /// If the node has any active channels it will fail to archive
    #[allow(dead_code)]
    pub(crate) async fn archive_node_by_uuid(&self, node_uuid: String) -> Result<(), MutinyError> {
        let mut node_storage = self.node_storage.lock().await;

        match node_storage.nodes.get(&node_uuid).map(|n| n.to_owned()) {
            None => Err(anyhow!("Could not find node to archive").into()),
            Some(mut node) => {
                node.archived = Some(true);
                let prev = node_storage.nodes.insert(node_uuid, node);

                // Check that we did override the previous node index
                debug_assert!(prev.is_some());

                Ok(())
            }
        }
    }

    /// Lists the pubkeys of the lightning node in the manager.
    pub async fn list_nodes(&self) -> Result<Vec<PublicKey>, MutinyError> {
        let nodes = self.nodes.lock().await;
        let peers = nodes.iter().map(|(_, n)| n.pubkey).collect();
        Ok(peers)
    }

    /// Attempts to connect to a peer from the selected node.
    pub async fn connect_to_peer(
        &self,
        self_node_pubkey: &PublicKey,
        connection_string: &str,
        label: Option<String>,
    ) -> Result<(), MutinyError> {
        if let Some(node) = self.nodes.lock().await.get(self_node_pubkey) {
            let connect_info = PubkeyConnectionInfo::new(connection_string)?;
            let label_opt = label.filter(|s| !s.is_empty()); // filter out empty strings
            let res = node.connect_peer(connect_info, label_opt).await;
            match res {
                Ok(_) => {
                    log_info!(self.logger, "connected to peer: {connection_string}");
                    return Ok(());
                }
                Err(e) => {
                    log_error!(
                        self.logger,
                        "could not connect to peer: {connection_string} - {e}"
                    );
                    return Err(e);
                }
            };
        }

        log_error!(
            self.logger,
            "could not find internal node {self_node_pubkey}"
        );
        Err(MutinyError::NotFound)
    }

    /// Disconnects from a peer from the selected node.
    pub async fn disconnect_peer(
        &self,
        self_node_pubkey: &PublicKey,
        peer: PublicKey,
    ) -> Result<(), MutinyError> {
        if let Some(node) = self.nodes.lock().await.get(self_node_pubkey) {
            node.disconnect_peer(peer);
            Ok(())
        } else {
            log_error!(
                self.logger,
                "could not find internal node {self_node_pubkey}"
            );
            Err(MutinyError::NotFound)
        }
    }

    /// Deletes a peer from the selected node.
    /// This will make it so that the node will not attempt to
    /// reconnect to the peer.
    pub async fn delete_peer(
        &self,
        self_node_pubkey: &PublicKey,
        peer: &NodeId,
    ) -> Result<(), MutinyError> {
        if let Some(node) = self.nodes.lock().await.get(self_node_pubkey) {
            gossip::delete_peer_info(&self.storage, &node._uuid, peer)?;
            Ok(())
        } else {
            log_error!(
                self.logger,
                "could not find internal node {self_node_pubkey}"
            );
            Err(MutinyError::NotFound)
        }
    }

    /// Sets the label of a peer from the selected node.
    pub fn label_peer(&self, node_id: &NodeId, label: Option<String>) -> Result<(), MutinyError> {
        gossip::set_peer_label(&self.storage, node_id, label)?;
        Ok(())
    }

    // all values in sats

    /// Creates a lightning invoice. The amount should be in satoshis.
    /// If no amount is provided, the invoice will be created with no amount.
    /// If no description is provided, the invoice will be created with no description.
    ///
    /// If the manager has more than one node it will create a phantom invoice.
    /// If there is only one node it will create an invoice just for that node.
    pub async fn create_invoice(
        &self,
        amount: Option<u64>,
        labels: Vec<String>,
    ) -> Result<MutinyInvoice, MutinyError> {
        let nodes = self.nodes.lock().await;
        let use_phantom = nodes.len() > 1 && self.lsp_clients.is_empty();
        if nodes.len() == 0 {
            return Err(MutinyError::InvoiceCreationFailed);
        }
        let route_hints: Option<Vec<PhantomRouteHints>> = if use_phantom {
            Some(
                nodes
                    .iter()
                    .map(|(_, n)| n.get_phantom_route_hint())
                    .collect(),
            )
        } else {
            None
        };

        // just create a normal invoice from the first node
        let first_node = if let Some(node) = nodes.values().next() {
            node
        } else {
            return Err(MutinyError::WalletOperationFailed);
        };
        let invoice = first_node
            .create_invoice(amount, labels, route_hints)
            .await?;

        Ok(invoice.into())
    }

    /// Pays a lightning invoice from the selected node.
    /// An amount should only be provided if the invoice does not have an amount.
    /// The amount should be in satoshis.
    pub async fn pay_invoice(
        &self,
        from_node: &PublicKey,
        invoice: &Invoice,
        amt_sats: Option<u64>,
        labels: Vec<String>,
    ) -> Result<MutinyInvoice, MutinyError> {
        if invoice.network() != self.network {
            return Err(MutinyError::IncorrectNetwork(invoice.network()));
        }

        let node = self.get_node(from_node).await?;
        node.pay_invoice_with_timeout(invoice, amt_sats, None, labels)
            .await
    }

    /// Sends a spontaneous payment to a node from the selected node.
    /// The amount should be in satoshis.
    pub async fn keysend(
        &self,
        from_node: &PublicKey,
        to_node: PublicKey,
        amt_sats: u64,
        labels: Vec<String>,
    ) -> Result<MutinyInvoice, MutinyError> {
        let node = self.get_node(from_node).await?;
        log_debug!(self.logger, "Keysending to {to_node}");
        node.keysend_with_timeout(to_node, amt_sats, labels, None)
            .await
    }

    /// Decodes a lightning invoice into useful information.
    /// Will return an error if the invoice is for a different network.
    pub async fn decode_invoice(&self, invoice: Invoice) -> Result<MutinyInvoice, MutinyError> {
        if invoice.network() != self.network {
            return Err(MutinyError::IncorrectNetwork(invoice.network()));
        }

        Ok(invoice.into())
    }

    /// Calls upon a LNURL to get the parameters for it.
    /// This contains what kind of LNURL it is (pay, withdrawal, auth, etc).
    // todo revamp LnUrlParams to be well designed
    pub async fn decode_lnurl(&self, lnurl: LnUrl) -> Result<LnUrlParams, MutinyError> {
        // handle LNURL-AUTH
        if lnurl.is_lnurl_auth() {
            return Ok(LnUrlParams {
                max: 0,
                min: 0,
                tag: "login".to_string(),
            });
        }

        let response = self.lnurl_client.make_request(&lnurl.url).await?;

        let params = match response {
            LnUrlResponse::LnUrlPayResponse(pay) => LnUrlParams {
                max: pay.max_sendable,
                min: pay.min_sendable,
                tag: "payRequest".to_string(),
            },
            LnUrlResponse::LnUrlChannelResponse(_chan) => LnUrlParams {
                max: 0,
                min: 0,
                tag: "channelRequest".to_string(),
            },
            LnUrlResponse::LnUrlWithdrawResponse(withdraw) => LnUrlParams {
                max: withdraw.max_withdrawable,
                min: withdraw.min_withdrawable.unwrap_or(0),
                tag: "withdrawRequest".to_string(),
            },
        };

        Ok(params)
    }

    /// Calls upon a LNURL and pays it.
    /// This will fail if the LNURL is not a LNURL pay.
    pub async fn lnurl_pay(
        &self,
        from_node: &PublicKey,
        lnurl: &LnUrl,
        amount_sats: u64,
        labels: Vec<String>,
    ) -> Result<MutinyInvoice, MutinyError> {
        let response = self.lnurl_client.make_request(&lnurl.url).await?;

        match response {
            LnUrlResponse::LnUrlPayResponse(pay) => {
                let msats = amount_sats * 1000;
                let invoice = self.lnurl_client.get_invoice(&pay, msats).await?;

                self.pay_invoice(from_node, &invoice.invoice(), None, labels)
                    .await
            }
            LnUrlResponse::LnUrlWithdrawResponse(_) => Err(MutinyError::IncorrectLnUrlFunction),
            LnUrlResponse::LnUrlChannelResponse(_) => Err(MutinyError::IncorrectLnUrlFunction),
        }
    }

    /// Calls upon a LNURL and withdraws from it.
    /// This will fail if the LNURL is not a LNURL withdrawal.
    pub async fn lnurl_withdraw(
        &self,
        lnurl: &LnUrl,
        amount_sats: u64,
    ) -> Result<bool, MutinyError> {
        let response = self.lnurl_client.make_request(&lnurl.url).await?;

        match response {
            LnUrlResponse::LnUrlPayResponse(_) => Err(MutinyError::IncorrectLnUrlFunction),
            LnUrlResponse::LnUrlChannelResponse(_) => Err(MutinyError::IncorrectLnUrlFunction),
            LnUrlResponse::LnUrlWithdrawResponse(withdraw) => {
                // fixme: do we need to use this description?
                let _description = withdraw.default_description.clone();
                let mutiny_invoice = self
                    .create_invoice(Some(amount_sats), vec!["LNURL Withdrawal".to_string()])
                    .await?;
                let invoice_str = mutiny_invoice.bolt11.expect("Invoice should have bolt11");
                let res = self
                    .lnurl_client
                    .do_withdrawal(&withdraw, &invoice_str.to_string())
                    .await?;
                match res {
                    Response::Ok { .. } => Ok(true),
                    Response::Error { .. } => Ok(false),
                }
            }
        }
    }

    /// Creates a new LNURL-auth profile.
    pub fn create_lnurl_auth_profile(&self, name: String) -> Result<u32, MutinyError> {
        self.auth.add_profile(name)
    }

    /// Gets all the LNURL-auth profiles.
    pub fn get_lnurl_auth_profiles(&self) -> Result<Vec<AuthProfile>, MutinyError> {
        self.auth.get_profiles()
    }

    /// Authenticates with a LNURL-auth for the given profile.
    pub async fn lnurl_auth(&self, profile_index: usize, lnurl: LnUrl) -> Result<(), MutinyError> {
        let url = Url::parse(&lnurl.url)?;
        let query_pairs: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let k1 = query_pairs.get("k1").ok_or(MutinyError::LnUrlFailure)?;
        let k1: [u8; 32] = FromHex::from_hex(k1).map_err(|_| MutinyError::LnUrlFailure)?;
        let (sig, key) = self.auth.sign(profile_index, url.clone(), &k1)?;

        let response = self.lnurl_client.lnurl_auth(lnurl, sig, key).await;
        match response {
            Ok(Response::Ok { .. }) => {
                // don't fail if we just can't save the service
                if let Err(e) = self.auth.add_used_service(profile_index, url) {
                    log_error!(self.logger, "Failed to save used lnurl auth service: {e}");
                }

                log_info!(self.logger, "LNURL auth successful!");
                Ok(())
            }
            Ok(Response::Error { reason }) => {
                log_error!(self.logger, "LNURL auth failed: {reason}");
                Err(MutinyError::LnUrlFailure)
            }
            Err(e) => {
                log_error!(self.logger, "LNURL auth failed: {e}");
                Err(MutinyError::LnUrlFailure)
            }
        }
    }

    /// Gets an invoice from the node manager.
    /// This includes sent and received invoices.
    pub async fn get_invoice(&self, invoice: &Invoice) -> Result<MutinyInvoice, MutinyError> {
        let nodes = self.nodes.lock().await;
        let inv_opt: Option<MutinyInvoice> =
            nodes.iter().find_map(|(_, n)| n.get_invoice(invoice).ok());
        match inv_opt {
            Some(i) => Ok(i),
            None => Err(MutinyError::NotFound),
        }
    }

    /// Gets an invoice from the node manager.
    /// This includes sent and received invoices.
    pub async fn get_invoice_by_hash(
        &self,
        hash: &sha256::Hash,
    ) -> Result<MutinyInvoice, MutinyError> {
        let nodes = self.nodes.lock().await;
        for (_, node) in nodes.iter() {
            if let Ok(inv) = node.get_invoice_by_hash(hash) {
                return Ok(inv);
            }
        }

        Err(MutinyError::NotFound)
    }

    /// Gets an invoice from the node manager.
    /// This includes sent and received invoices.
    pub async fn list_invoices(&self) -> Result<Vec<MutinyInvoice>, MutinyError> {
        let mut invoices: Vec<MutinyInvoice> = vec![];
        let nodes = self.nodes.lock().await;
        for (_, node) in nodes.iter() {
            if let Ok(mut invs) = node.list_invoices() {
                invoices.append(&mut invs)
            }
        }
        Ok(invoices)
    }

    pub async fn get_channel_closure(
        &self,
        user_channel_id: u128,
    ) -> Result<ChannelClosure, MutinyError> {
        let nodes = self.nodes.lock().await;
        for (_, node) in nodes.iter() {
            if let Ok(Some(closure)) = node.get_channel_closure(user_channel_id) {
                return Ok(closure);
            }
        }

        Err(MutinyError::NotFound)
    }

    pub async fn list_channel_closures(&self) -> Result<Vec<ChannelClosure>, MutinyError> {
        let mut channels: Vec<ChannelClosure> = vec![];
        let nodes = self.nodes.lock().await;
        for (_, node) in nodes.iter() {
            if let Ok(mut invs) = node.get_channel_closures() {
                channels.append(&mut invs)
            }
        }
        Ok(channels)
    }

    /// Opens a channel from our selected node to the given pubkey.
    /// The amount is in satoshis.
    ///
    /// The node must be online and have a connection to the peer.
    /// The wallet much have enough funds to open the channel.
    pub async fn open_channel(
        &self,
        from_node: &PublicKey,
        to_pubkey: Option<PublicKey>,
        amount: u64,
        user_channel_id: Option<u128>,
    ) -> Result<MutinyChannel, MutinyError> {
        let node = self.get_node(from_node).await?;

        let to_pubkey = match to_pubkey {
            Some(pubkey) => pubkey,
            None => {
                node.lsp_client
                    .as_ref()
                    .ok_or(MutinyError::PubkeyInvalid)?
                    .pubkey
            }
        };

        let outpoint = node
            .open_channel_with_timeout(to_pubkey, amount, user_channel_id, 60)
            .await?;

        let all_channels = node.channel_manager.list_channels();
        let found_channel = all_channels
            .iter()
            .find(|chan| chan.funding_txo.map(|a| a.into_bitcoin_outpoint()) == Some(outpoint));

        match found_channel {
            Some(channel) => Ok(channel.into()),
            None => Err(MutinyError::ChannelCreationFailed), // what should we do here?
        }
    }

    /// Opens a channel from our selected node to the given pubkey.
    /// It will spend the given utxos in full to fund the channel.
    ///
    /// The node must be online and have a connection to the peer.
    /// The UTXOs must all exist in the wallet.
    pub async fn sweep_utxos_to_channel(
        &self,
        user_chan_id: Option<u128>,
        from_node: &PublicKey,
        utxos: &[OutPoint],
        to_pubkey: Option<PublicKey>,
    ) -> Result<MutinyChannel, MutinyError> {
        let node = self.get_node(from_node).await?;

        let to_pubkey = match to_pubkey {
            Some(pubkey) => pubkey,
            None => {
                node.lsp_client
                    .as_ref()
                    .ok_or(MutinyError::PubkeyInvalid)?
                    .pubkey
            }
        };

        let outpoint = node
            .sweep_utxos_to_channel_with_timeout(user_chan_id, utxos, to_pubkey, 60)
            .await?;

        let all_channels = node.channel_manager.list_channels();
        let found_channel = all_channels
            .iter()
            .find(|chan| chan.funding_txo.map(|a| a.into_bitcoin_outpoint()) == Some(outpoint));

        match found_channel {
            Some(channel) => Ok(channel.into()),
            None => Err(MutinyError::ChannelCreationFailed), // what should we do here?
        }
    }

    /// Opens a channel from our selected node to the given pubkey.
    /// It will spend the all the on-chain utxo in full to fund the channel.
    ///
    /// The node must be online and have a connection to the peer.
    pub async fn sweep_all_to_channel(
        &self,
        user_chan_id: Option<u128>,
        from_node: &PublicKey,
        to_pubkey: Option<PublicKey>,
    ) -> Result<MutinyChannel, MutinyError> {
        let utxos = self
            .list_utxos()?
            .iter()
            .map(|u| u.outpoint)
            .collect::<Vec<_>>();

        self.sweep_utxos_to_channel(user_chan_id, from_node, &utxos, to_pubkey)
            .await
    }

    /// Closes a channel with the given outpoint.
    pub async fn close_channel(&self, outpoint: &OutPoint) -> Result<(), MutinyError> {
        let nodes = self.nodes.lock().await;
        let channel_opt: Option<(Arc<Node<S>>, ChannelDetails)> =
            nodes.iter().find_map(|(_, n)| {
                n.channel_manager
                    .list_channels()
                    .iter()
                    .find(|c| c.funding_txo.map(|f| f.into_bitcoin_outpoint()) == Some(*outpoint))
                    .map(|c| (n.clone(), c.clone()))
            });

        match channel_opt {
            Some((node, channel)) => {
                node.channel_manager
                    .close_channel(&channel.channel_id, &channel.counterparty.node_id)
                    .map_err(|e| {
                        log_error!(
                            self.logger,
                            "had an error closing channel {} with node {} : {e:?}",
                            &channel.channel_id.to_hex(),
                            &channel.counterparty.node_id.to_hex()
                        );
                        MutinyError::ChannelClosingFailed
                    })?;

                Ok(())
            }
            None => {
                log_error!(
                    self.logger,
                    "Channel not found with this transaction: {outpoint}",
                );
                Err(MutinyError::NotFound)
            }
        }
    }

    /// Lists all the channels for all the nodes in the node manager.
    pub async fn list_channels(&self) -> Result<Vec<MutinyChannel>, MutinyError> {
        let nodes = self.nodes.lock().await;
        let channels: Vec<ChannelDetails> = nodes
            .iter()
            .flat_map(|(_, n)| n.channel_manager.list_channels())
            .collect();

        let mutiny_channels: Vec<MutinyChannel> =
            channels.iter().map(MutinyChannel::from).collect();

        Ok(mutiny_channels)
    }

    /// Lists all the peers for all the nodes in the node manager.
    pub async fn list_peers(&self) -> Result<Vec<MutinyPeer>, MutinyError> {
        let peer_data = gossip::get_all_peers(&self.storage)?;

        // get peers saved in storage
        let mut storage_peers: Vec<MutinyPeer> = peer_data
            .iter()
            .map(|(node_id, metadata)| MutinyPeer {
                // node id should be safe here
                pubkey: PublicKey::from_slice(node_id.as_slice()).expect("Invalid pubkey"),
                connection_string: metadata.connection_string.clone(),
                alias: metadata.alias.clone(),
                color: metadata.color.clone(),
                label: metadata.label.clone(),
                is_connected: false,
            })
            .collect();

        let nodes = self.nodes.lock().await;

        // get peers we are connected to
        let connected_peers: Vec<PublicKey> = nodes
            .iter()
            .flat_map(|(_, n)| n.peer_manager.get_peer_node_ids())
            .collect();

        // correctly set is_connected
        for mut peer in &mut storage_peers {
            if connected_peers.contains(&peer.pubkey) {
                peer.is_connected = true;
            }
        }

        // add any connected peers that weren't in our storage,
        // likely new or inbound connections
        let mut missing: Vec<MutinyPeer> = Vec::new();
        for peer in connected_peers {
            if !storage_peers.iter().any(|p| p.pubkey == peer) {
                let new = MutinyPeer {
                    pubkey: peer,
                    connection_string: None,
                    alias: None,
                    color: None,
                    label: None,
                    is_connected: true,
                };
                missing.push(new);
            }
        }

        storage_peers.append(&mut missing);
        storage_peers.sort();

        Ok(storage_peers)
    }

    /// Gets the current bitcoin price in USD.
    pub async fn get_bitcoin_price(&self) -> Result<f32, MutinyError> {
        let now = crate::utils::now();

        let mut bitcoin_price_cache = self.bitcoin_price_cache.lock().await;

        let (price, timestamp) = match bitcoin_price_cache.as_ref() {
            Some((price, timestamp))
                if *timestamp + Duration::from_secs(BITCOIN_PRICE_CACHE_SEC) > now =>
            {
                // Cache is not expired
                (*price, *timestamp)
            }
            _ => {
                // Cache is either expired or empty, fetch new price
                match self.fetch_bitcoin_price().await {
                    Ok(new_price) => (new_price, now),
                    Err(e) => {
                        // If fetching price fails, return the cached price (if any)
                        if let Some((price, timestamp)) = bitcoin_price_cache.as_ref() {
                            log_warn!(self.logger, "price api failed, returning cached price");
                            (*price, *timestamp)
                        } else {
                            // If there is no cached price, return the error
                            log_error!(self.logger, "no cached price and price api failed");
                            return Err(e);
                        }
                    }
                }
            }
        };

        *bitcoin_price_cache = Some((price, timestamp));
        Ok(price)
    }

    async fn fetch_bitcoin_price(&self) -> Result<f32, MutinyError> {
        log_debug!(self.logger, "fetching new bitcoin price");

        let client = Client::builder()
            .build()
            .map_err(|_| MutinyError::BitcoinPriceError)?;

        let resp = client
            .get("https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd")
            .send()
            .await
            .map_err(|_| MutinyError::BitcoinPriceError)?;

        let response: CoingeckoResponse = resp
            .error_for_status()
            .map_err(|_| MutinyError::BitcoinPriceError)?
            .json()
            .await
            .map_err(|_| MutinyError::BitcoinPriceError)?;

        Ok(response.bitcoin.usd)
    }

    /// Retrieves the logs from storage.
    pub fn get_logs(&self) -> Result<Option<Vec<String>>, MutinyError> {
        self.logger.get_logs(&self.storage)
    }

    /// Resets the scorer and network graph. This can be useful if you get stuck in a bad state.
    pub async fn reset_router(&self) -> Result<(), MutinyError> {
        // if we're not connected to the db, start it up
        let needs_db_connection = !self.storage.clone().connected().unwrap_or(true);
        if needs_db_connection {
            self.storage.clone().start().await?;
        }

        // delete all the keys we use to store routing data
        self.storage
            .delete(&[GOSSIP_SYNC_TIME_KEY, NETWORK_GRAPH_KEY, PROB_SCORER_KEY])?;

        // shut back down after reading if it was already closed
        if needs_db_connection {
            self.storage.clone().stop();
        }

        Ok(())
    }

    /// Resets BDK's keychain tracker. This will require a re-sync of the blockchain.
    ///
    /// This can be useful if you get stuck in a bad state.
    pub async fn reset_onchain_tracker(&self) -> Result<(), MutinyError> {
        // if we're not connected to the db, start it up
        let needs_db_connection = !self.storage.clone().connected().unwrap_or(true);
        if needs_db_connection {
            self.storage.clone().start().await?;
        }

        // delete the bdk keychain store
        self.storage.delete(&[KEYCHAIN_STORE_KEY])?;

        // shut back down after reading if it was already closed
        if needs_db_connection {
            self.storage.clone().stop();
        }

        Ok(())
    }

    /// Exports the current state of the node manager to a json object.
    pub async fn export_json(&self) -> Result<Value, MutinyError> {
        let needs_db_connection = !self.storage.clone().connected().unwrap_or(true);
        if needs_db_connection {
            self.storage.clone().start().await?;
        }

        // get all the data from storage, scanning with prefix "" will get all keys
        let map = self.storage.scan("", None)?;
        let serde_map = serde_json::map::Map::from_iter(map.into_iter().filter(|(k, _)| {
            // filter out logs and network graph
            // these are really big and not needed for export
            !matches!(k.as_str(), LOGGING_KEY | NETWORK_GRAPH_KEY)
        }));

        // shut back down after reading if it was already closed
        if needs_db_connection {
            self.storage.clone().stop();
        }

        Ok(Value::Object(serde_map))
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
struct CoingeckoResponse {
    pub bitcoin: CoingeckoPrice,
}

#[derive(Deserialize, Clone, Copy, Debug)]
struct CoingeckoPrice {
    pub usd: f32,
}

// This will create a new node with a node manager and return the PublicKey of the node created.
pub(crate) async fn create_new_node_from_node_manager<S: MutinyStorage>(
    node_manager: &NodeManager<S>,
) -> Result<NodeIdentity, MutinyError> {
    // Begin with a mutex lock so that nothing else can
    // save or alter the node list while it is about to
    // be saved.
    let mut node_mutex = node_manager.node_storage.lock().await;

    // Get the current nodes and their bip32 indices
    // so that we can create another node with the next.
    // Always get it from our storage, the node_mutex is
    // mostly for read only and locking.
    let mut existing_nodes = node_manager.storage.get_nodes()?;
    let next_node_index = match existing_nodes
        .nodes
        .iter()
        .max_by_key(|(_, v)| v.child_index)
    {
        None => 0,
        Some((_, v)) => v.child_index + 1,
    };

    // Create and save a new node using the next child index
    let next_node_uuid = Uuid::new_v4().to_string();

    let lsp = if node_manager.lsp_clients.is_empty() {
        log_info!(
            node_manager.logger,
            "no lsp saved and no lsp clients available"
        );
        None
    } else {
        log_info!(node_manager.logger, "no lsp saved, picking random one");
        // If we don't have an lsp saved we should pick a random
        // one from our client list and save it for next time
        let rand = rand::random::<usize>() % node_manager.lsp_clients.len();
        Some(node_manager.lsp_clients[rand].url.clone())
    };

    let next_node = NodeIndex {
        child_index: next_node_index,
        lsp,
        archived: Some(false),
    };

    existing_nodes
        .nodes
        .insert(next_node_uuid.clone(), next_node.clone());

    node_manager.storage.insert_nodes(existing_nodes.clone())?;
    node_mutex.nodes = existing_nodes.nodes.clone();

    // now create the node process and init it
    #[cfg(target_arch = "wasm32")]
    let new_node_res = Node::new(
        next_node_uuid.clone(),
        &next_node,
        node_manager.stop.clone(),
        &node_manager.mnemonic,
        node_manager.storage.clone(),
        node_manager.gossip_sync.clone(),
        node_manager.scorer.clone(),
        node_manager.chain.clone(),
        node_manager.fee_estimator.clone(),
        node_manager.wallet.clone(),
        node_manager.network,
        node_manager.esplora.clone(),
        &node_manager.lsp_clients,
        node_manager.logger.clone(),
        node_manager.websocket_proxy_addr.clone(),
    )
    .await;

    #[cfg(not(target_arch = "wasm32"))]
    let new_node_res = Node::new(
        next_node_uuid.clone(),
        &next_node,
        node_manager.stop.clone(),
        &node_manager.mnemonic,
        node_manager.storage.clone(),
        node_manager.gossip_sync.clone(),
        node_manager.scorer.clone(),
        node_manager.chain.clone(),
        node_manager.fee_estimator.clone(),
        node_manager.wallet.clone(),
        node_manager.network,
        node_manager.esplora.clone(),
        &node_manager.lsp_clients,
        node_manager.logger.clone(),
    )
    .await;

    let new_node = match new_node_res {
        Ok(new_node) => new_node,
        Err(e) => return Err(e),
    };

    let node_pubkey = new_node.pubkey;
    node_manager
        .nodes
        .clone()
        .lock()
        .await
        .insert(node_pubkey, Arc::new(new_node));

    Ok(NodeIdentity {
        uuid: next_node_uuid.clone(),
        pubkey: node_pubkey,
    })
}

#[cfg(test)]
mod tests {
    use crate::nodemanager::{
        ActivityItem, ChannelClosure, MutinyInvoice, NodeManager, TransactionDetails,
    };
    use crate::{keymanager::generate_seed, MutinyWalletConfig};
    use bdk::chain::ConfirmationTime;
    use bitcoin::hashes::hex::{FromHex, ToHex};
    use bitcoin::hashes::{sha256, Hash};
    use bitcoin::secp256k1::PublicKey;
    use bitcoin::{Network, PackedLockTime, Transaction, TxOut, Txid};
    use lightning::ln::PaymentHash;
    use lightning_invoice::Invoice;
    use std::str::FromStr;

    use crate::test_utils::*;

    use crate::event::{HTLCStatus, MillisatAmount, PaymentInfo};
    use crate::storage::MemoryStorage;
    use wasm_bindgen_test::{wasm_bindgen_test as test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    const BOLT_11: &str = "lntbs1m1pjrmuu3pp52hk0j956d7s8azaps87amadshnrcvqtkvk06y2nue2w69g6e5vasdqqcqzpgxqyz5vqsp5wu3py6257pa3yzarw0et2200c08r5fu6k3u94yfwmlnc8skdkc9s9qyyssqc783940p82c64qq9pu3xczt4tdxzex9wpjn54486y866aayft2cxxusl9eags4cs3kcmuqdrvhvs0gudpj5r2a6awu4wcq29crpesjcqhdju55";

    #[test]
    async fn create_node_manager() {
        let test_name = "create_node_manager";
        log!("{}", test_name);

        let storage = MemoryStorage::new(Some(uuid::Uuid::new_v4().to_string()));

        assert!(!NodeManager::has_node_manager(storage.clone()));
        let c = MutinyWalletConfig::new(
            None,
            #[cfg(target_arch = "wasm32")]
            None,
            Some(Network::Regtest),
            None,
            None,
            None,
        );
        NodeManager::new(c, storage.clone())
            .await
            .expect("node manager should initialize");
        assert!(NodeManager::has_node_manager(storage));
    }

    #[test]
    async fn correctly_show_seed() {
        let test_name = "correctly_show_seed";
        log!("{}", test_name);

        let seed = generate_seed(12).expect("Failed to gen seed");
        let c = MutinyWalletConfig::new(
            Some(seed.clone()),
            #[cfg(target_arch = "wasm32")]
            None,
            Some(Network::Regtest),
            None,
            None,
            None,
        );
        let nm = NodeManager::new(c, ()).await.unwrap();

        assert_eq!(seed, nm.show_seed());
    }

    #[test]
    async fn created_new_nodes() {
        let test_name = "created_new_nodes";
        log!("{}", test_name);

        let storage = MemoryStorage::new(Some(uuid::Uuid::new_v4().to_string()));
        let seed = generate_seed(12).expect("Failed to gen seed");
        let c = MutinyWalletConfig::new(
            Some(seed),
            #[cfg(target_arch = "wasm32")]
            None,
            Some(Network::Regtest),
            None,
            None,
            None,
        );
        let nm = NodeManager::new(c, storage)
            .await
            .expect("node manager should initialize");

        {
            let node_identity = nm.new_node().await.expect("should create new node");
            let node_storage = nm.node_storage.lock().await;
            assert_ne!("", node_identity.uuid);
            assert_ne!("", node_identity.pubkey.to_string());
            assert_eq!(1, node_storage.nodes.len());

            let retrieved_node = node_storage.nodes.get(&node_identity.uuid).unwrap();
            assert_eq!(0, retrieved_node.child_index);
        }

        {
            let node_identity = nm.new_node().await.expect("node manager should initialize");
            let node_storage = nm.node_storage.lock().await;

            assert_ne!("", node_identity.uuid);
            assert_ne!("", node_identity.pubkey.to_string());
            assert_eq!(2, node_storage.nodes.len());

            let retrieved_node = node_storage.nodes.get(&node_identity.uuid).unwrap();
            assert_eq!(1, retrieved_node.child_index);
        }
    }

    #[test]
    async fn created_label_transaction() {
        let test_name = "created_new_nodes";
        log!("{}", test_name);

        let storage = MemoryStorage::new(Some(uuid::Uuid::new_v4().to_string()));
        let seed = generate_seed(12).expect("Failed to gen seed");
        let c = MutinyWalletConfig::new(
            Some(seed),
            #[cfg(target_arch = "wasm32")]
            None,
            Some(Network::Signet),
            None,
            None,
            None,
        );
        let nm = NodeManager::new(c, storage)
            .await
            .expect("node manager should initialize");

        let labels = vec![String::from("label1"), String::from("label2")];

        let address = nm
            .get_new_address(labels.clone())
            .expect("should create new address");

        let fake_tx = Transaction {
            version: 2,
            lock_time: PackedLockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: 1_000_000,
                script_pubkey: address.script_pubkey(),
            }],
        };

        // insert fake tx into wallet
        {
            let mut wallet = nm.wallet.wallet.try_write().unwrap();
            wallet
                .insert_tx(fake_tx.clone(), ConfirmationTime::Unconfirmed)
                .unwrap();
            wallet.commit().unwrap();
        }

        let txs = nm.list_onchain().expect("should list onchain txs");
        let tx_opt = nm
            .get_transaction(fake_tx.txid())
            .expect("should get transaction");

        assert_eq!(txs.len(), 1);
        let tx = &txs[0];
        assert_eq!(tx.txid, fake_tx.txid());
        assert_eq!(tx.labels, labels);

        assert!(tx_opt.is_some());
        let tx = tx_opt.unwrap();
        assert_eq!(tx.txid, fake_tx.txid());
        assert_eq!(tx.labels, labels);
    }

    #[test]
    fn test_bolt11_payment_info_into_mutiny_invoice() {
        let preimage: [u8; 32] =
            FromHex::from_hex("7600f5a9ad72452dea7ad86dabbc9cb46be96a1a2fcd961e041d066b38d93008")
                .unwrap();
        let secret: [u8; 32] =
            FromHex::from_hex("7722126954f07b120ba373f2b529efc3ce3a279ab4785a912edfe783c2cdb60b")
                .unwrap();

        let payment_hash = sha256::Hash::from_hex(
            "55ecf9169a6fa07e8ba181fdddf5b0bcc7860176659fa22a7cca9da2a359a33b",
        )
        .unwrap();

        let invoice = Invoice::from_str(BOLT_11).unwrap();

        let labels = vec!["label1".to_string(), "label2".to_string()];

        let payment_info = PaymentInfo {
            preimage: Some(preimage),
            secret: Some(secret),
            status: HTLCStatus::Succeeded,
            amt_msat: MillisatAmount(Some(100_000_000)),
            fee_paid_msat: None,
            bolt11: Some(invoice.clone()),
            payee_pubkey: None,
            last_update: 1681781585,
        };

        let expected: MutinyInvoice = MutinyInvoice {
            bolt11: Some(invoice),
            description: None,
            payment_hash,
            preimage: Some(preimage.to_hex()),
            payee_pubkey: None,
            amount_sats: Some(100_000),
            expire: 1681781649 + 86400,
            paid: true,
            fees_paid: None,
            inbound: true,
            labels: labels.clone(),
            last_updated: 1681781585,
        };

        let actual = MutinyInvoice::from(
            payment_info,
            PaymentHash(payment_hash.into_inner()),
            true,
            labels,
        )
        .unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_keysend_payment_info_into_mutiny_invoice() {
        let preimage: [u8; 32] =
            FromHex::from_hex("7600f5a9ad72452dea7ad86dabbc9cb46be96a1a2fcd961e041d066b38d93008")
                .unwrap();

        let payment_hash = sha256::Hash::from_hex(
            "55ecf9169a6fa07e8ba181fdddf5b0bcc7860176659fa22a7cca9da2a359a33b",
        )
        .unwrap();

        let pubkey = PublicKey::from_str(
            "02465ed5be53d04fde66c9418ff14a5f2267723810176c9212b722e542dc1afb1b",
        )
        .unwrap();

        let payment_info = PaymentInfo {
            preimage: Some(preimage),
            secret: None,
            status: HTLCStatus::Succeeded,
            amt_msat: MillisatAmount(Some(100_000)),
            fee_paid_msat: Some(1_000),
            bolt11: None,
            payee_pubkey: Some(pubkey),
            last_update: 1681781585,
        };

        let expected: MutinyInvoice = MutinyInvoice {
            bolt11: None,
            description: None,
            payment_hash,
            preimage: Some(preimage.to_hex()),
            payee_pubkey: Some(pubkey),
            amount_sats: Some(100),
            expire: 1681781585,
            paid: true,
            fees_paid: Some(1),
            inbound: false,
            labels: vec![],
            last_updated: 1681781585,
        };

        let actual = MutinyInvoice::from(
            payment_info,
            PaymentHash(payment_hash.into_inner()),
            false,
            vec![],
        )
        .unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_sort_activity_item() {
        let preimage: [u8; 32] =
            FromHex::from_hex("7600f5a9ad72452dea7ad86dabbc9cb46be96a1a2fcd961e041d066b38d93008")
                .unwrap();

        let payment_hash = sha256::Hash::from_hex(
            "55ecf9169a6fa07e8ba181fdddf5b0bcc7860176659fa22a7cca9da2a359a33b",
        )
        .unwrap();

        let pubkey = PublicKey::from_str(
            "02465ed5be53d04fde66c9418ff14a5f2267723810176c9212b722e542dc1afb1b",
        )
        .unwrap();

        let closure: ChannelClosure = ChannelClosure {
            user_channel_id: None,
            channel_id: None,
            node_id: None,
            reason: "".to_string(),
            timestamp: 1686258926,
        };

        let tx1: TransactionDetails = TransactionDetails {
            transaction: None,
            txid: Txid::all_zeros(),
            received: 0,
            sent: 0,
            fee: None,
            confirmation_time: ConfirmationTime::Unconfirmed,
            labels: vec![],
        };

        let tx2: TransactionDetails = TransactionDetails {
            transaction: None,
            txid: Txid::all_zeros(),
            received: 0,
            sent: 0,
            fee: None,
            confirmation_time: ConfirmationTime::Confirmed {
                height: 1,
                time: 1234,
            },
            labels: vec![],
        };

        let invoice1: MutinyInvoice = MutinyInvoice {
            bolt11: None,
            description: None,
            payment_hash,
            preimage: Some(preimage.to_hex()),
            payee_pubkey: Some(pubkey),
            amount_sats: Some(100),
            expire: 1681781585,
            paid: true,
            fees_paid: Some(1),
            inbound: false,
            labels: vec![],
            last_updated: 1681781585,
        };

        let invoice2: MutinyInvoice = MutinyInvoice {
            bolt11: None,
            description: None,
            payment_hash,
            preimage: Some(preimage.to_hex()),
            payee_pubkey: Some(pubkey),
            amount_sats: Some(100),
            expire: 1681781585,
            paid: true,
            fees_paid: Some(1),
            inbound: false,
            labels: vec![],
            last_updated: 1781781585,
        };

        let mut vec = vec![
            ActivityItem::OnChain(tx1.clone()),
            ActivityItem::OnChain(tx2.clone()),
            ActivityItem::Lightning(Box::new(invoice1.clone())),
            ActivityItem::Lightning(Box::new(invoice2.clone())),
            ActivityItem::ChannelClosed(closure.clone()),
        ];
        vec.sort();

        assert_eq!(
            vec,
            vec![
                ActivityItem::OnChain(tx2),
                ActivityItem::Lightning(Box::new(invoice1)),
                ActivityItem::ChannelClosed(closure),
                ActivityItem::Lightning(Box::new(invoice2)),
                ActivityItem::OnChain(tx1),
            ]
        );
    }
}
