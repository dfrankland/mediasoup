use crate::data_producer::DataProducerId;
use crate::data_structures::AppData;
use crate::event_handlers::{Bag, HandlerId};
use crate::messages::{
    DataConsumerCloseRequest, DataConsumerDumpRequest, DataConsumerGetBufferedAmountRequest,
    DataConsumerGetStatsRequest, DataConsumerInternal,
    DataConsumerSetBufferedAmountLowThresholdData,
    DataConsumerSetBufferedAmountLowThresholdRequest,
};
use crate::sctp_parameters::SctpStreamParameters;
use crate::transport::Transport;
use crate::uuid_based_wrapper_type;
use crate::worker::{Channel, RequestError, SubscriptionHandler};
use async_executor::Executor;
use log::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

uuid_based_wrapper_type!(DataConsumerId);

// TODO: Split into 2 for Direct and others or make an enum
#[derive(Debug)]
pub struct DataConsumerOptions {
    // The id of the DataProducer to consume.
    pub(crate) data_producer_id: DataProducerId,
    /// Just if consuming over SCTP.
    /// Whether data messages must be received in order. If true the messages will be sent reliably.
    /// Defaults to the value in the DataProducer if it has type 'Sctp' or to true if it has type
    /// 'Direct'.
    pub(crate) ordered: Option<bool>,
    /// Just if consuming over SCTP.
    /// When ordered is false indicates the time (in milliseconds) after which a SCTP packet will
    /// stop being retransmitted.
    /// Defaults to the value in the DataProducer if it has type 'Sctp' or unset if it has type
    /// 'Direct'.
    pub(crate) max_packet_life_time: Option<u16>,
    /// Just if consuming over SCTP.
    /// When ordered is false indicates the maximum number of times a packet will be retransmitted.
    /// Defaults to the value in the DataProducer if it has type 'Sctp' or unset if it has type
    /// 'Direct'.
    pub(crate) max_retransmits: Option<u16>,
    /// Custom application data.
    pub app_data: AppData,
}

impl DataConsumerOptions {
    /// Inherits parameters of corresponding DataProducer.
    pub fn new_sctp(data_producer_id: DataProducerId) -> Self {
        Self {
            data_producer_id,
            ordered: None,
            max_packet_life_time: None,
            max_retransmits: None,
            app_data: AppData::default(),
        }
    }

    /// For DirectTransport.
    pub fn new_direct(data_producer_id: DataProducerId) -> Self {
        Self {
            data_producer_id,
            ordered: Some(true),
            max_packet_life_time: None,
            max_retransmits: None,
            app_data: AppData::default(),
        }
    }

    /// Messages will be sent reliably in order.
    pub fn new_sctp_ordered(data_producer_id: DataProducerId) -> Self {
        Self {
            data_producer_id,
            ordered: None,
            max_packet_life_time: None,
            max_retransmits: None,
            app_data: AppData::default(),
        }
    }

    /// Messages will be sent unreliably with time (in milliseconds) after which a SCTP packet will
    /// stop being retransmitted.
    pub fn new_sctp_unordered_with_life_time(
        data_producer_id: DataProducerId,
        max_packet_life_time: u16,
    ) -> Self {
        Self {
            data_producer_id,
            ordered: None,
            max_packet_life_time: Some(max_packet_life_time),
            max_retransmits: None,
            app_data: AppData::default(),
        }
    }

    /// Messages will be sent unreliably with a limited number of retransmission attempts.
    pub fn new_sctp_unordered_with_retransmits(
        data_producer_id: DataProducerId,
        max_retransmits: u16,
    ) -> Self {
        Self {
            data_producer_id,
            ordered: None,
            max_packet_life_time: None,
            max_retransmits: Some(max_retransmits),
            app_data: AppData::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct DataConsumerDump {
    pub id: DataConsumerId,
    pub data_producer_id: DataProducerId,
    pub r#type: DataConsumerType,
    pub label: String,
    pub protocol: String,
    pub sctp_stream_parameters: Option<SctpStreamParameters>,
    pub buffered_amount: u32,
    pub buffered_amount_low_threshold: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataConsumerStat {
    // `type` field is present in worker, but ignored here
    pub timestamp: u64,
    pub label: String,
    pub protocol: String,
    pub messages_sent: usize,
    pub bytes_sent: usize,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataConsumerType {
    Sctp,
    Direct,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase", content = "data")]
enum Notification {
    DataProducerClose,
    SctpSendBufferFull,
    BufferedAmountLow,
}

#[derive(Default)]
struct Handlers {
    sctp_send_buffer_full: Bag<dyn Fn() + Send>,
    buffered_amount_low: Bag<dyn Fn() + Send>,
    closed: Bag<dyn FnOnce() + Send>,
}

struct Inner {
    id: DataConsumerId,
    r#type: DataConsumerType,
    sctp_stream_parameters: Option<SctpStreamParameters>,
    label: String,
    protocol: String,
    data_producer_id: DataProducerId,
    executor: Arc<Executor<'static>>,
    channel: Channel,
    payload_channel: Channel,
    handlers: Arc<Handlers>,
    app_data: AppData,
    transport: Box<dyn Transport>,
    // Drop subscription to consumer-specific notifications when consumer itself is dropped
    _subscription_handler: SubscriptionHandler,
}

impl Drop for Inner {
    fn drop(&mut self) {
        debug!("drop()");

        self.handlers.closed.call_once_simple();

        {
            let channel = self.channel.clone();
            let request = DataConsumerCloseRequest {
                internal: DataConsumerInternal {
                    router_id: self.transport.router_id(),
                    transport_id: self.transport.id(),
                    data_consumer_id: self.id,
                    data_producer_id: self.data_producer_id,
                },
            };
            self.executor
                .spawn(async move {
                    if let Err(error) = channel.request(request).await {
                        error!("consumer closing failed on drop: {}", error);
                    }
                })
                .detach();
        }
    }
}

#[derive(Clone)]
pub struct DataConsumer {
    inner: Arc<Inner>,
}

impl DataConsumer {
    pub(super) async fn new(
        id: DataConsumerId,
        r#type: DataConsumerType,
        sctp_stream_parameters: Option<SctpStreamParameters>,
        label: String,
        protocol: String,
        data_producer_id: DataProducerId,
        executor: Arc<Executor<'static>>,
        channel: Channel,
        payload_channel: Channel,
        app_data: AppData,
        transport: Box<dyn Transport>,
    ) -> Self {
        debug!("new()");

        let handlers = Arc::<Handlers>::default();

        let subscription_handler = {
            let handlers = Arc::clone(&handlers);

            channel
                .subscribe_to_notifications(id.to_string(), move |notification| {
                    match serde_json::from_value::<Notification>(notification) {
                        Ok(notification) => match notification {
                            Notification::DataProducerClose => {
                                // TODO: Handle this in some meaningful way
                            }
                            Notification::SctpSendBufferFull => {
                                handlers.sctp_send_buffer_full.call_simple();
                            }
                            Notification::BufferedAmountLow => {
                                handlers.buffered_amount_low.call_simple();
                            }
                        },
                        Err(error) => {
                            error!("Failed to parse notification: {}", error);
                        }
                    }
                })
                .await
                .unwrap()
        };
        // TODO: payload_channel subscription for direct transport

        let inner = Arc::new(Inner {
            id,
            r#type,
            sctp_stream_parameters,
            label,
            protocol,
            data_producer_id,
            executor,
            channel,
            payload_channel,
            handlers,
            app_data,
            transport,
            _subscription_handler: subscription_handler,
        });

        Self { inner }
    }

    /// DataConsumer id.
    pub fn id(&self) -> DataConsumerId {
        self.inner.id
    }

    /// Associated DataProducer id.
    pub fn data_producer_id(&self) -> DataProducerId {
        self.inner.data_producer_id
    }

    /// DataConsumer type.
    pub fn r#type(&self) -> DataConsumerType {
        self.inner.r#type
    }

    /// SCTP stream parameters.
    pub fn sctp_stream_parameters(&self) -> Option<SctpStreamParameters> {
        self.inner.sctp_stream_parameters
    }

    /// DataChannel label.
    pub fn label(&self) -> &String {
        &self.inner.label
    }

    /// DataChannel protocol.
    pub fn protocol(&self) -> &String {
        &self.inner.protocol
    }

    /// App custom data.
    pub fn app_data(&self) -> &AppData {
        &self.inner.app_data
    }

    /// Dump DataConsumer.
    #[doc(hidden)]
    pub async fn dump(&self) -> Result<DataConsumerDump, RequestError> {
        debug!("dump()");

        self.inner
            .channel
            .request(DataConsumerDumpRequest {
                internal: self.get_internal(),
            })
            .await
    }

    /// Get DataConsumer stats.
    pub async fn get_stats(&self) -> Result<Vec<DataConsumerStat>, RequestError> {
        debug!("get_stats()");

        self.inner
            .channel
            .request(DataConsumerGetStatsRequest {
                internal: self.get_internal(),
            })
            .await
    }

    /// Get buffered amount size.
    pub async fn get_buffered_amount(&self) -> Result<u32, RequestError> {
        debug!("get_buffered_amount()");

        let response = self
            .inner
            .channel
            .request(DataConsumerGetBufferedAmountRequest {
                internal: self.get_internal(),
            })
            .await?;

        Ok(response.buffered_amount)
    }

    /// Set buffered amount low threshold.
    pub async fn set_buffered_amount_low_threshold(
        &self,
        threshold: u32,
    ) -> Result<(), RequestError> {
        debug!(
            "set_buffered_amount_low_threshold() [threshold:{}]",
            threshold
        );

        self.inner
            .channel
            .request(DataConsumerSetBufferedAmountLowThresholdRequest {
                internal: self.get_internal(),
                data: DataConsumerSetBufferedAmountLowThresholdData { threshold },
            })
            .await
    }

    // TODO: Not sure what is the purpose of this: https://github.com/versatica/mediasoup/pull/444
    // /**
    //  * Send data.
    //  */
    // async send(message: string | Buffer, ppid?: number): Promise<void>
    // {
    // 	if (typeof message !== 'string' && !Buffer.isBuffer(message))
    // 	{
    // 		throw new TypeError('message must be a string or a Buffer');
    // 	}
    //
    // 	/*
    // 	 * +-------------------------------+----------+
    // 	 * | Value                         | SCTP     |
    // 	 * |                               | PPID     |
    // 	 * +-------------------------------+----------+
    // 	 * | WebRTC String                 | 51       |
    // 	 * | WebRTC Binary Partial         | 52       |
    // 	 * | (Deprecated)                  |          |
    // 	 * | WebRTC Binary                 | 53       |
    // 	 * | WebRTC String Partial         | 54       |
    // 	 * | (Deprecated)                  |          |
    // 	 * | WebRTC String Empty           | 56       |
    // 	 * | WebRTC Binary Empty           | 57       |
    // 	 * +-------------------------------+----------+
    // 	 */
    //
    // 	if (typeof ppid !== 'number')
    // 	{
    // 		ppid = (typeof message === 'string')
    // 			? message.length > 0 ? 51 : 56
    // 			: message.length > 0 ? 53 : 57;
    // 	}
    //
    // 	// Ensure we honor PPIDs.
    // 	if (ppid === 56)
    // 		message = ' ';
    // 	else if (ppid === 57)
    // 		message = Buffer.alloc(1);
    //
    // 	const requestData = { ppid };
    //
    // 	await this._payloadChannel.request(
    // 		'dataConsumer.send', this._internal, requestData, message);
    // }

    pub fn on_sctp_send_buffer_full<F: Fn() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner
            .handlers
            .sctp_send_buffer_full
            .add(Box::new(callback))
    }

    pub fn on_buffered_amount_low<F: Fn() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner
            .handlers
            .buffered_amount_low
            .add(Box::new(callback))
    }

    pub fn on_closed<F: FnOnce() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner.handlers.closed.add(Box::new(callback))
    }

    fn get_internal(&self) -> DataConsumerInternal {
        DataConsumerInternal {
            router_id: self.inner.transport.router_id(),
            transport_id: self.inner.transport.id(),
            data_consumer_id: self.inner.id,
            data_producer_id: self.inner.data_producer_id,
        }
    }
}
