# Rust repro of https://github.com/openai/openai-python/issues/769

```shell
OPENAI_API_KEY='my-fancy-key' cargo run
```

This program repeatedly pings the OpenAI API, which occasionally fails to respond. Each successful
request accumulates relevant headers, which are emitted when a timeout occurs (currently set to 10
minutes). Both http/1.1 and h2 are exercised and aggressive TCP keepalives are enabled. Within
approximately 300 requests the server ceases to respond, and a request times out, though usually
this happens sooner.
