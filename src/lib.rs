//! Provides a simple Hyper server wrapper that can be used for writing test
//! servers. Vaguely inspired by Go's
//! [httptest](https://pkg.go.dev/net/http/httptest#Server) package.
//!
//! Currently only supports HTTP/1.1 and does not support TLS.
//!
//! See [Server](Server) for an example on how to use the server. There are also
//! more examples as tests.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use async_trait::async_trait;
use http_body_util::BodyExt;
use hyper::{
    body::{self, Body, Bytes},
    server::conn::http1,
    service::service_fn,
    Request, Uri,
};
use tokio::net::TcpListener;

mod handler;
pub use handler::*;

pub use hyper;

/// Listens on a random port, running the given function to handle each request.
///
/// ```
/// # // Please keep this example up-to-date with README.md, but remove all
/// # // lines starting with `#` and their contents.
/// use std::sync::{Arc, Mutex};
///
/// use mini_http_test::{
///     handle_ok,
///     hyper::{body, Request, Response},
///     Server,
/// };
///
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let val = Arc::new(Mutex::new(1234));
/// let server = {
///     let val = val.clone();
///     Server::new(move |_: Request<body::Incoming>| async move {
///         let mut val = val.lock().expect("lock poisoned");
///         *val += 1;
///         handle_ok(Response::new(val.to_string().into()))
///     })
///     .await
///     .expect("create server")
/// };
///
/// let res = reqwest::Client::new()
///     .get(server.url("/").to_string())
///     .send()
///     .await
///     .expect("send request");
///
/// assert_eq!(res.status(), 200);
/// assert_eq!(*val.lock().expect("lock poisoned"), 1235);
/// assert_eq!(res.text().await.expect("read response"), "1235");
///
/// assert_eq!(server.req_count(), 1);
/// # });
/// ```
#[derive(Debug)]
pub struct Server {
    join_handle: tokio::task::JoinHandle<()>,
    addr: SocketAddr,

    req_count: Arc<Mutex<u64>>,
}

impl Server {
    /// Creates a new HTTP server on a random port running the given handler.
    /// The handler will be called on every request, and the total request count
    /// can be retrieved with [server.req_count()](Server::req_count).
    pub async fn new<H: Handler + Clone + Send + Sync + 'static>(
        handler: H,
    ) -> Result<Self, anyhow::Error> {
        let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
        let tcp_listener = TcpListener::bind(addr).await.context("bind TCP listener")?;
        let addr = tcp_listener
            .local_addr()
            .context("get listener socket address")?;

        let req_count = Arc::new(Mutex::new(0));
        let join_handle = {
            let req_count = req_count.clone();
            let handler = handler.clone();

            tokio::spawn(async move {
                loop {
                    let (tcp_stream, _) = match tcp_listener.accept().await {
                        Ok((tcp_stream, addr)) => (tcp_stream, addr),
                        Err(err) => {
                            eprintln!("Error while accepting TCP connection: {}", err);
                            return;
                        }
                    };

                    let handler = handler.clone();
                    let req_count = req_count.clone();
                    tokio::task::spawn(async move {
                        let handler = &handler;
                        let req_count = &req_count;

                        let service = service_fn(|req: Request<body::Incoming>| async move {
                            *req_count.lock().expect("lock poisoned") += 1;
                            run_handler(handler.clone(), req).await
                        });

                        if let Err(http_err) = http1::Builder::new()
                            .http1_keep_alive(true)
                            .serve_connection(tcp_stream, service)
                            .await
                        {
                            eprintln!("Error while serving HTTP connection: {}", http_err);
                        }
                    });
                }
            })
        };

        Ok(Self {
            join_handle,
            addr,
            req_count,
        })
    }

    /// Returns the socket address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Returns a valid request URL for the given path and query string.
    pub fn url(&self, path_and_query: &str) -> Uri {
        Uri::builder()
            .scheme("http")
            .authority(self.addr.to_string().as_str())
            .path_and_query(path_and_query)
            .build()
            .expect("should be a valid URL")
    }

    /// Returns the number of requests received by the server.
    ///
    /// At the end of tests, this should be asserted to be equal to the amount
    /// of requests sent.
    pub fn req_count(&self) -> u64 {
        *self.req_count.lock().expect("lock poisoned")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.join_handle.abort();
    }
}

/// A handy extension to [hyper::Request](hyper::Request) that allows for easily
/// reading the request body as a single `Bytes` object.
#[async_trait]
pub trait GetRequestBody {
    async fn body_bytes(self) -> Result<Bytes, hyper::Error>;
}

#[async_trait]
impl<B> GetRequestBody for Request<B>
where
    B: Body<Data = Bytes> + Send + Sync + 'static,
    <B as Body>::Error: Into<hyper::Error>,
{
    async fn body_bytes(self) -> Result<Bytes, hyper::Error> {
        self.into_body()
            .collect()
            .await
            .map(|full| full.to_bytes())
            .map_err(|err| err.into())
    }
}

#[cfg(test)]
mod test {
    use anyhow::bail;
    use http_body_util::Full;
    use hyper::{body::Bytes, Response};

    use super::*;

    #[tokio::test]
    async fn server_ok() {
        async fn handler(
            req: Request<body::Incoming>,
        ) -> Result<Response<Full<Bytes>>, hyper::Error> {
            let body = req.body_bytes().await?;
            Ok(Response::new(Full::new(body)))
        }

        let server = Server::new(handler).await.expect("create server");

        let client = reqwest::Client::new();

        static ITERATIONS: u64 = 10;
        for i in 0..ITERATIONS {
            let res = client
                .post(server.url("/").to_string())
                .body(format!("hello world {}", i))
                .send()
                .await
                .expect("send request");

            assert_eq!(res.status(), 200);
            assert_eq!(
                res.text().await.expect("read response"),
                format!("hello world {}", i)
            );

            assert_eq!(server.req_count(), i + 1);
        }

        assert_eq!(server.req_count(), ITERATIONS);
    }

    #[tokio::test]
    async fn server_move_closure_copy() {
        let val = 1234;
        let server = Server::new(move |_: Request<body::Incoming>| async move {
            handle_ok(Response::new(val.to_string().into()))
        })
        .await
        .expect("create server");

        let client = reqwest::Client::new();

        static ITERATIONS: u64 = 10;
        for i in 0..ITERATIONS {
            let res = client
                .get(server.url("/").to_string())
                .send()
                .await
                .expect("send request");

            assert_eq!(res.status(), 200);
            assert_eq!(res.text().await.expect("read response"), val.to_string());

            assert_eq!(server.req_count(), i + 1);
        }

        assert_eq!(server.req_count(), ITERATIONS);
    }

    #[tokio::test]
    async fn server_move_closure_arc() {
        let val = Arc::new(Mutex::new(1234));
        let server = {
            let val = val.clone();
            Server::new(move |_: Request<body::Incoming>| async move {
                let mut val = val.lock().expect("lock poisoned");
                *val += 1;
                handle_ok(Response::new(val.to_string().into()))
            })
            .await
            .expect("create server")
        };

        let client = reqwest::Client::new();

        static ITERATIONS: u64 = 10;
        for i in 0..ITERATIONS {
            let res = client
                .get(server.url("/").to_string())
                .send()
                .await
                .expect("send request");

            assert_eq!(res.status(), 200);
            assert_eq!(
                res.text().await.expect("read response"),
                val.lock().expect("lock poisoned").to_string()
            );

            assert_eq!(server.req_count(), i + 1);
        }

        assert_eq!(server.req_count(), ITERATIONS);
    }

    #[tokio::test]
    async fn server_failure() {
        async fn handler(
            _: Request<body::Incoming>,
        ) -> Result<Response<Full<Bytes>>, anyhow::Error> {
            bail!("expected test error")
        }

        let server = Server::new(handler).await.expect("create server");

        let client = reqwest::Client::new();

        static ITERATIONS: u64 = 10;
        for i in 0..ITERATIONS {
            let res = client
                .get(server.url("/").to_string())
                .send()
                .await
                .expect("send request");

            assert_eq!(res.status(), 500);
            assert_eq!(
                res.text().await.expect("read response"),
                "Internal Server Error"
            );

            assert_eq!(server.req_count(), i + 1);
        }

        assert_eq!(server.req_count(), ITERATIONS);
    }
}
