use opentelemetry_api::Context;
use opentelemetry_api::trace::TraceResult;
use opentelemetry_sdk::export::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::trace;
use tokio::sync::mpsc::Sender;
use tracing::error;

/// 何もしないでExporterに渡すだけのProcessor
/// デフォルトで用意されている [SimpleSpanProcessor](opentelemetry_sdk::trace::SimpleSpanProcessor) がTokioに対応してないので自作した。
/// なお、同じくデフォルトで用意されてる [BatchSpanProcessor](opentelemetry_sdk::trace::BatchSpanProcessor) もLambdaではNG(リクエスト処理が終わると実行環境はフリーズされるので、遅延処理は期待通りに動かない)
#[derive(Debug)]
pub(crate) struct SpanProcessor {
    tx: Sender<SpanData>,
}

impl SpanProcessor {
    pub(crate) fn new(mut exporter: Box<dyn SpanExporter>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<SpanData>(32);
        tokio::spawn(async move {
            while let Some(span) = rx.recv().await {
                println!(
                    "SpanProcessor received: {{ name: '{}', trace_id: {}, span_id: {}, parent_id: {}, ... }}",
                    span.name,
                    span.span_context.trace_id(),
                    span.span_context.span_id(),
                    span.parent_span_id,
                );
                if let Err(e) = exporter.export(vec![span]).await {
                    error!("SpanProcessor failed to export: {:?}", e);
                }
            }
        });

        SpanProcessor { tx }
    }
}

impl trace::SpanProcessor for SpanProcessor {
    fn on_start(&self, _span: &mut trace::Span, _cx: &Context) {}

    fn on_end(&self, span: SpanData) {
        if !span.span_context.is_sampled() {
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = tx.send(span).await {
                error!("SpanProcessor failed to send channel: {:?}", e);
            }
        });
    }

    fn force_flush(&self) -> TraceResult<()> {
        Ok(())
    }

    /// 特になにもしない。
    /// そもそもLambdaが一定時間呼び出しがなくてShutdownする際、アプリコードは一切呼ばれないので、実装する意味も無い。
    fn shutdown(&mut self) -> TraceResult<()> {
        Ok(())
    }
}
