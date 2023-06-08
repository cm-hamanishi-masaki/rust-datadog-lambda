use chrono::{DateTime, Utc};
use lambda_http::http::header::CONTENT_TYPE;
use lambda_http::http::{HeaderMap, HeaderValue};
use lambda_http::request::RequestContext;
use lambda_http::{Body, Request, RequestExt};
use lambda_runtime::Error;
use rand::Rng;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use tracing::field::Field;
use tracing::span::{Attributes, Record};
use tracing::{info_span, warn, Id};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::{LookupSpan, SpanRef};
use tracing_subscriber::Layer;

pub async fn handle_request_with_trace<Fut>(
    req: Request, f: impl FnOnce(Request) -> Fut,
) -> Result<lambda_http::Response<Body>, Error>
where
    Fut: Future<Output = Result<lambda_http::Response<Body>, Error>>,
{
    // リクエスト元のトレースと繋げる場合、ヘッダでトレーシングID情報が渡されるはずなのでそれを引き継ぐ。無ければ新規採番
    let trace_id = TraceId::from_header(req.headers()).unwrap_or_else(TraceId::new);
    let parent_id = ParentSpanId::from_header(req.headers()).unwrap_or_else(ParentSpanId::new);
    // TraceIdをThreadLocalに保存
    TraceId::store(trace_id);

    // 既にlambda-runtimeが作成したSpanの中なので、↓のようにロギングする事でそのSpanに対してもID情報をセットでき、
    // それにより一連のトレースに組み込む事も出来るが、
    // - それがRootになる
    // - Rootのくせにそこに結果情報を反映できない
    // - その他大した情報を持ってない
    // といった事から使い勝手が良くないので、推奨しない。
    // この場合、そのSpanもDatadogに送られるが、トレースとしては独立したものになる。
    //
    // info!(dd.trace_id = trace_id.0, dd.parent_id = parent_id.0);

    let lambda_ctx = req.lambda_context();
    let request_id = &lambda_ctx.request_id;
    let apigw_ctx = req.request_context();
    let RequestContext::ApiGatewayV1(rest_api_ctx) = apigw_ctx;
    let path = rest_api_ctx.path.unwrap();
    // RootSpanを作成する
    let span = info_span!(
        "handle_request_root",
        dd.trace_id = trace_id.0, // ログとトレースのマージのためにどこかでログ内にTraceIdを含めておきたい意図あり
        dd.parent_id = parent_id.0,
        // 以下任意でDatadogに渡したい値をセットして下さい。以下は一例です。
        // 後から `span.record(..)` で更新するケースでも、このタイミングで宣言しておく必要があります。
        dd.resource = path,
        dd.error = false,
        dd.meta.span.kind = "server",
        dd.meta.request_id = request_id,
        dd.meta.http.status_code = tracing::field::Empty,
        dd.meta.error.msg = None::<String>,
    );
    let _enter = span.enter();
    match f(req).await {
        Ok(ret) => {
            span.record("dd.meta.http.status_code", ret.status().as_u16());
            if ret.status().as_u16() >= 500 {
                span.record("dd.error", true);
            }
            Ok(ret)
        }
        Err(err) => {
            span.record("dd.error", true);
            span.record("dd.meta.error.msg", err.to_string());
            Err(err)
        }
    }
}

/// Reqwestを使ったHTTP処理において、Datadog用のトレース処理を挿入する関数。
/// 引数(リクエスト)やその他の属性をセットし、また処理結果をトレースに反映する。
pub async fn request_http(
    client: &reqwest::Client, mut req: reqwest::Request,
) -> Result<reqwest::Response, reqwest::Error> {
    let span = info_span!(
        "reqwest.http",
        dd.resource = req.url().to_string(),
        dd.error = false,
        dd.meta.span.kind = "client",
        dd.meta.http.method = req.method().to_string(),
        dd.meta.http.status_code = tracing::field::Empty,
        dd.meta.error.msg = None::<String>,
    );
    let _enter = span.enter();

    // リクエストヘッダにトレーシング用ヘッダを追加
    let trace_id = TraceId::get_current();
    let h = req.headers_mut();
    h.insert(TRACE_ID_HEADER, trace_id.0.into());
    h.insert(PARENT_ID_HEADER, span.id().unwrap().into_u64().into());
    match client.execute(req).await {
        Ok(ret) => {
            span.record("dd.meta.http.status_code", ret.status().as_u16());
            // レスポンスが5xxならエラーフラグを立てる。
            // 200でも実質エラーという処理系の場合は適宜対応する事
            if ret.status().as_u16() >= 500 {
                span.record("dd.error", true);
            }
            Ok(ret)
        }
        Err(err) => {
            span.record("dd.error", true);
            span.record("dd.meta.error.msg", err.to_string());
            Err(err)
        }
    }
}

thread_local!(static TRACE_ID: RefCell<TraceId> = RefCell::new(TraceId::new()));
const TRACE_ID_HEADER: &str = "x-datadog-trace-id";
const PARENT_ID_HEADER: &str = "x-datadog-parent-id";

#[derive(Copy, Clone, Debug)]
struct TraceId(u64);

impl TraceId {
    fn new() -> Self {
        TraceId(gen_trace_id())
    }

    fn from_header(headers: &HeaderMap<HeaderValue>) -> Option<TraceId> {
        let dd_trace_id = headers.get(TRACE_ID_HEADER)?.to_str().ok()?.parse::<u64>().ok()?;
        Some(TraceId(dd_trace_id))
    }

    fn store(id: TraceId) {
        TRACE_ID.with(|f| {
            *f.borrow_mut() = id;
        });
    }

    pub fn get_current() -> Self {
        TRACE_ID.with(|f| *f.borrow())
    }
}

#[derive(Copy, Clone, Debug)]
struct ParentSpanId(u64);
impl ParentSpanId {
    fn new() -> Self {
        ParentSpanId(gen_span_id())
    }
    fn from_header(headers: &HeaderMap<HeaderValue>) -> Option<ParentSpanId> {
        let dd_parent_id = headers.get(PARENT_ID_HEADER)?.to_str().ok()?.parse::<u64>().ok()?;
        Some(ParentSpanId(dd_parent_id))
    }
}

fn gen_trace_id() -> u64 {
    let mut rng = rand::thread_rng();
    rng.gen::<u64>()
}

fn gen_span_id() -> u64 {
    let mut rng = rand::thread_rng();
    rng.gen::<u64>()
}

struct TracingConfig {
    pub service_name: String,
}

impl Default for TracingConfig {
    fn default() -> Self {
        TracingConfig {
            service_name: "".to_string(),
        }
    }
}

pub struct TracingLayer {
    config: TracingConfig,
    client: reqwest::Client,
}

impl TracingLayer {
    pub fn new() -> Self {
        TracingLayer {
            config: TracingConfig::default(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_service_name(mut self, service_name: &str) -> Self {
        self.config.service_name = service_name.to_string();
        self
    }

    fn send_to_datadog_agent(&self, span: &mut DDSpan) {
        span.service = self.config.service_name.to_owned();
        let json = serde_json::to_string(&span).unwrap();
        let body = format!("[[{}]]", json); // spanを複数同時に送信可能だがlocalhost宛なので、、
        let endpoint = "http://localhost:8126/v0.3/traces";
        let client = self.client.clone();
        tokio::spawn(async move {
            println!("@@ will send to ddagent: {}", body);
            if let Err(e) = client
                .post(endpoint)
                .header(CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
            {
                // if *ENV != Env::Local {
                warn!("send to datadog-agent failed: {:?}", e);
                // }
            }
        });
    }

    fn with_dd_span<'a, S>(span: SpanRef<'a, S>, f: impl FnOnce(&mut DDSpan))
    where
        S: LookupSpan<'a>,
    {
        span.extensions_mut().get_mut::<DDSpan>().map(f);
    }
}

impl<S> Layer<S> for TracingLayer
where
    S: tracing::Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    /// spanが作成された時に呼ばれる。
    /// AttributesなどからDatadog用のStruct(DDSpan)を作成する。
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).unwrap();
        let name = span.name().to_string();
        // let (id1, id2) = TRACING_IDS.with(|ids| {
        //     (ids.borrow().0, ids.borrow().1)
        // });
        // let mut dd_span = TRACING_IDS.with(|ids| {
        //     DDSpan::new(name, ids.borrow(), id)
        // });
        // let ids = TracingIds::get_current();

        // 親SpanからTraceId(と親のSpanId)を取得する
        // 親がいなかったり、親が持ってない場合は新規採番するが、最終的にAttributesの値が反映される
        let ids = span.parent().and_then(|s| {
            s.extensions_mut()
                .get_mut::<DDSpan>()
                .map(|ds| (ds.trace_id, ds.span_id))
        });
        let ids = ids.unwrap_or((TraceId::new().0, 0));
        let mut dd_span = DDSpan {
            name,
            trace_id: ids.0,
            parent_id: ids.1,
            span_id: id.into_u64(), // gen_span_id(),
            ..Default::default()
        };

        // Attributesを反映
        let mut updator = DDSpanUpdator(&mut dd_span);
        attrs.record(&mut updator);

        // extensionsに保存する
        let mut extensions = span.extensions_mut();
        extensions.insert::<DDSpan>(dd_span);
    }

    /// `span.record(...)`メソッドで情報が追加・更新された時に呼ばれる。それらの情報でDDSpanを更新する。
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        Self::with_dd_span(ctx.span(id).unwrap(), |ds| {
            let mut updator = DDSpanUpdator(ds);
            values.record(&mut updator);
        });
    }

    /// Span内でログ出力された時に呼ばれる。その出力内容からDDSpanを更新する。
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.event_span(event) {
            Self::with_dd_span(span, |ds| {
                let mut updator = DDSpanUpdator(ds);
                event.record(&mut updator);
            });
        };
    }

    /// SpanがEnteredになった時の時間を、開始時間としてDDSpanに記録する。
    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        Self::with_dd_span(ctx.span(id).unwrap(), |ds| ds.set_start_once());
    }

    /// SpanがExitした時の時間を、終了時間としてDDSpanに記録する。
    /// 非同期処理でpollingされる都度、EnterとExitを繰り返す挙動になるので、最後のExitの時刻を補足する必要がある点に注意。
    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        Self::with_dd_span(ctx.span(id).unwrap(), |ds| ds.update_end());
    }

    /// SpanがクローズされたらDDSpanをDatadogに送信する。
    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        Self::with_dd_span(ctx.span(&id).unwrap(), |ds| self.send_to_datadog_agent(ds));
    }
}

/// https://docs.datadoghq.com/ja/tracing/guide/send_traces_to_agent_by_api/
/// https://docs.datadoghq.com/ja/tracing/trace_collection/tracing_naming_convention
#[derive(Serialize, Debug)]
struct DDSpan {
    name: String,
    trace_id: u64,
    #[serde(skip_serializing_if = "DDSpan::is_zero")]
    parent_id: u64,
    span_id: u64,
    start: u64,      // epoch nano
    duration: u64,   // nano sec from start
    service: String, // 必須。ddagentの方の環境変数で指定してても省略不可
    resource: String,
    error: i32,
    meta: HashMap<String, String>,
    metrics: HashMap<String, f64>,
    r#type: String,
}

impl DDSpan {
    fn is_zero(num: &u64) -> bool {
        *num == 0
    }

    fn utc_epoch_nanos(dt: DateTime<Utc>) -> u64 {
        let a = dt.timestamp() * 1_000_000_000;
        let b = dt.timestamp_subsec_nanos() as i64;
        (a + b) as u64
    }

    fn set_start_once(&mut self) {
        if self.start == 0 {
            self.start = Self::utc_epoch_nanos(Utc::now());
        }
    }

    fn update_end(&mut self) {
        let n = Self::utc_epoch_nanos(Utc::now());
        self.duration = n - self.start;
    }
}

impl Default for DDSpan {
    fn default() -> Self {
        DDSpan {
            name: "".to_string(),
            trace_id: 0,
            span_id: 0,
            parent_id: 0,
            service: "".to_string(),
            resource: "".to_string(),
            start: 0,
            duration: 0,
            error: 0,
            meta: Default::default(),
            metrics: Default::default(),
            r#type: "".to_string(),
        }
    }
}

struct DDSpanUpdator<'a>(&'a mut DDSpan);

#[allow(clippy::single_match)]
impl tracing::field::Visit for DDSpanUpdator<'_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        if !field.name().starts_with("dd.") {
            return;
        }
        match field.name() {
            "dd.meta.http.status_code" => {
                self.0.meta.insert("http.status_code".to_string(), value.to_string());
            }
            _ => {}
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if !field.name().starts_with("dd.") {
            return;
        }
        match field.name() {
            "dd.trace_id" => {
                self.0.trace_id = value;
            }
            "dd.parent_id" => {
                self.0.parent_id = value;
            }
            _ => {}
        }
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        if !field.name().starts_with("dd.") {
            return;
        }
        match field.name() {
            "dd.error" => {
                self.0.error = i32::from(value) // trueなら1;
            }
            _ => {}
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if !field.name().starts_with("dd.") {
            return;
        }
        match field.name() {
            "dd.resource" => {
                println!("*** resource ****  {value}");
                self.0.resource = value.to_string();
            }
            "dd.meta.request_id" => {
                self.0.meta.insert("request_id".to_string(), value.to_string());
            }
            "dd.meta.error.msg" => {
                self.0.meta.insert("error.msg".to_string(), value.to_string());
            }
            "dd.meta.span.kind" => {
                self.0.meta.insert("span.kind".to_string(), value.to_string());
            }
            "dd.meta.http.url" => {
                self.0.meta.insert("http.url".to_string(), value.to_string());
            }
            "dd.meta.http.method" => {
                self.0.meta.insert("http.method".to_string(), value.to_string());
            }
            _ => {}
        }
    }
    fn record_debug(&mut self, field: &Field, _value: &dyn Debug) {
        if !field.name().starts_with("dd.") {
            return;
        }
        warn!("{} is not yet implemented", field.name());
    }
}
