//! Provides a simple Hyper server wrapper that can be used for writing test
//! servers. Vaguely inspired by Go's
//! [httptest](https://pkg.go.dev/net/http/httptest#Server) package.
//!
//! Currently only supports HTTP/1.1 and does not support TLS. Only supports the
//! Tokio async runtime.
//!
//! ## Example
//!
//! ```
//! # // Please keep this example up-to-date with README.md, but remove all
//! # // lines starting with `#` and their contents.
//! use std::sync::{Arc, Mutex};
//!
//! use mini_http_test::{
//!     handle_ok,
//!     hyper::{body, Request, Response},
//!     Server,
//! };
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let val = Arc::new(Mutex::new(1234));
//! let server = {
//!     let val = val.clone();
//!     Server::new(move |_: Request<body::Incoming>| async move {
//!         let mut val = val.lock().expect("lock poisoned");
//!         *val += 1;
//!         handle_ok(Response::new(val.to_string().into()))
//!     })
//!     .await
//!     .expect("create server")
//! };
//!
//! let res = reqwest::Client::new()
//!     .get(server.url("/").to_string())
//!     .send()
//!     .await
//!     .expect("send request");
//!
//! assert_eq!(res.status(), 200);
//! assert_eq!(*val.lock().expect("lock poisoned"), 1235);
//! assert_eq!(res.text().await.expect("read response"), "1235");
//!
//! assert_eq!(server.req_count(), 1);
//! # });
//! ```
//!
//! There are also more examples as tests.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context};
use async_trait::async_trait;
use http_body_util::BodyExt;
use hyper::{
    body::{self, Body, Bytes},
    server::conn::http1,
    service::service_fn,
    Request, Uri,
};
use tokio::{net::TcpListener, select, sync::watch, time::Instant};

mod handler;
pub use handler::*;

pub use hyper;

/// Listens on a random port, running the given function to handle each request.
///
/// See the crate documentation for an example.
#[derive(Debug, Clone)]
pub struct Server {
    close_tx: Arc<watch::Sender<u8>>,
    addr: SocketAddr,
    req_count: Arc<Mutex<u64>>,
    concurrent_req_count: Arc<Mutex<u64>>,
}

impl Server {
    /// Creates a new HTTP server on a random port running the given handler.
    /// The handler will be called on every request, and the total request count
    /// can be retrieved with [server.req_count()](Server::req_count).
    ///
    /// The server can be safely cloned and used from multiple threads. When the
    /// final reference to the server is dropped, the server will be shut down
    /// and all pending requests will be aborted. Aborting the server will
    /// happen in the background and will not block.
    pub async fn new<H: Handler + Clone + Send + Sync + 'static>(
        handler: H,
    ) -> Result<Self, anyhow::Error> {
        let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
        let tcp_listener = TcpListener::bind(addr).await.context("bind TCP listener")?;
        let addr = tcp_listener
            .local_addr()
            .context("get listener socket address")?;

        let (close_tx, close_rx) = watch::channel::<u8>(0);
        let req_count = Arc::new(Mutex::new(0));
        let concurrent_req_count = Arc::new(Mutex::new(0));

        {
            let handler = handler.clone();
            let req_count = req_count.clone();
            let concurrent_req_count = concurrent_req_count.clone();

            tokio::spawn(async move {
                let mut close_rx = close_rx.clone();

                loop {
                    let (tcp_stream, _) = select! {
                        _ = close_rx.changed() => {
                            return;
                        }
                        res = tcp_listener.accept() => {
                            match res {
                                Ok(res) => res,
                                Err(err) => {
                            eprintln!("Error while accepting TCP connection: {}", err);
                                    return;
                                }
                            }
                        }
                    };

                    let handler = handler.clone();
                    let mut close_rx = close_rx.clone();
                    let req_count = req_count.clone();
                    let concurrent_req_count = concurrent_req_count.clone();
                    tokio::spawn(async move {
                        let handler = &handler;
                        let req_count = &req_count;
                        let concurrent_req_count = &concurrent_req_count;

                        let service = service_fn(|req: Request<body::Incoming>| async move {
                            *concurrent_req_count.lock().expect("lock poisoned") += 1;
                            let res = run_handler(handler.clone(), req).await;
                            *concurrent_req_count.lock().expect("lock poisoned") -= 1;
                            *req_count.lock().expect("lock poisoned") += 1;
                            res
                        });

                        let res = select! {
                            _ = close_rx.changed() => {
                                return;
                            }
                            res = http1::Builder::new()
                                .http1_keep_alive(true)
                                .serve_connection(tcp_stream, service) => res,
                        };

                        if let Err(http_err) = res {
                            eprintln!("Error while serving HTTP connection: {}", http_err);
                        }
                    });
                }
            });
        };

        Ok(Self {
            close_tx: Arc::new(close_tx),
            addr,
            req_count,
            concurrent_req_count,
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

    /// Returns the number of requests handled by the server. This value is
    /// incremented after the request handler has finished, but before the
    /// response has been sent.
    ///
    /// At the end of tests, this should be asserted to be equal to the amount
    /// of requests sent.
    pub fn req_count(&self) -> u64 {
        *self.req_count.lock().expect("lock poisoned")
    }

    /// Await req_count reaching a certain number. This polls every 10ms and
    /// times out after the given duration.
    pub async fn await_req_count(
        &self,
        count: u64,
        timeout: Duration,
    ) -> Result<(), anyhow::Error> {
        let start = Instant::now();
        loop {
            let current_count = self.req_count();
            if current_count == count {
                return Ok(());
            }

            if start.elapsed() > timeout {
                bail!(
                    "req_count did not reach {} (currently {}) within {:?}",
                    count,
                    current_count,
                    timeout
                );
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Returns the number of concurrent requests currently being handled by the
    /// server.
    pub fn concurrent_req_count(&self) -> u64 {
        *self.concurrent_req_count.lock().expect("lock poisoned")
    }

    /// Await concurrent_req_count reaching a certain number. This polls every
    /// 10ms and times out after the given duration.
    pub async fn await_concurrent_req_count(
        &self,
        count: u64,
        timeout: Duration,
    ) -> Result<(), anyhow::Error> {
        let start = Instant::now();
        loop {
            let current_count = self.concurrent_req_count();
            if current_count == count {
                return Ok(());
            }

            if start.elapsed() > timeout {
                bail!(
                    "concurrent_req_count did not reach {} (currently {}) within {:?}",
                    count,
                    current_count,
                    timeout
                );
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// close kills the server and aborts all pending requests. This does not
    /// block for all requests to finish.
    pub fn close(&self) {
        self.close_tx.send(1).expect("failed to close server");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if Arc::strong_count(&self.close_tx) == 1 {
            self.close();
        }
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    async fn server_await_req_count() {
        async fn handler(
            _: Request<body::Incoming>,
        ) -> Result<Response<Full<Bytes>>, anyhow::Error> {
            Ok(Response::new("hello world".into()))
        }

        let server = Server::new(handler).await.expect("create server");

        let client = reqwest::Client::new();

        // Spawn tasks that will send requests to the server.
        static ITERATIONS: u64 = 10;
        let url = server.url("/").to_string();
        let futures: Vec<tokio::task::JoinHandle<()>> = (0..ITERATIONS)
            .map(|_| {
                let client = client.clone();
                let url = url.clone();

                tokio::spawn(async move {
                    let res = client.get(url).send().await.expect("send request");
                    assert_eq!(res.status(), 200);
                })
            })
            .collect();

        server
            .await_req_count(ITERATIONS, Duration::from_secs(1))
            .await
            .expect("requests finished");
        assert_eq!(server.req_count(), ITERATIONS);

        // Ensure all requests have finished.
        for fut in futures {
            fut.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 16)]
    async fn server_long_requests_cancellation() {
        async fn handler(
            _: Request<body::Incoming>,
        ) -> Result<Response<Full<Bytes>>, anyhow::Error> {
            // Sleep for 10 seconds to simulate a long request.
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(Response::new("hello world".into()))
        }

        let server = Server::new(handler).await.expect("create server");

        let client = reqwest::Client::new();

        // Spawn tasks that will send requests to the server.
        static ITERATIONS: u64 = 10;
        let url = server.url("/").to_string();
        let futures: Vec<tokio::task::JoinHandle<Result<(), anyhow::Error>>> = (0..ITERATIONS)
            .map(|_| {
                let client = client.clone();
                let url = url.clone();

                tokio::spawn(async move {
                    let res = client.get(url).send().await;
                    match res {
                        Ok(_) => Err(anyhow::anyhow!("expected request to be canceled")),
                        Err(_) => Ok(()),
                    }
                })
            })
            .collect();

        server
            .await_concurrent_req_count(ITERATIONS, Duration::from_secs(1))
            .await
            .expect("requests start");
        assert_eq!(server.concurrent_req_count(), ITERATIONS);

        // Drop the server and the requests should be canceled.
        drop(server);

        // Wait for the requests to be canceled.
        for fut in futures {
            fut.await.unwrap().expect("request canceled");
        }
    }
}
