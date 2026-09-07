//! Streaming body with a hard size limit (does not buffer the entire payload first).

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Debug)]
pub enum BodyLimitError {
    TooLarge,
    Hyper(hyper::Error),
}

impl std::fmt::Display for BodyLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "body too large"),
            Self::Hyper(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BodyLimitError {}

pin_project! {
    pub struct LimitedBody<B> {
        #[pin]
        inner: B,
        max: usize,
        seen: usize,
    }
}

impl<B> LimitedBody<B> {
    pub fn new(inner: B, max: usize) -> Self {
        Self {
            inner,
            max,
            seen: 0,
        }
    }
}

impl<B> Body for LimitedBody<B>
where
    B: Body<Data = Bytes>,
    B::Error: Into<hyper::Error>,
{
    type Data = Bytes;
    type Error = BodyLimitError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    *this.seen += data.len();
                    if *this.seen > *this.max {
                        return Poll::Ready(Some(Err(BodyLimitError::TooLarge)));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(BodyLimitError::Hyper(e.into())))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Collect body with streaming size enforcement.
pub async fn collect_limited<B>(body: B, max: usize) -> Result<Bytes, BodyLimitError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<hyper::Error>,
{
    let limited = LimitedBody::new(body, max);
    match limited.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) => Err(e),
    }
}
