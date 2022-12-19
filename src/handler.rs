use std::{convert::Infallible, fmt::Display, future::Future, pin::Pin};

use async_trait::async_trait;
use http_body_util::Full;
use hyper::{
    body::{Bytes, Incoming as IncomingBody},
    Request, Response,
};

/// A handler that can be used by a [Server](crate::Server). If the handler
/// returns an error, the error will be logged and a 500 response will be
/// returned to the client.
#[async_trait]
pub trait Handler {
    type Error: Display;
    async fn handle(self, req: Request<IncomingBody>)
        -> Result<Response<Full<Bytes>>, Self::Error>;
}

impl<F, Fut, E> Handler for F
where
    F: FnOnce(Request<IncomingBody>) -> Fut,
    Fut: Future<Output = Result<Response<Full<Bytes>>, E>> + Send + 'static,
    E: Display,
{
    type Error = E;

    fn handle<'async_trait>(
        self,
        req: Request<IncomingBody>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Response<Full<Bytes>>, Self::Error>> + Send + 'async_trait>,
    > {
        Box::pin(self(req))
    }
}

/// Converts a value into a [Result](Result)<T, [Infallible](Infallible)> so it
/// can be used as the return type for a Handler.
///
/// Useful for closures where you can't specify the return type and you don't
/// need to return an error.
pub fn handle_ok<T>(val: T) -> Result<T, Infallible> {
    Ok(val)
}

pub(crate) async fn run_handler<H: Handler>(
    handler: H,
    req: Request<IncomingBody>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    match handler.handle(req).await {
        Ok(resp) => Ok(resp),
        Err(err) => {
            eprintln!("Error while handling request: {}", err);
            Ok(Response::builder()
                .status(500)
                .body(Full::from(Bytes::from_static(b"Internal Server Error")))
                .expect("should be a valid response"))
        }
    }
}
