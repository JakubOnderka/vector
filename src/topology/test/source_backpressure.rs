use async_graphql::futures_util::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{timeout, Duration};
use vrl::prelude::fmt::{Debug, Formatter};

use crate::config::{DataType, SinkConfig, SinkContext, SinkOuter};
use crate::event::Event;
use crate::sinks::blackhole::BlackholeConfig;
use crate::sinks::util::StreamSink;
use crate::sinks::{Healthcheck, VectorSink};
use crate::sources::demo_logs::OutputFormat;
use crate::topology::builder::PIPELINE_BUFFER_SIZE;
use crate::{
    config::Config,
    sinks::{
        console::{ConsoleSinkConfig, Target},
        util::encoding::StandardEncodings,
    },
    sources::demo_logs::DemoLogsConfig,
    test_util::start_topology,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vector_core::buffers::{BufferConfig, BufferType, WhenFull};

pub const MEMORY_BUFFER_DEFAULT_MAX_EVENTS: usize = 500;

#[tokio::test]
async fn serial_backpressure() {
    let mut config = Config::builder();

    let events_to_sink = 100;

    let expected_sourced_events =
        events_to_sink + MEMORY_BUFFER_DEFAULT_MAX_EVENTS + PIPELINE_BUFFER_SIZE + 3;

    let source_counter = Arc::new(AtomicUsize::new(0));
    config.add_source(
        "in",
        test_source::TestBackpressureSourceConfig {
            counter: source_counter.clone(),
        },
    );
    config.add_sink(
        "out",
        &["in"],
        test_sink::TestBackpressureSinkConfig {
            num_to_consume: events_to_sink,
        },
    );

    let (_topology, _crash) = start_topology(config.build().unwrap(), false).await;

    // allow the topology to run
    tokio::time::sleep(Duration::from_millis(500)).await;

    let sourced_events = source_counter.load(Ordering::Relaxed);

    assert_eq!(sourced_events, expected_sourced_events);
}

#[tokio::test]
async fn default_fan_out() {
    let mut config = Config::builder();

    let events_to_sink = 100;

    let expected_sourced_events =
        events_to_sink + MEMORY_BUFFER_DEFAULT_MAX_EVENTS + PIPELINE_BUFFER_SIZE + 3;

    let source_counter = Arc::new(AtomicUsize::new(0));
    config.add_source(
        "in",
        test_source::TestBackpressureSourceConfig {
            counter: source_counter.clone(),
        },
    );
    config.add_sink(
        "out1",
        &["in"],
        test_sink::TestBackpressureSinkConfig {
            num_to_consume: events_to_sink * 2,
        },
    );

    config.add_sink(
        "out2",
        &["in"],
        test_sink::TestBackpressureSinkConfig {
            num_to_consume: events_to_sink,
        },
    );

    let (_topology, _crash) = start_topology(config.build().unwrap(), false).await;

    // allow the topology to run
    tokio::time::sleep(Duration::from_millis(500)).await;

    let sourced_events = source_counter.load(Ordering::Relaxed);

    assert_eq!(sourced_events, expected_sourced_events);
}

#[tokio::test]
async fn buffer_drop_fan_out() {
    let mut config = Config::builder();

    let events_to_sink = 100;

    let expected_sourced_events =
        events_to_sink + MEMORY_BUFFER_DEFAULT_MAX_EVENTS + PIPELINE_BUFFER_SIZE + 3;

    let source_counter = Arc::new(AtomicUsize::new(0));
    config.add_source(
        "in",
        test_source::TestBackpressureSourceConfig {
            counter: source_counter.clone(),
        },
    );
    config.add_sink(
        "out1",
        &["in"],
        test_sink::TestBackpressureSinkConfig {
            num_to_consume: events_to_sink,
        },
    );

    let mut sink_outer = SinkOuter::new(
        vec!["in".to_string()],
        Box::new(test_sink::TestBackpressureSinkConfig {
            num_to_consume: events_to_sink / 2,
        }),
    );
    sink_outer.buffer = BufferConfig {
        stages: vec![BufferType::MemoryV1 {
            max_events: MEMORY_BUFFER_DEFAULT_MAX_EVENTS,
            when_full: WhenFull::DropNewest,
        }],
    };
    config.add_sink_outer("out2", sink_outer);

    let (_topology, _crash) = start_topology(config.build().unwrap(), false).await;

    // allow the topology to run
    tokio::time::sleep(Duration::from_millis(500)).await;

    let sourced_events = source_counter.load(Ordering::Relaxed);

    assert_eq!(sourced_events, expected_sourced_events);
}

mod test_sink {
    use crate::config::{DataType, SinkConfig, SinkContext};
    use crate::event::Event;
    use crate::sinks::util::StreamSink;
    use crate::sinks::{Healthcheck, VectorSink};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use futures::{FutureExt, StreamExt};
    use serde::{Deserialize, Serialize};

    #[derive(Debug)]
    struct TestBackpressureSink {
        // It consumes this many then stops.
        num_to_consume: usize,
    }

    #[async_trait]
    impl StreamSink for TestBackpressureSink {
        async fn run(self: Box<Self>, input: BoxStream<'_, Event>) -> Result<(), ()> {
            let _num_taken = input.take(self.num_to_consume).count().await;
            futures::future::pending::<()>().await;
            Ok(())
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct TestBackpressureSinkConfig {
        pub num_to_consume: usize,
    }

    #[async_trait]
    #[typetag::serde(name = "test-backpressure-sink")]
    impl SinkConfig for TestBackpressureSinkConfig {
        async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
            let sink = TestBackpressureSink {
                num_to_consume: self.num_to_consume,
            };
            let healthcheck = futures::future::ok(()).boxed();
            Ok((VectorSink::Stream(Box::new(sink)), healthcheck))
        }

        fn input_type(&self) -> DataType {
            DataType::Any
        }

        fn sink_type(&self) -> &'static str {
            "test-backpressure-sink"
        }
    }
}

mod test_source {
    use std::sync::Arc;
    // use std::cmp::Ordering;
    use crate::config::{DataType, SourceConfig, SourceContext};
    use crate::event::Event;
    use crate::sources::Source;
    use async_trait::async_trait;
    use futures::{FutureExt, SinkExt, StreamExt};
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Serialize, Deserialize)]
    pub struct TestBackpressureSourceConfig {
        // The number of events to send. Can be read to see the current progress
        #[serde(skip)]
        pub counter: Arc<AtomicUsize>,
    }

    #[async_trait]
    #[typetag::serde(name = "test-backpressure-source")]
    impl SourceConfig for TestBackpressureSourceConfig {
        async fn build(&self, mut cx: SourceContext) -> crate::Result<Source> {
            let counter = self.counter.clone();
            Ok(async move {
                for i in 0.. {
                    cx.out.send(Event::from(format!("event-{}", i))).await;
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            .boxed())
        }

        fn output_type(&self) -> DataType {
            DataType::Any
        }

        fn source_type(&self) -> &'static str {
            "test-backpressure-source"
        }
    }
}
