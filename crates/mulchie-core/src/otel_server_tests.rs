use super::*;
use crate::otel::AggLimits;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn test_limits() -> Limits {
    Limits {
        line: 1024,
        headers: 4096,
        body: 64 * 1024,
        deadline: Duration::from_millis(300),
        max_connections: 2,
        decode: DecodeLimits::default(),
    }
}

fn loopback0() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn start(limits: Limits) -> (Server, Arc<Mutex<Aggregator>>) {
    let agg = Arc::new(Mutex::new(Aggregator::new(AggLimits::default())));
    let srv = serve(loopback0(), agg.clone(), limits).unwrap();
    (srv, agg)
}

/// Send raw bytes, read the whole answer: (status, headers, body).
fn raw(srv: &Server, req: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(srv.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(req);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    parse_resp(&out)
}

fn parse_resp(out: &[u8]) -> (u16, String, Vec<u8>) {
    let mut rest = out;
    // Skip any interim 100 Continue.
    while rest.starts_with(b"HTTP/1.1 100") {
        let i = rest.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        rest = &rest[i + 4..];
    }
    let i = rest
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(out)));
    let head = String::from_utf8_lossy(&rest[..i]).to_string();
    let status = head[9..12].parse().unwrap();
    (status, head, rest[i + 4..].to_vec())
}

fn post(srv: &Server, ctype: &str, extra: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut req = format!(
        "POST /v1/traces HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\n\r\n",
        srv.port(),
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    raw(srv, &req)
}

const JSON_BODY: &str = r#"{"resourceSpans":[{"resource":{"attributes":[
  {"key":"service.name","value":{"stringValue":"shop"}},
  {"key":"process.pid","value":{"intValue":"77"}}]},
  "scopeSpans":[{"spans":[{"name":"GET /cart","kind":2,
  "startTimeUnixNano":"START","endTimeUnixNano":"END"}]}]}]}"#;

fn json_body() -> String {
    let end = otel::now_ms() * 1_000_000;
    JSON_BODY
        .replace("START", &(end - 40_000_000).to_string())
        .replace("END", &end.to_string())
}

#[test]
fn refuses_non_loopback_addresses() {
    let sink: Arc<dyn Sink> = Arc::new(FnSink(|_b: TraceBatch| {}));
    for ip in [
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        "::ffff:127.0.0.1".parse().unwrap(),
        "fe80::1".parse().unwrap(),
    ] {
        let e = serve(SocketAddr::new(ip, 0), sink.clone(), test_limits())
            .err()
            .unwrap_or_else(|| panic!("{ip} must be refused"));
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{ip}");
    }
    assert_eq!(default_addr().port(), 4318);
    assert!(default_addr().ip().is_loopback());
}

#[test]
fn json_end_to_end() {
    let (srv, agg) = start(test_limits());
    let (status, head, body) = post(
        &srv,
        "application/json; charset=utf-8",
        "",
        json_body().as_bytes(),
    );
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("Content-Type: application/json"));
    assert_eq!(body, b"{}");
    let a = agg.lock().unwrap();
    let ops = a.operations(0, u64::MAX);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].service, "shop");
    assert_eq!(ops[0].operation, "GET /cart");
    assert_eq!(ops[0].resource.pid, Some(77));
    assert!((ops[0].stats.max_ms - 40.0).abs() < 1e-9);
    drop(a);
    srv.stop();
}

#[test]
fn protobuf_end_to_end() {
    use crate::otel::tests::{pb_kv, pb_request, pb_span, PbSpan, V};
    let (srv, agg) = start(test_limits());
    let end = otel::now_ms() * 1_000_000;
    let body = pb_request(
        &[pb_kv("service.name", V::S("worker"))],
        &[pb_span(&PbSpan {
            name: "process job",
            kind: 5,
            parent: &[7; 8],
            start: end - 5_000_000,
            end,
            status: 2,
            attrs: vec![],
        })],
    );
    let (status, head, resp) = post(
        &srv,
        "Application/X-Protobuf",
        "Content-Encoding: identity\r\n",
        &body,
    );
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("Content-Type: application/x-protobuf"));
    assert!(resp.is_empty());
    let ops = agg.lock().unwrap().operations(0, u64::MAX);
    assert_eq!(ops[0].service, "worker");
    assert_eq!(ops[0].stats.errors, 1);
}

#[test]
fn expect_continue_is_answered() {
    let (srv, agg) = start(test_limits());
    let (status, _, _) = post(
        &srv,
        "application/json",
        "Expect: 100-continue\r\n",
        json_body().as_bytes(),
    );
    assert_eq!(status, 200);
    assert_eq!(agg.lock().unwrap().status().recorded_spans, 1);
}

#[test]
fn malformed_bodies_get_400_in_the_request_encoding() {
    let (srv, agg) = start(test_limits());
    let (status, _, body) = post(&srv, "application/json", "", b"{not json");
    assert_eq!(status, 400);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["code"], 3);
    assert!(v["message"].as_str().unwrap().contains("malformed"));

    let (status, head, body) = post(
        &srv,
        "application/x-protobuf",
        "",
        &[0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f],
    );
    assert_eq!(status, 400);
    assert!(head.contains("application/x-protobuf"));
    // google.rpc.Status: field 1 code = 3, field 2 message.
    assert_eq!(&body[..2], &[0x08, 0x03]);
    assert_eq!(body[2], 0x12);
    assert_eq!(agg.lock().unwrap().status().received_spans, 0);
}

#[test]
fn routes_methods_types_and_encodings() {
    let (srv, _agg) = start(test_limits());
    let port = srv.port();
    let req = |s: &str| raw(&srv, s.replace("PORT", &port.to_string()).as_bytes()).0;
    assert_eq!(req("POST /v1/metrics HTTP/1.1\r\nHost: localhost:PORT\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"), 404);
    let (st, head, _) = raw(
        &srv,
        format!("GET /v1/traces HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes(),
    );
    assert_eq!(st, 405);
    assert!(head.contains("Allow: POST"));
    assert_eq!(
        req("OPTIONS /v1/traces HTTP/1.1\r\nHost: localhost:PORT\r\n\r\n"),
        405
    );
    assert_eq!(req("POST /v1/traces HTTP/1.1\r\nHost: localhost:PORT\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}"), 415);
    assert_eq!(
        req("POST /v1/traces HTTP/1.1\r\nHost: localhost:PORT\r\nContent-Length: 2\r\n\r\n{}"),
        415
    );
    let (st, _, body) = post(
        &srv,
        "application/json",
        "Content-Encoding: gzip\r\n",
        b"\x1f\x8b",
    );
    assert_eq!(st, 415);
    assert!(String::from_utf8_lossy(&body).contains("OTEL_EXPORTER_OTLP_COMPRESSION=none"));
    assert_eq!(req("POST /v1/traces HTTP/1.1\r\nHost: localhost:PORT\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"), 411);
    // Query strings on the path are fine.
    assert_eq!(req("POST /v1/traces?x=1 HTTP/1.1\r\nHost: [::1]:PORT\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"), 200);
}

#[test]
fn host_must_be_loopback() {
    let (srv, agg) = start(test_limits());
    for host in [
        "evil.example:4318",
        "127.0.0.2",
        "localhost.evil.example",
        "[::2]:1",
    ] {
        let req = format!(
            "POST /v1/traces HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
        );
        assert_eq!(raw(&srv, req.as_bytes()).0, 403, "{host}");
    }
    let no_host =
        "POST /v1/traces HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(raw(&srv, no_host.as_bytes()).0, 403);
    for host in ["localhost", "LOCALHOST:1", "127.0.0.1", "[::1]"] {
        assert!(host_ok(host), "{host}");
    }
    assert_eq!(agg.lock().unwrap().status().received_spans, 0);
}

#[test]
fn size_and_time_limits() {
    let (srv, _agg) = start(test_limits());
    let port = srv.port();
    // Body over the limit: refused before it is read.
    let big = format!(
        "POST /v1/traces HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        test_limits().body + 1
    );
    assert_eq!(raw(&srv, big.as_bytes()).0, 413);
    // A request line over the limit.
    let long = format!("POST /{} HTTP/1.1\r\n\r\n", "a".repeat(2000));
    assert_eq!(raw(&srv, long.as_bytes()).0, 414);
    // Headers over the limit.
    let mut many = "POST /v1/traces HTTP/1.1\r\n".to_string();
    for i in 0..100 {
        many.push_str(&format!("X-Pad-{i}: {}\r\n", "b".repeat(60)));
    }
    many.push_str("\r\n");
    assert_eq!(raw(&srv, many.as_bytes()).0, 431);
    // Malformed lines.
    assert_eq!(raw(&srv, b"GARBAGE\r\n\r\n").0, 400);
    assert_eq!(raw(&srv, b"POST /v1/traces HTTP/2\r\n\r\n").0, 400);
    assert_eq!(
        raw(&srv, b"POST /v1/traces HTTP/1.1\r\nNoColon\r\n\r\n").0,
        400
    );
    assert_eq!(
        raw(
            &srv,
            b"POST /v1/traces HTTP/1.1\r\nContent-Length: -1\r\n\r\n"
        )
        .0,
        400
    );
    // A body shorter than promised, then the deadline.
    let slow = format!(
        "POST /v1/traces HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    let mut s = TcpStream::connect(srv.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(slow.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    assert_eq!(parse_resp(&out).0, 408);
}

#[test]
fn too_many_connections_get_503() {
    let (srv, _agg) = start(Limits {
        deadline: Duration::from_secs(3),
        ..test_limits()
    });
    // Two idle connections hold both slots.
    let a = TcpStream::connect(srv.local_addr()).unwrap();
    let b = TcpStream::connect(srv.local_addr()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let (status, _, _) = raw(&srv, b"POST /v1/traces HTTP/1.1\r\n\r\n");
    assert_eq!(status, 503);
    drop((a, b));
}

#[test]
fn closed_without_a_request_is_not_answered() {
    let (srv, _agg) = start(test_limits());
    let s = TcpStream::connect(srv.local_addr()).unwrap();
    drop(s);
    // The server is still up.
    let (status, _, _) = post(&srv, "application/json", "", json_body().as_bytes());
    assert_eq!(status, 200);
}

#[test]
fn hostile_bodies_never_take_the_server_down() {
    let (srv, _agg) = start(test_limits());
    let mut st = 0x1234_5678_9abc_def0u64;
    for i in 0..60u64 {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        let len = (st % 512) as usize + 1;
        let body: Vec<u8> = (0..len)
            .map(|j| (st >> ((j % 8) * 8)) as u8 ^ (j as u8))
            .collect();
        let ctype = if i % 2 == 0 {
            "application/x-protobuf"
        } else {
            "application/json"
        };
        let (status, _, _) = post(&srv, ctype, "", &body);
        assert!(status == 200 || status == 400, "{status}");
    }
    let deep = format!("{}{}", "[".repeat(20_000), "]".repeat(20_000));
    assert_eq!(post(&srv, "application/json", "", deep.as_bytes()).0, 400);
    assert_eq!(
        post(&srv, "application/json", "", json_body().as_bytes()).0,
        200
    );
}

#[test]
fn stop_closes_the_port_and_drop_stops_too() {
    let (srv, _agg) = start(test_limits());
    let addr = srv.local_addr();
    srv.stop();
    // The accept thread is gone; a later connection is not served.
    if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        s.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let _ = s.write_all(b"POST /v1/traces HTTP/1.1\r\n\r\n");
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        assert!(out.is_empty());
    }

    let v6 = serve(
        SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
        Arc::new(FnSink(|_b: TraceBatch| {})),
        test_limits(),
    );
    // IPv6 may be unavailable on a CI host; when it binds, it must stop.
    if let Ok(v6) = v6 {
        assert!(v6.local_addr().is_ipv6());
        drop(v6);
    }
}

#[test]
fn closure_sink_receives_batches() {
    let seen = Arc::new(AtomicUsize::new(0));
    let s2 = seen.clone();
    let srv = serve(
        loopback0(),
        Arc::new(FnSink(move |b: TraceBatch| {
            s2.fetch_add(b.span_count(), Ordering::SeqCst);
        })),
        test_limits(),
    )
    .unwrap();
    assert_eq!(
        post(&srv, "application/json", "", json_body().as_bytes()).0,
        200
    );
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[test]
fn a_poisoned_aggregator_still_takes_batches() {
    let agg = Arc::new(Mutex::new(Aggregator::default()));
    let a2 = agg.clone();
    let _ = std::thread::spawn(move || {
        let _g = a2.lock().unwrap();
        panic!("poison");
    })
    .join();
    assert!(agg.is_poisoned());
    let b = otel::decode_json(json_body().as_bytes(), &DecodeLimits::default()).unwrap();
    agg.accept(b);
    let n = agg
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .status()
        .recorded_spans;
    assert_eq!(n, 1);
}
