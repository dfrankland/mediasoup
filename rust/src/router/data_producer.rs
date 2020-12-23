use crate::data_structures::{AppData, WebRtcMessage};
use crate::messages::{
    DataProducerCloseRequest, DataProducerDumpRequest, DataProducerGetStatsRequest,
    DataProducerInternal, DataProducerSendData, DataProducerSendNotification,
};
use crate::sctp_parameters::SctpStreamParameters;
use crate::transport::{Transport, TransportGeneric};
use crate::uuid_based_wrapper_type;
use crate::worker::{Channel, NotificationError, PayloadChannel, RequestError};
use async_executor::Executor;
use event_listener_primitives::{BagOnce, HandlerId};
use log::*;
use parking_lot::Mutex as SyncMutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

uuid_based_wrapper_type!(DataProducerId);

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DataProducerOptions {
    /// DataProducer id (just for `Router::pipe_*_to_router()` methods).
    /// DataProducer id, should not be specified explicitly, specified by pipe transport only
    pub(super) id: Option<DataProducerId>,
    /// SCTP parameters defining how the endpoint is sending the data.
    /// Required if SCTP/DataChannel is used.
    /// Must not be given if the data producer is created on a DirectTransport.
    pub(super) sctp_stream_parameters: Option<SctpStreamParameters>,
    /// A label which can be used to distinguish this DataChannel from others.
    pub label: String,
    /// Name of the sub-protocol used by this DataChannel.
    pub protocol: String,
    /// Custom application data.
    pub app_data: AppData,
}

impl DataProducerOptions {
    pub(super) fn new_pipe_transport(
        data_producer_id: DataProducerId,
        sctp_stream_parameters: SctpStreamParameters,
    ) -> Self {
        Self {
            id: Some(data_producer_id),
            sctp_stream_parameters: Some(sctp_stream_parameters),
            label: "".to_string(),
            protocol: "".to_string(),
            app_data: AppData::default(),
        }
    }

    pub fn new_sctp(sctp_stream_parameters: SctpStreamParameters) -> Self {
        Self {
            id: None,
            sctp_stream_parameters: Some(sctp_stream_parameters),
            label: "".to_string(),
            protocol: "".to_string(),
            app_data: AppData::default(),
        }
    }

    /// For DirectTransport.
    pub fn new_direct() -> Self {
        Self {
            id: None,
            sctp_stream_parameters: None,
            label: "".to_string(),
            protocol: "".to_string(),
            app_data: AppData::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataProducerType {
    Sctp,
    Direct,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
#[non_exhaustive]
pub struct DataProducerDump {
    pub id: DataProducerId,
    pub r#type: DataProducerType,
    pub label: String,
    pub protocol: String,
    pub sctp_stream_parameters: Option<SctpStreamParameters>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DataProducerStat {
    // `type` field is present in worker, but ignored here
    pub timestamp: u64,
    pub label: String,
    pub protocol: String,
    pub messages_received: usize,
    pub bytes_received: usize,
}

#[derive(Default)]
struct Handlers {
    transport_close: BagOnce<Box<dyn FnOnce() + Send>>,
    close: BagOnce<Box<dyn FnOnce() + Send>>,
}

struct Inner {
    id: DataProducerId,
    r#type: DataProducerType,
    sctp_stream_parameters: Option<SctpStreamParameters>,
    label: String,
    protocol: String,
    executor: Arc<Executor<'static>>,
    channel: Channel,
    payload_channel: PayloadChannel,
    handlers: Arc<Handlers>,
    app_data: AppData,
    transport: Arc<Box<dyn Transport>>,
    closed: AtomicBool,
    _on_transport_close_handler: SyncMutex<HandlerId>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        debug!("drop()");

        self.close();
    }
}

impl Inner {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            debug!("close()");

            self.handlers.close.call_simple();

            {
                let channel = self.channel.clone();
                let request = DataProducerCloseRequest {
                    internal: DataProducerInternal {
                        router_id: self.transport.router_id(),
                        transport_id: self.transport.id(),
                        data_producer_id: self.id,
                    },
                };
                let transport = Arc::clone(&self.transport);
                self.executor
                    .spawn(async move {
                        if let Err(error) = channel.request(request).await {
                            error!("data producer closing failed on drop: {}", error);
                        }

                        drop(transport);
                    })
                    .detach();
            }
        }
    }
}

#[derive(Clone)]
pub struct RegularDataProducer {
    inner: Arc<Inner>,
}

impl From<RegularDataProducer> for DataProducer {
    fn from(producer: RegularDataProducer) -> Self {
        DataProducer::Regular(producer)
    }
}

#[derive(Clone)]
pub struct DirectDataProducer {
    inner: Arc<Inner>,
}

impl From<DirectDataProducer> for DataProducer {
    fn from(producer: DirectDataProducer) -> Self {
        DataProducer::Direct(producer)
    }
}

#[derive(Clone)]
#[non_exhaustive]
pub enum DataProducer {
    Regular(RegularDataProducer),
    Direct(DirectDataProducer),
}

impl DataProducer {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn new<Dump, Stat, Transport>(
        id: DataProducerId,
        r#type: DataProducerType,
        sctp_stream_parameters: Option<SctpStreamParameters>,
        label: String,
        protocol: String,
        executor: Arc<Executor<'static>>,
        channel: Channel,
        payload_channel: PayloadChannel,
        app_data: AppData,
        transport: Transport,
        direct: bool,
    ) -> Self
    where
        Dump: Debug + DeserializeOwned + 'static,
        Stat: Debug + DeserializeOwned + 'static,
        Transport: TransportGeneric<Dump, Stat> + 'static,
    {
        debug!("new()");

        let handlers = Arc::<Handlers>::default();

        let inner_weak = Arc::<SyncMutex<Option<Weak<Inner>>>>::default();
        let on_transport_close_handler = transport.on_close({
            let inner_weak = Arc::clone(&inner_weak);

            move || {
                if let Some(inner) = inner_weak
                    .lock()
                    .as_ref()
                    .and_then(|weak_inner| weak_inner.upgrade())
                {
                    inner.handlers.transport_close.call_simple();
                    inner.close();
                }
            }
        });
        let inner = Arc::new(Inner {
            id,
            r#type,
            sctp_stream_parameters,
            label,
            protocol,
            executor,
            channel,
            payload_channel,
            handlers,
            app_data,
            transport: Arc::new(Box::new(transport)),
            closed: AtomicBool::new(false),
            _on_transport_close_handler: SyncMutex::new(on_transport_close_handler),
        });

        inner_weak.lock().replace(Arc::downgrade(&inner));

        if direct {
            Self::Direct(DirectDataProducer { inner })
        } else {
            Self::Regular(RegularDataProducer { inner })
        }
    }

    /// DataProducer id.
    pub fn id(&self) -> DataProducerId {
        self.inner().id
    }

    /// DataProducer type.
    pub fn r#type(&self) -> DataProducerType {
        self.inner().r#type
    }

    /// SCTP stream parameters.
    pub fn sctp_stream_parameters(&self) -> Option<SctpStreamParameters> {
        self.inner().sctp_stream_parameters
    }

    /// DataChannel label.
    pub fn label(&self) -> &String {
        &self.inner().label
    }

    /// DataChannel protocol.
    pub fn protocol(&self) -> &String {
        &self.inner().protocol
    }

    /// App custom data.
    pub fn app_data(&self) -> &AppData {
        &self.inner().app_data
    }

    pub fn closed(&self) -> bool {
        self.inner().closed.load(Ordering::SeqCst)
    }

    /// Dump DataProducer.
    #[doc(hidden)]
    pub async fn dump(&self) -> Result<DataProducerDump, RequestError> {
        debug!("dump()");

        self.inner()
            .channel
            .request(DataProducerDumpRequest {
                internal: self.get_internal(),
            })
            .await
    }

    /// Get DataProducer stats.
    pub async fn get_stats(&self) -> Result<Vec<DataProducerStat>, RequestError> {
        debug!("get_stats()");

        self.inner()
            .channel
            .request(DataProducerGetStatsRequest {
                internal: self.get_internal(),
            })
            .await
    }

    pub fn on_transport_close<F: FnOnce() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner()
            .handlers
            .transport_close
            .add(Box::new(callback))
    }

    pub fn on_close<F: FnOnce() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner().handlers.close.add(Box::new(callback))
    }

    pub(super) fn close(&self) {
        self.inner().close();
    }

    pub(super) fn downgrade(&self) -> WeakDataProducer {
        WeakDataProducer {
            inner: Arc::downgrade(&self.inner()),
        }
    }

    fn inner(&self) -> &Arc<Inner> {
        match self {
            DataProducer::Regular(data_producer) => &data_producer.inner,
            DataProducer::Direct(data_producer) => &data_producer.inner,
        }
    }

    fn get_internal(&self) -> DataProducerInternal {
        DataProducerInternal {
            router_id: self.inner().transport.router_id(),
            transport_id: self.inner().transport.id(),
            data_producer_id: self.inner().id,
        }
    }
}

impl DirectDataProducer {
    /// Send data.
    pub async fn send(&self, message: WebRtcMessage) -> Result<(), NotificationError> {
        let (ppid, payload) = message.into_ppid_and_payload();

        self.inner
            .payload_channel
            .notify(
                DataProducerSendNotification {
                    internal: DataProducerInternal {
                        router_id: self.inner.transport.router_id(),
                        transport_id: self.inner.transport.id(),
                        data_producer_id: self.inner.id,
                    },
                    data: DataProducerSendData { ppid },
                },
                payload,
            )
            .await
    }
}

#[derive(Clone)]
pub(super) struct WeakDataProducer {
    inner: Weak<Inner>,
}

impl WeakDataProducer {
    pub(super) fn upgrade(&self) -> Option<DataProducer> {
        Some(DataProducer::Regular(RegularDataProducer {
            inner: self.inner.upgrade()?,
        }))
    }
}