//! OpenTelemetry traces from apps in local development: decoding and
//! aggregation. Not wired into the engine yet; `otel_server.rs` receives the
//! requests.
//!
//! Decoding reads an OTLP `ExportTraceServiceRequest` in either encoding:
//! OTLP/HTTP JSON ([`decode_json`]) or protobuf ([`decode_protobuf`], a small
//! hand-written wire-format reader). Both keep only what aggregation needs:
//! the span name, kind, start and end time, status code, whether the span is a
//! root, and a fixed list of attributes ([`SPAN_KEYS`], [`RESOURCE_KEYS`]).
//! Everything else, `url.full`, `db.statement` and query strings included, is
//! skipped while decoding and never stored. Operation names have their query
//! string cut off and go through `redact.rs`.
//!
//! [`Aggregator`] keeps, for each (`service.name`, operation) and each time
//! window (5 minutes by default, 24 hours kept), a count, an error count, a
//! fixed-bucket duration histogram (p50, p95, p99), the slowest span, and the
//! resource attributes last seen ([`Resource`]). Every structure is capped
//! ([`DecodeLimits`], [`AggLimits`]); past a cap the span or value is dropped
//! and counted in [`Drops`], so memory stays bounded.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::redact;

// ------------------------------------------------------------------ model

/// `SpanKind` values.
pub const KIND_SERVER: i32 = 2;
pub const KIND_CONSUMER: i32 = 5;
/// `StatusCode::STATUS_CODE_ERROR`.
pub const STATUS_ERROR: i32 = 2;

/// Span attributes kept by the decoders; every other one is skipped.
pub const SPAN_KEYS: &[&str] = &[
    "http.route",
    "url.template",
    "http.request.method",
    "http.method",
];

/// Resource attributes kept by the decoders; every other one is skipped.
pub const RESOURCE_KEYS: &[&str] = &[
    "service.name",
    "process.pid",
    "service.version",
    "vcs.ref.head.revision",
    "vcs.repository.ref.revision",
    "deployment.environment",
    "deployment.environment.name",
];

/// An attribute value. Arrays, key-value lists and bytes are skipped.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
    Double(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub key: String,
    pub value: AttrValue,
}

/// The part of a span that aggregation uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub name: String,
    pub kind: i32,
    /// No parent span id, or an all-zero one.
    pub is_root: bool,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub status_code: i32,
    /// Only [`SPAN_KEYS`].
    pub attributes: Vec<Attr>,
}

impl Default for Span {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: 0,
            is_root: true,
            start_unix_nano: 0,
            end_unix_nano: 0,
            status_code: 0,
            attributes: Vec::new(),
        }
    }
}

/// The spans of one resource (one process, usually), scopes flattened.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceSpans {
    /// Only [`RESOURCE_KEYS`].
    pub resource: Vec<Attr>,
    pub spans: Vec<Span>,
}

/// One decoded export request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceBatch {
    pub resource_spans: Vec<ResourceSpans>,
    /// What decoding dropped or cut short because of [`DecodeLimits`].
    pub dropped: DecodeDrops,
}

impl TraceBatch {
    pub fn span_count(&self) -> usize {
        self.resource_spans.iter().map(|r| r.spans.len()).sum()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeDrops {
    /// Spans past [`DecodeLimits::max_spans`].
    pub spans: u64,
    /// Attributes past [`DecodeLimits::max_attributes`] on one span or resource.
    pub attributes: u64,
    /// Kept strings cut to [`DecodeLimits::max_string`].
    pub truncated_strings: u64,
}

/// Caps applied while decoding one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Spans per request.
    pub max_spans: usize,
    /// Attributes looked at per span or resource; the rest are skipped.
    pub max_attributes: usize,
    /// Bytes per kept string (span name, attribute value).
    pub max_string: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_spans: 20_000,
            max_attributes: 128,
            max_string: 256,
        }
    }
}

/// Input that is not a valid export request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

fn derr(msg: impl Into<String>) -> DecodeError {
    DecodeError(msg.into())
}

/// `s` cut to at most `max` bytes on a character boundary.
fn cut(mut s: String, max: usize, drops: &mut DecodeDrops) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        drops.truncated_strings += 1;
    }
    s
}

/// UTF-8 (lossy) of at most `max` bytes of `b`, without copying the rest.
fn cut_bytes(b: &[u8], max: usize, drops: &mut DecodeDrops) -> String {
    if b.len() <= max {
        return cut(String::from_utf8_lossy(b).into_owned(), max, drops);
    }
    // A few extra bytes so a character on the boundary is not mangled; `cut`
    // then trims to `max` and counts the truncation (lossy decoding never
    // makes a string shorter, so the head is still longer than `max`).
    let head = &b[..max.saturating_add(4).min(b.len())];
    cut(String::from_utf8_lossy(head).into_owned(), max, drops)
}

/// Collects kept attributes under the per-owner cap.
struct AttrCollector<'k> {
    keys: &'k [&'k str],
    seen: usize,
    out: Vec<Attr>,
}

impl<'k> AttrCollector<'k> {
    fn new(keys: &'k [&'k str]) -> Self {
        Self {
            keys,
            seen: 0,
            out: Vec::new(),
        }
    }

    /// Whether one more attribute may be looked at; counts a drop if not.
    fn admit(&mut self, limits: &DecodeLimits, drops: &mut DecodeDrops) -> bool {
        self.seen += 1;
        if self.seen > limits.max_attributes {
            drops.attributes += 1;
            return false;
        }
        true
    }

    fn wants(&self, key: &[u8]) -> Option<&'k str> {
        self.keys.iter().copied().find(|k| k.as_bytes() == key)
    }

    fn push(&mut self, key: &'k str, value: AttrValue) {
        // Last value wins for a repeated key.
        self.out.retain(|a| a.key != key);
        self.out.push(Attr {
            key: key.to_string(),
            value,
        });
    }
}

// --------------------------------------------------------------- protobuf

/// A reader over protobuf wire format. Every length is checked against what
/// is left; nothing is allocated from a length read off the wire.
struct Pb<'a> {
    buf: &'a [u8],
    pos: usize,
}

const WT_VARINT: u8 = 0;
const WT_I64: u8 = 1;
const WT_LEN: u8 = 2;
const WT_I32: u8 = 5;

impl<'a> Pb<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let Some(&b) = self.buf.get(self.pos) else {
                return Err(derr("truncated varint"));
            };
            self.pos += 1;
            if i == 9 && b > 1 {
                return Err(derr("varint overflows 64 bits"));
            }
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(derr("varint longer than 10 bytes"))
    }

    /// A field key: (field number, wire type).
    fn key(&mut self) -> Result<(u64, u8), DecodeError> {
        let k = self.varint()?;
        let field = k >> 3;
        let wt = (k & 7) as u8;
        if field == 0 || field > (1 << 29) - 1 {
            return Err(derr("invalid field number"));
        }
        Ok((field, wt))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| derr("length runs past the end of the message"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn len_delimited(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.varint()?;
        let n = usize::try_from(n).map_err(|_| derr("length too large"))?;
        self.take(n)
    }

    fn fixed64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn skip(&mut self, wt: u8) -> Result<(), DecodeError> {
        match wt {
            WT_VARINT => self.varint().map(|_| ()),
            WT_I64 => self.take(8).map(|_| ()),
            WT_LEN => self.len_delimited().map(|_| ()),
            WT_I32 => self.take(4).map(|_| ()),
            _ => Err(derr(format!("unsupported wire type {wt}"))),
        }
    }
}

fn expect(wt: u8, want: u8, what: &str) -> Result<(), DecodeError> {
    if wt == want {
        Ok(())
    } else {
        Err(derr(format!("{what} has wire type {wt}, expected {want}")))
    }
}

/// Decode a protobuf `ExportTraceServiceRequest`.
pub fn decode_protobuf(body: &[u8], limits: &DecodeLimits) -> Result<TraceBatch, DecodeError> {
    let mut out = TraceBatch::default();
    let mut spans_left = limits.max_spans;
    let mut r = Pb::new(body);
    while !r.done() {
        let (field, wt) = r.key()?;
        if field == 1 {
            expect(wt, WT_LEN, "resource_spans")?;
            let b = r.len_delimited()?;
            let rs = pb_resource_spans(b, limits, &mut spans_left, &mut out.dropped)?;
            // A resource with no spans adds nothing.
            if !rs.spans.is_empty() {
                out.resource_spans.push(rs);
            }
        } else {
            r.skip(wt)?;
        }
    }
    Ok(out)
}

fn pb_resource_spans(
    buf: &[u8],
    limits: &DecodeLimits,
    spans_left: &mut usize,
    drops: &mut DecodeDrops,
) -> Result<ResourceSpans, DecodeError> {
    let mut out = ResourceSpans::default();
    let mut res = AttrCollector::new(RESOURCE_KEYS);
    let mut r = Pb::new(buf);
    while !r.done() {
        let (field, wt) = r.key()?;
        match field {
            1 => {
                expect(wt, WT_LEN, "resource")?;
                let mut rr = Pb::new(r.len_delimited()?);
                while !rr.done() {
                    let (f, w) = rr.key()?;
                    if f == 1 {
                        expect(w, WT_LEN, "resource attribute")?;
                        let kv = rr.len_delimited()?;
                        pb_key_value(kv, &mut res, limits, drops)?;
                    } else {
                        rr.skip(w)?;
                    }
                }
            }
            2 => {
                expect(wt, WT_LEN, "scope_spans")?;
                let mut sr = Pb::new(r.len_delimited()?);
                while !sr.done() {
                    let (f, w) = sr.key()?;
                    if f == 2 {
                        expect(w, WT_LEN, "span")?;
                        let sb = sr.len_delimited()?;
                        if *spans_left == 0 {
                            drops.spans += 1;
                            continue;
                        }
                        *spans_left -= 1;
                        out.spans.push(pb_span(sb, limits, drops)?);
                    } else {
                        sr.skip(w)?;
                    }
                }
            }
            _ => r.skip(wt)?,
        }
    }
    out.resource = res.out;
    Ok(out)
}

fn pb_span(
    buf: &[u8],
    limits: &DecodeLimits,
    drops: &mut DecodeDrops,
) -> Result<Span, DecodeError> {
    let mut s = Span::default();
    let mut attrs = AttrCollector::new(SPAN_KEYS);
    let mut r = Pb::new(buf);
    while !r.done() {
        let (field, wt) = r.key()?;
        match field {
            4 => {
                expect(wt, WT_LEN, "parent_span_id")?;
                s.is_root = r.len_delimited()?.iter().all(|&b| b == 0);
            }
            5 => {
                expect(wt, WT_LEN, "name")?;
                s.name = cut_bytes(r.len_delimited()?, limits.max_string, drops);
            }
            6 => {
                expect(wt, WT_VARINT, "kind")?;
                s.kind = r.varint()? as i32;
            }
            7 => {
                expect(wt, WT_I64, "start_time_unix_nano")?;
                s.start_unix_nano = r.fixed64()?;
            }
            8 => {
                expect(wt, WT_I64, "end_time_unix_nano")?;
                s.end_unix_nano = r.fixed64()?;
            }
            9 => {
                expect(wt, WT_LEN, "attribute")?;
                let kv = r.len_delimited()?;
                pb_key_value(kv, &mut attrs, limits, drops)?;
            }
            15 => {
                expect(wt, WT_LEN, "status")?;
                let mut st = Pb::new(r.len_delimited()?);
                while !st.done() {
                    let (f, w) = st.key()?;
                    if f == 3 {
                        expect(w, WT_VARINT, "status code")?;
                        s.status_code = st.varint()? as i32;
                    } else {
                        st.skip(w)?;
                    }
                }
            }
            _ => r.skip(wt)?,
        }
    }
    s.attributes = attrs.out;
    Ok(s)
}

/// One `KeyValue`. The value is decoded only when the key is kept.
fn pb_key_value(
    buf: &[u8],
    attrs: &mut AttrCollector<'_>,
    limits: &DecodeLimits,
    drops: &mut DecodeDrops,
) -> Result<(), DecodeError> {
    let mut key: &[u8] = &[];
    let mut value: Option<&[u8]> = None;
    let mut r = Pb::new(buf);
    while !r.done() {
        let (field, wt) = r.key()?;
        match field {
            1 => {
                expect(wt, WT_LEN, "key")?;
                key = r.len_delimited()?;
            }
            2 => {
                expect(wt, WT_LEN, "value")?;
                value = Some(r.len_delimited()?);
            }
            _ => r.skip(wt)?,
        }
    }
    if !attrs.admit(limits, drops) {
        return Ok(());
    }
    let (Some(k), Some(v)) = (attrs.wants(key), value) else {
        return Ok(());
    };
    if let Some(v) = pb_any_value(v, limits, drops)? {
        attrs.push(k, v);
    }
    Ok(())
}

/// An `AnyValue`. Arrays, key-value lists and bytes are skipped unread, so
/// nesting depth costs nothing.
fn pb_any_value(
    buf: &[u8],
    limits: &DecodeLimits,
    drops: &mut DecodeDrops,
) -> Result<Option<AttrValue>, DecodeError> {
    let mut out = None;
    let mut r = Pb::new(buf);
    while !r.done() {
        let (field, wt) = r.key()?;
        match field {
            1 => {
                expect(wt, WT_LEN, "string_value")?;
                let b = r.len_delimited()?;
                out = Some(AttrValue::Str(cut_bytes(b, limits.max_string, drops)));
            }
            2 => {
                expect(wt, WT_VARINT, "bool_value")?;
                out = Some(AttrValue::Bool(r.varint()? != 0));
            }
            3 => {
                expect(wt, WT_VARINT, "int_value")?;
                out = Some(AttrValue::Int(r.varint()? as i64));
            }
            4 => {
                expect(wt, WT_I64, "double_value")?;
                out = Some(AttrValue::Double(f64::from_bits(r.fixed64()?)));
            }
            _ => r.skip(wt)?,
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------- JSON

/// A JSON number, or a number in a string (OTLP/JSON writes 64-bit integers
/// as strings).
#[derive(Deserialize)]
#[serde(untagged)]
enum JNum {
    U(u64),
    I(i64),
    F(f64),
    S(String),
}

impl JNum {
    fn as_u64(&self) -> Option<u64> {
        match self {
            JNum::U(v) => Some(*v),
            JNum::I(v) => u64::try_from(*v).ok(),
            JNum::F(_) => None,
            JNum::S(s) => s.trim().parse().ok(),
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            JNum::U(v) => i64::try_from(*v).ok(),
            JNum::I(v) => Some(*v),
            JNum::F(_) => None,
            JNum::S(s) => s.trim().parse().ok(),
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            JNum::U(v) => Some(*v as f64),
            JNum::I(v) => Some(*v as f64),
            JNum::F(v) => Some(*v),
            JNum::S(s) => s.trim().parse().ok(),
        }
    }

    /// An enum: its number, or its name in `names` (index = value).
    fn as_enum(&self, names: &[&str]) -> i32 {
        if let JNum::S(s) = self {
            if let Some(i) = names.iter().position(|n| n == s) {
                return i as i32;
            }
        }
        self.as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0)
    }
}

const KIND_NAMES: &[&str] = &[
    "SPAN_KIND_UNSPECIFIED",
    "SPAN_KIND_INTERNAL",
    "SPAN_KIND_SERVER",
    "SPAN_KIND_CLIENT",
    "SPAN_KIND_PRODUCER",
    "SPAN_KIND_CONSUMER",
];

const STATUS_NAMES: &[&str] = &["STATUS_CODE_UNSET", "STATUS_CODE_OK", "STATUS_CODE_ERROR"];

// Arrays are folded while they are parsed ([`Folded`]): each element is
// converted at once and only what is kept stays in memory, so the caps bound
// what is allocated, not just what is returned. The caps and drop counts of
// the request being parsed live in a thread-local [`JsonCtx`].

struct JsonCtx {
    limits: DecodeLimits,
    spans_left: usize,
    drops: DecodeDrops,
}

thread_local! {
    static JSON_CTX: std::cell::RefCell<JsonCtx> = const {
        std::cell::RefCell::new(JsonCtx {
            limits: DecodeLimits {
                max_spans: 0,
                max_attributes: 0,
                max_string: 0,
            },
            spans_left: 0,
            drops: DecodeDrops {
                spans: 0,
                attributes: 0,
                truncated_strings: 0,
            },
        })
    };
}

fn with_ctx<T>(f: impl FnOnce(&mut JsonCtx) -> T) -> T {
    JSON_CTX.with(|c| f(&mut c.borrow_mut()))
}

/// Folds the elements of a JSON array one at a time.
trait Fold: Default {
    type Item: serde::de::DeserializeOwned;
    /// Whether there is room for another element; if not, the next one is
    /// skipped unparsed and [`Fold::reject`] counts it.
    fn has_room(&self) -> bool {
        true
    }
    fn add(&mut self, item: Self::Item);
    fn reject(&mut self) {}
}

struct Folded<F>(F);

struct FoldVisitor<F>(std::marker::PhantomData<F>);

impl<'de, F: Fold> serde::de::Visitor<'de> for FoldVisitor<F> {
    type Value = Folded<F>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an array")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut fold = F::default();
        loop {
            if fold.has_room() {
                match seq.next_element::<F::Item>()? {
                    Some(item) => fold.add(item),
                    None => break,
                }
            } else {
                match seq.next_element::<serde::de::IgnoredAny>()? {
                    Some(_) => fold.reject(),
                    None => break,
                }
            }
        }
        Ok(Folded(fold))
    }
}

impl<'de, F: Fold> Deserialize<'de> for Folded<F> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_seq(FoldVisitor(std::marker::PhantomData))
    }
}

#[derive(Default)]
struct ResourceFold(Vec<ResourceSpans>);

impl Fold for ResourceFold {
    type Item = JResourceSpans;
    fn add(&mut self, j: JResourceSpans) {
        let spans = j.scope_spans.map(|f| f.0 .0).unwrap_or_default();
        // A resource with no spans adds nothing.
        if spans.is_empty() {
            return;
        }
        let resource = j
            .resource
            .and_then(|r| r.attributes)
            .map(|f| f.0.c.out)
            .unwrap_or_default();
        self.0.push(ResourceSpans { resource, spans });
    }
}

#[derive(Default)]
struct ScopeFold(Vec<Span>);

impl Fold for ScopeFold {
    type Item = JScopeSpans;
    fn add(&mut self, j: JScopeSpans) {
        if let Some(f) = j.spans {
            self.0.extend(f.0 .0);
        }
    }
}

#[derive(Default)]
struct SpanFold(Vec<Span>);

impl Fold for SpanFold {
    type Item = JSpan;
    fn has_room(&self) -> bool {
        with_ctx(|c| c.spans_left > 0)
    }
    fn add(&mut self, j: JSpan) {
        let span = with_ctx(|c| {
            c.spans_left = c.spans_left.saturating_sub(1);
            json_span(j, &c.limits, &mut c.drops)
        });
        self.0.push(span);
    }
    fn reject(&mut self) {
        with_ctx(|c| c.drops.spans += 1);
    }
}

/// Which attribute keys an [`AttrFold`] keeps.
trait Keys {
    const KEYS: &'static [&'static str];
}

struct SpanKeys;

impl Keys for SpanKeys {
    const KEYS: &'static [&'static str] = SPAN_KEYS;
}

struct ResourceKeys;

impl Keys for ResourceKeys {
    const KEYS: &'static [&'static str] = RESOURCE_KEYS;
}

struct AttrFold<K> {
    c: AttrCollector<'static>,
    _k: std::marker::PhantomData<K>,
}

impl<K: Keys> Default for AttrFold<K> {
    fn default() -> Self {
        Self {
            c: AttrCollector::new(K::KEYS),
            _k: std::marker::PhantomData,
        }
    }
}

impl<K: Keys> Fold for AttrFold<K> {
    type Item = JKeyValue;
    fn has_room(&self) -> bool {
        with_ctx(|ctx| self.c.seen < ctx.limits.max_attributes)
    }
    fn add(&mut self, kv: JKeyValue) {
        self.c.seen += 1;
        with_ctx(|ctx| json_key_value(kv, &mut self.c, &ctx.limits, &mut ctx.drops));
    }
    fn reject(&mut self) {
        with_ctx(|c| c.drops.attributes += 1);
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JRequest {
    #[serde(alias = "resource_spans")]
    resource_spans: Option<Folded<ResourceFold>>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JResourceSpans {
    resource: Option<JResource>,
    #[serde(alias = "scope_spans")]
    scope_spans: Option<Folded<ScopeFold>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct JResource {
    attributes: Option<Folded<AttrFold<ResourceKeys>>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct JScopeSpans {
    spans: Option<Folded<SpanFold>>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JSpan {
    #[serde(alias = "parent_span_id")]
    parent_span_id: Option<String>,
    name: Option<String>,
    kind: Option<JNum>,
    #[serde(alias = "start_time_unix_nano")]
    start_time_unix_nano: Option<JNum>,
    #[serde(alias = "end_time_unix_nano")]
    end_time_unix_nano: Option<JNum>,
    attributes: Option<Folded<AttrFold<SpanKeys>>>,
    status: Option<JStatus>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct JStatus {
    code: Option<JNum>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct JKeyValue {
    key: Option<String>,
    value: Option<JAnyValue>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JAnyValue {
    #[serde(alias = "string_value")]
    string_value: Option<String>,
    #[serde(alias = "bool_value")]
    bool_value: Option<bool>,
    #[serde(alias = "int_value")]
    int_value: Option<JNum>,
    #[serde(alias = "double_value")]
    double_value: Option<JNum>,
}

/// Decode an OTLP/HTTP JSON `ExportTraceServiceRequest`. Keys are
/// lowerCamelCase (snake_case is accepted too), unknown fields are ignored,
/// 64-bit integers may be strings or numbers, enums numbers or names.
pub fn decode_json(body: &[u8], limits: &DecodeLimits) -> Result<TraceBatch, DecodeError> {
    with_ctx(|c| {
        c.limits = *limits;
        c.spans_left = limits.max_spans;
        c.drops = DecodeDrops::default();
    });
    let parsed: Result<JRequest, _> = serde_json::from_slice(body);
    let dropped = with_ctx(|c| std::mem::take(&mut c.drops));
    let req = parsed.map_err(|e| derr(format!("invalid JSON: {e}")))?;
    Ok(TraceBatch {
        resource_spans: req.resource_spans.map(|f| f.0 .0).unwrap_or_default(),
        dropped,
    })
}

fn json_span(js: JSpan, limits: &DecodeLimits, drops: &mut DecodeDrops) -> Span {
    Span {
        name: cut(js.name.unwrap_or_default(), limits.max_string, drops),
        kind: js.kind.map(|k| k.as_enum(KIND_NAMES)).unwrap_or(0),
        is_root: js
            .parent_span_id
            .as_deref()
            .is_none_or(|p| p.bytes().all(|b| b == b'0')),
        start_unix_nano: js
            .start_time_unix_nano
            .and_then(|n| n.as_u64())
            .unwrap_or(0),
        end_unix_nano: js.end_time_unix_nano.and_then(|n| n.as_u64()).unwrap_or(0),
        status_code: js
            .status
            .and_then(|s| s.code)
            .map(|c| c.as_enum(STATUS_NAMES))
            .unwrap_or(0),
        attributes: js.attributes.map(|f| f.0.c.out).unwrap_or_default(),
    }
}

/// Keep one attribute if its key is wanted. The cap was checked before it
/// was parsed.
fn json_key_value(
    kv: JKeyValue,
    attrs: &mut AttrCollector<'_>,
    limits: &DecodeLimits,
    drops: &mut DecodeDrops,
) {
    let Some(k) = kv.key.as_deref().and_then(|k| attrs.wants(k.as_bytes())) else {
        return;
    };
    let Some(v) = kv.value else { return };
    let value = if let Some(s) = v.string_value {
        Some(AttrValue::Str(cut(s, limits.max_string, drops)))
    } else if let Some(b) = v.bool_value {
        Some(AttrValue::Bool(b))
    } else if let Some(i) = v.int_value.and_then(|n| n.as_i64()) {
        Some(AttrValue::Int(i))
    } else {
        v.double_value
            .and_then(|n| n.as_f64())
            .map(AttrValue::Double)
    };
    if let Some(value) = value {
        attrs.push(k, value);
    }
}

// -------------------------------------------------------------- histogram

/// Number of duration buckets. Bucket 0 holds 0 µs; bucket `1 + 4e + q`
/// holds `[2^e (4+q)/4, 2^e (5+q)/4)` µs for `e` in `0..=32`, so a bucket is
/// at most 25% wide; everything from 2^33 µs (about 2.4 hours) up lands in the
/// last one. Fixed buckets merge exactly across windows.
pub const BUCKETS: usize = 1 + 4 * 33;

fn bucket_of(us: u64) -> usize {
    if us == 0 {
        return 0;
    }
    let e = 63 - us.leading_zeros() as usize;
    if e > 32 {
        return BUCKETS - 1;
    }
    let q = if e >= 2 {
        (us >> (e - 2)) & 3
    } else {
        (us << (2 - e)) & 3
    } as usize;
    1 + 4 * e + q
}

/// Lower and upper bound of bucket `i`, in µs.
fn bucket_bounds(i: usize) -> (f64, f64) {
    if i == 0 {
        return (0.0, 0.0);
    }
    let e = (i - 1) / 4;
    let q = ((i - 1) % 4) as f64;
    let base = (1u64 << e) as f64;
    (base * (4.0 + q) / 4.0, base * (5.0 + q) / 4.0)
}

/// The `p` quantile (0..=1) of `hist`, in µs: the middle of the bucket it
/// falls in, never above `max_us`.
fn quantile_us(hist: &[u32], total: u64, p: f64, max_us: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let rank = ((p * total as f64).ceil() as u64).clamp(1, total);
    let mut seen = 0u64;
    for (i, &c) in hist.iter().enumerate() {
        seen += u64::from(c);
        if seen >= rank {
            let (lo, hi) = bucket_bounds(i);
            return ((lo + hi) / 2.0).min(max_us as f64);
        }
    }
    max_us as f64
}

// -------------------------------------------------------------- aggregator

/// The resource attributes used to attribute a slowdown later.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    /// `process.pid`.
    pub pid: Option<i64>,
    /// `service.version`.
    pub service_version: Option<String>,
    /// `vcs.ref.head.revision`, else `vcs.repository.ref.revision`.
    pub vcs_revision: Option<String>,
    /// `deployment.environment.name`, else `deployment.environment`.
    pub deployment_environment: Option<String>,
}

/// Caps and window sizes for [`Aggregator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggLimits {
    pub window_ms: u64,
    /// Windows older than this roll off (not counted as drops).
    pub retention_ms: u64,
    pub max_services: usize,
    pub max_operations_per_service: usize,
    /// (service, operation, window) cells in all. Each holds a histogram of
    /// [`BUCKETS`] `u32`s, so the default caps the histograms at about 4.4 MB.
    pub max_cells: usize,
    /// A span ending later than now plus this is counted at now.
    pub max_future_skew_ms: u64,
}

impl Default for AggLimits {
    fn default() -> Self {
        Self {
            window_ms: 5 * 60 * 1000,
            retention_ms: 24 * 60 * 60 * 1000,
            max_services: 32,
            max_operations_per_service: 128,
            max_cells: 8192,
            max_future_skew_ms: 60 * 1000,
        }
    }
}

/// What was not recorded, by reason. Decoding drops are added in from each
/// batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drops {
    /// Spans of a new service past [`AggLimits::max_services`].
    pub services: u64,
    /// Spans of a new operation past [`AggLimits::max_operations_per_service`].
    pub operations: u64,
    /// Spans that needed a new window past [`AggLimits::max_cells`].
    pub cells: u64,
    /// Spans that ended before the retention period.
    pub too_old: u64,
    /// Spans with no end time or ending before they started.
    pub invalid: u64,
    /// Decoding: spans past [`DecodeLimits::max_spans`].
    pub spans_over_request_cap: u64,
    /// Decoding: attributes past [`DecodeLimits::max_attributes`].
    pub attributes: u64,
    /// Decoding: strings cut to [`DecodeLimits::max_string`] (still recorded).
    pub truncated_strings: u64,
}

struct Cell {
    start_ms: u64,
    count: u64,
    errors: u64,
    sum_us: u64,
    max_us: u64,
    hist: Box<[u32; BUCKETS]>,
    resource: Arc<Resource>,
}

impl Cell {
    fn new(start_ms: u64, resource: Arc<Resource>) -> Self {
        Self {
            start_ms,
            count: 0,
            errors: 0,
            sum_us: 0,
            max_us: 0,
            hist: Box::new([0; BUCKETS]),
            resource,
        }
    }
}

struct Operation {
    first_seen_ms: u64,
    last_seen_ms: u64,
    /// Sorted by `start_ms`.
    cells: Vec<Cell>,
}

struct Service {
    first_seen_ms: u64,
    last_seen_ms: u64,
    resource: Arc<Resource>,
    ops: BTreeMap<String, Operation>,
}

/// Duration statistics over one or more windows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub count: u64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub mean_ms: f64,
    pub max_ms: f64,
}

#[derive(Default)]
struct Merge {
    count: u64,
    errors: u64,
    sum_us: u64,
    max_us: u64,
    hist: Vec<u32>,
}

impl Merge {
    fn add(&mut self, c: &Cell) {
        if self.hist.is_empty() {
            self.hist = vec![0; BUCKETS];
        }
        self.count += c.count;
        self.errors += c.errors;
        self.sum_us = self.sum_us.saturating_add(c.sum_us);
        self.max_us = self.max_us.max(c.max_us);
        for (a, b) in self.hist.iter_mut().zip(c.hist.iter()) {
            *a = a.saturating_add(*b);
        }
    }

    fn stats(&self) -> Stats {
        let ms = |us: f64| us / 1000.0;
        let q = |p| ms(quantile_us(&self.hist, self.count, p, self.max_us));
        Stats {
            count: self.count,
            errors: self.errors,
            p50_ms: q(0.50),
            p95_ms: q(0.95),
            p99_ms: q(0.99),
            mean_ms: if self.count == 0 {
                0.0
            } else {
                ms(self.sum_us as f64 / self.count as f64)
            },
            max_ms: ms(self.max_us as f64),
        }
    }
}

/// One (service, operation) over a time range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationSummary {
    pub service: String,
    pub operation: String,
    #[serde(flatten)]
    pub stats: Stats,
    /// Over the whole retention period, not just the range.
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// As last seen in the range.
    pub resource: Resource,
}

/// One window of one (service, operation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub start_ms: u64,
    pub end_ms: u64,
    #[serde(flatten)]
    pub stats: Stats,
    /// As last seen in this window.
    pub resource: Resource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    pub operations: usize,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub resource: Resource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggStatus {
    pub services: usize,
    pub operations: usize,
    pub cells: usize,
    /// Spans handed to [`Aggregator::ingest`].
    pub received_spans: u64,
    pub recorded_spans: u64,
    /// Spans that are neither server, consumer nor root: not aggregated, not
    /// a drop.
    pub ignored_spans: u64,
    pub drops: Drops,
    pub limits: AggLimits,
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Bounded per-window aggregates of server, consumer and root spans, keyed
/// by (`service.name`, operation).
pub struct Aggregator {
    limits: AggLimits,
    services: BTreeMap<String, Service>,
    cells: usize,
    received: u64,
    recorded: u64,
    ignored: u64,
    drops: Drops,
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new(AggLimits::default())
    }
}

/// `service.name` when a resource has none.
pub const UNKNOWN_SERVICE: &str = "unknown_service";

fn attr<'a>(attrs: &'a [Attr], key: &str) -> Option<&'a AttrValue> {
    attrs.iter().find(|a| a.key == key).map(|a| &a.value)
}

fn attr_str<'a>(attrs: &'a [Attr], key: &str) -> Option<&'a str> {
    match attr(attrs, key) {
        Some(AttrValue::Str(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn resource_of(attrs: &[Attr]) -> Resource {
    let s = |k: &str| attr_str(attrs, k).map(str::to_string);
    Resource {
        pid: match attr(attrs, "process.pid") {
            Some(AttrValue::Int(i)) => Some(*i),
            Some(AttrValue::Str(s)) => s.parse().ok(),
            _ => None,
        },
        service_version: s("service.version"),
        vcs_revision: s("vcs.ref.head.revision").or_else(|| s("vcs.repository.ref.revision")),
        deployment_environment: s("deployment.environment.name")
            .or_else(|| s("deployment.environment")),
    }
}

/// A path segment that should not be kept: a known token format, or a long
/// run of letters and digits mixed (an id or a key).
fn secret_segment(seg: &str) -> bool {
    if redact::looks_like_token(seg) {
        return true;
    }
    seg.len() >= 32
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && seg.chars().any(|c| c.is_ascii_digit())
        && seg.chars().any(|c| c.is_ascii_alphabetic())
}

/// An operation name safe to keep: no query string or fragment, secrets
/// redacted, at most `max` bytes.
pub fn sanitize_operation(raw: &str, max: usize) -> String {
    let base = raw.split(['?', '#']).next().unwrap_or("");
    let cleaned = redact::redact_command_line(base);
    let words: Vec<String> = cleaned
        .split(' ')
        .map(|w| {
            w.split('/')
                .map(|seg| {
                    if secret_segment(seg) {
                        redact::REDACTED
                    } else {
                        seg
                    }
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    let mut s = words.join(" ");
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    if s.is_empty() {
        "unnamed".to_string()
    } else {
        s
    }
}

/// The operation a span is counted under: `http.route` or `url.template`
/// (after the HTTP method when there is one), else the span name.
pub fn operation_of(span: &Span, max: usize) -> String {
    let route = attr_str(&span.attributes, "http.route")
        .or_else(|| attr_str(&span.attributes, "url.template"));
    let raw = match route {
        Some(route) => {
            let method = attr_str(&span.attributes, "http.request.method")
                .or_else(|| attr_str(&span.attributes, "http.method"))
                .filter(|m| {
                    m.len() <= 16 && m.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                });
            match method {
                Some(m) => format!("{m} {route}"),
                None => route.to_string(),
            }
        }
        None => span.name.clone(),
    };
    sanitize_operation(&raw, max)
}

/// Whether a span is aggregated: server, consumer, or a root of any kind.
pub fn is_entry_span(span: &Span) -> bool {
    span.kind == KIND_SERVER || span.kind == KIND_CONSUMER || span.is_root
}

/// Longest operation or service name kept.
const MAX_NAME: usize = 256;

impl Aggregator {
    pub fn new(limits: AggLimits) -> Self {
        let mut limits = limits;
        limits.window_ms = limits.window_ms.max(1);
        Self {
            limits,
            services: BTreeMap::new(),
            cells: 0,
            received: 0,
            recorded: 0,
            ignored: 0,
            drops: Drops::default(),
        }
    }

    pub fn limits(&self) -> AggLimits {
        self.limits
    }

    /// Record a decoded batch received at `now_ms`. Spans are placed in the
    /// window of their end time, clamped to `now_ms` plus the allowed skew.
    pub fn ingest(&mut self, batch: &TraceBatch, now_ms: u64) {
        self.drops.spans_over_request_cap += batch.dropped.spans;
        self.drops.attributes += batch.dropped.attributes;
        self.drops.truncated_strings += batch.dropped.truncated_strings;
        self.prune(now_ms);
        for rs in &batch.resource_spans {
            let service = attr_str(&rs.resource, "service.name")
                .map(|s| sanitize_operation(s, MAX_NAME))
                .unwrap_or_else(|| UNKNOWN_SERVICE.to_string());
            let resource = Arc::new(resource_of(&rs.resource));
            for span in &rs.spans {
                self.received += 1;
                self.record(&service, &resource, span, now_ms);
            }
        }
    }

    fn record(&mut self, service: &str, resource: &Arc<Resource>, span: &Span, now_ms: u64) {
        if !is_entry_span(span) {
            self.ignored += 1;
            return;
        }
        if span.end_unix_nano == 0 || span.end_unix_nano < span.start_unix_nano {
            self.drops.invalid += 1;
            return;
        }
        let dur_us = (span.end_unix_nano - span.start_unix_nano) / 1000;
        let mut end_ms = span.end_unix_nano / 1_000_000;
        if end_ms > now_ms.saturating_add(self.limits.max_future_skew_ms) {
            end_ms = now_ms;
        }
        if end_ms < self.oldest_kept(now_ms) {
            self.drops.too_old += 1;
            return;
        }
        let window = end_ms - end_ms % self.limits.window_ms;
        let op_name = operation_of(span, MAX_NAME);

        let services_full = self.services.len() >= self.limits.max_services;
        let max_ops = self.limits.max_operations_per_service;
        let cells_full = self.cells >= self.limits.max_cells;
        if !self.services.contains_key(service) {
            if services_full {
                self.drops.services += 1;
                return;
            }
            if cells_full {
                self.drops.cells += 1;
                return;
            }
            self.services.insert(
                service.to_string(),
                Service {
                    first_seen_ms: end_ms,
                    last_seen_ms: end_ms,
                    resource: resource.clone(),
                    ops: BTreeMap::new(),
                },
            );
        }
        let Some(svc) = self.services.get_mut(service) else {
            return;
        };
        if !svc.ops.contains_key(&op_name) {
            if svc.ops.len() >= max_ops {
                self.drops.operations += 1;
                self.remove_if_empty(service);
                return;
            }
            if cells_full {
                self.drops.cells += 1;
                self.remove_if_empty(service);
                return;
            }
            svc.ops.insert(
                op_name.clone(),
                Operation {
                    first_seen_ms: end_ms,
                    last_seen_ms: end_ms,
                    cells: Vec::new(),
                },
            );
        }
        let Some(op) = svc.ops.get_mut(&op_name) else {
            return;
        };
        let idx = match op.cells.binary_search_by_key(&window, |c| c.start_ms) {
            Ok(i) => i,
            Err(i) => {
                if cells_full {
                    self.drops.cells += 1;
                    return;
                }
                op.cells.insert(i, Cell::new(window, resource.clone()));
                self.cells += 1;
                i
            }
        };
        let cell = &mut op.cells[idx];
        cell.count += 1;
        if span.status_code == STATUS_ERROR {
            cell.errors += 1;
        }
        cell.sum_us = cell.sum_us.saturating_add(dur_us);
        cell.max_us = cell.max_us.max(dur_us);
        let b = &mut cell.hist[bucket_of(dur_us)];
        *b = b.saturating_add(1);
        if !Arc::ptr_eq(&cell.resource, resource) && *cell.resource != **resource {
            cell.resource = resource.clone();
        }
        op.first_seen_ms = op.first_seen_ms.min(end_ms);
        op.last_seen_ms = op.last_seen_ms.max(end_ms);
        svc.first_seen_ms = svc.first_seen_ms.min(end_ms);
        if end_ms >= svc.last_seen_ms {
            svc.last_seen_ms = end_ms;
            if *svc.resource != **resource {
                svc.resource = resource.clone();
            }
        }
        self.recorded += 1;
    }

    /// Drop a service that a refused span left with no operations.
    fn remove_if_empty(&mut self, service: &str) {
        if self.services.get(service).is_some_and(|s| s.ops.is_empty()) {
            self.services.remove(service);
        }
    }

    /// The start of the oldest window still kept at `now_ms`.
    fn oldest_kept(&self, now_ms: u64) -> u64 {
        let w = self.limits.window_ms;
        let from = now_ms.saturating_sub(self.limits.retention_ms);
        from - from % w
    }

    /// Roll off windows past the retention period, then operations and
    /// services left with none.
    pub fn prune(&mut self, now_ms: u64) {
        let oldest = self.oldest_kept(now_ms);
        let mut removed = 0usize;
        self.services.retain(|_, svc| {
            svc.ops.retain(|_, op| {
                let before = op.cells.len();
                op.cells.retain(|c| c.start_ms >= oldest);
                removed += before - op.cells.len();
                !op.cells.is_empty()
            });
            !svc.ops.is_empty()
        });
        self.cells -= removed;
    }

    /// Every (service, operation) with spans in windows starting in
    /// `[from_ms, to_ms)`, slowest p95 first.
    pub fn operations(&self, from_ms: u64, to_ms: u64) -> Vec<OperationSummary> {
        let mut out = Vec::new();
        for (sname, svc) in &self.services {
            for (oname, op) in &svc.ops {
                let mut m = Merge::default();
                let mut resource = None;
                for c in op
                    .cells
                    .iter()
                    .filter(|c| c.start_ms >= from_ms && c.start_ms < to_ms)
                {
                    m.add(c);
                    resource = Some(c.resource.clone());
                }
                let Some(resource) = resource else { continue };
                out.push(OperationSummary {
                    service: sname.clone(),
                    operation: oname.clone(),
                    stats: m.stats(),
                    first_seen_ms: op.first_seen_ms,
                    last_seen_ms: op.last_seen_ms,
                    resource: (*resource).clone(),
                });
            }
        }
        out.sort_by(|a, b| b.stats.p95_ms.total_cmp(&a.stats.p95_ms));
        out
    }

    /// The windows of one (service, operation), oldest first.
    pub fn windows(&self, service: &str, operation: &str) -> Vec<WindowSummary> {
        let Some(op) = self
            .services
            .get(service)
            .and_then(|s| s.ops.get(operation))
        else {
            return Vec::new();
        };
        op.cells
            .iter()
            .map(|c| {
                let mut m = Merge::default();
                m.add(c);
                WindowSummary {
                    start_ms: c.start_ms,
                    end_ms: c.start_ms + self.limits.window_ms,
                    stats: m.stats(),
                    resource: (*c.resource).clone(),
                }
            })
            .collect()
    }

    pub fn services(&self) -> Vec<ServiceSummary> {
        self.services
            .iter()
            .map(|(name, s)| ServiceSummary {
                name: name.clone(),
                operations: s.ops.len(),
                first_seen_ms: s.first_seen_ms,
                last_seen_ms: s.last_seen_ms,
                resource: (*s.resource).clone(),
            })
            .collect()
    }

    pub fn drops(&self) -> Drops {
        self.drops
    }

    pub fn status(&self) -> AggStatus {
        AggStatus {
            services: self.services.len(),
            operations: self.services.values().map(|s| s.ops.len()).sum(),
            cells: self.cells,
            received_spans: self.received,
            recorded_spans: self.recorded,
            ignored_spans: self.ignored,
            drops: self.drops,
            limits: self.limits,
        }
    }
}

#[cfg(test)]
#[path = "otel_tests.rs"]
pub(crate) mod tests;
