//! Stream adapters: re-emit upstream openai/Codex/Gemini SSE in the
//! client's dialect. Codex/Gemini translators live in their wire modules.

use axum::body::Bytes;
use futures::Stream;
use std::pin::Pin;

pub(crate) struct AnthropicSse<S> {
    inner: S,
    tr: crate::translate::SseTranslator,
    tail: Option<Bytes>,
}

impl<S> AnthropicSse<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            inner,
            tr: crate::translate::SseTranslator::new(),
            tail: None,
        }
    }
}

impl<S> Stream for AnthropicSse<S>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
{
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        if let Some(t) = this.tail.take() {
            return std::task::Poll::Ready(Some(Ok(t)));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                let frames = this.tr.push_bytes(&bytes);
                if frames.is_empty() {
                    // no complete anthropic event yet: poll again
                    cx.waker().wake_by_ref();
                    return std::task::Poll::Pending;
                }
                std::task::Poll::Ready(Some(Ok(Bytes::from(frames.concat()))))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => {
                let frames = this.tr.finish();
                if frames.is_empty() {
                    std::task::Poll::Ready(None)
                } else {
                    this.tail = None;
                    std::task::Poll::Ready(Some(Ok(Bytes::from(frames.concat()))))
                }
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
