/*!
# Transaction Stream Processor - Ties everything together and does the heavy lifting

This module holds the main struct that processes transactions from a [`TransactionStream`],
a default implementation of a [`TransactionHandler`], and a struct that processes events in a [`TransactionHandler`].
*/

use crate::{
    error::{
        EventHandlerError, TransactionHandlerError,
        TransactionStreamProcessorError,
    },
    event_handler::{EventHandlerContext, HandlerRegistry, State},
    logger::{DefaultLogger, Logger},
    models::Transaction,
    stream::TransactionStream,
    transaction_handler::{TransactionHandler, TransactionHandlerContext},
};
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
use tokio::sync::RwLock;

// Default retry intervals for transactions and events.
const TRANSACTION_RETRY_INTERVAL_MS: u64 = 10_000;
const EVENT_RETRY_INTERVAL_MS: u64 = 10_000;

/// The main struct that processes transactions from a [`TransactionStream`].
/// It processes transactions by calling a [`TransactionHandler`] for each transaction
/// that has at least one event with an [`EventHandler`][crate::event_handler::EventHandler] registered.
/// It can be created using a builder pattern, where you can set the [`TransactionHandler`],
/// retry intervals, and logger.
///
/// If you don't set a transaction handler explicitly, the processor will use a default handler
/// that simply calls [`EventProcessor::process_events`] on the transaction, without any custom logic.
#[allow(non_camel_case_types)]
pub struct TransactionStreamProcessor<STREAM, STATE>
where
    STREAM: TransactionStream,
    STATE: State,
{
    transaction_stream: STREAM,
    handler_registry: HandlerRegistry,
    transaction_handler: Box<dyn TransactionHandler<STATE>>,
    state: STATE,
    transaction_retry_delay: Duration,
    event_retry_delay: Duration,
    logger: Option<Arc<RwLock<Box<dyn Logger>>>>,
    periodic_logging_joinhandle: Option<tokio::task::JoinHandle<()>>,
}

#[allow(non_camel_case_types)]
impl<STREAM, STATE> TransactionStreamProcessor<STREAM, STATE>
where
    STREAM: TransactionStream,
    STATE: State,
{
    /// Creates a new [`TransactionStreamProcessor`] with the given
    /// [`TransactionStream`], [`HandlerRegistry`], and `STATE`.
    ///
    /// - The [`TransactionHandler`] is set to a default handler that
    /// simply calls [`EventProcessor::process_events`] on the transaction, without
    /// any custom logic.
    ///
    /// - The default retry intervals for transactions and events are
    /// set to 10 seconds.
    ///
    /// - The logger is set to a default logger that logs to stdout.
    ///
    /// Change the default handler, retry intervals, or logger using
    /// the builder methods.
    pub fn new(
        transaction_stream: STREAM,
        handler_registry: HandlerRegistry,
        state: STATE,
    ) -> Self {
        TransactionStreamProcessor {
            transaction_stream,
            handler_registry,
            transaction_handler: Box::new(DefaultTransactionHandler),
            state: state,
            transaction_retry_delay: Duration::from_millis(
                TRANSACTION_RETRY_INTERVAL_MS,
            ),
            event_retry_delay: Duration::from_millis(EVENT_RETRY_INTERVAL_MS),
            logger: Some(Arc::new(RwLock::new(Box::new(
                DefaultLogger::default(),
            )))),
            periodic_logging_joinhandle: None,
        }
    }

    /// Sets the [`TransactionHandler`] for the processor.
    /// This handler is called for each transaction that has at least one event which
    /// has event handlers registered.
    pub fn transaction_handler(
        self,
        transaction_handler: impl TransactionHandler<STATE>,
    ) -> Self {
        TransactionStreamProcessor {
            transaction_handler: Box::new(transaction_handler),
            ..self
        }
    }

    /// Sets the retry delay for transactions that fail to process and return a `TransactionRetryError`
    /// (see [`crate::error::TransactionHandlerError`]).
    pub fn transaction_retry_delay(
        self,
        transaction_retry_delay: Duration,
    ) -> Self {
        TransactionStreamProcessor {
            transaction_retry_delay,
            ..self
        }
    }

    /// Sets the retry delay for events that fail to process and return an `EventRetryError`.
    /// (see [`crate::error::EventHandlerError`]).
    pub fn event_retry_delay(self, event_retry_delay: Duration) -> Self {
        TransactionStreamProcessor {
            event_retry_delay,
            ..self
        }
    }

    /// Sets the logger for the processor. It should implement the [`Logger`] trait.
    pub fn logger(self, logger: impl Logger + 'static) -> Self {
        TransactionStreamProcessor {
            logger: Some(Arc::new(RwLock::new(Box::new(logger)))),
            ..self
        }
    }

    /// Sets the logger for the processor to the default logger, but with
    /// a custom report interval given by `interval`.
    pub fn default_logger_with_report_interval(
        self,
        interval: Duration,
    ) -> Self {
        TransactionStreamProcessor {
            logger: Some(Arc::new(RwLock::new(Box::new(
                DefaultLogger::with_custom_report_interval(interval),
            )))),
            ..self
        }
    }

    /// Disables logging for the processor by setting the logger to `None`.
    pub fn disable_logging(self) -> Self {
        TransactionStreamProcessor {
            logger: None,
            ..self
        }
    }

    /// Processes a single transaction.
    ///
    /// Returns:
    /// - `Ok(true)` if the transaction was processed successfully,
    /// - `Ok(false)` if the transaction was skipped because it had no handlers,
    /// - `Err(TransactionStreamProcessorError)` if an unrecoverable error occurred somewhere in a handler.
    pub async fn process_transaction(
        &mut self,
        transaction: &Transaction,
    ) -> Result<bool, TransactionStreamProcessorError> {
        // Find out if there are any events inside this transaction
        // that have a handler registered.
        let handler_exists = transaction.events.iter().any(|event| {
            self.handler_registry
                .handler_exists(event.emitter.address(), &event.name)
        });

        if let Some(logger) = &self.logger {
            logger
                .write()
                .await
                .receive_transaction(transaction, handler_exists, false)
                .await;
        }

        if !handler_exists {
            // If there are no handlers for any of the events in this transaction,
            // we can skip processing it.
            return Ok(false);
        }

        // Keep trying to handle the transaction in case
        // the handler requests this through a TransactionHandlerError.
        while let Err(err) = self
            .transaction_handler
            .handle(TransactionHandlerContext {
                state: &mut self.state,
                transaction,
                event_processor: &mut EventProcessor {
                    event_retry_interval: self.event_retry_delay,
                    transaction,
                    logger: &self.logger,
                },
                handler_registry: &mut self.handler_registry,
            })
            .await
        {
            match err {
                TransactionHandlerError::TransactionRetryError(e) => {
                    if let Some(logger) = &self.logger {
                        logger
                            .write()
                            .await
                            .transaction_retry_error(
                                transaction,
                                &e,
                                self.transaction_retry_delay,
                            )
                            .await;
                    }
                    tokio::time::sleep(self.transaction_retry_delay).await;
                    if let Some(logger) = &self.logger {
                        logger
                            .write()
                            .await
                            .receive_transaction(
                                transaction,
                                handler_exists,
                                true,
                            )
                            .await;
                    }
                    continue;
                }
                TransactionHandlerError::UnrecoverableError(e) => {
                    if let Some(logger) = &self.logger {
                        logger.write().await.unrecoverable_error(&e).await;
                    }
                    return Err(
                        TransactionStreamProcessorError::UnrecoverableError(e),
                    );
                }
            }
        }

        Ok(true)
    }

    /// Starts processing transactions from the [`TransactionStream`].
    pub async fn run(&mut self) -> Result<(), TransactionStreamProcessorError> {
        // Start the transaction stream and get a receiver.
        // This often involves starting a task that fetches transactions
        // from a remote source and sends them to the receiver.
        let mut receiver =
            self.transaction_stream.start().await.map_err(|error| {
                TransactionStreamProcessorError::UnrecoverableError(error)
            })?;
        let logger = self.logger.clone();
        self.periodic_logging_joinhandle = if let Some(logger) = logger {
            let interval = logger.read().await.periodic_report_interval();
            Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    logger.read().await.periodic_report().await;
                }
            }))
        } else {
            None
        };
        // Process transactions as they arrive.
        while let Some(transaction) = receiver.recv().await {
            let handled = self.process_transaction(&transaction).await?;
            if let Some(logger) = &self.logger {
                logger
                    .write()
                    .await
                    .finish_transaction(&transaction, handled)
                    .await;
            }
        }
        // If the transmitting half of the channel is dropped,
        // the receiver will return None and we will exit the loop.
        // The processor will exit gracefully.

        if let Some(handle) = self.periodic_logging_joinhandle.take() {
            handle.abort();
        }
        Ok(())
    }
}

/// A default transaction handler that simply calls [`EventProcessor::process_events`]
/// on the transaction, without any custom logic.
#[derive(Clone)]
struct DefaultTransactionHandler;

#[async_trait]
impl<STATE> TransactionHandler<STATE> for DefaultTransactionHandler
where
    STATE: State,
{
    async fn handle(
        &self,
        input: TransactionHandlerContext<'_, STATE>,
    ) -> Result<(), TransactionHandlerError> {
        input
            .event_processor
            .process_events(input.state, input.handler_registry, &mut ())
            .await?;
        Ok(())
    }
}

/// The [`EventProcessor`]'s only purpose is to have a convenience method to process events in a transaction.
/// The user calls [`EventProcessor::process_events`] when implementing a custom [`TransactionHandler`].
/// It will iterate over the events in the transaction and call the appropriate event handlers.
/// It handles retries for events that fail to process, and calls logging hooks.
/// It is highly recommended to use this method when implementing a custom [`TransactionHandler`].
pub struct EventProcessor<'a> {
    event_retry_interval: Duration,
    transaction: &'a Transaction,
    logger: &'a Option<Arc<RwLock<Box<dyn Logger>>>>,
}

#[allow(non_camel_case_types)]
impl<'a> EventProcessor<'a> {
    pub async fn process_events<STATE: State, TRANSACTION_CONTEXT: 'static>(
        &self,
        state: &mut STATE,
        handler_registry: &mut HandlerRegistry,
        transaction_context: &mut TRANSACTION_CONTEXT,
    ) -> Result<(), EventHandlerError> {
        for event in self.transaction.events.iter() {
            let handler_exists = handler_registry
                .handler_exists(event.emitter.address(), &event.name);
            if !handler_exists {
                continue;
            }
            if let Some(logger) = self.logger {
                logger
                    .write()
                    .await
                    .receive_event(
                        self.transaction,
                        event,
                        handler_exists,
                        false,
                    )
                    .await;
            }
            let event_handler = {
                handler_registry
                    .get_handler::<STATE, TRANSACTION_CONTEXT>(
                        event.emitter.address(),
                        &event.name,
                    )
                    .unwrap()
            };
            let event_handler = event_handler.clone();
            while let Err(err) = event_handler
                .handle(
                    EventHandlerContext {
                        state: state,
                        transaction: self.transaction,
                        event,
                        handler_registry,
                        transaction_context,
                    },
                    &event.binary_sbor_data,
                )
                .await
            {
                match err {
                    EventHandlerError::EventRetryError(e) => {
                        if let Some(logger) = self.logger {
                            logger
                                .write()
                                .await
                                .event_retry_error(
                                    self.transaction,
                                    event,
                                    &e,
                                    self.event_retry_interval,
                                )
                                .await;
                        }
                        tokio::time::sleep(self.event_retry_interval).await;
                        if let Some(logger) = self.logger {
                            logger
                                .write()
                                .await
                                .receive_event(
                                    self.transaction,
                                    event,
                                    handler_exists,
                                    true,
                                )
                                .await;
                        }
                        continue;
                    }
                    _ => {
                        return Err(err);
                    }
                }
            }
            if let Some(logger) = self.logger {
                logger
                    .write()
                    .await
                    .finish_event(self.transaction, event, handler_exists)
                    .await;
            }
        }
        Ok(())
    }
}
