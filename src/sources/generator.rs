use crate::{
    codecs::{self, DecodingConfig, FramingConfig, ParserConfig},
    config::{log_schema, DataType, SourceConfig, SourceContext, SourceDescription},
    internal_events::GeneratorEventProcessed,
    serde::{default_decoding, default_framing_message_based},
    shutdown::ShutdownSignal,
    sources::util::TcpError,
    Pipeline,
};
use bytes::Bytes;
use chrono::Utc;
use fakedata::logs::*;
use futures::{SinkExt, StreamExt};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::task::Poll;
use tokio::time::{self, Duration};
use tokio_util::codec::FramedRead;

#[derive(Clone, Debug, Derivative, Deserialize, Serialize)]
#[derivative(Default)]
#[serde(default)]
pub struct GeneratorConfig {
    #[serde(alias = "batch_interval")]
    #[derivative(Default(value = "default_interval()"))]
    interval: f64,
    #[derivative(Default(value = "default_count()"))]
    count: usize,
    #[serde(flatten)]
    format: OutputFormat,
    #[derivative(Default(value = "default_framing_message_based()"))]
    framing: Box<dyn FramingConfig>,
    #[derivative(Default(value = "default_decoding()"))]
    decoding: Box<dyn ParserConfig>,
}

const fn default_interval() -> f64 {
    1.0
}

const fn default_count() -> usize {
    isize::MAX as usize
}

#[derive(Debug, PartialEq, Snafu)]
pub enum GeneratorConfigError {
    #[snafu(display("A non-empty list of lines is required for the shuffle format"))]
    ShuffleGeneratorItemsEmpty,
}

#[derive(Clone, Debug, Derivative, Deserialize, Serialize)]
#[derivative(Default)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum OutputFormat {
    Shuffle {
        #[serde(default)]
        sequence: bool,
        lines: Vec<String>,
    },
    ApacheCommon,
    ApacheError,
    #[serde(alias = "rfc5424")]
    Syslog,
    #[serde(alias = "rfc3164")]
    BsdSyslog,
    #[derivative(Default)]
    Json,
}

impl OutputFormat {
    fn generate_line(&self, n: usize) -> String {
        emit!(&GeneratorEventProcessed);

        match self {
            Self::Shuffle {
                sequence,
                ref lines,
            } => Self::shuffle_generate(*sequence, lines, n),
            Self::ApacheCommon => apache_common_log_line(),
            Self::ApacheError => apache_error_log_line(),
            Self::Syslog => syslog_5424_log_line(),
            Self::BsdSyslog => syslog_3164_log_line(),
            Self::Json => json_log_line(),
        }
    }

    fn shuffle_generate(sequence: bool, lines: &[String], n: usize) -> String {
        // unwrap can be called here because `lines` can't be empty
        let line = lines.choose(&mut rand::thread_rng()).unwrap();

        if sequence {
            format!("{} {}", n, line)
        } else {
            line.into()
        }
    }

    // Ensures that the `lines` list is non-empty if `Shuffle` is chosen
    pub(self) fn validate(&self) -> Result<(), GeneratorConfigError> {
        match self {
            Self::Shuffle { lines, .. } => {
                if lines.is_empty() {
                    Err(GeneratorConfigError::ShuffleGeneratorItemsEmpty)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

impl GeneratorConfig {
    #[allow(dead_code)] // to make check-component-features pass
    pub fn repeat(lines: Vec<String>, count: usize, interval: f64) -> Self {
        Self {
            count,
            interval,
            format: OutputFormat::Shuffle {
                lines,
                sequence: false,
            },
            framing: default_framing_message_based(),
            decoding: default_decoding(),
        }
    }
}

async fn generator_source(
    interval: f64,
    count: usize,
    format: OutputFormat,
    decoder: codecs::Decoder,
    mut shutdown: ShutdownSignal,
    mut out: Pipeline,
) -> Result<(), ()> {
    let maybe_interval: Option<f64> = if interval != 0.0 {
        Some(interval)
    } else {
        None
    };

    let mut interval = maybe_interval.map(|i| time::interval(Duration::from_secs_f64(i)));

    for n in 0..count {
        if matches!(futures::poll!(&mut shutdown), Poll::Ready(_)) {
            break;
        }

        if let Some(interval) = &mut interval {
            interval.tick().await;
        }

        let line = format.generate_line(n);

        let mut stream = FramedRead::new(line.as_bytes(), decoder.clone());
        while let Some(next) = stream.next().await {
            match next {
                Ok((events, _byte_size)) => {
                    let now = Utc::now();

                    for mut event in events {
                        let log = event.as_mut_log();

                        log.try_insert(log_schema().source_type_key(), Bytes::from("generator"));
                        log.try_insert(log_schema().timestamp_key(), now);

                        out.send(event)
                            .await
                            .map_err(|_: crate::pipeline::ClosedError| {
                                error!(message = "Failed to forward events; downstream is closed.");
                            })?;
                    }
                }
                Err(error) => {
                    // Error is logged by `crate::codecs::Decoder`, no further
                    // handling is needed here.
                    if !error.can_continue() {
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

inventory::submit! {
    SourceDescription::new::<GeneratorConfig>("generator")
}

impl_generate_config_from_default!(GeneratorConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "generator")]
impl SourceConfig for GeneratorConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        self.format.validate()?;
        let decoder = DecodingConfig::new(self.framing.clone(), self.decoding.clone()).build()?;
        Ok(Box::pin(generator_source(
            self.interval,
            self.count,
            self.format.clone(),
            decoder,
            cx.shutdown,
            cx.out,
        )))
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "generator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::log_schema, event::Event, shutdown::ShutdownSignal, Pipeline};
    use futures::{channel::mpsc, poll, StreamExt};
    use std::time::{Duration, Instant};

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<GeneratorConfig>();
    }

    async fn runit(config: &str) -> mpsc::Receiver<Event> {
        let (tx, rx) = Pipeline::new_test();
        let config: GeneratorConfig = toml::from_str(config).unwrap();
        let decoder = DecodingConfig::new(default_framing_message_based(), default_decoding())
            .build()
            .unwrap();
        generator_source(
            config.interval,
            config.count,
            config.format,
            decoder,
            ShutdownSignal::noop(),
            tx,
        )
        .await
        .unwrap();
        rx
    }

    #[test]
    fn config_shuffle_lines_not_empty() {
        let empty_lines: Vec<String> = Vec::new();

        let errant_config = GeneratorConfig {
            format: OutputFormat::Shuffle {
                sequence: false,
                lines: empty_lines,
            },
            ..GeneratorConfig::default()
        };

        assert_eq!(
            errant_config.format.validate(),
            Err(GeneratorConfigError::ShuffleGeneratorItemsEmpty)
        );
    }

    #[tokio::test]
    async fn shuffle_generator_copies_lines() {
        let message_key = log_schema().message_key();
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two", "three", "four"]
               count = 5"#,
        )
        .await;

        let lines = &["one", "two", "three", "four"];

        for _ in 0..5 {
            let event = match poll!(rx.next()) {
                Poll::Ready(event) => event.unwrap(),
                _ => unreachable!(),
            };
            let log = event.as_log();
            let message = log[&message_key].to_string_lossy();
            assert!(lines.contains(&&*message));
        }

        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn shuffle_generator_limits_count() {
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two"]
               count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn shuffle_generator_adds_sequence() {
        let message_key = log_schema().message_key();
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two"]
               sequence = true
               count = 5"#,
        )
        .await;

        for n in 0..5 {
            let event = match poll!(rx.next()) {
                Poll::Ready(event) => event.unwrap(),
                _ => unreachable!(),
            };
            let log = event.as_log();
            let message = log[&message_key].to_string_lossy();
            assert!(message.starts_with(&n.to_string()));
        }

        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn shuffle_generator_obeys_interval() {
        let start = Instant::now();
        let mut rx = runit(
            r#"format = "shuffle"
               lines = ["one", "two"]
               count = 3
               interval = 1.0"#,
        )
        .await;

        for _ in 0..3 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));

        let duration = start.elapsed();
        assert!(duration >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn apache_common_format_generates_output() {
        let mut rx = runit(
            r#"format = "apache_common"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn apache_error_format_generates_output() {
        let mut rx = runit(
            r#"format = "apache_error"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn syslog_5424_format_generates_output() {
        let mut rx = runit(
            r#"format = "syslog"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn syslog_3164_format_generates_output() {
        let mut rx = runit(
            r#"format = "bsd_syslog"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            assert!(poll!(rx.next()).is_ready());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }

    #[tokio::test]
    async fn json_format_generates_output() {
        let message_key = log_schema().message_key();
        let mut rx = runit(
            r#"format = "json"
            count = 5"#,
        )
        .await;

        for _ in 0..5 {
            let event = match poll!(rx.next()) {
                Poll::Ready(event) => event.unwrap(),
                _ => unreachable!(),
            };
            let log = event.as_log();
            let message = log[&message_key].to_string_lossy();
            assert!(serde_json::from_str::<serde_json::Value>(&message).is_ok());
        }
        assert_eq!(poll!(rx.next()), Poll::Ready(None));
    }
}
