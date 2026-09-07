//! Streaming body helpers: size limit + per-frame idle timeout.

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use pin_project_lite::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{sleep, Instant, Sleep};

#[derive(Debug)]
pub enum BodyLimitError {
    TooLarge,
    IdleTimeout,
    Other(String),
}

impl std::fmt::Display for BodyLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "body too large"),
            Self::IdleTimeout => write!(f, "body idle timeout"),
            Self::Other(e) => write!(f, "{e}"),
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
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(BodyLimitError::Other(e.to_string()))))
            }
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

pin_project! {
    /// Fails if no body frame arrives within `idle`.
    pub struct IdleBody<B> {
        #[pin]
        inner: B,
        #[pin]
        delay: Sleep,
        idle: Duration,
    }
}

impl<B> IdleBody<B> {
    pub fn new(inner: B, idle: Duration) -> Self {
        let idle = idle.max(Duration::from_millis(50));
        Self {
            inner,
            delay: sleep(idle),
            idle,
        }
    }

    fn reset_timer(self: Pin<&mut Self>) {
        let idle = self.idle;
        let mut this = self.project();
        this.delay.as_mut().reset(Instant::now() + idle);
    }
}

impl<B> Body for IdleBody<B>
where
    B: Body<Data = Bytes, Error = BodyLimitError>,
{
    type Data = Bytes;
    type Error = BodyLimitError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let poll = {
            let mut this = self.as_mut().project();
            this.inner.as_mut().poll_frame(cx)
        };

        match poll {
            Poll::Ready(Some(Ok(frame))) => {
                self.as_mut().reset_timer();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                let mut this = self.as_mut().project();
                match this.delay.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Some(Err(BodyLimitError::IdleTimeout))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Adapt any Body into BodyLimitError.
pin_project! {
    pub struct MapErrBody<B> {
        #[pin]
        inner: B,
    }
}

impl<B> MapErrBody<B> {
    pub fn new(inner: B) -> Self {
        Self { inner }
    }
}

impl<B> Body for MapErrBody<B>
where
    B: Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = BodyLimitError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.project().inner.poll_frame(cx) {
            Poll::Ready(Some(Ok(f))) => Poll::Ready(Some(Ok(f))),
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(BodyLimitError::Other(e.to_string()))))
            }
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

/// Size limit + idle timeout between frames, then collect.
pub async fn collect_limited<B>(
    body: B,
    max: usize,
    idle: Duration,
) -> Result<Bytes, BodyLimitError>
where
    B: Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    let mapped = MapErrBody::new(body);
    let limited = LimitedBody::new(mapped, max);
    let idle_wrapped = IdleBody::new(limited, idle);
    match idle_wrapped.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) => Err(e),
    }
}

/// Wrap an upstream response body with idle timeout (streaming path).
pub fn idle_wrap<B>(body: B, idle: Duration) -> IdleBody<MapErrBody<B>>
where
    B: Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    IdleBody::new(MapErrBody::new(body), idle)
}
