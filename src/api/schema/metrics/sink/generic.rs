use crate::{
    api::schema::metrics::{self, MetricsFilter},
    event::Metric,
};
use async_graphql::Object;

#[derive(Debug, Clone)]
pub struct GenericSinkMetrics(Vec<Metric>);

impl GenericSinkMetrics {
    pub fn new(metrics: Vec<Metric>) -> Self {
        Self(metrics)
    }
}

#[Object]
impl GenericSinkMetrics {
    /// Events processed for the current sink
    pub async fn processed_events_total(&self) -> Option<metrics::ProcessedEventsTotal> {
        self.0.processed_events_total()
    }

    /// Bytes processed for the current sink
    pub async fn processed_bytes_total(&self) -> Option<metrics::ProcessedBytesTotal> {
        self.0.processed_bytes_total()
    }

    /// Total incoming events for the current sink
    pub async fn events_in_total(&self) -> Option<metrics::EventsInTotal> {
        self.0.events_in_total()
    }

    /// Total received events for the current sink
    pub async fn received_events_total(&self) -> Option<metrics::ReceivedEventsTotal> {
        self.0.received_events_total()
    }

    /// Total outgoing events for the current sink
    pub async fn events_out_total(&self) -> Option<metrics::EventsOutTotal> {
        self.0.events_out_total()
    }

    /// Total outgoing events for the current sink
    pub async fn sent_events_total(&self) -> Option<metrics::SentEventsTotal> {
        self.0.sent_events_total()
    }
}
