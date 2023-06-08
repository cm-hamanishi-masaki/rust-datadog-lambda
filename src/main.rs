use lambda_http::{run, service_fn, Body, Request, Response};
use lambda_runtime::Error;
use std::sync::Mutex;
use tracing::{error, info, instrument, Level};
use tracing_subscriber::filter::{Filtered, Targets};
use tracing_subscriber::fmt::format::{Format, Json, JsonFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

#[cfg_attr(feature = "owned", path = "datadog_helper.rs")]
#[cfg_attr(not(feature = "owned"), path = "otel_helper.rs")]
pub mod helper;

fn get_logger() -> Filtered<tracing_subscriber::fmt::Layer<Registry, JsonFields, Format<Json, ()>>, Targets, Registry> {
    let log_filter = Targets::new()
        .with_target("hyper", Level::INFO)
        .with_target("tower", Level::INFO)
        .with_target("h2", Level::INFO)
        .with_default(Level::DEBUG);

    tracing_subscriber::fmt::layer()
        .json()
        .without_time()
        .with_current_span(false)
        .with_span_list(true)
        .with_target(false)
        .with_filter(log_filter)
}

/// 独自実装したTracingクレートを使用する
#[cfg(feature = "owned")]
#[tokio::main]
async fn main() -> Result<(), Error> {
    use std::time::Duration;
    println!("--- owned mode --------------");

    // Datadog Tracing Layer
    // 外部クレートの中で作成されている tracing::span も対象になるので、フィルター設定に注意
    // なおdatadog-agentの設定でSpanをフィルタする機能もあるらしい
    let tracing = helper::TracingLayer::new()
        .with_service_name("test-owned")
        .with_filter(Targets::new().with_default(Level::INFO));

    tracing_subscriber::registry().with(get_logger()).with(tracing).init();

    run(service_fn(|req: Request| async {
        helper::handle_request_with_trace(req, handle_request).await
    }))
    .await?;

    // BackgroundでAPIにPostしてる可能性があるので、異常終了に備えてWaitさせる必要あり
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// opentelemetry_datadog 使用版
#[cfg(feature = "otel_dd")]
#[tokio::main]
async fn main() -> Result<(), Error> {
    use opentelemetry_api::Key;
    use opentelemetry_api::Value::String;
    use opentelemetry_datadog::DatadogPropagator;
    use opentelemetry_sdk::trace;
    use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler};
    println!("--- otel_dd mode --------------");

    // tracerに opentelemetry_datadog を使用する
    let tracer = opentelemetry_datadog::new_pipeline()
        .with_service_name("test-otel-dd")
        .with_api_version(opentelemetry_datadog::ApiVersion::Version05)
        .with_agent_endpoint("http://localhost:8126")
        .with_name_mapping(|span, _config| {
            // デフォルトでは全て 'opentelemetry-datadog' になってしまうので要変更
            span.name.as_ref()
        })
        .with_resource_mapping(|span, _config| {
            // デフォルトではspanのnameになるので変える
            // Fieldで`resource`をセットしているSpanならそれを使用する
            let key = Key::from_static_str("resource");
            match span.attributes.get(&key) {
                Some(String(v)) => v.as_str(),
                _ => "", //但し空文字だとDatadog上ではspan.nameで代替されるが
            }
        })
        .with_trace_config(
            trace::config()
                .with_sampler(Sampler::AlwaysOn)
                .with_id_generator(RandomIdGenerator::default()),
        )
        .install_batch(opentelemetry::runtime::Tokio)?;

    let tracing = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        // .with_exception_fields(true) 良くわからん。違いが見えない
        .with_filter(Targets::new().with_default(Level::INFO));

    tracing_subscriber::registry().with(get_logger()).with(tracing).init();

    // Propagatorを登録する(が自動でこれが呼ばれたりはしない？？)
    // opentelemetry_datadog にはPropagatorも有り
    // opentelemetry_otlp にはPropagator実装が無いので、独自実装するかopentelemetry_datadogから借用する必要がある
    opentelemetry::global::set_text_map_propagator(DatadogPropagator::default());

    run(service_fn(|req: Request| async {
        helper::handle_request_with_trace(req, handle_request).await
    }))
    .await?;

    opentelemetry::global::shutdown_tracer_provider();
    Ok(())
}

/// opentelemetry_otlp 使用版
/// opentelemetry_datadogとは、Datadog上で修飾のされ方が異なるケースがあるが、大きな問題ではない？
/// 追加で以下が必要
/// - datadog-agentで `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT=0.0.0.0:4317` をセット
/// - Datadog側で `opentelemetry integration` を有効にする
#[cfg(feature = "otel_otlp")]
#[tokio::main]
async fn main() -> Result<(), Error> {
    use opentelemetry_api::KeyValue;
    use opentelemetry_datadog::DatadogPropagator;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::trace::{RandomIdGenerator, Sampler};
    use opentelemetry_sdk::{trace, Resource};
    println!("--- otel_otlp mode --------------");

    // tracerに opentelemetry_otlp を使用する以外は差異無し
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint("http://localhost:4317"), // .with_timeout(Duration::from_secs(3))
                                                         // .with_metadata(map)
        )
        .with_trace_config(
            trace::config()
                .with_sampler(Sampler::AlwaysOn)
                .with_id_generator(RandomIdGenerator::default())
                .with_resource(Resource::new(vec![KeyValue::new("service.name", "test-otel-otlp")])),
        )
        .install_batch(opentelemetry::runtime::Tokio)?;

    let tracing = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(Targets::new().with_default(Level::INFO));

    tracing_subscriber::registry().with(get_logger()).with(tracing).init();

    // Propagatorを登録する
    // opentelemetry_otlp にはPropagator実装が無いので、opentelemetry_datadogから借用する
    opentelemetry::global::set_text_map_propagator(DatadogPropagator::default());

    run(service_fn(|req: Request| async {
        helper::handle_request_with_trace(req, handle_request).await
    }))
    .await?;

    opentelemetry::global::shutdown_tracer_provider();
    Ok(())
}

/// 適当な実装
pub async fn handle_request(_req: Request) -> Result<Response<Body>, Error> {
    info!("start process request");

    let resp = match do_something().await {
        Ok(_) => Response::builder()
            .status(200)
            .header("content-type", "text/html")
            .body("ok".into())
            .map_err(Box::new)?,
        Err(e) => {
            error!("{}", e);
            Response::builder()
                .status(500)
                .header("content-type", "text/html")
                .body(e.to_string().into())
                .map_err(Box::new)?
        }
    };
    Ok(resp)
}

#[instrument]
async fn do_something() -> Result<(), Error> {
    info!("get from some url.");
    let c = reqwest::Client::new();
    let req = c.get(get_url()).build().unwrap();
    match helper::request_http(&c, req).await {
        Ok(_) => {
            // ... do something with result
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

static mut FLAG: Mutex<bool> = Mutex::new(true);

/// 正常なURLとアクセスエラーになるURLを交互に返す
fn get_url<'a>() -> &'a str {
    unsafe {
        let v = FLAG.get_mut().unwrap();
        let ret = if *v {
            "https://www.google.com"
        } else {
            "https://hoge.invalid"
        };
        *v = !*v;
        ret
    }
}
