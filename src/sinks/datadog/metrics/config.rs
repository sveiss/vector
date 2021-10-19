use crate::{
    config::{DataType, SinkConfig, SinkContext},
    http::HttpClient,
    sinks::{
        datadog::{healthcheck, Region},
        util::{
            batch::{BatchConfig, BatchSettings},
            Concurrency, ServiceBuilderExt, TowerRequestConfig,
        },
        Healthcheck, UriParseError, VectorSink,
    },
};
use futures::FutureExt;
use http::{uri::InvalidUri, Uri};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use tower::ServiceBuilder;
use vector_core::config::proxy::ProxyConfig;

use super::{
    request_builder::DatadogMetricsRequestBuilder,
    service::{DatadogMetricsRetryLogic, DatadogMetricsService},
    sink::DatadogMetricsSink,
};

// TODO: revisit our concurrency and batching defaults
const DEFAULT_REQUEST_LIMITS: TowerRequestConfig =
    TowerRequestConfig::new(Concurrency::None).retry_attempts(5);

// This default is centered around "series" data, which should be the lion's share of what we
// process.  Given that a single series, when encoded, is in the 150-300 byte range, we can fit a
// lot of these into a single request, something like 150-200K series.  Simply to be a little more
// conservative, though, we use 100K here.  This will also get a little more tricky when it comes to
// distributions and sketches, but we're going to have to implement incremental encoding to handle
// "we've exceeded our maximum payload size, split this batch" scenarios anyways.
const DEFAULT_BATCH_SETTINGS: BatchSettings<()> =
    BatchSettings::const_default().events(100000).timeout(2);

pub const MAXIMUM_SERIES_PAYLOAD_COMPRESSED_SIZE: usize = 3_200_000;
pub const MAXIMUM_SERIES_PAYLOAD_SIZE: usize = 62_914_560;

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("Invalid host {:?}: {:?}", host, source))]
    InvalidHost { host: String, source: InvalidUri },
}

/// Various metric type-specific API types.
///
/// Each of these corresponds to a specific request path when making a request to the agent API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatadogMetricsEndpoint {
    Series,
    Distribution,
    Sketch,
}

impl DatadogMetricsEndpoint {
    pub fn content_type(&self) -> &'static str {
        match self {
            DatadogMetricsEndpoint::Series => "application/json",
            DatadogMetricsEndpoint::Distribution => "application/json",
            DatadogMetricsEndpoint::Sketch => "application/x-protobuf",
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DatadogMetricsConfig {
    #[serde(alias = "namespace")]
    pub default_namespace: Option<String>,
    // Deprecated name
    #[serde(alias = "host")]
    pub endpoint: Option<String>,
    // Deprecated, replaced by the site option
    pub region: Option<Region>,
    pub site: Option<String>,
    pub api_key: String,
    #[serde(default)]
    pub batch: BatchConfig,
    #[serde(default)]
    pub request: TowerRequestConfig,
}

impl_generate_config_from_default!(DatadogMetricsConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "datadog_metrics")]
impl SinkConfig for DatadogMetricsConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        let client = self.build_client(&cx.proxy)?;
        let healthcheck = self.build_healthcheck(client.clone());
        let sink = self.build_sink(client, cx)?;

        Ok((sink, healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Metric
    }

    fn sink_type(&self) -> &'static str {
        "datadog_metrics"
    }
}

impl DatadogMetricsConfig {
    /// Creates a default [`DatadogMetricsConfig`] with the given API key.
    pub fn from_api_key<T: Into<String>>(api_key: T) -> Self {
        Self {
            api_key: api_key.into(),
            ..Self::default()
        }
    }

    /// Gets the base URI of the Datadog agent API.
    ///
    /// Per the Datadog agent convention, we should include a unique identifier as part of the
    /// domain to indicate that these metrics are being submitted by Vector, including the version,
    /// likely useful for detecting if a specific version of the agent (Vector, in this case) is
    /// doing something wrong, for understanding issues from the API side.
    ///
    /// The `endpoint` configuration field will be used here if it is present.
    fn get_base_agent_endpoint(&self) -> String {
        self.endpoint.clone().unwrap_or_else(|| {
            let version = str::replace(crate::built_info::PKG_VERSION, ".", "-");
            format!("https://{}-vector.agent.{}", version, self.get_site())
        })
    }

    /// Generates the full URIs to use for the various type-specific metrics endpoints.
    fn generate_metric_endpoints(&self) -> crate::Result<Vec<(DatadogMetricsEndpoint, Uri)>> {
        let base_uri = self.get_base_agent_endpoint();
        let series_endpoint = build_uri(&base_uri, "/api/v1/series")?;
        let distribution_endpoint = build_uri(&base_uri, "/api/v1/distribution_points")?;
        let sketch_endpoint = build_uri(&base_uri, "/api/beta/sketches")?;

        Ok(vec![
            (DatadogMetricsEndpoint::Series, series_endpoint),
            (DatadogMetricsEndpoint::Distribution, distribution_endpoint),
            (DatadogMetricsEndpoint::Sketch, sketch_endpoint),
        ])
    }

    /// Gets the base URI of the Datadog API.
    ///
    /// The `endpoint` configuration field will be used here if it is present.
    fn get_api_endpoint(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| format!("https://api.{}", self.get_site()))
    }

    /// Gets the base domain to use for any calls to Datadog.
    ///
    /// If `site` is not specified, we fallback to `region`, and if that is not specified, we
    /// fallback to the Datadog US domain.
    fn get_site(&self) -> &str {
        self.site.as_deref().unwrap_or_else(|| match self.region {
            Some(Region::Eu) => "datadoghq.eu",
            None | Some(Region::Us) => "datadoghq.com",
        })
    }

    fn build_client(&self, proxy: &ProxyConfig) -> crate::Result<HttpClient> {
        let client = HttpClient::new(None, proxy)?;
        Ok(client)
    }

    fn build_healthcheck(&self, client: HttpClient) -> Healthcheck {
        healthcheck(self.get_api_endpoint(), self.api_key.clone(), client).boxed()
    }

    fn build_sink(&self, client: HttpClient, cx: SinkContext) -> crate::Result<VectorSink> {
        let batcher_settings = DEFAULT_BATCH_SETTINGS
            .parse_config(self.batch)?
            .into_batcher_settings()?;

        let request_limits = self.request.unwrap_with(&DEFAULT_REQUEST_LIMITS);
        let metric_endpoints = self.generate_metric_endpoints()?;
        let service = ServiceBuilder::new()
            .settings(request_limits, DatadogMetricsRetryLogic)
            .service(DatadogMetricsService::new(client, self.api_key.as_str()));

        let request_builder = DatadogMetricsRequestBuilder::new(
            metric_endpoints,
            self.default_namespace.clone(),
        );

        let sink = DatadogMetricsSink::new(cx, service, request_builder, batcher_settings);

        Ok(VectorSink::Stream(Box::new(sink)))
    }
}

fn build_uri(host: &str, endpoint: &str) -> crate::Result<Uri> {
    let result = format!("{}{}", host, endpoint)
        .parse::<Uri>()
        .context(UriParseError)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<DatadogMetricsConfig>();
    }
}
