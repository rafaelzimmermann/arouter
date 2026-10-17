//! SSE usage scanning and exactly-once stream finalization.

use crate::budget;
use crate::health::Health;
use crate::metrics::{self, Metrics};
use crate::usage::{unix_now, UsageLog, UsageRecord};
use axum::body::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

// ─────────────────────────── SSE usage tap ───────────────────────────

pub struct TapMeta {
    pub pool: String,
    pub profile: String,
    pub requested: String,
    pub served: String,
    pub caller: String,
    pub project: Option<String>,
    pub model: String,
    pub cost_in: f64,
    pub cost_out: f64,
    pub degraded: bool,
}

/// Forwards upstream bytes untouched while scanning SSE lines for a final
/// `usage` object; records one UsageRecord (usage log + metrics) when the
/// stream ends.
pub struct TapStream {
    inner: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
    scan: UsageScan,
    meta: TapMeta,
    usage: Arc<UsageLog>,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    t0: Instant,
    /// Budget hold for this stream (ondemand only) — settled with the
    /// actual usage in `finish`, released by Drop if never finished.
    reserve: Option<budget::ReserveGuard>,
    /// E10b: token daily budgets accrue settled spend on stream finish.
    tokens: Arc<crate::tokens::TokenStore>,
    /// F12: the concurrency gauge counts the STREAM BODY too — the
    /// handler's own guard dies when it returns headers.
    in_flight: Option<metrics::InFlightGuard>,
    first_byte: Option<Instant>,
    recorded: bool,
}

impl TapStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inner: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
        meta: TapMeta,
        usage: Arc<UsageLog>,
        metrics: Arc<Metrics>,
        health: Arc<Health>,
        tokens: Arc<crate::tokens::TokenStore>,
        t0: Instant,
        reserve: Option<budget::ReserveGuard>,
    ) -> Self {
        let in_flight = metrics.in_flight(); // F12: body counts as in-flight
        Self {
            inner: Box::pin(inner),
            scan: UsageScan::default(),
            meta,
            usage,
            metrics,
            health,
            tokens,
            t0,
            reserve,
            in_flight: Some(in_flight),
            first_byte: None,
            recorded: false,
        }
    }

    /// Exactly-once finalization (review F4). Statuses:
    /// - "ok": clean EOF — health success
    /// - "aborted": downstream hung up mid-stream — the upstream WAS
    ///   serving, so health success, but the attempt is recorded and
    ///   any billable tokens scanned so far still spend
    /// - "upstream_error": the upstream stream broke — health failure
    ///   (retryable), attempt recorded
    fn finish(&mut self, status: &str) {
        // F2: actual usage replaces the estimate hold exactly once
        if let Some(g) = self.reserve.take() {
            g.settle();
        }
        if self.recorded {
            return;
        }
        self.recorded = true;
        match status {
            "ok" | "aborted" => self.health.record_success_latency(
                &self.meta.model,
                Some(self.t0.elapsed().as_millis() as u64),
            ),
            "upstream_error" => self.health.record_failure(&self.meta.model, true),
            _ => {}
        }
        let (tin, tout) = self.scan.usage();
        let rec = UsageRecord {
            ts_unix: unix_now(),
            pool: self.meta.pool.clone(),
            profile: self.meta.profile.clone(),
            caller: self.meta.caller.clone(),
            project: self.meta.project.clone(),
            grade_requested: self.meta.requested.clone(),
            grade_served: self.meta.served.clone(),
            model: self.meta.model.clone(),
            status: status.into(),
            http_status: Some(200),
            tokens_in: tin,
            tokens_out: tout,
            cost_usd: UsageRecord::cost_for(tin, tout, self.meta.cost_in, self.meta.cost_out),
            latency_ms: self.t0.elapsed().as_millis() as u64,
            ttfb_ms: self
                .first_byte
                .map(|t| t.duration_since(self.t0).as_millis() as u64),
            degraded: self.meta.degraded,
        };
        self.usage.record(rec.clone());
        self.metrics.record(&rec);
        // E10b: accrue settled spend against the token's daily budget
        // ("aborted" streams keep their scanned tokens — they bill).
        self.tokens
            .charge_by_caller(&self.meta.caller, rec.cost_usd);
    }
}

impl Stream for TapStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                if self.first_byte.is_none() {
                    self.first_byte = Some(Instant::now());
                }
                self.scan.feed(&bytes);
                std::task::Poll::Ready(Some(Ok(bytes)))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                self.finish("upstream_error");
                std::task::Poll::Ready(Some(Err(e)))
            }
            std::task::Poll::Ready(None) => {
                self.finish("ok");
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Client cancellation is finalization too (review F4): when hyper
/// drops the response body, the attempt must still be recorded —
/// exactly once (the `recorded` flag makes the explicit paths no-ops).
impl Drop for TapStream {
    fn drop(&mut self) {
        self.finish("aborted");
        // F12: stream body over — release the in-flight gauge slot
        drop(self.in_flight.take());
    }
}

/// Wraps a stream of upstream openai-SSE bytes and re-emits anthropic
/// SSE frames (E7). Sits *outside* the TapStream so usage accounting,
/// health and metrics still see the openai wire they already parse.

#[derive(Default)]
pub struct UsageScan {
    buf: Vec<u8>,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
}

impl UsageScan {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // strip \n
            self.handle_line(&line);
        }
        // Cap: never let a malformed stream grow the buffer unbounded.
        if self.buf.len() > 1_048_576 {
            self.buf.clear();
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        let line = match line.strip_suffix(b"\r") {
            Some(l) => l,
            None => line,
        };
        let Some(payload) = line.strip_prefix(b"data:") else {
            return;
        };
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);
        if payload == b"[DONE]" {
            return;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
            if let Some(u) = v.get("usage") {
                if let Some(t) = u.get("prompt_tokens").and_then(|x| x.as_u64()) {
                    self.tokens_in = Some(t);
                }
                if let Some(t) = u.get("completion_tokens").and_then(|x| x.as_u64()) {
                    self.tokens_out = Some(t);
                }
            }
        }
    }

    pub fn usage(&self) -> (Option<u64>, Option<u64>) {
        (self.tokens_in, self.tokens_out)
    }
}

/// Extract `usage` from a non-streaming chat completion body.
pub(crate) fn extract_usage(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| {
            let u = v.get("usage")?;
            Some((
                u.get("prompt_tokens").and_then(|x| x.as_u64()),
                u.get("completion_tokens").and_then(|x| x.as_u64()),
            ))
        })
        .unwrap_or((None, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use crate::usage::UsageLog;
    use futures::{pin_mut, Stream};
    use std::sync::Arc;
    use std::task::Poll;
    use std::time::Instant;

    struct HeldStream {
        _guard: (),
    }
    impl Stream for HeldStream {
        type Item = std::io::Result<Bytes>;
        fn poll_next(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending // never resolves: simulates a live stream
        }
    }

    fn tap_meta() -> TapMeta {
        TapMeta {
            pool: "quota".into(),
            profile: "default".into(),
            requested: "A".into(),
            served: "A".into(),
            caller: "t".into(),
            project: None,
            model: "p/m".into(),
            cost_in: 0.0,
            cost_out: 0.0,
            degraded: false,
        }
    }

    #[test]
    fn dropping_a_pending_stream_records_the_abort() {
        // review F4 contract: client cancellation must record the attempt
        let usage = Arc::new(UsageLog::in_memory());
        let metrics = Arc::new(Metrics::new());
        let health = crate::health::Health::new(Default::default());
        let tokens = Arc::new(crate::tokens::TokenStore::in_memory());
        {
            let tap = TapStream::new(
                HeldStream { _guard: () },
                tap_meta(),
                usage.clone(),
                metrics.clone(),
                health.clone(),
                tokens.clone(),
                Instant::now(),
                None,
            );
            pin_mut!(tap);
            assert!(matches!(
                tap.as_mut().poll_next(&mut std::task::Context::from_waker(
                    futures::task::noop_waker_ref()
                )),
                Poll::Pending
            ));
            // drop happens here — the client hung up mid-stream
        }
        let recs = usage.records();
        assert_eq!(recs.len(), 1, "abort recorded");
        assert_eq!(recs[0].status, "aborted");
    }

    #[tokio::test]
    async fn upstream_stream_error_is_failure_not_success() {
        // review F4 contract: a broken upstream stream must NOT count as
        // health success
        let usage = Arc::new(UsageLog::in_memory());
        let metrics = Arc::new(Metrics::new());
        let health = crate::health::Health::new(Default::default());
        let tokens = Arc::new(crate::tokens::TokenStore::in_memory());
        // a REAL reqwest error (connection refused) — constructing one
        // synthetically isn't public API
        let err = reqwest::get("http://127.0.0.1:1/x")
            .await
            .expect_err("nothing listens on port 1");
        let broken = futures::stream::iter(vec![Err::<Bytes, std::io::Error>(
            std::io::Error::other(err.to_string()),
        )]);
        {
            let tap = TapStream::new(
                broken,
                tap_meta(),
                usage.clone(),
                metrics.clone(),
                health.clone(),
                tokens.clone(),
                Instant::now(),
                None,
            );
            pin_mut!(tap);
            let out = tap.as_mut().poll_next(&mut std::task::Context::from_waker(
                futures::task::noop_waker_ref(),
            ));
            assert!(matches!(&out, Poll::Ready(Some(Err(_)))));
            // scope end drops the TapStream itself — the Err arm already
            // finished, so Drop must not double-record
        }
        let recs = usage.records();
        assert_eq!(recs.len(), 1, "exactly once");
        assert_eq!(recs[0].status, "upstream_error");
        let snap = health.snapshot();
        assert!(
            snap.get("p/m").is_some_and(|s| s.fail_ratio_1h > 0.0),
            "health failure recorded"
        );
    }

    fn feed_chunks(scan: &mut UsageScan, chunks: &[&str]) -> (Option<u64>, Option<u64>) {
        for c in chunks {
            scan.feed(c.as_bytes());
        }
        scan.usage()
    }

    #[test]
    fn scan_extracts_usage_from_final_chunk() {
        let mut s = UsageScan::default();
        let (tin, tout) = feed_chunks(
            &mut s,
            &[
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n",
                "data: [DONE]\n\n",
            ],
        );
        assert_eq!((tin, tout), (Some(7), Some(2)));
    }

    #[test]
    fn scan_handles_split_boundaries() {
        let mut s = UsageScan::default();
        // one logical line split across three network chunks
        let (tin, tout) = feed_chunks(
            &mut s,
            &[
                "data: {\"usage\":{\"prompt",
                "_tokens\":42,\"completion",
                "_tokens\":9}}\n",
            ],
        );
        assert_eq!((tin, tout), (Some(42), Some(9)));
    }

    #[test]
    fn scan_ignores_non_data_and_done() {
        let mut s = UsageScan::default();
        let (tin, tout) = feed_chunks(
            &mut s,
            &[
                ": keep-alive\n",
                "event: message\n",
                "data: [DONE]\n",
                "data: {\"choices\":[]}\n",
            ],
        );
        assert_eq!((tin, tout), (None, None));
    }

    #[test]
    fn scan_no_usage_in_stream() {
        let mut s = UsageScan::default();
        let (tin, tout) = feed_chunks(
            &mut s,
            &["data: {\"choices\":[{\"delta\":{}}]}\ndata: [DONE]\n"],
        );
        assert_eq!((tin, tout), (None, None));
    }

    #[test]
    fn extract_usage_non_stream() {
        let body = r#"{"id":"1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#;
        assert_eq!(extract_usage(body.as_bytes()), (Some(10), Some(4)));
        assert_eq!(extract_usage(b"{}"), (None, None));
        assert_eq!(extract_usage(b"not json"), (None, None));
    }
}
