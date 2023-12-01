use futures::future::FutureExt as _;

fn measure<T>(
    fut: impl std::future::Future<Output = T>,
) -> impl std::future::Future<Output = (T, tokio::time::Duration)> {
    let now = tokio::time::Instant::now();
    fut.map(move |result| (result, now.elapsed()))
}

enum PoolTx<B> {
    Http1(hyper::client::conn::http1::SendRequest<B>),
    Http2(hyper::client::conn::http2::SendRequest<B>),
}

impl<B> PoolTx<B> {
    fn ready(&mut self) -> impl std::future::Future<Output = hyper::Result<()>> + '_ {
        match self {
            PoolTx::Http1(tx) => tx.ready().left_future(),
            PoolTx::Http2(tx) => tx.ready().right_future(),
        }
    }
}

impl<B: hyper::body::Body + 'static> PoolTx<B> {
    fn send_request(
        &mut self,
        req: hyper::Request<B>,
    ) -> impl std::future::Future<Output = hyper::Result<hyper::Response<hyper::body::Incoming>>>
    {
        match self {
            PoolTx::Http1(tx) => tx.send_request(req).left_future(),
            PoolTx::Http2(tx) => tx.send_request(req).right_future(),
        }
    }
}

async fn run(connection_index: usize, h2: bool, request: &hyper::Request<String>) {
    let (stream, elapsed) = measure(tokio::net::TcpStream::connect("api.openai.com:443")).await;
    println!("{connection_index}: TcpStream::connect: {elapsed:?}");
    let stream = stream.unwrap();
    let () = socket2::SockRef::from(&stream)
        .set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(1))
                .with_interval(std::time::Duration::from_secs(1))
                .with_retries(3),
        )
        .unwrap();
    let connector = tokio_native_tls::native_tls::TlsConnector::builder()
        .request_alpns(if h2 { &["h2"] } else { &["http/1.1"] })
        .build()
        .unwrap();
    let connector = tokio_native_tls::TlsConnector::from(connector);
    let (stream, elapsed) = measure(connector.connect("api.openai.com", stream)).await;
    println!("{connection_index}: TlsConnector::connect: {elapsed:?}");
    let stream = stream.unwrap();
    let alpn = stream.get_ref().negotiated_alpn().unwrap().unwrap();
    let alpn = String::from_utf8(alpn).unwrap();
    let stream = hyper_util::rt::tokio::TokioIo::new(stream);
    let fut = {
        use futures::future::TryFutureExt as _;

        match alpn.as_str() {
            "h2" => hyper::client::conn::http2::Builder::new(
                hyper_util::rt::tokio::TokioExecutor::new(),
            )
            .handshake(stream)
            .map_ok(|(request_sender, connection)| {
                (PoolTx::Http2(request_sender), connection.left_future())
            })
            .left_future(),
            "http/1.1" => hyper::client::conn::http1::Builder::new()
                .handshake(stream)
                .map_ok(|(request_sender, connection)| {
                    (PoolTx::Http1(request_sender), connection.right_future())
                })
                .right_future(),
            alpn => {
                panic!("{connection_index}: unknown ALPN: {alpn}");
            }
        }
    };
    let (connection, elapsed) = measure(fut).await;
    println!("{connection_index}: http::handshake({alpn}): {elapsed:?}");
    let (mut request_sender, connection) = connection.unwrap();
    tokio::spawn(connection.map(move |result| match result {
        Ok(()) => {
            println!("{connection_index}: connection closed");
        }
        Err(err) => {
            println!("{connection_index}: {err:?}");
        }
    }));
    let mut seen_headers =
        hyper::header::HeaderMap::<Vec<hyper::header::HeaderValue>>::with_capacity(0);
    for request_index in 0.. {
        let (ready, elapsed) = measure(request_sender.ready()).await;
        println!("{connection_index}:{request_index}: SendRequest::ready: {elapsed:?}");
        let () = ready.unwrap();
        let (response, elapsed) = match tokio::time::timeout(
            std::time::Duration::from_secs(60), // This timeout catches the bug. Decrease if impatient.
            measure(request_sender.send_request(request.to_owned())),
        )
        .await
        {
            Ok((response, elapsed)) => (response, elapsed),
            Err(tokio::time::error::Elapsed { .. }) => {
                for values in seen_headers.values_mut() {
                    match values.as_slice() {
                        [] => {}
                        [first, rest @ ..] => {
                            if rest.iter().all(|value| value == first) {
                                values.truncate(1);
                            }
                        }
                    }
                }
                panic!("{connection_index}:{request_index}: SendRequest::send_request: timeout {seen_headers:?}");
            }
        };
        println!("{connection_index}:{request_index}: SendRequest::send_request: {elapsed:?}");
        let response = response.unwrap();
        let (
            hyper::http::response::Parts {
                status,
                version: _,
                headers,
                extensions: _,
                ..
            },
            mut body,
        ) = response.into_parts();

        match status {
            hyper::StatusCode::OK => {}
            hyper::StatusCode::TOO_MANY_REQUESTS => {
                for value in headers.get_all("x-ratelimit-reset-requests") {
                    let bytes = value.as_bytes();

                    let mut total = std::time::Duration::ZERO;
                    let mut iter = bytes.iter().enumerate().rev();
                    if let Some((mut multiplier_index, byte)) = iter.next() {
                        let mut multiplier = match char::from(*byte) {
                            's' => std::time::Duration::from_secs(1),
                            'm' => std::time::Duration::from_secs(60),
                            'h' => std::time::Duration::from_secs(3600),
                            c => {
                                panic!("{connection_index}:{request_index}: unknown unit: {c}");
                            }
                        };
                        while let Some((index, byte)) = iter.next() {
                            match char::from(*byte) {
                                '0'..='9' | '.' => {}
                                c => {
                                    let multiplier = std::mem::replace(
                                        &mut multiplier,
                                        match c {
                                            's' => std::time::Duration::from_secs(1),
                                            'm' => std::time::Duration::from_secs(60),
                                            'h' => std::time::Duration::from_secs(3600),
                                            c => {
                                                panic!(
                                                "{connection_index}:{request_index}: unknown unit: {c}"
                                            );
                                            }
                                        },
                                    );
                                    let start = index + 1;
                                    let end = std::mem::replace(&mut multiplier_index, index);
                                    let number = &bytes[start..end];
                                    let number = std::str::from_utf8(number).unwrap();
                                    let number: f32 = number.parse().unwrap();
                                    let number = multiplier.mul_f32(number);
                                    total = total.checked_add(number).unwrap();
                                }
                            }
                        }
                        let number = &bytes[..multiplier_index];
                        let number = std::str::from_utf8(number).unwrap();
                        let number: f32 = number.parse().unwrap();
                        let number = multiplier.mul_f32(number);
                        total = total.checked_add(number).unwrap();
                    }

                    println!(
                        "{connection_index}:{request_index}: {} {total:?}",
                        value.to_str().unwrap()
                    );
                    tokio::time::sleep(total).await;
                }
            }
            status => println!("{connection_index}:{request_index}: {status:?} {headers:?}"),
        }
        loop {
            use http_body_util::BodyExt as _;

            let (frame, elapsed) = measure(body.frame()).await;
            println!("{connection_index}:{request_index}: Incoming:frame: {elapsed:?}");
            let Some(frame) = frame else { break };
            let frame = frame.unwrap();
            if status != hyper::StatusCode::OK {
                if let Some(chunk) = frame.data_ref() {
                    println!("{connection_index}:{request_index}: {}", chunk.len());
                }
            }
        }
        for (key, value) in headers {
            if let Some(key) = key {
                {
                    let key = key.as_str();
                    if !(key.starts_with("x-") || key.contains("openai")) {
                        continue;
                    }
                    if key.starts_with("x-ratelimit-") {
                        continue;
                    }
                }
                seen_headers.entry(key).or_insert_with(Vec::new).push(value);
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    use futures::stream::StreamExt as _;

    let api_key = std::env::var("OPENAI_API_KEY").unwrap();
    let request = hyper::Request::post("https://api.openai.com/v1/chat/completions")
        .header(hyper::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::HOST, "api.openai.com")
        .header(hyper::header::CONNECTION, "keep-alive")
        .body(
            r#"{
    "model": "gpt-3.5-turbo",
    "messages": [
        {
            "role": "user",
            "content": "Say this is a test"
        }
    ]
}"#
            .to_string(),
        )
        .unwrap();

    futures::stream::iter(
        std::iter::repeat([false, true])
            .take(25) // Number of connections per protocol (HTTP/1.1 and HTTP/2). Increase if impatient.
            .flatten()
            .enumerate(),
    )
    .for_each_concurrent(None, |(connection_index, h2)| {
        run(connection_index, h2, &request)
    })
    .await;
}
