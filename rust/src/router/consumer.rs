use crate::data_structures::{AppData, EventDirection};
use crate::messages::{
    ConsumerCloseRequest, ConsumerDumpRequest, ConsumerEnableTraceEventData,
    ConsumerEnableTraceEventRequest, ConsumerGetStatsRequest, ConsumerInternal,
    ConsumerPauseRequest, ConsumerRequestKeyFrameRequest, ConsumerResumeRequest,
    ConsumerSetPreferredLayersRequest, ConsumerSetPriorityData, ConsumerSetPriorityRequest,
};
use crate::producer::{ProducerId, ProducerStat, ProducerType};
use crate::rtp_parameters::{MediaKind, MimeType, RtpCapabilities, RtpParameters};
use crate::transport::Transport;
use crate::uuid_based_wrapper_type;
use crate::worker::{Channel, RequestError, SubscriptionHandler};
use async_executor::Executor;
use event_listener_primitives::{Bag, HandlerId};
use log::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

uuid_based_wrapper_type!(ConsumerId);

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerLayers {
    /// The spatial layer index (from 0 to N).
    pub spatial_layer: u8,
    /// The temporal layer index (from 0 to N).
    pub temporal_layer: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerScore {
    /// The score of the RTP stream of the consumer.
    score: u8,
    /// The score of the currently selected RTP stream of the producer.
    producer_score: u8,
    /// The scores of all RTP streams in the producer ordered by encoding (just useful when the
    /// producer uses simulcast).
    producer_scores: Vec<u8>,
}

#[derive(Debug)]
#[non_exhaustive]
pub struct ConsumerOptions {
    /// The id of the Producer to consume.
    pub producer_id: ProducerId,
    /// RTP capabilities of the consuming endpoint.
    pub rtp_capabilities: RtpCapabilities,
    /// Whether the Consumer must start in paused mode. Default false.
    ///
    /// When creating a video Consumer, it's recommended to set paused to true, then transmit the
    /// Consumer parameters to the consuming endpoint and, once the consuming endpoint has created
    /// its local side Consumer, unpause the server side Consumer using the resume() method. This is
    /// an optimization to make it possible for the consuming endpoint to render the video as far as
    /// possible. If the server side Consumer was created with paused: false, mediasoup will
    /// immediately request a key frame to the remote Producer and such a key frame may reach the
    /// consuming endpoint even before it's ready to consume it, generating “black” video until the
    /// device requests a keyframe by itself.
    pub paused: bool,
    /// Preferred spatial and temporal layer for simulcast or SVC media sources.
    /// If unset, the highest ones are selected.
    pub preferred_layers: Option<ConsumerLayers>,
    /// Custom application data.
    pub app_data: AppData,
}

impl ConsumerOptions {
    pub fn new(producer_id: ProducerId, rtp_capabilities: RtpCapabilities) -> Self {
        Self {
            producer_id,
            rtp_capabilities,
            paused: false,
            preferred_layers: None,
            app_data: AppData::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct RtpStreamParams {
    clock_rate: u32,
    cname: String,
    encoding_idx: usize,
    mime_type: MimeType,
    payload_type: u8,
    spatial_layers: u8,
    ssrc: u32,
    temporal_layers: u8,
    use_dtx: bool,
    use_in_band_fec: bool,
    use_nack: bool,
    use_pli: bool,
    rid: Option<String>,
    rtc_ssrc: Option<u32>,
    rtc_payload_type: Option<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct RtpStream {
    params: RtpStreamParams,
    score: u8,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct RtpRtxParameters {
    ssrc: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct ConsumableRtpEncoding {
    ssrc: Option<u32>,
    rid: Option<String>,
    codec_payload_type: Option<u8>,
    rtx: Option<RtpRtxParameters>,
    max_bitrate: Option<u32>,
    max_framerate: Option<f64>,
    dtx: Option<bool>,
    scalability_mode: Option<String>,
    spatial_layers: Option<u8>,
    temporal_layers: Option<u8>,
    ksvc: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct ConsumerDump {
    pub id: ConsumerId,
    pub kind: MediaKind,
    pub paused: bool,
    pub priority: u8,
    pub producer_id: ProducerId,
    pub producer_paused: bool,
    pub rtp_parameters: RtpParameters,
    pub supported_codec_payload_types: Vec<u8>,
    pub trace_event_types: String,
    pub r#type: ConsumerType,
    pub consumable_rtp_encodings: Vec<ConsumableRtpEncoding>,
    pub rtp_stream: RtpStream,
    pub preferred_spatial_layer: Option<u8>,
    pub target_spatial_layer: Option<u8>,
    pub current_spatial_layer: Option<u8>,
    pub preferred_temporal_layer: Option<u8>,
    pub target_temporal_layer: Option<u8>,
    pub current_temporal_layer: Option<u8>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsumerType {
    Simple,
    Simulcast,
    SVC,
    Pipe,
}

impl From<ProducerType> for ConsumerType {
    fn from(producer_type: ProducerType) -> Self {
        match producer_type {
            ProducerType::Simple => ConsumerType::Simple,
            ProducerType::Simulcast => ConsumerType::Simulcast,
            ProducerType::SVC => ConsumerType::SVC,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerStat {
    // Common to all RtpStreams.
    // `type` field is present in worker, but ignored here
    pub timestamp: u64,
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
    pub kind: String,
    pub mime_type: MimeType,
    pub packets_lost: u32,
    pub fraction_lost: u8,
    pub packets_discarded: usize,
    pub packets_retransmitted: usize,
    pub packets_repaired: usize,
    pub nack_count: usize,
    pub nack_packet_count: usize,
    pub pli_count: usize,
    pub fir_count: usize,
    pub score: u8,
    pub packet_count: usize,
    pub byte_count: usize,
    pub bitrate: u32,
    pub round_trip_time: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ConsumerStats {
    JustConsumer((ConsumerStat,)),
    WithProducer((ConsumerStat, ProducerStat)),
}

/// 'trace' event data.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConsumerTraceEventData {
    RTP {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: EventDirection,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
    KeyFrame {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: EventDirection,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
    NACK {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: EventDirection,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
    PLI {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: EventDirection,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
    FIR {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: EventDirection,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
}

/// Valid types for 'trace' event.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsumerTraceEventType {
    RTP,
    KeyFrame,
    NACK,
    PLI,
    FIR,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase", content = "data")]
enum Notification {
    ProducerClose,
    ProducerPause,
    ProducerResume,
    Score(ConsumerScore),
    LayersChange(ConsumerLayers),
    Trace(ConsumerTraceEventData),
}

#[derive(Default)]
struct Handlers {
    pause: Bag<'static, dyn Fn() + Send>,
    resume: Bag<'static, dyn Fn() + Send>,
    score: Bag<'static, dyn Fn(&ConsumerScore) + Send>,
    layers_change: Bag<'static, dyn Fn(&ConsumerLayers) + Send>,
    trace: Bag<'static, dyn Fn(&ConsumerTraceEventData) + Send>,
    closed: Bag<'static, dyn FnOnce() + Send>,
}

struct Inner {
    id: ConsumerId,
    producer_id: ProducerId,
    kind: MediaKind,
    r#type: ConsumerType,
    rtp_parameters: RtpParameters,
    paused: Arc<Mutex<bool>>,
    executor: Arc<Executor<'static>>,
    channel: Channel,
    payload_channel: Channel,
    producer_paused: Arc<Mutex<bool>>,
    priority: Mutex<u8>,
    score: Arc<Mutex<ConsumerScore>>,
    preferred_layers: Mutex<Option<ConsumerLayers>>,
    current_layers: Arc<Mutex<Option<ConsumerLayers>>>,
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
            let request = ConsumerCloseRequest {
                internal: ConsumerInternal {
                    router_id: self.transport.router_id(),
                    transport_id: self.transport.id(),
                    consumer_id: self.id,
                    producer_id: self.producer_id,
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
pub struct Consumer {
    inner: Arc<Inner>,
}

impl Consumer {
    pub(super) async fn new(
        id: ConsumerId,
        producer_id: ProducerId,
        kind: MediaKind,
        r#type: ConsumerType,
        rtp_parameters: RtpParameters,
        paused: bool,
        executor: Arc<Executor<'static>>,
        channel: Channel,
        payload_channel: Channel,
        producer_paused: bool,
        score: ConsumerScore,
        preferred_layers: Option<ConsumerLayers>,
        app_data: AppData,
        transport: Box<dyn Transport>,
    ) -> Self {
        debug!("new()");

        let handlers = Arc::<Handlers>::default();
        let score = Arc::new(Mutex::new(score));
        let paused = Arc::new(Mutex::new(paused));
        let producer_paused = Arc::new(Mutex::new(producer_paused));
        let current_layers = Arc::<Mutex<Option<ConsumerLayers>>>::default();

        let subscription_handler = {
            let handlers = Arc::clone(&handlers);
            let paused = Arc::clone(&paused);
            let producer_paused = Arc::clone(&producer_paused);
            let score = Arc::clone(&score);
            let current_layers = Arc::clone(&current_layers);

            channel
                .subscribe_to_notifications(id.to_string(), move |notification| {
                    match serde_json::from_value::<Notification>(notification) {
                        Ok(notification) => match notification {
                            Notification::ProducerClose => {
                                // TODO: Handle this in some meaningful way
                            }
                            Notification::ProducerPause => {
                                let mut producer_paused = producer_paused.lock().unwrap();
                                let was_paused = *paused.lock().unwrap() || *producer_paused;
                                *producer_paused = true;

                                if !was_paused {
                                    handlers.pause.call_simple();
                                }
                            }
                            Notification::ProducerResume => {
                                let mut producer_paused = producer_paused.lock().unwrap();
                                let paused = *paused.lock().unwrap();
                                let was_paused = paused || *producer_paused;
                                *producer_paused = false;

                                if was_paused && !paused {
                                    handlers.resume.call_simple();
                                }
                            }
                            Notification::Score(consumer_score) => {
                                *score.lock().unwrap() = consumer_score.clone();
                                handlers.score.call(|callback| {
                                    callback(&consumer_score);
                                });
                            }
                            Notification::LayersChange(consumer_layers) => {
                                *current_layers.lock().unwrap() = Some(consumer_layers.clone());
                                handlers.layers_change.call(|callback| {
                                    callback(&consumer_layers);
                                });
                            }
                            Notification::Trace(trace_event_data) => {
                                handlers.trace.call(|callback| {
                                    callback(&trace_event_data);
                                });
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
            producer_id,
            kind,
            r#type,
            rtp_parameters,
            paused,
            producer_paused,
            priority: Mutex::new(1u8),
            score,
            preferred_layers: Mutex::new(preferred_layers),
            current_layers,
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

    /// Consumer id.
    pub fn id(&self) -> ConsumerId {
        self.inner.id
    }

    /// Associated Producer id.
    pub fn producer_id(&self) -> ProducerId {
        self.inner.producer_id
    }

    /// Media kind.
    pub fn kind(&self) -> MediaKind {
        self.inner.kind
    }

    /// RTP parameters.
    pub fn rtp_parameters(&self) -> &RtpParameters {
        &self.inner.rtp_parameters
    }

    /// Consumer type.
    pub fn r#type(&self) -> ConsumerType {
        self.inner.r#type
    }

    /// Whether the Consumer is paused.
    pub fn paused(&self) -> bool {
        *self.inner.paused.lock().unwrap()
    }

    /// Whether the associate Producer is paused.
    pub fn producer_paused(&self) -> bool {
        *self.inner.producer_paused.lock().unwrap()
    }

    /// Current priority.
    pub fn priority(&self) -> u8 {
        *self.inner.priority.lock().unwrap()
    }

    /// Consumer score.
    pub fn score(&self) -> ConsumerScore {
        self.inner.score.lock().unwrap().clone()
    }

    /// Preferred video layers.
    pub fn preferred_layers(&self) -> Option<ConsumerLayers> {
        self.inner.preferred_layers.lock().unwrap().clone()
    }

    /// Current video layers.
    pub fn current_layers(&self) -> Option<ConsumerLayers> {
        self.inner.current_layers.lock().unwrap().clone()
    }

    /// App custom data.
    pub fn app_data(&self) -> &AppData {
        &self.inner.app_data
    }

    /// Dump Consumer.
    #[doc(hidden)]
    pub async fn dump(&self) -> Result<ConsumerDump, RequestError> {
        debug!("dump()");

        self.inner
            .channel
            .request(ConsumerDumpRequest {
                internal: self.get_internal(),
            })
            .await
    }

    /// Get Consumer stats.
    pub async fn get_stats(&self) -> Result<ConsumerStats, RequestError> {
        debug!("get_stats()");

        self.inner
            .channel
            .request(ConsumerGetStatsRequest {
                internal: self.get_internal(),
            })
            .await
    }

    /// Pause the Consumer.
    pub async fn pause(&self) -> Result<(), RequestError> {
        debug!("pause()");

        self.inner
            .channel
            .request(ConsumerPauseRequest {
                internal: self.get_internal(),
            })
            .await?;

        let mut paused = self.inner.paused.lock().unwrap();
        let was_paused = *paused || *self.inner.producer_paused.lock().unwrap();
        *paused = true;

        if !was_paused {
            self.inner.handlers.pause.call_simple();
        }

        Ok(())
    }

    /// Resume the Consumer.
    pub async fn resume(&self) -> Result<(), RequestError> {
        debug!("resume()");

        self.inner
            .channel
            .request(ConsumerResumeRequest {
                internal: self.get_internal(),
            })
            .await?;

        let mut paused = self.inner.paused.lock().unwrap();
        let was_paused = *paused || *self.inner.producer_paused.lock().unwrap();
        *paused = false;

        if was_paused {
            self.inner.handlers.resume.call_simple();
        }

        Ok(())
    }

    /// Set preferred video layers.
    pub async fn set_preferred_layers(
        &self,
        consumer_layers: ConsumerLayers,
    ) -> Result<(), RequestError> {
        debug!("set_preferred_layers()");

        let consumer_layers = self
            .inner
            .channel
            .request(ConsumerSetPreferredLayersRequest {
                internal: self.get_internal(),
                data: consumer_layers,
            })
            .await?;

        *self.inner.preferred_layers.lock().unwrap() = consumer_layers;

        Ok(())
    }

    /// Set priority.
    pub async fn set_priority(&self, priority: u8) -> Result<(), RequestError> {
        debug!("set_preferred_layers()");

        let result = self
            .inner
            .channel
            .request(ConsumerSetPriorityRequest {
                internal: self.get_internal(),
                data: ConsumerSetPriorityData { priority },
            })
            .await?;

        *self.inner.priority.lock().unwrap() = result.priority;

        Ok(())
    }

    /// Unset priority.
    pub async fn unset_priority(&self) -> Result<(), RequestError> {
        debug!("unset_priority()");

        let priority = 1;

        let result = self
            .inner
            .channel
            .request(ConsumerSetPriorityRequest {
                internal: self.get_internal(),
                data: ConsumerSetPriorityData { priority },
            })
            .await?;

        *self.inner.priority.lock().unwrap() = result.priority;

        Ok(())
    }

    /// Request a key frame to the Producer.
    pub async fn request_key_frame(&self) -> Result<(), RequestError> {
        debug!("request_key_frame()");

        self.inner
            .channel
            .request(ConsumerRequestKeyFrameRequest {
                internal: self.get_internal(),
            })
            .await
    }

    /// Enable 'trace' event.
    pub async fn enable_trace_event(
        &self,
        types: Vec<ConsumerTraceEventType>,
    ) -> Result<(), RequestError> {
        debug!("enable_trace_event()");

        self.inner
            .channel
            .request(ConsumerEnableTraceEventRequest {
                internal: self.get_internal(),
                data: ConsumerEnableTraceEventData { types },
            })
            .await
    }

    pub fn on_pause<F: Fn() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner.handlers.pause.add(Box::new(callback))
    }

    pub fn on_resume<F: Fn() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner.handlers.resume.add(Box::new(callback))
    }

    pub fn on_score<F: Fn(&ConsumerScore) + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner.handlers.score.add(Box::new(callback))
    }

    pub fn on_layers_change<F: Fn(&ConsumerLayers) + Send + 'static>(
        &self,
        callback: F,
    ) -> HandlerId {
        self.inner.handlers.layers_change.add(Box::new(callback))
    }

    pub fn on_trace<F: Fn(&ConsumerTraceEventData) + Send + 'static>(
        &self,
        callback: F,
    ) -> HandlerId {
        self.inner.handlers.trace.add(Box::new(callback))
    }

    pub fn on_closed<F: FnOnce() + Send + 'static>(&self, callback: F) -> HandlerId {
        self.inner.handlers.closed.add(Box::new(callback))
    }

    fn get_internal(&self) -> ConsumerInternal {
        ConsumerInternal {
            router_id: self.inner.transport.router_id(),
            transport_id: self.inner.transport.id(),
            consumer_id: self.inner.id,
            producer_id: self.inner.producer_id,
        }
    }
}