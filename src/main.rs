mod datadog;

use lambda_http::{run, service_fn, Body, Error, Request, Response};
use tracing::{debug, info, instrument, Level};
use tracing_subscriber::filter::Targets;
// use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{ SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

/// This is the main body for the function.
/// Write your code inside it.
/// There are some code example in the following URLs:
/// - https://github.com/awslabs/aws-lambda-rust-runtime/tree/main/examples
async fn handle_request(_req: Request) -> Result<Response<Body>, Error> {
    info!("start process request");
    let message = format!("Hello");
    get_some().await;
    let resp = Response::builder()
        .status(200)
        .header("content-type", "text/html")
        .body(message.into())
        .map_err(Box::new)?;
    Ok(resp)
}

#[instrument]
async fn get_some() {
    let c = reqwest::Client::new();
    let req = c.get("https://www.google.comm").build().unwrap();
    let _ret = datadog::request_http(&c, req).await;
    // do something with result...
    info!("done something.")
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let log_filter = Targets::new()
        .with_target("hyper", Level::INFO)
        .with_default(Level::DEBUG);
    // let offset = time::UtcOffset::from_hms(9, 0, 0).unwrap();
    // let time_format = time::format_description::parse("[hour]:[minute]:[second].[subsecond]").unwrap();
    // let json_formatter = tracing_subscriber::fmt::layer().json()
    //     .without_time()
    //     // .with_timer(OffsetTime::new(offset, time_format))
    //     ;

    let json_logger = tracing_subscriber::fmt::layer().json()
        .without_time()
        .with_current_span(false)
        .with_span_list(true)
        .with_target(false)
        .with_filter(log_filter);

    // Datadog Tracing Layer
    // 外部クレートの中で作成されている tracing::span も対象になるので、フィルター設定に注意
    // なおdatadog-agentの設定でSpanをフィルタする機能もあるらしい
    let datadog_tracing = datadog::DatadogTracingLayer::new()
        .with_filter(Targets::new()
            .with_default(Level::INFO));

    tracing_subscriber::registry()
        .with(json_logger)
        .with(datadog_tracing)
        .init();

    run(service_fn(|req: Request| async {
        datadog::handle_request_with_trace(req, handle_request).await
    })).await
}
