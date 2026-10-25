//! rpt-cs — C ABI shim over [rpt-rs]: Crystal Reports `.rpt` bytes + a JSON request → PDF bytes.
//!
//! The whole engine (decode, formula VM, data pipeline, layout, typography, PDF writer) is
//! rpt-rs; this crate is the thin, stateless boundary for the `RptCs` .NET library:
//!
//! - [`cre_version`] — the shim's version string.
//! - [`cre_inspect`] — report metadata as JSON (parameters, fields, subreports, saved data, tables).
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
use rpt_model::{
    area_objects, DbFieldDef, FieldRefKind, FieldValueType, ParameterValueKind, Report,
    ReportObjectKind, Table, TableJoinKind, TableLinkOperator,
};
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
    dataset: Option<DatasetSpec>,
    /// Rows per subreport, keyed by subreport name (or `parent.child` path for nested ones).
    subreports: std::collections::BTreeMap<String, DatasetSpec>,
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

/// The live rows for one report scope, in either form:
///
/// - flat: one table's `columns` + positional `rows` — the original single-table form;
/// - named: `{"tables": {name: Dataset}}` — the push model (SDK `SetDataSource(DataSet)`): each
///   report table binds to a named dataset table (by the report's table name or alias,
///   case-insensitive) and the engine joins them along the report's own stored links. Report
///   tables without a match resolve to Null and are reported, never swallowed.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum DatasetSpec {
    Named(NamedTables),
    Flat(Dataset),
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedTables {
    tables: std::collections::BTreeMap<String, Dataset>,
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

// ---------------------------------------------------------------------------
// Named-table datasets (push model): bind the caller's tables to the report's
// tables and join them along the report's own stored links.
// ---------------------------------------------------------------------------

/// One report table's materialized rows: each row maps lowercase `alias.field` and bare `field`
/// keys to values, mirroring how the engine's `Row` resolves fields.
type TableRows = Vec<std::collections::BTreeMap<String, Value>>;

/// Match a named dataset table to a report table by the report's table name or alias,
/// case-insensitively (the SDK's `SetDataSource(DataSet)` binding rule).
fn matching_table_key<'a>(
    tables: &'a std::collections::BTreeMap<String, Dataset>,
    table: &Table,
) -> Option<&'a String> {
    tables
        .keys()
        .find(|k| k.eq_ignore_ascii_case(&table.name) || k.eq_ignore_ascii_case(&table.alias))
}

/// List up to eight names, then a count — diagnostics stay readable on wide tables.
fn summarize_names(names: &[&str]) -> String {
    const MAX: usize = 8;
    if names.len() <= MAX {
        return names.join(", ");
    }
    format!("{}, … (+{} more)", names[..MAX].join(", "), names.len() - MAX)
}

/// Parse one named-table cell against its field's declared type, tolerating the temporal
/// coercion a typed DataSet forces: a `DateTime` column on a `Date`-declared field (and the
/// reverse) lands as the declared kind instead of failing the bind.
fn cell_value_lenient(kind: FieldValueType, cell: &Json) -> Result<Value, String> {
    match cell_value(kind, cell) {
        Ok(value) => Ok(value),
        Err(_) => {
            let text = json_scalar_text(cell);
            match kind {
                FieldValueType::Date => parse_iso_datetime(&text)
                    .map(|(d, _)| Value::Date(d))
                    .ok_or_else(|| format!("not an ISO date or datetime: {text}")),
                FieldValueType::Time => parse_iso_datetime(&text)
                    .map(|(_, t)| Value::Time(t))
                    .ok_or_else(|| format!("not an ISO time or datetime: {text}")),
                FieldValueType::DateTime => parse_iso_date(&text)
                    .map(|d| Value::DateTime(d, Time::new(0, 0, 0)))
                    .ok_or_else(|| format!("not an ISO date, time or datetime: {text}")),
                _ => Err(format!("value does not match the declared type: {text}")),
            }
        }
    }
}

/// Materialize one report table's rows from its matched named dataset. Columns bind against the
/// table's declared fields (by name, then provider short name) and parse with the field's
/// declared type; missing fields become Null and are reported; surplus dataset columns are
/// counted, never silently swallowed.
fn table_rows(table: &Table, data: &Dataset, notes: &mut Vec<String>) -> Result<TableRows, String> {
    let mut bound: Vec<(&DbFieldDef, usize)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for field in &table.data_fields {
        match data
            .columns
            .iter()
            .position(|c| {
                c.name.eq_ignore_ascii_case(&field.name)
                    || field.short_name.as_deref().is_some_and(|s| c.name.eq_ignore_ascii_case(s))
            }) {
            Some(index) => bound.push((field, index)),
            None => missing.push(field.name.as_str()),
        }
    }
    if !missing.is_empty() {
        notes.push(format!(
            "table `{}`: field(s) not provided by the dataset, resolved to Null: {}",
            table.alias,
            summarize_names(&missing),
        ));
    }
    let extra = data.columns.len() - bound.len();
    if extra > 0 {
        notes.push(format!(
            "table `{}`: {extra} dataset column(s) matched no report field and were ignored",
            table.alias,
        ));
    }
    if bound.is_empty() {
        notes.push(format!("table `{}` declares no bindable fields", table.alias));
        return Ok(Vec::new());
    }
    let mut rows = Vec::with_capacity(data.rows.len());
    for (i, cells) in data.rows.iter().enumerate() {
        if cells.len() != data.columns.len() {
            return Err(format!(
                "table `{}`: row {} has {} cells but {} columns were declared",
                table.alias,
                i,
                cells.len(),
                data.columns.len()
            ));
        }
        let mut map = std::collections::BTreeMap::new();
        for (field, index) in &bound {
            let value = cell_value_lenient(field.value_type, &cells[*index]).map_err(|e| {
                format!("table `{}`, field `{}`, row {i}: {e}", table.alias, field.name)
            })?;
            let qualified = format!("{}.{}", table.alias.to_lowercase(), field.name.to_lowercase());
            map.insert(qualified, value.clone());
            map.insert(field.name.to_lowercase(), value);
        }
        rows.push(map);
    }
    Ok(rows)
}

/// Canonical join key for one value; `None` means the value can never match (absent or Null).
fn join_key(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null => return None,
        Value::Str(s) => format!("s{s}"),
        Value::Number(n) | Value::Currency(n) => format!("n{n:?}"),
        Value::Bool(b) => format!("b{b:?}"),
        Value::Date(d) => format!("d{d:?}"),
        Value::Time(t) => format!("t{t:?}"),
        Value::DateTime(d, t) => format!("dt{d:?}T{t:?}"),
        other => format!("{other:?}"),
    })
}

/// Resolve one row's join-predicate key: every paired field must resolve and be non-Null, else
/// the row cannot match. Mirrors `Row::get`: qualified `alias.field` first, then the bare name.
fn predicate_key(map: &std::collections::BTreeMap<String, Value>, fields: &[String]) -> Option<String> {
    let mut key = String::new();
    for field in fields {
        let lowered = field.to_lowercase();
        let value = map
            .get(&lowered)
            .or_else(|| map.get(lowered.rsplit('.').next().unwrap_or(&lowered)))?;
        key.push_str(&join_key(value)?);
        key.push('\u{1}');
    }
    Some(key)
}

/// Resolve one row's predicate values (for non-equality joins); any missing or Null field
/// disqualifies the row.
fn predicate_values(map: &std::collections::BTreeMap<String, Value>, fields: &[String]) -> Option<Vec<Value>> {
    fields
        .iter()
        .map(|field| {
            let lowered = field.to_lowercase();
            let value = map
                .get(&lowered)
                .or_else(|| map.get(lowered.rsplit('.').next().unwrap_or(&lowered)))?;
            if matches!(value, Value::Null) {
                return None;
            }
            Some(value.clone())
        })
        .collect()
}

/// Compare two values for a non-equality join: same-class scalars only (numbers with numbers,
/// text with text); anything else never matches.
fn compare_join_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    fn number_of(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) | Value::Currency(n) => Some(*n),
            _ => None,
        }
    }
    if let (Some(a), Some(b)) = (number_of(left), number_of(right)) {
        return a.partial_cmp(&b);
    }
    if let (Value::Str(a), Value::Str(b)) = (left, right) {
        return Some(a.cmp(b));
    }
    None
}

/// One join step: merge `right_rows` into `left_rows` along the predicate. `left_keep` /
/// `right_keep` carry the outer-ness, already normalized to the traversal direction. Merged
/// values prefer the right side on bare-name collisions, exactly like `Row::insert` would.
fn join_rows(
    left_rows: &TableRows,
    right_rows: &TableRows,
    left_fields: &[String],
    right_fields: &[String],
    operator: TableLinkOperator,
    left_keep: bool,
    right_keep: bool,
    label: &str,
    notes: &mut Vec<String>,
) -> TableRows {
    fn merge(left: &std::collections::BTreeMap<String, Value>, right: &std::collections::BTreeMap<String, Value>) -> std::collections::BTreeMap<String, Value> {
        let mut merged = left.clone();
        merged.extend(right.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged
    }
    let mut out: TableRows = Vec::new();
    let mut right_matched;
    match operator {
        TableLinkOperator::Equal => {
            let mut index: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
            for (i, row) in right_rows.iter().enumerate() {
                if let Some(key) = predicate_key(row, right_fields) {
                    index.entry(key).or_default().push(i);
                }
            }
            right_matched = vec![false; right_rows.len()];
            for left in left_rows {
                let mut matched = false;
                if let Some(key) = predicate_key(left, left_fields) {
                    if let Some(bucket) = index.get(&key) {
                        for &i in bucket {
                            out.push(merge(left, &right_rows[i]));
                            right_matched[i] = true;
                            matched = true;
                        }
                    }
                }
                if !matched && left_keep {
                    out.push(left.clone());
                }
            }
        }
        other => {
            notes.push(format!(
                "link {label}: non-equality join ({other:?}) compares numeric and text fields only",
            ));
            right_matched = vec![false; right_rows.len()];
            for left in left_rows {
                let mut matched = false;
                let Some(left_values) = predicate_values(left, left_fields) else {
                    if left_keep {
                        out.push(left.clone());
                    }
                    continue;
                };
                for (i, right) in right_rows.iter().enumerate() {
                    let Some(right_values) = predicate_values(right, right_fields) else {
                        continue;
                    };
                    if left_values.len() != right_values.len() {
                        continue;
                    }
                    let holds = left_values.iter().zip(right_values.iter()).all(|(a, b)| {
                        match (compare_join_values(a, b), other) {
                            (Some(ord), TableLinkOperator::NotEqual) => ord != std::cmp::Ordering::Equal,
                            (Some(ord), TableLinkOperator::LessThan) => ord == std::cmp::Ordering::Less,
                            (Some(ord), TableLinkOperator::LessOrEqual) => ord != std::cmp::Ordering::Greater,
                            (Some(ord), TableLinkOperator::GreaterThan) => ord == std::cmp::Ordering::Greater,
                            (Some(ord), TableLinkOperator::GreaterOrEqual) => ord != std::cmp::Ordering::Less,
                            _ => false,
                        }
                    });
                    if holds {
                        out.push(merge(left, right));
                        right_matched[i] = true;
                        matched = true;
                    }
                }
                if !matched && left_keep {
                    out.push(left.clone());
                }
            }
        }
    }
    if right_keep {
        for (i, matched) in right_matched.iter().enumerate() {
            if !*matched {
                out.push(right_rows[i].clone());
            }
        }
    }
    out
}

/// Bind the named tables to the report's tables and join them along the report's stored links.
/// Every report table contributes its declared fields as qualified columns; tables without data
/// (or never reached by the join graph) contribute Null columns and are reported — the pipeline
/// never silently drops a table or a row.
fn build_named_dataset(
    tables: &std::collections::BTreeMap<String, Dataset>,
    report: &Report,
    notes: &mut Vec<String>,
) -> Result<RowData, String> {
    let db_tables = &report.database.tables;
    let mut data: Vec<Option<TableRows>> = Vec::with_capacity(db_tables.len());
    let mut matched_keys: std::collections::HashSet<&String> = std::collections::HashSet::new();
    for table in db_tables {
        match matching_table_key(tables, table) {
            Some(key) => {
                data.push(Some(table_rows(table, &tables[key], notes)?));
                matched_keys.insert(key);
            }
            None => {
                notes.push(format!(
                    "table `{}` has no dataset table; its fields resolve to Null",
                    table.alias
                ));
                data.push(None);
            }
        }
    }
    for key in tables.keys() {
        if !matched_keys.contains(key) {
            notes.push(format!("dataset table `{key}` ignored: no such report table"));
        }
    }

    let index_of_alias = |alias: &str| -> Option<usize> {
        db_tables.iter().position(|t| t.alias.eq_ignore_ascii_case(alias))
    };
    let mut joined = vec![false; db_tables.len()];
    let mut applied = vec![false; report.database.links.len()];
    let mut current: TableRows = match data.iter().position(|d| d.is_some()) {
        Some(start) => {
            joined[start] = true;
            data[start].clone().unwrap()
        }
        None => Vec::new(),
    };

    // Grow the join result along the links: each round applies every link that connects the
    // joined set to a new table (the join), or that constrains two already-joined tables (a
    // further predicate applied as a filter). A link between two tables without data is skipped
    // with a note.
    loop {
        let mut progressed = false;
        for (link_index, link) in report.database.links.iter().enumerate() {
            if applied[link_index] {
                continue;
            }
            let Some(source) = index_of_alias(&link.source_table_alias) else {
                notes.push(format!(
                    "link `{}` → `{}` skipped: source alias matches no report table",
                    link.source_table_alias, link.target_table_alias
                ));
                applied[link_index] = true;
                continue;
            };
            let Some(target) = index_of_alias(&link.target_table_alias) else {
                notes.push(format!(
                    "link `{}` → `{}` skipped: target alias matches no report table",
                    link.source_table_alias, link.target_table_alias
                ));
                applied[link_index] = true;
                continue;
            };
            if source == target {
                applied[link_index] = true;
                continue;
            }
            let label = format!("`{}` → `{}`", link.source_table_alias, link.target_table_alias);
            let (from, to, reversed) = if joined[source] && !joined[target] {
                (source, target, false)
            } else if joined[target] && !joined[source] {
                (target, source, true)
            } else if joined[source] && joined[target] {
                // A second link between tables that are already joined constrains the result:
                // rows failing the predicate are dropped (inner semantics; the outer sides of the
                // original join are preserved because the unmatched rows carry Null fields).
                let left_fields: Vec<String> = link
                    .source_fields
                    .iter()
                    .map(|f| format!("{}.{}", db_tables[source].alias.to_lowercase(), f.to_lowercase()))
                    .collect();
                let right_fields: Vec<String> = link
                    .target_fields
                    .iter()
                    .map(|f| format!("{}.{}", db_tables[target].alias.to_lowercase(), f.to_lowercase()))
                    .collect();
                let before = current.len();
                current.retain(|row| {
                    let Some(left_values) = predicate_values(row, &left_fields) else {
                        return false;
                    };
                    let Some(right_values) = predicate_values(row, &right_fields) else {
                        return false;
                    };
                    left_values.len() == right_values.len()
                        && left_values.iter().zip(right_values.iter()).all(|(a, b)| {
                            match (compare_join_values(a, b), link.operator) {
                                (Some(ord), TableLinkOperator::Equal) => ord == std::cmp::Ordering::Equal,
                                (Some(ord), TableLinkOperator::NotEqual) => ord != std::cmp::Ordering::Equal,
                                (Some(ord), TableLinkOperator::LessThan) => ord == std::cmp::Ordering::Less,
                                (Some(ord), TableLinkOperator::LessOrEqual) => ord != std::cmp::Ordering::Greater,
                                (Some(ord), TableLinkOperator::GreaterThan) => ord == std::cmp::Ordering::Greater,
                                (Some(ord), TableLinkOperator::GreaterOrEqual) => ord != std::cmp::Ordering::Less,
                                _ => false,
                            }
                        })
                });
                notes.push(format!(
                    "link {label}: additional predicate between joined tables; {before} → {} row(s)",
                    current.len()
                ));
                applied[link_index] = true;
                progressed = true;
                continue;
            } else {
                continue;
            };
            if data[to].is_none() {
                notes.push(format!(
                    "link {label} skipped: table `{}` has no data",
                    db_tables[to].alias
                ));
                applied[link_index] = true;
                continue;
            }
            let (mut left_keep, mut right_keep) = match link.join_kind {
                TableJoinKind::Inner => (false, false),
                TableJoinKind::LeftOuter => (true, false),
                TableJoinKind::RightOuter => (false, true),
                TableJoinKind::FullOuter => (true, true),
                other => {
                    notes.push(format!(
                        "link {label}: unknown join kind {other:?}; treated as inner"
                    ));
                    (false, false)
                }
            };
            // The stored link runs source → target; traversing it backwards swaps the outer
            // sides and the paired field lists.
            if reversed {
                std::mem::swap(&mut left_keep, &mut right_keep);
            }
            let (left_fields, right_fields): (Vec<String>, Vec<String>) = if reversed {
                (
                    link.target_fields
                        .iter()
                        .map(|f| format!("{}.{}", db_tables[from].alias.to_lowercase(), f.to_lowercase()))
                        .collect(),
                    link.source_fields
                        .iter()
                        .map(|f| format!("{}.{}", db_tables[to].alias.to_lowercase(), f.to_lowercase()))
                        .collect(),
                )
            } else {
                (
                    link.source_fields
                        .iter()
                        .map(|f| format!("{}.{}", db_tables[from].alias.to_lowercase(), f.to_lowercase()))
                        .collect(),
                    link.target_fields
                        .iter()
                        .map(|f| format!("{}.{}", db_tables[to].alias.to_lowercase(), f.to_lowercase()))
                        .collect(),
                )
            };
            let joined_rows = join_rows(
                &current,
                data[to].as_ref().unwrap(),
                &left_fields,
                &right_fields,
                link.operator,
                left_keep,
                right_keep,
                &label,
                notes,
            );
            current = joined_rows;
            joined[to] = true;
            applied[link_index] = true;
            progressed = true;
        }
        if !progressed {
            break;
        }
    }

    for (i, table) in db_tables.iter().enumerate() {
        if data[i].is_some() && !joined[i] {
            notes.push(format!(
                "table `{}` carries rows but is not linked to the joined tables; its fields resolve to Null",
                table.alias
            ));
        }
    }

    let mut columns: Vec<Column> = Vec::new();
    for table in db_tables {
        for field in &table.data_fields {
            if columns.len() >= u16::MAX as usize {
                return Err("too many columns".into());
            }
            columns.push(Column {
                name: format!("{}.{}", table.alias, field.name),
                value_type: field.value_type,
            });
        }
    }
    let mut rows = Vec::with_capacity(current.len());
    for map in &current {
        let mut row = Row::default();
        for column in &columns {
            let value = map
                .get(&column.name.to_lowercase())
                .cloned()
                .unwrap_or(Value::Null);
            row.insert(&column.name, value);
        }
        rows.push(row);
    }
    Ok(RowData::new(columns, rows))
}

/// Build one scope's live rows from the request's dataset spec: flat (single table) or named
/// (push model — the scope's own tables joined along its own stored links).
fn build_scope_rows(spec: &DatasetSpec, report: &Report, notes: &mut Vec<String>) -> Result<RowData, String> {
    match spec {
        DatasetSpec::Flat(dataset) => build_dataset(dataset),
        DatasetSpec::Named(named) => build_named_dataset(&named.tables, report, notes),
    }
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

/// Live rows per subreport, keyed by report identity: `rpt-layout` hands each subreport's own
/// `&Report` to [`ScopeData::rows_for`], and every subreport's `Report` lives inside the main
/// report's `subreports` tree, so matching the borrowed pointer identifies the scope exactly.
struct JsonScopes<'a> {
    /// (full path name, simple name, subreport report, rows) for every subreport in the tree.
    scopes: Vec<(String, String, &'a Report, RowData)>,
}

impl ScopeData for JsonScopes<'_> {
    fn rows_for(&self, report: &Report) -> Option<Box<dyn RowSource>> {
        self.scopes
            .iter()
            .find(|(_, _, p, _)| std::ptr::eq(*p, report))
            .map(|(_, _, _, data)| Box::new(data.clone()) as Box<dyn RowSource>)
    }
}

/// Collect every subreport scope in the tree as `(full path name, simple name, report)`.
fn collect_subreport_scopes<'a>(report: &'a Report, prefix: &str, out: &mut Vec<(String, String, &'a Report)>) {
    for sub in &report.subreports {
        let full = if prefix.is_empty() {
            sub.name.clone()
        } else {
            format!("{prefix}.{}", sub.name)
        };
        out.push((full.clone(), sub.name.clone(), sub.report.as_ref()));
        collect_subreport_scopes(sub.report.as_ref(), &full, out);
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

/// Report metadata for the caller: which parameters to prompt for, which fields the report's
/// field objects reference, which subreports need rows, whether the file carries usable saved data.
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
    // The report's field objects and the reference each displays (SDK: a FieldObject's
    // DataSource), deduplicated in layout order. Database fields carry the parsed
    // `table.field` pair; other kinds keep the raw reference text.
    let mut fields: Vec<Json> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for object in area_objects(&report.report_definition.areas) {
        let ReportObjectKind::Field(field) = &object.kind else {
            continue;
        };
        let kind = field_kind_name(field.ref_kind);
        if !seen.insert((kind.to_string(), field.data_source.clone())) {
            continue;
        }
        let inner = field.data_source.trim_matches(|c| c == '{' || c == '}');
        let (table, name) = if field.ref_kind == FieldRefKind::DatabaseField {
            match inner.rsplit_once('.') {
                Some((t, f)) => (t.to_string(), f.to_string()),
                None => (String::new(), inner.to_string()),
            }
        } else {
            (String::new(), inner.to_string())
        };
        fields.push(json!({
            "reference": field.data_source,
            "kind": kind,
            "table": table,
            "name": name,
        }));
    }
    let mut pointers: Vec<(String, String, &Report)> = Vec::new();
    collect_subreport_scopes(report, "", &mut pointers);
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
        "fields": fields,
        "subreports": subreports,
        "tables": tables,
    })
}

fn field_kind_name(kind: FieldRefKind) -> &'static str {
    match kind {
        FieldRefKind::DatabaseField => "database",
        FieldRefKind::Formula => "formula",
        FieldRefKind::Summary => "summary",
        FieldRefKind::Special => "special",
        FieldRefKind::GroupName => "groupname",
        FieldRefKind::RunningTotal => "runningtotal",
        FieldRefKind::Parameter => "parameter",
        FieldRefKind::SqlExpression => "sqlexpression",
        FieldRefKind::Unknown => "unknown",
    }
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
        Some(spec) => {
            Some(build_scope_rows(spec, report, &mut notes).map_err(ShimError::InvalidArgument)?)
        }
        None => None,
    };
    let mut pointers: Vec<(String, String, &Report)> = Vec::new();
    collect_subreport_scopes(report, "", &mut pointers);
    let mut scopes: Vec<(String, String, &Report, RowData)> = Vec::new();
    for (name, spec) in &req.subreports {
        match pointers
            .iter()
            .find(|(full, simple, _)| full == name || simple == name)
        {
            Some((full, simple, sub)) => {
                let data =
                    build_scope_rows(spec, sub, &mut notes).map_err(ShimError::InvalidArgument)?;
                scopes.push((full.clone(), simple.clone(), *sub, data));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn num(n: f64) -> Value {
        Value::Number(n)
    }

    fn row_of(pairs: &[(&str, f64)]) -> std::collections::BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), num(*v))).collect()
    }

    fn fields(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    fn join(
        left: &TableRows,
        right: &TableRows,
        operator: TableLinkOperator,
        left_keep: bool,
        right_keep: bool,
    ) -> TableRows {
        join_rows(
            left,
            right,
            &fields(&["a.id"]),
            &fields(&["b.id"]),
            operator,
            left_keep,
            right_keep,
            "`a` → `b`",
            &mut Vec::new(),
        )
    }

    #[test]
    fn inner_join_matches_and_drops_unmatched() {
        let left = vec![row_of(&[("a.id", 1.0)]), row_of(&[("a.id", 2.0)])];
        let right = vec![row_of(&[("b.id", 1.0)]), row_of(&[("b.id", 3.0)])];
        let out = join(&left, &right, TableLinkOperator::Equal, false, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("a.id"), Some(&num(1.0)));
        assert_eq!(out[0].get("b.id"), Some(&num(1.0)));
    }

    #[test]
    fn left_outer_keeps_unmatched_left_rows() {
        let left = vec![row_of(&[("a.id", 1.0)]), row_of(&[("a.id", 2.0)])];
        let right = vec![row_of(&[("b.id", 1.0)])];
        let out = join(&left, &right, TableLinkOperator::Equal, true, false);
        assert_eq!(out.len(), 2);
        assert!(out[0].contains_key("b.id"), "matched pair merges both sides");
        assert!(!out[1].contains_key("b.id"), "unmatched left row carries only its own fields");
    }

    #[test]
    fn right_outer_keeps_unmatched_right_rows() {
        let left = vec![row_of(&[("a.id", 1.0)])];
        let right = vec![row_of(&[("b.id", 1.0)]), row_of(&[("b.id", 9.0)])];
        let out = join(&left, &right, TableLinkOperator::Equal, false, true);
        assert_eq!(out.len(), 2);
        assert!(out[1].contains_key("b.id") && !out[1].contains_key("a.id"));
    }

    #[test]
    fn full_outer_keeps_both_unmatched_sides() {
        let left = vec![row_of(&[("a.id", 1.0)]), row_of(&[("a.id", 2.0)])];
        let right = vec![row_of(&[("b.id", 1.0)]), row_of(&[("b.id", 9.0)])];
        let out = join(&left, &right, TableLinkOperator::Equal, true, true);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].get("a.id"), Some(&num(1.0)));
        assert_eq!(out[1].get("a.id"), Some(&num(2.0)));
        assert_eq!(out[2].get("b.id"), Some(&num(9.0)));
    }

    #[test]
    fn null_and_missing_join_fields_never_match() {
        let mut left = row_of(&[("a.id", 1.0)]);
        left.insert("a.id".to_string(), Value::Null);
        let right = vec![row_of(&[("b.id", 1.0)])];
        let out = join(&vec![left], &right, TableLinkOperator::Equal, true, true);
        assert_eq!(out.len(), 2, "Null key: left row unmatched-kept, right row unmatched-kept");
    }

    #[test]
    fn one_to_many_joins_fan_out() {
        let left = vec![row_of(&[("a.id", 1.0)])];
        let right = vec![row_of(&[("b.id", 1.0)]), row_of(&[("b.id", 1.0)])];
        let out = join(&left, &right, TableLinkOperator::Equal, false, false);
        assert_eq!(out.len(), 2, "each matching right row emits its own merged row");
    }

    #[test]
    fn greater_than_join_compares_numbers() {
        let left = vec![row_of(&[("a.id", 5.0)]), row_of(&[("a.id", 1.0)])];
        let right = vec![row_of(&[("b.id", 3.0)])];
        let out = join(&left, &right, TableLinkOperator::GreaterThan, false, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("a.id"), Some(&num(5.0)));
    }

    #[test]
    fn not_equal_join_compares_text() {
        let left = vec![
            std::collections::BTreeMap::from([("a.id".to_string(), Value::Str("x".into()))]),
            std::collections::BTreeMap::from([("a.id".to_string(), Value::Str("y".into()))]),
        ];
        let right = vec![std::collections::BTreeMap::from([("b.id".to_string(), Value::Str("y".into()))])];
        let out = join(&left, &right, TableLinkOperator::NotEqual, false, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get("a.id"), Some(&Value::Str("x".into())));
    }

    #[test]
    fn named_dataset_binds_and_joins_fixture_tables() {
        let bytes =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../samples/customer_orders_by_country.rpt"))
                .unwrap();
        let rpt = rpt_reader::Rpt::read(&*bytes).unwrap();
        let report = rpt.report();
        let named: NamedTables = serde_json::from_value(serde_json::json!({
            "tables": {
                "Customer": {
                    "columns": [
                        { "name": "Customer ID", "type": "number" },
                        { "name": "Customer Name", "type": "string" }
                    ],
                    "rows": [[1, "Alpha Co"], [2, "Beta SpA"]]
                },
                "Orders": {
                    "columns": [
                        { "name": "Order ID", "type": "number" },
                        { "name": "Customer ID", "type": "number" },
                        { "name": "Order Date", "type": "date" }
                    ],
                    "rows": [[10, 1, "2024-03-01"], [11, 2, "2024-03-02"]]
                },
                "Orders Detail": {
                    "columns": [
                        { "name": "Order ID", "type": "number" }
                    ],
                    "rows": [[10], [10], [11]]
                }
            }
        }))
        .unwrap();
        let mut notes = Vec::new();
        let data = build_named_dataset(&named.tables, report, &mut notes).unwrap();
        // Unbound fields are reported (fail-open), but nothing fails the bind itself.
        let text = notes.join(" | ");
        assert!(!text.contains("has no dataset table"), "{text}");
        assert!(!text.contains("ignored: no such report table"), "{text}");
        assert!(!text.contains("not linked"), "{text}");
        // Inner join: 2 customers × 2 orders (1:1) × 3 detail rows (order 10 twice, order 11 once).
        assert_eq!(RowSource::rows(&data).len(), 3);
        let column_names: Vec<&str> = RowSource::columns(&data).iter().map(|c| c.name.as_str()).collect();
        assert!(column_names.contains(&"Customer.Customer Name"));
        assert!(column_names.contains(&"Orders.Order Amount"));
        assert!(column_names.contains(&"Orders_Detail.Quantity"));
        // The bound table keyed by its NAME ("Orders Detail") matched the alias "Orders_Detail".
        let merged = &RowSource::rows(&data)[0];
        assert_eq!(merged.get("Customer.Customer Name"), Some(&Value::Str("Alpha Co".into())));
        assert_eq!(merged.get("Orders_Detail.Order ID"), Some(&num(10.0)));
    }

    #[test]
    fn named_dataset_notes_missing_and_unknown_tables() {
        let bytes =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../samples/customer_orders_by_country.rpt"))
                .unwrap();
        let rpt = rpt_reader::Rpt::read(&*bytes).unwrap();
        let report = rpt.report();
        let named: NamedTables = serde_json::from_value(serde_json::json!({
            "tables": {
                "Customer": { "columns": [ { "name": "Customer ID", "type": "number" } ], "rows": [[1], [2]] },
                "NoSuchTable": { "columns": [ { "name": "x", "type": "string" } ], "rows": [] }
            }
        }))
        .unwrap();
        let mut notes = Vec::new();
        let data = build_named_dataset(&named.tables, report, &mut notes).unwrap();
        let text = notes.join(" | ");
        assert!(text.contains("has no dataset table"), "{text}");
        assert!(text.contains("NoSuchTable"), "{text}");
        // Orders/Orders_Detail links are skipped; Customer's rows still come through.
        assert_eq!(RowSource::rows(&data).len(), 2);
        assert_eq!(
            RowSource::rows(&data)[0].get("Customer.Customer ID"),
            Some(&num(1.0))
        );
        // Orders never bound: its column exists and resolves to Null in every row.
        assert_eq!(
            RowSource::rows(&data)[0].get("Orders.Order ID"),
            Some(&Value::Null)
        );
    }
}