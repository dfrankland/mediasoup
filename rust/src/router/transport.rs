use crate::data_structures::{AppData, TransportInternal};
use crate::messages::{
    TransportDumpRequest, TransportGetStatsRequest, TransportSetMaxIncomingBitrateData,
    TransportSetMaxIncomingBitrateRequest,
};
use crate::producer::{Producer, ProducerOptions};
use crate::router::RouterId;
use crate::uuid_based_wrapper_type;
use crate::worker::{Channel, RequestError};
use async_trait::async_trait;
use futures_lite::FutureExt;
use log::debug;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Debug;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

uuid_based_wrapper_type!(TransportId);

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportTraceEventData {
    Probation {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: Direction,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
    Bwe {
        /// Event timestamp.
        timestamp: u64,
        /// Event direction.
        direction: Direction,
        // TODO: Clarify value structure
        /// Per type information.
        info: Value,
    },
}

#[async_trait]
pub trait Transport<Dump, Stat, RemoteParameters> {
    /// Transport id.
    fn id(&self) -> TransportId;

    /// App custom data.
    fn app_data(&self) -> &AppData;

    /// Dump Transport.
    async fn dump(&self) -> Result<Dump, RequestError>;

    /// Get Transport stats.
    async fn get_stats(&self) -> Result<Vec<Stat>, RequestError>;

    /// Provide the Transport remote parameters.
    async fn connect(&self, remote_parameters: RemoteParameters) -> Result<(), RequestError>;

    async fn set_max_incoming_bitrate(&self, bitrate: u32) -> Result<(), RequestError>;

    async fn produce(&self, producer_options: ProducerOptions) -> Result<Producer, RequestError>;

    fn connect_closed<F: FnOnce() + Send + 'static>(&self, callback: F);
    // TODO
}

pub(super) trait TransportImpl<Dump, Stat, RemoteParameters>:
    Transport<Dump, Stat, RemoteParameters>
where
    Dump: Debug + DeserializeOwned + Send + Sync,
    Stat: Debug + DeserializeOwned + Send + Sync,
{
    fn router_id(&self) -> RouterId;

    fn channel(&self) -> &Channel;

    fn dump_impl<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Dump, RequestError>> + Send + 'a>>
    where
        Dump: 'a,
    {
        self.channel()
            .request(TransportDumpRequest {
                internal: TransportInternal {
                    router_id: self.router_id(),
                    transport_id: self.id(),
                },
                phantom_data: PhantomData {},
            })
            .boxed()
    }

    fn get_stats_impl<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Stat>, RequestError>> + Send + 'a>>
    where
        Stat: 'a,
    {
        self.channel()
            .request(TransportGetStatsRequest {
                internal: TransportInternal {
                    router_id: self.router_id(),
                    transport_id: self.id(),
                },
                phantom_data: PhantomData {},
            })
            .boxed()
    }

    fn set_max_incoming_bitrate_impl(
        &self,
        bitrate: u32,
    ) -> Pin<Box<dyn Future<Output = Result<(), RequestError>> + Send + '_>> {
        self.channel()
            .request(TransportSetMaxIncomingBitrateRequest {
                internal: TransportInternal {
                    router_id: self.router_id(),
                    transport_id: self.id(),
                },
                data: TransportSetMaxIncomingBitrateData { bitrate },
            })
            .boxed()
    }

    fn produce_impl(
        &self,
        producer_options: ProducerOptions,
    ) -> Pin<Box<dyn Future<Output = Result<Producer, RequestError>> + Send>> {
        todo!()
    }
}
