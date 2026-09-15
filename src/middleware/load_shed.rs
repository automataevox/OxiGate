use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, Empty};
use hyper::body::Bytes;
use tower::Service;

pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;
pub type DynResponseBody = BoxBody<Bytes, BoxError>;

#[derive(Clone)]
pub struct LoadShedMiddleware<S> {
    inner: S,
    max_concurrency: usize,
    in_flight: Arc<AtomicUsize>,
}

impl<S> LoadShedMiddleware<S> {
    pub fn new(inner: S, max_concurrency: usize, in_flight: Arc<AtomicUsize>) -> Self {
        Self {
            inner,
            max_concurrency,
            in_flight,
        }
    }
}

impl<S, ReqBody> Service<Request<ReqBody>> for LoadShedMiddleware<S>
where
    S: Service<
        Request<ReqBody>,
        Response = Response<DynResponseBody>,
        Error = BoxError,
    > + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<DynResponseBody>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst);

        if current >= self.max_concurrency {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            // Construct 503 response with explicit BoxError body signature
            let res = Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(
                    Empty::<Bytes>::new()
                        .map_err(|never: std::convert::Infallible| -> BoxError { match never {} })
                        .boxed(),
                )
                .unwrap();

            return Box::pin(async move { Ok(res) });
        }

        let in_flight = self.in_flight.clone();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let res = fut.await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            res
        })
    }
}