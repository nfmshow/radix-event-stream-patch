//! A transaction stream that fetches transactions from a Radix Gateway API.

use std::time::Duration;

use crate::{
    encodings::programmatic_json_to_bytes,
    models::{Event, EventEmitter, Transaction},
    stream::TransactionStream,
};

use async_trait::async_trait;
use radix_client::{
    gateway::{
        models::{CommittedTransactionInfo, EventEmitterIdentifier},
        stream::stream_client::TransactionStreamAsync,
    },
    GatewayClientAsync,
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::sleep,
};

const DEFAULT_CAUGHT_UP_TIMEOUT_MS: u64 = 500;
const PUBLIC_MAINNET_GATEWAY_URL: &str = "https://mainnet.radixdlt.com";
const DEFAULT_STATE_VERSION: u64 = 1;
const DEFAULT_PAGE_SIZE: u32 = 100;
const DEFAULT_BUFFER_CAPACITY: u64 = 10000;

impl From<radix_client::gateway::models::Event> for Event {
    fn from(event: radix_client::gateway::models::Event) -> Self {
        let emitter = match event.emitter {
            EventEmitterIdentifier::Method { entity, .. } => {
                EventEmitter::Method {
                    entity_address: entity.entity_address,
                }
            }
            EventEmitterIdentifier::Function {
                package_address,
                blueprint_name,
            } => EventEmitter::Function {
                package_address,
                blueprint_name,
            },
        };
        Self {
            name: event.name,
            emitter,
            binary_sbor_data: programmatic_json_to_bytes(&event.data).expect(
                "Should always able to convert Programmatic JSON to binary SBOR",
            ),
        }
    }
}

impl From<CommittedTransactionInfo> for Transaction {
    fn from(transaction: CommittedTransactionInfo) -> Self {
        Self {
            intent_hash: transaction
                .intent_hash
                .expect("Transaction should have tx id"),
            state_version: transaction.state_version,
            confirmed_at: transaction.confirmed_at,
            events: transaction
                .receipt
                .expect("Transaction should have receipt")
                .events
                .expect("Transaction receipt should have events")
                .into_iter()
                .map(|event| event.into())
                .collect(),
        }
    }
}

/// A struct that fetches transactions from a Radix Gateway API.
/// It uses a builder pattern for initialization, with some sensible defaults.
#[derive(Debug)]
pub struct GatewayTransactionStream {
    gateway_url: String,
    from_state_version: u64,
    limit_per_page: u32,
    buffer_capacity: u64,
    caught_up_timeout_ms: u64,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Default for GatewayTransactionStream {
    fn default() -> Self {
        Self {
            gateway_url: PUBLIC_MAINNET_GATEWAY_URL.to_string(),
            from_state_version: DEFAULT_STATE_VERSION,
            limit_per_page: DEFAULT_PAGE_SIZE,
            buffer_capacity: DEFAULT_BUFFER_CAPACITY,
            caught_up_timeout_ms: DEFAULT_CAUGHT_UP_TIMEOUT_MS,
            handle: None,
        }
    }
}

impl GatewayTransactionStream {
    /// Creates a new GatewayTransactionStream with default settings.
    pub fn new() -> Self {
        Default::default()
    }

    /// Sets the state version to start fetching transactions from.
    /// This is inclusive, so the transaction with this state version will be included.
    pub fn from_state_version(mut self, from_state_version: u64) -> Self {
        self.from_state_version = from_state_version;
        self
    }

    /// Sets the URL of the Radix Gateway API to fetch transactions from.
    pub fn gateway_url(mut self, gateway_url: String) -> Self {
        self.gateway_url = gateway_url;
        self
    }

    /// Sets the number of transactions to fetch per page.
    pub fn limit_per_page(mut self, limit_per_page: u32) -> Self {
        self.limit_per_page = limit_per_page;
        self
    }

    /// Sets the buffer capacity of the channel through which transactions are sent to the transaction processor.
    /// This is the maximum number of transactions that can be buffered before the processor starts to block.
    /// If the stream is producing transactions faster than the transaction processor can consume them,
    /// this buffer will fill up.
    /// You may want to play with this value, based on the performance of the API and the transaction processor.
    pub fn buffer_capacity(mut self, buffer_capacity: u64) -> Self {
        self.buffer_capacity = buffer_capacity;
        self
    }

    /// Sets the timeout in milliseconds to wait for after each poll of the gateway API when the stream is caught up.
    /// Tweak this to prevent the stream from polling the API too frequently while there are no transactions to fetch.
    pub fn caught_up_timeout_ms(mut self, caught_up_timeout_ms: u64) -> Self {
        self.caught_up_timeout_ms = caught_up_timeout_ms;
        self
    }
}

/// A fetcher which is passed to the new task created by the stream.
struct GatewayFetcher {
    stream: TransactionStreamAsync,
    caught_up_timeout_ms: u64,
    tx: Sender<Transaction>,
}

impl GatewayFetcher {
    pub fn new(
        gateway_url: String,
        from_state_version: u64,
        limit_per_page: u32,
        caught_up_timeout_ms: u64,
        tx: Sender<Transaction>,
    ) -> Self {
        let client = GatewayClientAsync::new(gateway_url);
        let stream = TransactionStreamAsync::new(
            &client,
            from_state_version,
            limit_per_page,
        );
        Self {
            stream,
            tx,
            caught_up_timeout_ms,
        }
    }

    /// Fetches transactions from the gateway and sends them to the transaction processor.
    async fn run(&mut self) {
        loop {
            let mut response = self.stream.next().await;
            while let Err(err) = response {
                log::warn!(
                    "Error fetching transactions: {:?}\n Trying again...",
                    err
                );
                response = self.stream.next().await;
            }
            let response = response.unwrap();
            if response.items.is_empty() {
                sleep(Duration::from_millis(self.caught_up_timeout_ms)).await;
            }
            let transactions: Vec<Transaction> =
                response.items.into_iter().map(|item| item.into()).collect();
            for transaction in transactions {
                // Stop fetching if the receiving end is closed
                if self.tx.send(transaction).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[async_trait]
impl TransactionStream for GatewayTransactionStream {
    async fn start(&mut self) -> Result<Receiver<Transaction>, anyhow::Error> {
        let (tx, rx) =
            tokio::sync::mpsc::channel(self.buffer_capacity as usize);
        let mut fetcher = GatewayFetcher::new(
            self.gateway_url.clone(),
            self.from_state_version,
            self.limit_per_page,
            self.caught_up_timeout_ms,
            tx,
        );
        let handle = tokio::spawn(async move { fetcher.run().await });
        self.handle = Some(handle);
        Ok(rx)
    }

    async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
