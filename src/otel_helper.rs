//! ヘルパー関数群
//!

use lambda_http::request::RequestContext;
use lambda_http::{Body, Request, RequestExt};
use lambda_runtime::Error;
use opentelemetry_api::trace::TraceContextExt;
use opentelemetry_api::Context;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use rand::Rng;
use std::collections::HashMap;
use std::future::Future;
use tracing::info_span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

fn gen_trace_id() -> u64 {
    let mut rng = rand::thread_rng();
    rng.gen::<u64>()
}

/// ContextからトレースIDを取得する。
/// Contextは適切な取得の仕方をしないと期待した結果にならないので注意。
/// 基本的には例のように `Span::current()` から辿る。
///
/// ## Example
/// ログ出力時に付加すれば、Datadog上で当該トレースとマージされる。
/// ```
/// let ctx = tracing::Span::current().context();
/// let trace_id = get_trace_id_from(&ctx);
/// info!(trace_id, "hello");
/// ```
pub fn get_trace_id_from(ctx: &Context) -> u64 {
    let trace_id = ctx.span().span_context().trace_id();
    u128::from_be_bytes(trace_id.to_bytes()) as u64
}

/// リクエストを処理する際に挿入するヘルパー関数。
/// リクエスト毎に以下の処理を行う。
/// - トレースIDをヘッダから取り出し、Contextに格納する(ヘッダに含まれない場合は新規採番する)。
/// - 処理の起点となるSpanを作成し、付属情報を色々セットする。
/// - 処理結果をSpanに反映する。
///
/// ## Example
/// ```
/// async fn handle_request(req: Request) -> Result<(), Error> {
///     ...
/// }
///
/// #[tokio::main]
/// async fn main() -> Result<(), Error> {
///
///     ..subscribe tracing layers
///
///     run(service_fn(|req: Request| async {
///         helper::handle_request_with_trace(req, handle_request).await
///     })).await?;
///
///     Ok(())
/// ```
pub async fn handle_request_with_trace<Fut>(
    req: Request, f: impl FnOnce(Request) -> Fut,
) -> Result<lambda_http::Response<Body>, Error>
where
    Fut: Future<Output = Result<lambda_http::Response<Body>, Error>>,
{
    // リクエストヘッダをPropagatorに渡して、トレースID等をContextに保持する
    // Propagatorはmain.rsの冒頭でsetしたDatadogPropagator(のはず
    let ctx = opentelemetry::global::get_text_map_propagator(|propagator| {
        // ヘッダにトレースID等が含まれない場合、Propagatorは無効なContextを返す実装になっており、結果的にトレースが送られないので
        // その場合は新規採番して処理させる
        if req.headers().contains_key("x-datadog-trace-id") {
            propagator.extract(&HeaderExtractor(req.headers()))
        } else {
            let mut map = HashMap::new();
            map.insert("x-datadog-trace-id".to_string(), gen_trace_id().to_string());
            map.insert("x-datadog-parent-id".to_string(), "0".to_string());
            propagator.extract(&map)
        }
    });

    let lambda_ctx = req.lambda_context();
    let request_id = &lambda_ctx.request_id;
    let apigw_ctx = req.request_context();
    let RequestContext::ApiGatewayV1(rest_api_ctx) = apigw_ctx;
    let path = rest_api_ctx.path.unwrap();
    // RootSpanを作成する
    let root_span = info_span!(
        "handle_request_root",
        // Logとのマージ用にトレースIDを出力しておく。Log毎にトレースIDを
        trace_id = get_trace_id_from(&ctx),
        // 以下任意でDatadogに渡したい値をセットする。以下は一例。
        // 後から `span.record(..)` で更新するケースでも、このタイミングで宣言しておく必要がある。
        // otel.で始まるフィールドは特別に処理されたりしてる。詳細は実装(tracing-opentelemetry-0.19.0/src/layer.rs)参照
        request_id,
        resource = path,
        otel.kind = "server",
        otel.status_code = "unset", // ok/error/else=unset。okをセットするのは必須ではない
        error.message = None::<String>
    );

    // lambda-runtimeを使用していると、この時点で`Lambda runtime invoke`というSpanが作成済みだが、
    // ここで作成しているRootSpanと被るので、それは無視してこのSpanにParentを設定する。
    // ちなみに無視したそのSpanもDatadogには送られる(がトレースからは孤立したSpanになる。
    root_span.set_parent(ctx);
    let _enter = root_span.enter();

    let handle_request_result = f(req).await;
    match handle_request_result {
        Ok(ret) => {
            root_span.record("http.status_code", ret.status().as_u16());
            if ret.status().as_u16() >= 500 {
                // エラー時には、OtelSpanのStatusをErrorにしたい(そうすればDatadog上でもフラグが立つ)
                // OtelのSpanにはそれらのためのメソッドが用意されてるが、tracing経由だとアクセスできないので、
                // 従来通り tracing::Span.record() する
                // なお、span内からエラーレベルのログを出力した場合も、SpanStatusはErrorになる。
                root_span.record("otel.status_code", "error");
            }
            Ok(ret)
        }
        Err(err) => {
            root_span.record("otel.status_code", "error");
            // エラーメッセージを回収する(ログとマージするなら冗長かもしれないが)
            // tracing-otelでは `exception.message` という名前を指定しているようだが、それだとDatadog側で認識されない
            root_span.record("error.message", err.to_string());
            Err(err)
        }
    }
}

/// 外部へのHTTPアクセスする際に差し込むヘルパー関数
/// 処理内容は
/// - リクエストヘッダにトレースID等を挿入する(これにより、アクセス先がDatadogに対応していればトレースが繋がる)
/// - Spanを新規に作成し、URLなどの付属情報を色々セットする。
/// - レスポンスをSpanに反映する。
///
/// ここではクライアントとして `Reqwest` を使う実装となっている。
///
/// ## Example
/// ```
/// let client = reqwest::Client::new();
/// let req = client.get("https://www.google.com").build().unwrap();
/// let res = helper::request_http(&client, req);
/// ```
pub async fn request_http(
    client: &reqwest::Client, mut req: reqwest::Request,
) -> Result<reqwest::Response, reqwest::Error> {
    let span = info_span!(
        "reqwest.http",
        resource = req.url().to_string(),
        http.url = req.url().to_string(),
        http.method = req.method().to_string(),
        http.status_code = tracing::field::Empty,
        otel.kind = "client",
        otel.status_code = "unset",
        error.message = None::<String>,
    );
    let _enter = span.enter();

    // リクエストヘッダにトレーシング用ヘッダを追加する
    // アクセス先もDatadogに対応していればトレースが繋がる想定
    let headers = req.headers_mut();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        let mut injector = HeaderInjector(headers);
        propagator.inject_context(&span.context(), &mut injector)
    });

    match client.execute(req).await {
        Ok(ret) => {
            span.record("http.status_code", ret.status().as_u16());
            // レスポンスが5xxならエラーフラグを立てる。
            // 200でも実質エラーという処理系の場合は適宜対応する事
            if ret.status().as_u16() >= 500 {
                span.record("otel.status_code", "error");
            }
            Ok(ret)
        }
        Err(err) => {
            span.record("otel.status_code", "error");
            span.record("error.message", err.to_string());
            Err(err)
        }
    }
}
