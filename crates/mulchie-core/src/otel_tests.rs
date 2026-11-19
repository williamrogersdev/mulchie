use super::*;

// ------------------------------------------------- protobuf fixture builder

pub(crate) fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub(crate) fn tag(out: &mut Vec<u8>, field: u64, wt: u8) {
    varint(out, (field << 3) | u64::from(wt));
}

pub(crate) fn len_field(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    tag(out, field, 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

pub(crate) fn fixed64_field(out: &mut Vec<u8>, field: u64, v: u64) {
    tag(out, field, 1);
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn varint_field(out: &mut Vec<u8>, field: u64, v: u64) {
    tag(out, field, 0);
    varint(out, v);
}

pub(crate) enum V<'a> {
    S(&'a str),
    I(i64),
    B(bool),
    D(f64),
}

pub(crate) fn pb_kv(key: &str, v: V<'_>) -> Vec<u8> {
    let mut any = Vec::new();
    match v {
        V::S(s) => len_field(&mut any, 1, s.as_bytes()),
        V::B(b) => varint_field(&mut any, 2, u64::from(b)),
        V::I(i) => varint_field(&mut any, 3, i as u64),
        V::D(d) => fixed64_field(&mut any, 4, d.to_bits()),
    }
    let mut kv = Vec::new();
    len_field(&mut kv, 1, key.as_bytes());
    len_field(&mut kv, 2, &any);
    kv
}

pub(crate) struct PbSpan<'a> {
    pub name: &'a str,
    pub kind: u64,
    pub parent: &'a [u8],
    pub start: u64,
    pub end: u64,
    pub status: u64,
    pub attrs: Vec<Vec<u8>>,
}

pub(crate) fn pb_span(s: &PbSpan<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    len_field(&mut b, 1, &[1u8; 16]); // trace_id
    len_field(&mut b, 2, &[2u8; 8]); // span_id
    if !s.parent.is_empty() {
        len_field(&mut b, 4, s.parent);
    }
    len_field(&mut b, 5, s.name.as_bytes());
    varint_field(&mut b, 6, s.kind);
    fixed64_field(&mut b, 7, s.start);
    fixed64_field(&mut b, 8, s.end);
    for a in &s.attrs {
        len_field(&mut b, 9, a);
    }
    // flags: fixed32, must be skipped.
    tag(&mut b, 16, 5);
    b.extend_from_slice(&1u32.to_le_bytes());
    let mut st = Vec::new();
    len_field(&mut st, 2, b"msg");
    varint_field(&mut st, 3, s.status);
    len_field(&mut b, 15, &st);
    b
}

pub(crate) fn pb_request(resource: &[Vec<u8>], spans: &[Vec<u8>]) -> Vec<u8> {
    let mut res = Vec::new();
    for a in resource {
        len_field(&mut res, 1, a);
    }
    let mut scope = Vec::new();
    let mut inst = Vec::new();
    len_field(&mut inst, 1, b"my-lib");
    len_field(&mut scope, 1, &inst);
    for s in spans {
        len_field(&mut scope, 2, s);
    }
    len_field(&mut scope, 3, b"https://schema");
    let mut rs = Vec::new();
    len_field(&mut rs, 1, &res);
    len_field(&mut rs, 2, &scope);
    let mut req = Vec::new();
    len_field(&mut req, 1, &rs);
    req
}

const NOW_MS: u64 = 1_800_000_000_000;

fn ns(ms: u64) -> u64 {
    ms * 1_000_000
}

fn server_span(name: &str, end_ms: u64, dur_ms: u64) -> Span {
    Span {
        name: name.into(),
        kind: KIND_SERVER,
        is_root: false,
        start_unix_nano: ns(end_ms - dur_ms),
        end_unix_nano: ns(end_ms),
        status_code: 0,
        attributes: Vec::new(),
    }
}

fn batch(service: &str, spans: Vec<Span>) -> TraceBatch {
    TraceBatch {
        resource_spans: vec![ResourceSpans {
            resource: vec![Attr {
                key: "service.name".into(),
                value: AttrValue::Str(service.into()),
            }],
            spans,
        }],
        dropped: DecodeDrops::default(),
    }
}

fn s(v: &str) -> AttrValue {
    AttrValue::Str(v.into())
}

// ------------------------------------------------------------ protobuf

#[test]
fn protobuf_decodes_spans_and_resource() {
    let req = pb_request(
        &[
            pb_kv("service.name", V::S("web")),
            pb_kv("process.pid", V::I(4242)),
            pb_kv("service.version", V::S("1.2.3")),
            pb_kv("vcs.ref.head.revision", V::S("abc123")),
            pb_kv("deployment.environment.name", V::S("dev")),
            pb_kv("host.name", V::S("ignored")),
        ],
        &[pb_span(&PbSpan {
            name: "GET /users/:id",
            kind: 2,
            parent: &[0u8; 8],
            start: 1_000,
            end: 5_000,
            status: 2,
            attrs: vec![
                pb_kv("http.route", V::S("/users/:id")),
                pb_kv("http.request.method", V::S("GET")),
                pb_kv("url.full", V::S("http://x/users/1?token=secret")),
                pb_kv("db.statement", V::S("select secret")),
                pb_kv("some.bool", V::B(true)),
                pb_kv("some.double", V::D(1.5)),
            ],
        })],
    );
    let b = decode_protobuf(&req, &DecodeLimits::default()).unwrap();
    assert_eq!(b.resource_spans.len(), 1);
    let rs = &b.resource_spans[0];
    let keys: Vec<&str> = rs.resource.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "service.name",
            "process.pid",
            "service.version",
            "vcs.ref.head.revision",
            "deployment.environment.name"
        ]
    );
    assert_eq!(rs.resource[1].value, AttrValue::Int(4242));
    let sp = &rs.spans[0];
    assert_eq!(sp.name, "GET /users/:id");
    assert_eq!(sp.kind, KIND_SERVER);
    assert!(sp.is_root, "an all-zero parent id is a root");
    assert_eq!((sp.start_unix_nano, sp.end_unix_nano), (1_000, 5_000));
    assert_eq!(sp.status_code, STATUS_ERROR);
    // Only the kept keys: no url.full, no db.statement.
    let keys: Vec<&str> = sp.attributes.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["http.route", "http.request.method"]);
}

#[test]
fn protobuf_literal_bytes_fixture() {
    // Written by hand from the .proto field numbers, independent of the
    // builder above: one resource_spans with service.name "a", one span
    // named "s", kind SERVER, start 1, end 2, parent id 0x01, status ERROR.
    #[rustfmt::skip]
    let kv: &[u8] = &[
        0x0a, 0x0c, b's', b'e', b'r', b'v', b'i', b'c', b'e', b'.', b'n', b'a', b'm', b'e',
        0x12, 0x03, 0x0a, 0x01, b'a',
    ];
    let mut resource = vec![0x0a, kv.len() as u8];
    resource.extend_from_slice(kv);
    #[rustfmt::skip]
    let span: &[u8] = &[
        0x22, 0x01, 0x01,                   // parent_span_id = [1]
        0x2a, 0x01, b's',                   // name = "s"
        0x30, 0x02,                         // kind = 2
        0x39, 1, 0, 0, 0, 0, 0, 0, 0,       // start = 1 (fixed64)
        0x41, 2, 0, 0, 0, 0, 0, 0, 0,       // end = 2 (fixed64)
        0x85, 0x01, 1, 0, 0, 0,             // flags = 1 (field 16, fixed32)
        0x7a, 0x02, 0x18, 0x02,             // status { code = 2 }
    ];
    let mut scope = vec![0x12, span.len() as u8];
    scope.extend_from_slice(span);
    let mut rs = vec![0x0a, resource.len() as u8];
    rs.extend_from_slice(&resource);
    rs.extend_from_slice(&[0x12, scope.len() as u8]);
    rs.extend_from_slice(&scope);
    let mut req = vec![0x0a, rs.len() as u8];
    req.extend_from_slice(&rs);

    let b = decode_protobuf(&req, &DecodeLimits::default()).unwrap();
    let rs = &b.resource_spans[0];
    assert_eq!(rs.resource[0].value, s("a"));
    let sp = &rs.spans[0];
    assert_eq!(sp.name, "s");
    assert_eq!(sp.kind, 2);
    assert!(!sp.is_root);
    assert_eq!((sp.start_unix_nano, sp.end_unix_nano), (1, 2));
    assert_eq!(sp.status_code, 2);
}

#[test]
fn protobuf_negative_int_and_empty_request() {
    let req = pb_request(&[pb_kv("process.pid", V::I(-7))], &[]);
    let b = decode_protobuf(&req, &DecodeLimits::default()).unwrap();
    assert_eq!(b.resource_spans[0].resource[0].value, AttrValue::Int(-7));
    let empty = decode_protobuf(&[], &DecodeLimits::default()).unwrap();
    assert!(empty.resource_spans.is_empty());
}

#[test]
fn protobuf_skips_unknown_fields_of_every_wire_type() {
    let mut span = pb_span(&PbSpan {
        name: "x",
        kind: 5,
        parent: &[],
        start: 1,
        end: 2,
        status: 0,
        attrs: vec![],
    });
    varint_field(&mut span, 99, 123456789);
    fixed64_field(&mut span, 98, 7);
    len_field(&mut span, 97, b"whatever");
    tag(&mut span, 96, 5);
    span.extend_from_slice(&[0, 0, 0, 0]);
    let mut req = pb_request(&[], &[span]);
    varint_field(&mut req, 50, 1);
    let b = decode_protobuf(&req, &DecodeLimits::default()).unwrap();
    assert_eq!(b.resource_spans[0].spans[0].kind, KIND_CONSUMER);
    assert!(b.resource_spans[0].spans[0].is_root);
}

#[test]
fn protobuf_rejects_hostile_input() {
    let l = DecodeLimits::default();
    let bad: &[(&str, Vec<u8>)] = &[
        ("truncated varint", vec![0x0a, 0x80]),
        ("eleven-byte varint", {
            // Field 2 (unknown, varint), skipped through a bad varint.
            let mut v = vec![0x10];
            v.extend_from_slice(&[0xff; 10]);
            v.push(0x01);
            v
        }),
        ("huge length", {
            let mut v = vec![0x0a];
            varint(&mut v, u64::MAX >> 1);
            v
        }),
        ("length past end", vec![0x0a, 0x05, 0x01]),
        ("field zero", vec![0x02, 0x00]),
        ("group wire type", vec![0x0b]),
        ("wire type 7", vec![0x0f]),
        ("wrong wire type for resource_spans", vec![0x08, 0x01]),
        ("truncated fixed64", {
            let span = vec![0x39, 1, 2, 3];
            pb_request(&[], &[span])
        }),
        ("nested length past parent", {
            // A span claiming 10 bytes inside a scope that has 3.
            let scope = vec![0x12, 0x0a, 0x00];
            let mut rs = Vec::new();
            len_field(&mut rs, 2, &scope);
            let mut req = Vec::new();
            len_field(&mut req, 1, &rs);
            req
        }),
    ];
    for (what, input) in bad {
        assert!(decode_protobuf(input, &l).is_err(), "{what} should fail");
    }
}

#[test]
fn protobuf_deep_nesting_is_skipped_without_recursion() {
    // A kvlist nested 10 000 deep inside a kept attribute's value.
    let mut inner: Vec<u8> = Vec::new();
    for _ in 0..10_000 {
        let mut any = Vec::new();
        len_field(&mut any, 6, &inner); // kvlist_value
        inner = any;
    }
    let mut kv = Vec::new();
    len_field(&mut kv, 1, b"http.route");
    len_field(&mut kv, 2, &inner);
    let span = pb_span(&PbSpan {
        name: "deep",
        kind: 2,
        parent: &[],
        start: 1,
        end: 2,
        status: 0,
        attrs: vec![kv],
    });
    let b = decode_protobuf(&pb_request(&[], &[span]), &DecodeLimits::default()).unwrap();
    assert!(b.resource_spans[0].spans[0].attributes.is_empty());
}

/// xorshift64*, deterministic.
fn rng(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

#[test]
fn protobuf_random_and_mutated_input_never_panics() {
    let good = pb_request(
        &[pb_kv("service.name", V::S("svc"))],
        &[pb_span(&PbSpan {
            name: "n",
            kind: 2,
            parent: &[],
            start: 1,
            end: 9,
            status: 1,
            attrs: vec![pb_kv("http.route", V::S("/a"))],
        })],
    );
    let l = DecodeLimits::default();
    let mut st = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..20_000 {
        let mut m = good.clone();
        let n = 1 + (rng(&mut st) % 4) as usize;
        for _ in 0..n {
            let i = (rng(&mut st) as usize) % m.len();
            m[i] = rng(&mut st) as u8;
        }
        let cut = (rng(&mut st) as usize) % (m.len() + 1);
        let _ = decode_protobuf(&m[..cut], &l);
        let _ = decode_protobuf(&m, &l);
    }
    for _ in 0..5_000 {
        let len = (rng(&mut st) % 64) as usize;
        let v: Vec<u8> = (0..len).map(|_| rng(&mut st) as u8).collect();
        let _ = decode_protobuf(&v, &l);
        let _ = decode_json(&v, &l);
    }
}

// ----------------------------------------------------------------- JSON

const JSON_FIXTURE: &str = r#"{
  "resourceSpans": [{
    "resource": {"attributes": [
      {"key": "service.name", "value": {"stringValue": "api"}},
      {"key": "process.pid", "value": {"intValue": "31337"}},
      {"key": "vcs.repository.ref.revision", "value": {"stringValue": "deadbeef"}},
      {"key": "deployment.environment", "value": {"stringValue": "local"}},
      {"key": "telemetry.sdk.name", "value": {"stringValue": "opentelemetry"}},
      {"key": "nested", "value": {"kvlistValue": {"values": [{"key": "a", "value": {"arrayValue": {"values": []}}}]}}}
    ]},
    "scopeSpans": [{
      "scope": {"name": "x"},
      "spans": [
        {
          "traceId": "5b8efff798038103d269b633813fc60c",
          "spanId": "eee19b7ec3c1b174",
          "parentSpanId": "",
          "name": "POST /orders",
          "kind": 2,
          "startTimeUnixNano": "1800000000000000000",
          "endTimeUnixNano": "1800000000250000000",
          "attributes": [
            {"key": "http.route", "value": {"stringValue": "/orders"}},
            {"key": "http.request.method", "value": {"stringValue": "POST"}},
            {"key": "url.full", "value": {"stringValue": "http://x/orders?api_key=s3cret"}}
          ],
          "status": {"code": 2, "message": "boom"},
          "unknownField": {"deep": [1, 2, 3]}
        },
        {
          "parentSpanId": "eee19b7ec3c1b174",
          "name": "SELECT",
          "kind": "SPAN_KIND_CLIENT",
          "startTimeUnixNano": 1800000000000000000,
          "endTimeUnixNano": 1800000000100000000
        }
      ]
    }]
  }]
}"#;

#[test]
fn json_decodes_strings_numbers_and_enum_names() {
    let b = decode_json(JSON_FIXTURE.as_bytes(), &DecodeLimits::default()).unwrap();
    let rs = &b.resource_spans[0];
    assert_eq!(rs.resource.len(), 4);
    assert_eq!(resource_of(&rs.resource).pid, Some(31337));
    assert_eq!(
        resource_of(&rs.resource).vcs_revision.as_deref(),
        Some("deadbeef")
    );
    let a = &rs.spans[0];
    assert!(a.is_root);
    assert_eq!(a.kind, KIND_SERVER);
    assert_eq!(a.status_code, STATUS_ERROR);
    assert_eq!(a.end_unix_nano - a.start_unix_nano, 250_000_000);
    let keys: Vec<&str> = a.attributes.iter().map(|x| x.key.as_str()).collect();
    assert_eq!(keys, ["http.route", "http.request.method"]);
    let c = &rs.spans[1];
    assert!(!c.is_root);
    assert_eq!(c.kind, 3);
    assert_eq!(c.end_unix_nano, 1_800_000_000_100_000_000);
}

#[test]
fn json_accepts_snake_case_and_nulls() {
    let j = r#"{"resource_spans":[{"resource":null,"scope_spans":[{"spans":[
        {"name":"x","kind":5,"start_time_unix_nano":"1","end_time_unix_nano":"2","attributes":null,"status":null}
    ]}]}]}"#;
    let b = decode_json(j.as_bytes(), &DecodeLimits::default()).unwrap();
    let sp = &b.resource_spans[0].spans[0];
    assert_eq!((sp.kind, sp.end_unix_nano), (5, 2));
    assert!(decode_json(b"{}", &DecodeLimits::default())
        .unwrap()
        .resource_spans
        .is_empty());
}

#[test]
fn json_rejects_malformed_and_too_deep() {
    let l = DecodeLimits::default();
    for bad in [
        &b""[..],
        b"{",
        b"null",
        b"{\"resourceSpans\": 5}",
        b"{\"resourceSpans\":[{\"scopeSpans\":[{\"spans\":[{\"name\":7}]}]}]}",
    ] {
        assert!(
            decode_json(bad, &l).is_err(),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
    // Deep nesting in an unknown field is skipped without using the stack.
    let deep = format!("{{\"x\":{}{}}}", "[".repeat(100_000), "]".repeat(100_000));
    let b = decode_json(deep.as_bytes(), &l).unwrap();
    assert!(b.resource_spans.is_empty());
    // Deep nesting where a value is decoded hits serde_json's recursion
    // limit instead.
    let deep = format!(
        "{{\"resourceSpans\":{}{}}}",
        "[".repeat(100_000),
        "]".repeat(100_000)
    );
    assert!(decode_json(deep.as_bytes(), &l).is_err());
    // A kvlist value 10 000 deep is skipped too, whether it parses as
    // unknown content or hits the limit.
    let nested = format!(
        "{}{{}}{}",
        "{\"values\":[{\"key\":\"a\",\"value\":{\"kvlistValue\":".repeat(10_000),
        "}}]}".repeat(10_000)
    );
    let deep = [
        r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"kvlistValue":"#,
        &nested,
        "}}]}}]}",
    ]
    .concat();
    let b = decode_json(deep.as_bytes(), &l).unwrap();
    assert!(b.resource_spans[0].resource.is_empty());
}

// ----------------------------------------------------------------- caps

#[test]
fn decode_caps_spans_attributes_and_strings() {
    let limits = DecodeLimits {
        max_spans: 2,
        max_attributes: 3,
        max_string: 8,
    };
    let span = |name: &str| {
        pb_span(&PbSpan {
            name,
            kind: 2,
            parent: &[],
            start: 1,
            end: 2,
            status: 0,
            attrs: vec![
                pb_kv("a", V::S("1")),
                pb_kv("b", V::S("1")),
                pb_kv("c", V::S("1")),
                pb_kv("http.route", V::S("/past/the/cap")),
            ],
        })
    };
    let req = pb_request(
        &[],
        &[span("a-very-long-name"), span("é-é-é-é-é"), span("third")],
    );
    let b = decode_protobuf(&req, &limits).unwrap();
    assert_eq!(b.span_count(), 2);
    assert_eq!(b.dropped.spans, 1);
    assert_eq!(b.dropped.attributes, 2);
    assert_eq!(b.dropped.truncated_strings, 2);
    let sp = &b.resource_spans[0].spans;
    assert_eq!(sp[0].name, "a-very-l");
    assert!(sp[1].name.len() <= 8 && sp[1].name.starts_with("é-é"));
    assert!(sp[0].attributes.is_empty(), "http.route was past the cap");

    // Same caps through JSON.
    let j = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[
        {"name":"abcdefghijkl","attributes":[{"key":"a"},{"key":"b"},{"key":"c"},{"key":"http.route","value":{"stringValue":"/x"}}]},
        {"name":"b"},{"name":"c"}]}]}]}"#;
    let b = decode_json(j.as_bytes(), &limits).unwrap();
    assert_eq!(
        b.dropped,
        DecodeDrops {
            spans: 1,
            attributes: 1,
            truncated_strings: 1
        }
    );
    assert_eq!(b.resource_spans[0].spans[0].name, "abcdefgh");
}

#[test]
fn aggregation_caps_services_operations_and_cells() {
    let mut agg = Aggregator::new(AggLimits {
        max_services: 2,
        max_operations_per_service: 2,
        max_cells: 5,
        ..AggLimits::default()
    });
    let t = NOW_MS - 1000;
    agg.ingest(
        &batch(
            "a",
            vec![server_span("op1", t, 5), server_span("op2", t, 5)],
        ),
        NOW_MS,
    );
    agg.ingest(&batch("a", vec![server_span("op3", t, 5)]), NOW_MS);
    assert_eq!(agg.drops().operations, 1);
    agg.ingest(&batch("b", vec![server_span("op1", t, 5)]), NOW_MS);
    agg.ingest(&batch("c", vec![server_span("op1", t, 5)]), NOW_MS);
    assert_eq!(agg.drops().services, 1);
    // 3 cells used; two more windows fit, the third does not.
    let w = AggLimits::default().window_ms;
    agg.ingest(
        &batch(
            "a",
            vec![
                server_span("op1", t - w, 5),
                server_span("op1", t - 2 * w, 5),
                server_span("op1", t - 3 * w, 5),
            ],
        ),
        NOW_MS,
    );
    let st = agg.status();
    assert_eq!(st.cells, 5);
    assert_eq!(st.drops.cells, 1);
    assert_eq!(st.services, 2);
    assert_eq!(st.recorded_spans, 5);
    assert_eq!(st.received_spans, 8);
    // Existing cells still take spans at the cap.
    agg.ingest(&batch("b", vec![server_span("op1", t, 7)]), NOW_MS);
    assert_eq!(agg.status().recorded_spans, 6);
}

#[test]
fn a_new_service_refused_for_cells_is_not_left_empty() {
    let mut agg = Aggregator::new(AggLimits {
        max_cells: 1,
        ..AggLimits::default()
    });
    agg.ingest(&batch("a", vec![server_span("x", NOW_MS, 1)]), NOW_MS);
    agg.ingest(&batch("b", vec![server_span("x", NOW_MS, 1)]), NOW_MS);
    agg.ingest(&batch("a", vec![server_span("y", NOW_MS, 1)]), NOW_MS);
    assert_eq!(agg.status().services, 1);
    assert_eq!(agg.drops().cells, 2);
}

// ---------------------------------------------------------- aggregation

#[test]
fn quantiles_from_the_histogram() {
    let mut agg = Aggregator::default();
    // 1..=100 ms.
    let spans: Vec<Span> = (1..=100)
        .map(|d| server_span("GET /x", NOW_MS, d))
        .collect();
    agg.ingest(&batch("web", spans), NOW_MS);
    let ops = agg.operations(0, u64::MAX);
    assert_eq!(ops.len(), 1);
    let st = &ops[0].stats;
    assert_eq!(st.count, 100);
    assert_eq!(st.max_ms, 100.0);
    assert!((st.mean_ms - 50.5).abs() < 1e-9);
    // Buckets are at most 25% wide, so each quantile is within ~13%.
    let near = |got: f64, want: f64| (got - want).abs() / want < 0.13;
    assert!(near(st.p50_ms, 50.0), "p50 {}", st.p50_ms);
    assert!(near(st.p95_ms, 95.0), "p95 {}", st.p95_ms);
    assert!(near(st.p99_ms, 99.0), "p99 {}", st.p99_ms);
    assert!(st.p99_ms <= st.max_ms);
}

#[test]
fn buckets_are_monotonic_and_cover_their_values() {
    let mut last = 0;
    for us in [
        0u64,
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        8,
        9,
        100,
        1_000,
        123_456,
        1 << 32,
        (1 << 33) - 1,
    ] {
        let b = bucket_of(us);
        assert!(b >= last, "{us}");
        last = b;
        let (lo, hi) = bucket_bounds(b);
        if us > 0 {
            assert!(lo <= us as f64 && (us as f64) < hi, "{us} in [{lo}, {hi})");
        }
    }
    assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
    assert_eq!(bucket_of(1 << 33), BUCKETS - 1);
}

#[test]
fn errors_windows_and_resource_attribution() {
    let mut agg = Aggregator::default();
    let w = AggLimits::default().window_ms;
    let mut b1 = batch("web", vec![server_span("op", NOW_MS - w, 10)]);
    b1.resource_spans[0].resource.push(Attr {
        key: "vcs.ref.head.revision".into(),
        value: s("old"),
    });
    let mut err = server_span("op", NOW_MS, 30);
    err.status_code = STATUS_ERROR;
    let mut b2 = batch("web", vec![server_span("op", NOW_MS, 20), err]);
    b2.resource_spans[0].resource.extend([
        Attr {
            key: "vcs.ref.head.revision".into(),
            value: s("new"),
        },
        Attr {
            key: "process.pid".into(),
            value: AttrValue::Int(99),
        },
    ]);
    agg.ingest(&b1, NOW_MS);
    agg.ingest(&b2, NOW_MS);

    let ws = agg.windows("web", "op");
    assert_eq!(ws.len(), 2);
    assert!(ws[0].start_ms < ws[1].start_ms);
    assert_eq!(ws[0].end_ms - ws[0].start_ms, w);
    assert_eq!((ws[0].stats.count, ws[0].stats.errors), (1, 0));
    assert_eq!((ws[1].stats.count, ws[1].stats.errors), (2, 1));
    assert_eq!(ws[0].resource.vcs_revision.as_deref(), Some("old"));
    assert_eq!(ws[1].resource.vcs_revision.as_deref(), Some("new"));
    assert_eq!(ws[1].resource.pid, Some(99));

    // A range covering only the newer window.
    let now_window = ws[1].start_ms;
    let ops = agg.operations(now_window, u64::MAX);
    assert_eq!(ops[0].stats.count, 2);
    assert_eq!(ops[0].first_seen_ms, NOW_MS - w);
    let svc = &agg.services()[0];
    assert_eq!(svc.name, "web");
    assert_eq!(svc.resource.vcs_revision.as_deref(), Some("new"));
}

#[test]
fn which_spans_are_kept_and_invalid_or_old_ones() {
    let mut agg = Aggregator::default();
    let mut client_child = server_span("child", NOW_MS, 1);
    client_child.kind = 3;
    let mut internal_root = server_span("job", NOW_MS, 1);
    internal_root.kind = 1;
    internal_root.is_root = true;
    let mut consumer = server_span("queue", NOW_MS, 1);
    consumer.kind = KIND_CONSUMER;
    let mut backwards = server_span("bad", NOW_MS, 1);
    backwards.start_unix_nano = backwards.end_unix_nano + 1;
    let mut no_end = server_span("bad", NOW_MS, 1);
    no_end.end_unix_nano = 0;
    no_end.start_unix_nano = 0;
    let old = server_span(
        "old",
        NOW_MS - AggLimits::default().retention_ms - 3_600_000,
        1,
    );
    let future = server_span("future", NOW_MS + 3_600_000, 1);
    agg.ingest(
        &batch(
            "svc",
            vec![
                client_child,
                internal_root,
                consumer,
                backwards,
                no_end,
                old,
                future,
            ],
        ),
        NOW_MS,
    );
    let st = agg.status();
    assert_eq!(st.ignored_spans, 1);
    assert_eq!(st.drops.invalid, 2);
    assert_eq!(st.drops.too_old, 1);
    assert_eq!(st.recorded_spans, 3);
    let names: Vec<String> = agg
        .operations(0, u64::MAX)
        .into_iter()
        .map(|o| o.operation)
        .collect();
    assert!(names.contains(&"job".to_string()) && names.contains(&"queue".to_string()));
    // The future span was counted at now.
    let f = agg.windows("svc", "future");
    assert!(f[0].start_ms <= NOW_MS && NOW_MS < f[0].end_ms);
}

#[test]
fn windows_roll_off_and_free_their_keys() {
    let mut agg = Aggregator::new(AggLimits {
        max_operations_per_service: 1,
        ..AggLimits::default()
    });
    agg.ingest(&batch("svc", vec![server_span("a", NOW_MS, 1)]), NOW_MS);
    assert_eq!(agg.status().cells, 1);
    let later = NOW_MS + AggLimits::default().retention_ms + 2 * AggLimits::default().window_ms;
    // The old key rolled off, so a new operation fits under the cap of 1.
    agg.ingest(&batch("svc", vec![server_span("b", later, 1)]), later);
    let st = agg.status();
    assert_eq!((st.cells, st.operations, st.drops.operations), (1, 1, 0));
    assert_eq!(agg.windows("svc", "a").len(), 0);
    agg.prune(later + 10 * AggLimits::default().retention_ms);
    assert_eq!(agg.status().services, 0);
}

#[test]
fn decode_drops_are_carried_into_aggregator_drops() {
    let mut agg = Aggregator::default();
    let mut b = batch("svc", vec![]);
    b.dropped = DecodeDrops {
        spans: 3,
        attributes: 4,
        truncated_strings: 5,
    };
    agg.ingest(&b, NOW_MS);
    let d = agg.drops();
    assert_eq!(
        (d.spans_over_request_cap, d.attributes, d.truncated_strings),
        (3, 4, 5)
    );
}

// ------------------------------------------------------------- naming

#[test]
fn operation_prefers_route_with_method() {
    let mut sp = server_span("GET /users/42?x=1", NOW_MS, 1);
    assert_eq!(operation_of(&sp, 256), "GET /users/42");
    sp.attributes.push(Attr {
        key: "http.route".into(),
        value: s("/users/{id}"),
    });
    assert_eq!(operation_of(&sp, 256), "/users/{id}");
    sp.attributes.push(Attr {
        key: "http.method".into(),
        value: s("GET"),
    });
    assert_eq!(operation_of(&sp, 256), "GET /users/{id}");
    let mut t = server_span("x", NOW_MS, 1);
    t.attributes.push(Attr {
        key: "url.template".into(),
        value: s("/a/{b}"),
    });
    t.attributes.push(Attr {
        key: "http.request.method".into(),
        value: s("bad method"),
    });
    assert_eq!(operation_of(&t, 256), "/a/{b}");
}

#[test]
fn operation_names_are_redacted() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnopqrstuv";
    let got = sanitize_operation(&format!("GET /reset/{jwt}/done?token=abc"), 256);
    assert_eq!(got, "GET /reset/[redacted]/done");
    let got = sanitize_operation("GET /k/ghp_abcdefghijklmnopqrstuvwxyz0123456789", 256);
    assert!(!got.contains("ghp_"), "{got}");
    let got = sanitize_operation("GET /u/0123456789abcdef0123456789abcdef01", 256);
    assert_eq!(got, "GET /u/[redacted]");
    assert!(!sanitize_operation("GET /x --token=s3cr3tvalue", 256).contains("s3cr3t"));
    assert_eq!(sanitize_operation("", 256), "unnamed");
    assert_eq!(sanitize_operation("?only=query", 256), "unnamed");
    assert_eq!(sanitize_operation("ééé", 3), "é");
}

#[test]
fn missing_service_name_is_unknown_service() {
    let mut agg = Aggregator::default();
    let mut b = batch("x", vec![server_span("op", NOW_MS, 1)]);
    b.resource_spans[0].resource.clear();
    agg.ingest(&b, NOW_MS);
    assert_eq!(agg.services()[0].name, UNKNOWN_SERVICE);
}

#[test]
fn json_and_protobuf_aggregate_alike() {
    let mut a = Aggregator::default();
    let j = decode_json(JSON_FIXTURE.as_bytes(), &DecodeLimits::default()).unwrap();
    a.ingest(&j, NOW_MS);
    let ops = a.operations(0, u64::MAX);
    assert_eq!(ops.len(), 1, "the client child span is not aggregated");
    assert_eq!(ops[0].service, "api");
    assert_eq!(ops[0].operation, "POST /orders");
    assert_eq!(ops[0].stats.errors, 1);
    assert_eq!(
        ops[0].resource.deployment_environment.as_deref(),
        Some("local")
    );
    assert!((ops[0].stats.max_ms - 250.0).abs() < 1e-9);
}
