//! rpt-cs — C ABI shim over [rpt-rs]: Crystal Reports `.rpt` bytes + a JSON request → PDF bytes.
//!
//! The whole engine (decode, formula VM, data pipeline, layout, typography, PDF writer) is
//! rpt-rs; this crate is the thin, stateless boundary for the `RptCs` .NET library:
//!
//! - [`cre_version`] — the shim's version string.
//! - [`cre_inspect`] — report metadata as JSON (parameters, subreports, saved data, tables).
//! - [`cre_render`] — render to PDF bytes + a diagnostics JSON document.
//! - [`cre_result_free`] — free a result returned by any of the above.
//!
//! Every call is stateless: bytes in, one heap-allocated [`CreResult`] out, freed by the caller.
//! The FFI never unwinds (panics are caught and reported as code `INTERNAL_ERROR`).
//!
//! [rpt-rs]: https://github.com/MrSrsen/rpt-rs

use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Once;

use rpt_data::{normalize_param_name, Column, Row, RowData, RowSource, ScopeData};
use rpt_formula::eval::{Date, Time, Value};
use rpt_model::{FieldValueType, ParameterValueKind, Report};
use rpt_reader::Rpt;
use rpt_render::{
    render_with, semantics_of, try_render_document, Conformance, DateTimeSpecials, FontSource,
    Locale, PdfOptions, Parameters, RenderOptions, RenderSource,
};
use serde_json::{json, Map, Value as Json};

const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 1;
const DECODE_ERROR: i32 = 2;
const PDF_ERROR: i32 = 3;
const INTERNAL_ERROR: i32 = 4;

/// One call's outcome, handed across the boundary. Buffers are heap allocations freed by
/// [`cre_result_free`]; `pdf` is the PDF bytes (render success only) and `json` is the request's
/// companion document — diagnostics on success, `{"error": …}` on failure, inspect metadata.
#[repr(C)]
pub struct CreResult {
    /// 0 = ok; 1 = invalid argument; 2 = decode error; 3 = PDF error; 4 = internal error.
    pub code: i32,
    pub pdf: *mut u8,
    pub pdf_len: usize,
    pub json: *mut u8,
    pub json_len: usize,
}

fn leak_buf(v: Vec<u8>) -> (*mut u8, usize) {
    let boxed = v.into_boxed_slice();
    let len = boxed.len();
    let ptr = boxed.as_ptr() as *mut u8;
    std::mem::forget(boxed);
    (ptr, len)
}

unsafe fn free_buf(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        // SAFETY: the buffer was created by `leak_buf` from a boxed slice of exactly `len` bytes,
        // and is freed once, by the same module that created it.
        unsafe {
            drop(Vec::from_raw_parts(ptr, len, len));
        }
    }
}

fn make_result(code: i32, pdf: Option<Vec<u8>>, json: Option<Vec<u8>>) -> *mut CreResult {
    let (pdf, pdf_len) = pdf.map_or((std::ptr::null_mut(), 0), leak_buf);
    let (json, json_len) = json.map_or((std::ptr::null_mut(), 0), leak_buf);
    Box::into_raw(Box::new(CreResult {
        code,
        pdf,
        pdf_len,
        json,
        json_len,
    }))
}

/// Install a no-op panic hook once, so a bug inside the engine reports through the result JSON
/// instead of writing to the host process's stderr.
fn silence_panics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::panic::set_hook(Box::new(|_| {})));
}

fn slice<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the caller passes a live pointer+len per the contract documented on each export.
        Some(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
}

// ---------------------------------------------------------------------------
// Request model
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Request {
    /// Report parameter values, keyed by parameter name (braces/`?` tolerated, case-insensitive).
    params: Map<String, Json>,
    /// BCP-47-ish render locale (`it-IT`, `de-DE`, …). Unknown tags fall back to `en-US` with a
    /// diagnostic.
    locale: Option<String>,
    /// `"bundled"` (default, byte-reproducible) or `"system"` (host's installed faces).
    fonts: Option<String>,
    /// Conformance claim: `none` (default), `pdfa1b`, `pdfa2b`, `pdfa3b`, `pdfa1a`, `pdfa2a`,
    /// `pdfa3a`, `pdfua1`. Checked claims: an unmet requirement fails the render, naming it.
    conformance: Option<String>,
    /// Document title override (needed by PDF/UA-1 when the report has none).
    title: Option<String>,
    /// Document natural language (needed by level-A / PDF/UA conformance).
    language: Option<String>,
    /// Alternate text per picture/chart object name (needed by level-A / PDF/UA conformance).
    alt: std::collections::BTreeMap<String, String>,
    /// Unix-seconds instant resolving `CurrentDate`/`Today`/… — frozen for reproducible renders.
    #[serde(rename = "asOf")]
    as_of: Option<i64>,
    /// Rows for the main report (live feed instead of the file's saved data).
    dataset: Option<Dataset>,
    /// Rows per subreport, keyed by subreport name (or `parent.child` path for nested ones).
    subreports: std::collections::BTreeMap<String, Dataset>,
}

#[derive(Debug, serde::Deserialize)]
struct Dataset {
    columns: Vec<DatasetColumn>,
    /// Rows as positional arrays, aligned with `columns`.
    rows: Vec<Vec<Json>>,
}

#[derive(Debug, serde::Deserialize)]
struct DatasetColumn {
    name: String,
    /// Column value type: `string`, `number`, `int`, `currency`, `boolean`, `date`, `time`,
    /// `datetime`, `blob` (base64). Default: inferred as `string`.
    #[serde(rename = "type")]
    kind: Option<String>,
}

// ---------------------------------------------------------------------------
// JSON → engine value mapping
// ---------------------------------------------------------------------------

fn field_value_type(kind: &str) -> Option<FieldValueType> {
    match kind.to_ascii_lowercase().as_str() {
        "string" | "memo" => Some(FieldValueType::String),
        "number" => Some(FieldValueType::Number),
        "int" => Some(FieldValueType::Int32s),
        "currency" => Some(FieldValueType::Currency),
        "boolean" | "bool" => Some(FieldValueType::Boolean),
        "date" => Some(FieldValueType::Date),
        "time" => Some(FieldValueType::Time),
        "datetime" => Some(FieldValueType::DateTime),
        "blob" | "bytes" => Some(FieldValueType::Blob),
        _ => None,
    }
}

fn param_kind_name(kind: ParameterValueKind) -> &'static str {
    match kind {
        ParameterValueKind::StringParameter => "string",
        ParameterValueKind::NumberParameter => "number",
        ParameterValueKind::CurrencyParameter => "currency",
        ParameterValueKind::BooleanParameter => "boolean",
        ParameterValueKind::DateParameter => "date",
        ParameterValueKind::TimeParameter => "time",
        ParameterValueKind::DateTimeParameter => "datetime",
        _ => "string",
    }
}

fn parse_iso_date(s: &str) -> Option<Date> {
    let s = s.trim();
    let mut it = s.splitn(3, '-');
    let y: i32 = it.next()?.trim().parse().ok()?;
    let m: u8 = it.next()?.trim().parse().ok()?;
    let d: u8 = it.next()?.trim().parse().ok()?;
    Some(Date::new(y, m, d))
}

fn parse_iso_time(s: &str) -> Option<Time> {
    let s = s.trim();
    let base = s.split('.').next().unwrap_or(s);
    let mut it = base.split(':');
    let h: u8 = it.next()?.trim().parse().ok()?;
    let m: u8 = it.next()?.trim().parse().ok()?;
    let sec: u8 = it.next().map(|v| v.trim().parse().unwrap_or(0)).unwrap_or(0);
    Some(Time::new(h, m, sec))
}

fn parse_iso_datetime(s: &str) -> Option<(Date, Time)> {
    let s = s.trim();
    let (d, t) = s.split_once('T').or_else(|| s.split_once(' '))?;
    Some((parse_iso_date(d)?, parse_iso_time(t)?))
}

fn json_number(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn json_scalar_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Convert one dataset cell to the engine value its column declares. Tolerant on scalars (a number
/// formatted as text still lands as text), strict on structured values.
fn cell_value(kind: FieldValueType, cell: &Json) -> Result<Value, String> {
    if cell.is_null() {
        return Ok(Value::Null);
    }
    match kind {
        FieldValueType::String | FieldValueType::PersistentMemo | FieldValueType::Unknown
        | FieldValueType::Other(_) => {
            Ok(Value::Str(json_scalar_text(cell)))
        }
        FieldValueType::Number | FieldValueType::Int8s | FieldValueType::Int16s
        | FieldValueType::Int32s | FieldValueType::Int32u => {
            json_number(cell).map(Value::Number).ok_or_else(|| format!("not a number: {cell}"))
        }
        FieldValueType::Currency => {
            json_number(cell).map(Value::Currency).ok_or_else(|| format!("not a number: {cell}"))
        }
        FieldValueType::Boolean => match cell {
            Json::Bool(b) => Ok(Value::Bool(*b)),
            Json::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                _ => Err(format!("not a boolean: {cell}")),
            },
            _ => Err(format!("not a boolean: {cell}")),
        },
        FieldValueType::Date => {
            let s = json_scalar_text(cell);
            parse_iso_date(&s).map(Value::Date).ok_or_else(|| format!("not an ISO date: {s}"))
        }
        FieldValueType::Time => {
            let s = json_scalar_text(cell);
            parse_iso_time(&s).map(Value::Time).ok_or_else(|| format!("not an ISO time: {s}"))
        }
        FieldValueType::DateTime => {
            let s = json_scalar_text(cell);
            parse_iso_datetime(&s)
                .map(|(d, t)| Value::DateTime(d, t))
                .ok_or_else(|| format!("not an ISO datetime: {s}"))
        }
        FieldValueType::Blob => match cell {
            Json::String(s) => base64_decode(s).map(Value::Bytes).ok_or("not valid base64".into()),
            _ => Err(format!("blob cells must be base64 strings: {cell}")),
        },
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for &c in chunk {
            acc = (acc << 6) | val(c)?;
        }
        let shift = (4 - chunk.len()) * 6;
        let acc = acc << shift;
        out.push((acc >> 16) as u8);
        if chunk.len() >= 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() >= 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

fn build_dataset(d: &Dataset) -> Result<RowData, String> {
    if d.columns.len() > u16::MAX as usize {
        return Err("too many columns".into());
    }
    let mut columns = Vec::with_capacity(d.columns.len());
    for c in &d.columns {
        let kind = match &c.kind {
            Some(k) => field_value_type(k).ok_or_else(|| format!("unknown column type `{k}` for column `{}`", c.name))?,
            None => FieldValueType::String,
        };
        columns.push(Column {
            name: c.name.clone(),
            value_type: kind,
        });
    }
    let mut rows = Vec::with_capacity(d.rows.len());
    for (i, r) in d.rows.iter().enumerate() {
        if r.len() != columns.len() {
            return Err(format!(
                "row {} has {} cells but {} columns were declared",
                i,
                r.len(),
                columns.len()
            ));
        }
        let mut row = Row::default();
        for (cell, col) in r.iter().zip(&columns) {
            let value = cell_value(col.value_type, cell)
                .map_err(|e| format!("column `{}`, row {i}: {e}", col.name))?;
            row.insert(&col.name, value);
        }
        rows.push(row);
    }
    Ok(RowData::new(columns, rows))
}

/// Coerce a JSON parameter value to the engine value its declared kind asks for.
fn coerce_param(kind: Option<ParameterValueKind>, v: &Json) -> Result<Value, String> {
    if let Json::Array(items) = v {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(coerce_param(kind, item)?);
        }
        return Ok(Value::Array(out));
    }
    if v.is_null() {
        return Ok(Value::Null);
    }
    match kind {
        Some(ParameterValueKind::NumberParameter) => {
            json_number(v).map(Value::Number).ok_or_else(|| format!("not a number: {v}"))
        }
        Some(ParameterValueKind::CurrencyParameter) => {
            json_number(v).map(Value::Currency).ok_or_else(|| format!("not a number: {v}"))
        }
        Some(ParameterValueKind::BooleanParameter) => match v {
            Json::Bool(b) => Ok(Value::Bool(*b)),
            _ => Err(format!("not a boolean: {v}")),
        },
        Some(ParameterValueKind::DateParameter) => {
            let s = json_scalar_text(v);
            parse_iso_date(&s).map(Value::Date).ok_or_else(|| format!("not an ISO date: {s}"))
        }
        Some(ParameterValueKind::TimeParameter) => {
            let s = json_scalar_text(v);
            parse_iso_time(&s).map(Value::Time).ok_or_else(|| format!("not an ISO time: {s}"))
        }
        Some(ParameterValueKind::DateTimeParameter) => {
            let s = json_scalar_text(v);
            parse_iso_datetime(&s)
                .map(|(d, t)| Value::DateTime(d, t))
                .ok_or_else(|| format!("not an ISO datetime: {s}"))
        }
        _ => Ok(Value::Str(json_scalar_text(v))),
    }
}

// ---------------------------------------------------------------------------
// Subreport scope provider
// ---------------------------------------------------------------------------

/// Live rows per subreport, keyed by pointer identity: `rpt-layout` hands each subreport's own
/// `&Report` to [`ScopeData::rows_for`], and every subreport's `Report` lives inside the main
/// report's `subreports` tree, so matching the borrowed pointer identifies the scope exactly.
struct JsonScopes {
    /// (full path name, simple name, subreport pointer, rows) for every subreport in the tree.
    scopes: Vec<(String, String, *const Report, RowData)>,
}

impl ScopeData for JsonScopes {
    fn rows_for(&self, report: &Report) -> Option<Box<dyn RowSource>> {
        let ptr = report as *const Report;
        self.scopes
            .iter()
            .find(|(_, _, p, _)| std::ptr::eq(*p, ptr))
            .map(|(_, _, _, data)| Box::new(data.clone()) as Box<dyn RowSource>)
    }
}

fn collect_subreport_pointers(report: &Report, prefix: &str, out: &mut Vec<(String, String, *const Report)>) {
    for sub in &report.subreports {
        let full = if prefix.is_empty() {
            sub.name.clone()
        } else {
            format!("{prefix}.{}", sub.name)
        };
        out.push((full.clone(), sub.name.clone(), sub.report.as_ref() as *const Report));
        collect_subreport_pointers(sub.report.as_ref(), &full, out);
    }
}

// ---------------------------------------------------------------------------
// Plain (non-FFI) API — what the FFI exports wrap and the tests drive directly
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ShimError {
    /// The request JSON is malformed or names something unknown (code 1).
    InvalidArgument(String),
    /// The .rpt bytes did not decode (code 2).
    Decode(String),
    /// The PDF backend refused the render — conformance claims name every unmet requirement (code 3).
    Pdf(String),
}

impl ShimError {
    fn code(&self) -> i32 {
        match self {
            ShimError::InvalidArgument(_) => INVALID_ARGUMENT,
            ShimError::Decode(_) => DECODE_ERROR,
            ShimError::Pdf(_) => PDF_ERROR,
        }
    }
    fn to_json(&self) -> Vec<u8> {
        let msg = match self {
            ShimError::InvalidArgument(m) | ShimError::Decode(m) | ShimError::Pdf(m) => m,
        };
        serde_json::to_vec(&json!({ "error": msg })).unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec())
    }
}

/// Report metadata for the caller: which parameters to prompt for, which subreports need rows,
/// whether the file carries usable saved data.
fn inspect_json(report: &Report) -> Json {
    let parameters: Vec<Json> = report
        .data_definition
        .parameter_fields()
        .map(|(def, p)| {
            json!({
                "name": def.name,
                "type": param_kind_name(p.value_kind),
                "allowMultiple": p.allow_multiple_values,
                "optional": p.optional_prompt,
            })
        })
        .collect();
    let mut pointers = Vec::new();
    collect_subreport_pointers(report, "", &mut pointers);
    let subreports = pointers.iter().map(|(full, _, _)| full.clone()).collect::<Vec<_>>();
    let tables: Vec<Json> = report
        .database
        .tables
        .iter()
        .map(|t| json!({ "name": t.name, "alias": t.alias }))
        .collect();
    json!({
        "title": report.summary_info.title,
        "author": report.summary_info.author,
        "savedData": report.saved_data.is_some(),
        "parameters": parameters,
        "subreports": subreports,
        "tables": tables,
    })
}

#[derive(Debug)]
pub struct RenderOutcome {
    pub pdf: Vec<u8>,
    pub result_json: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Result<Rpt, ShimError> {
    Rpt::read(bytes).map_err(|e| ShimError::Decode(e.to_string()))
}

/// Inspect a decoded report. Returns the inspect JSON document.
pub fn inspect(bytes: &[u8]) -> Result<Vec<u8>, ShimError> {
    let rpt = decode(bytes)?;
    serde_json::to_vec(&inspect_json(rpt.report())).map_err(|e| ShimError::Decode(e.to_string()))
}

/// Render a report to PDF bytes + a result JSON (`{pages, diagnostics}`).
pub fn render(bytes: &[u8], request: &[u8]) -> Result<RenderOutcome, ShimError> {
    let req: Request = serde_json::from_slice(request)
        .map_err(|e| ShimError::InvalidArgument(format!("invalid request JSON: {e}")))?;
    let rpt = decode(bytes)?;
    let report = rpt.report();

    // Datasets: main + subreports, with honest diagnostics for every request key that matches
    // nothing in the report's subreport tree.
    let mut notes: Vec<String> = Vec::new();
    let main = match &req.dataset {
        Some(d) => Some(build_dataset(d).map_err(ShimError::InvalidArgument)?),
        None => None,
    };
    let mut sub_datasets: Vec<(String, RowData)> = Vec::new();
    for (name, d) in &req.subreports {
        let data = build_dataset(d).map_err(ShimError::InvalidArgument)?;
        sub_datasets.push((name.clone(), data));
    }
    let mut pointers = Vec::new();
    collect_subreport_pointers(report, "", &mut pointers);
    let mut scopes: Vec<(String, String, *const Report, RowData)> = Vec::new();
    for (name, data) in sub_datasets {
        match pointers
            .iter()
            .find(|(full, simple, _)| full == &name || simple == &name)
        {
            Some((full, simple, ptr)) => {
                scopes.push((full.clone(), simple.clone(), *ptr, data));
            }
            None => notes.push(format!(
                "subreport data for `{name}` ignored: no such subreport (known: {:?})",
                pointers.iter().map(|(f, _, _)| f).collect::<Vec<_>>()
            )),
        }
    }
    let scope_provider = JsonScopes { scopes };

    // Parameters, coerced to each parameter's declared kind; unknown names are surfaced, not swallowed.
    let declared: std::collections::HashMap<String, ParameterValueKind> = report
        .data_definition
        .parameter_fields()
        .map(|(def, p)| (normalize_param_name(&def.name), p.value_kind))
        .collect();
    let mut params: Parameters = Parameters::new();
    for (name, v) in &req.params {
        let key = normalize_param_name(name);
        match declared.get(&key) {
            Some(kind) => {
                let value = coerce_param(Some(*kind), v).map_err(ShimError::InvalidArgument)?;
                params.insert(key, value);
            }
            None => {
                notes.push(format!("parameter `{name}` ignored: not declared by the report"));
                let value = coerce_param(None, v).map_err(ShimError::InvalidArgument)?;
                params.insert(key, value);
            }
        }
    }

    // Locale: unknown tags fall back to en-US with a note.
    let locale = match &req.locale {
        Some(tag) => match Locale::lookup(tag) {
            Some(l) => l,
            None => {
                notes.push(format!("locale `{tag}` unknown; fell back to en-US"));
                Locale::default()
            }
        },
        None => Locale::default(),
    };
    let fonts = match req.fonts.as_deref() {
        None | Some("bundled") => FontSource::Bundled,
        Some("system") => FontSource::System,
        Some(other) => {
            return Err(ShimError::InvalidArgument(format!(
                "unknown fonts mode `{other}` (expected `bundled` or `system`)"
            )))
        }
    };
    let as_of = req.as_of.map(DateTimeSpecials::from_unix_seconds);

    let options = match main.as_ref() {
        Some(data) => RenderOptions {
            datasource: RenderSource::Rows(data),
            params,
            locale,
            scope: Some(&scope_provider),
            as_of,
            fonts,
        },
        None => RenderOptions {
            datasource: RenderSource::Saved,
            params,
            locale,
            scope: Some(&scope_provider),
            as_of,
            fonts,
        },
    };

    let doc = render_with(report, options);

    // Conformance + semantics: the report states what it can (title from summary info, tooltips as
    // alt text); the request overrides or completes it.
    let conformance = match req.conformance.as_deref() {
        None | Some("none") => Conformance::None,
        Some(c) => match c {
            "pdfa1b" => Conformance::PdfA1b,
            "pdfa2b" => Conformance::PdfA2b,
            "pdfa3b" => Conformance::PdfA3b,
            "pdfa1a" => Conformance::PdfA1a,
            "pdfa2a" => Conformance::PdfA2a,
            "pdfa3a" => Conformance::PdfA3a,
            "pdfua1" => Conformance::PdfUa1,
            other => {
                return Err(ShimError::InvalidArgument(format!(
                    "unknown conformance `{other}` (expected none, pdfa1b|2b|3b|1a|2a|3a, or pdfua1)"
                )))
            }
        },
    };
    let mut semantics = semantics_of(report);
    if let Some(t) = &req.title {
        semantics.title = Some(t.clone());
    }
    semantics.language = req.language.clone();
    for (object, text) in &req.alt {
        semantics.alt_text.insert(object.clone(), text.clone());
    }

    let pdf_options = PdfOptions {
        fonts,
        conformance,
        semantics,
        ..PdfOptions::default()
    };
    let pdf = try_render_document(&doc, &pdf_options).map_err(|e| ShimError::Pdf(e.to_string()))?;

    let mut diagnostics = json!(doc.diagnostics);
    if let Some(arr) = diagnostics.as_array_mut() {
        for note in &notes {
            arr.push(json!({
                "severity": "warning",
                "kind": "Other",
                "message": note,
            }));
        }
    }
    let result_json = serde_json::to_vec(&json!({
        "pages": doc.pages.len(),
        "diagnostics": diagnostics,
    }))
    .map_err(|e| ShimError::Pdf(e.to_string()))?;

    Ok(RenderOutcome { pdf, result_json })
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

fn run_ffi(f: impl FnOnce() -> *mut CreResult) -> *mut CreResult {
    silence_panics();
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => make_result(
            INTERNAL_ERROR,
            None,
            Some(error_json("internal error: the render panicked")),
        ),
    }
}

fn error_json(message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "error": message }))
        .unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec())
}

/// The shim's version, as a static NUL-terminated string (never freed).
///
/// # Safety
/// Always safe: returns a pointer to a static string.
#[unsafe(no_mangle)]
pub extern "C" fn cre_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), " (rpt-rs ", env!("RPT_RS_REV"), ")\0").as_ptr() as *const c_char
}

/// Inspect `.rpt` bytes. Returns a [`CreResult`] whose `json` holds the metadata document.
///
/// # Safety
/// `bytes` must point to `len` readable bytes, or be null with `len == 0`. The returned result is
/// freed with [`cre_result_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_inspect(bytes: *const u8, len: usize) -> *mut CreResult {
    run_ffi(|| {
        let Some(bytes) = slice(bytes, len) else {
            return make_result(INVALID_ARGUMENT, None, Some(error_json("null report bytes")));
        };
        match inspect(bytes) {
            Ok(json) => make_result(OK, None, Some(json)),
            Err(e) => make_result(e.code(), None, Some(e.to_json())),
        }
    })
}

/// Render `.rpt` bytes with a JSON request. Returns a [`CreResult`] whose `pdf` holds the PDF bytes
/// and whose `json` holds `{pages, diagnostics}` on success or `{"error"}` on failure.
///
/// # Safety
/// `bytes`/`request` must point to `len`/`req_len` readable bytes (null + 0 allowed for `request`,
/// meaning an empty request). The returned result is freed with [`cre_result_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_render(
    bytes: *const u8,
    len: usize,
    request: *const u8,
    req_len: usize,
) -> *mut CreResult {
    run_ffi(|| {
        let Some(bytes) = slice(bytes, len) else {
            return make_result(INVALID_ARGUMENT, None, Some(error_json("null report bytes")));
        };
        let request = slice(request, req_len).unwrap_or(&[]);
        match render(bytes, request) {
            Ok(outcome) => make_result(OK, Some(outcome.pdf), Some(outcome.result_json)),
            Err(e) => make_result(e.code(), None, Some(e.to_json())),
        }
    })
}

/// Free a result previously returned by [`cre_inspect`], [`cre_render`], or the error path.
///
/// # Safety
/// `result` must be null or a pointer handed out by one of this library's exports, not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_result_free(result: *mut CreResult) {
    if result.is_null() {
        return;
    }
    // SAFETY: per the contract, the caller passes back an unmodified pointer we handed out.
    unsafe {
        let r = Box::from_raw(result);
        free_buf(r.pdf, r.pdf_len);
        free_buf(r.json, r.json_len);
    }
}