# mini-http-test

Small library that provides a simple Hyper server wrapper for writing HTTP test
servers. Vaguely inspired by Go's
[httptest](https://pkg.go.dev/net/http/httptest#Server) package.

Currently only supports HTTP/1.1 and does not support TLS.

## Example

```rust
use std::sync::{Arc, Mutex};

use mini_http_test::{
    handle_ok,
    hyper::{body, Request, Response},
    Server,
};

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

let res = reqwest::Client::new()
    .get(server.url("/").to_string())
    .send()
    .await
    .expect("send request");

assert_eq!(res.status(), 200);
assert_eq!(*val.lock().expect("lock poisoned"), 1235);
assert_eq!(res.text().await.expect("read response"), "1235");

assert_eq!(server.req_count(), 1);
```

## License

Licensed under the MIT license. See [LICENSE](LICENSE) for more details.
