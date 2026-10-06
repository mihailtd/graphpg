//! Media types of the REST API beyond JSON: content negotiation (`Accept`), CSV (RFC 4180) as a
//! response and as a request body, `multipart/form-data` request bodies (RFC 7578), and the
//! binary value of a `bytea` column as `application/octet-stream`.
use super::problem::ProblemDetails;
use crate::catalog::{ColumnMetadata, PgType, TableMetadata};
use axum::http::{header, HeaderMap};
use serde_json::{Map, Value};

pub const CSV: &str = "text/csv";
pub const JSON: &str = "application/json";
pub const MULTIPART: &str = "multipart/form-data";
pub const OCTET_STREAM: &str = "application/octet-stream";

/// What a read answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Json,
    Csv,
}

/// The media types in an `Accept` header, best first (by `q`, then in the order written).
fn accepted(headers: &HeaderMap) -> Option<Vec<String>> {
    let raw = headers.get(header::ACCEPT)?.to_str().ok()?;
    let mut entries: Vec<(f32, usize, String)> = raw
        .split(',')
        .enumerate()
        .filter_map(|(i, part)| {
            let mut pieces = part.split(';');
            let media = pieces.next()?.trim().to_ascii_lowercase();
            if media.is_empty() {
                return None;
            }
            let q = pieces
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            Some((q, i, media))
        })
        .filter(|(q, _, _)| *q > 0.0)
        .collect();
    entries.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    Some(entries.into_iter().map(|(_, _, m)| m).collect())
}

fn matches(range: &str, media: &str) -> bool {
    match range {
        "*/*" => true,
        _ => match range.strip_suffix("/*") {
            Some(kind) => media.split('/').next() == Some(kind),
            None => range == media,
        },
    }
}

/// The representation a list is sent as: JSON unless `Accept` prefers CSV. `406` when
/// `Accept` allows neither.
pub fn negotiate(headers: &HeaderMap) -> Result<Media, ProblemDetails> {
    let Some(accepted) = accepted(headers) else {
        return Ok(Media::Json);
    };
    for range in &accepted {
        if matches(range, JSON) {
            return Ok(Media::Json);
        }
        if matches(range, CSV) {
            return Ok(Media::Csv);
        }
    }
    Err(ProblemDetails::not_acceptable(format!(
        "This resource is sent as `{JSON}` or `{CSV}`"
    )))
}

/// Whether `Accept` allows a response of `media` (any, when it is absent).
pub fn accepts(headers: &HeaderMap, media: &str) -> bool {
    accepted(headers).is_none_or(|a| a.iter().any(|range| matches(range, media)))
}

/// The base media type of `Content-Type`, lower case, without parameters.
pub fn content_type(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    Some(raw.split(';').next()?.trim().to_ascii_lowercase())
}

// ---------------------------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------------------------

/// Rows as a CSV document: a header of the members of the first row (or `columns`), nulls
/// empty, objects and lists as JSON text.
pub fn rows_to_csv(rows: &[Value], columns: Option<&[String]>) -> String {
    let header: Vec<String> = match (columns, rows.first()) {
        (Some(columns), _) => columns.to_vec(),
        (None, Some(Value::Object(first))) => first.keys().cloned().collect(),
        _ => return String::new(),
    };
    let mut out = super::super::bulk::csv_line(header.iter().map(|k| Some(k.clone())));
    for row in rows {
        out.push_str(&super::super::bulk::csv_line(
            header
                .iter()
                .map(|k| super::super::bulk::csv_cell(row.get(k))),
        ));
    }
    out
}

/// The records of a CSV document (RFC 4180): quoted fields hold separators, line breaks and
/// doubled quotes. A field is `(text, quoted)`.
fn csv_records(text: &str) -> Result<Vec<Vec<(String, bool)>>, String> {
    let mut records = Vec::new();
    let mut record: Vec<(String, bool)> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut chars = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .chars()
        .peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => in_quotes = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !quoted => {
                in_quotes = true;
                quoted = true;
            }
            '"' => return Err("a quote inside an unquoted field".to_string()),
            ',' => {
                record.push((std::mem::take(&mut field), quoted));
                quoted = false;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                record.push((std::mem::take(&mut field), quoted));
                quoted = false;
                // A blank line is no record
                if !(record.len() == 1 && record[0].0.is_empty() && !record[0].1) {
                    records.push(std::mem::take(&mut record));
                } else {
                    record.clear();
                }
            }
            _ => field.push(c),
        }
    }
    if in_quotes {
        return Err("a quoted field is not closed".to_string());
    }
    if !field.is_empty() || quoted || !record.is_empty() {
        record.push((field, quoted));
        records.push(record);
    }
    Ok(records)
}

/// A text cell as the JSON value its column takes. Empty cells are null.
fn typed_cell(
    column: Option<&ColumnMetadata>,
    text: &str,
    quoted: bool,
    place: &str,
) -> Result<Value, ProblemDetails> {
    if text.is_empty() && !quoted {
        return Ok(Value::Null);
    }
    let Some(column) = column else {
        return Ok(Value::String(text.to_string()));
    };
    let bad = |expected: &str| {
        ProblemDetails::invalid(format!("`{text}` is not {expected}"), place.to_string())
    };
    Ok(match &column.pg_type {
        PgType::Int2 | PgType::Int4 => text
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| bad("an integer"))?,
        PgType::Float4 | PgType::Float8 => text
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number))
            .ok_or_else(|| bad("a number"))?,
        PgType::Bool => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" => Value::Bool(true),
            "false" | "f" | "0" | "no" => Value::Bool(false),
            _ => return Err(bad("a boolean (`true` or `false`)")),
        },
        PgType::Json | PgType::Jsonb | PgType::Array(_) | PgType::Multirange(_) => {
            serde_json::from_str(text).map_err(|_| bad("JSON"))?
        }
        _ => Value::String(text.to_string()),
    })
}

/// A CSV request body as the array of rows of a bulk create: the first record names the
/// fields (GraphQL field names, as in JSON), the cells are typed by their columns.
pub fn csv_to_rows(body: &[u8], table: &TableMetadata) -> Result<Value, ProblemDetails> {
    let text = std::str::from_utf8(body)
        .map_err(|_| ProblemDetails::bad_request("A CSV body must be UTF-8"))?;
    let mut records = csv_records(text)
        .map_err(|e| ProblemDetails::bad_request(format!("Invalid CSV body: {e}")))?
        .into_iter();
    let Some(header) = records.next() else {
        return Err(ProblemDetails::bad_request(
            "A CSV body needs a header record naming the fields",
        ));
    };
    let names: Vec<&str> = header.iter().map(|(name, _)| name.trim()).collect();
    if let Some(i) = names.iter().position(|n| n.is_empty()) {
        return Err(ProblemDetails::bad_request(format!(
            "The CSV header has no name in column {}",
            i + 1
        )));
    }
    let mut rows = Vec::new();
    for (index, record) in records.enumerate() {
        if record.len() != names.len() {
            return Err(ProblemDetails::bad_request(format!(
                "CSV record {} has {} fields, the header has {}",
                index + 1,
                record.len(),
                names.len()
            )));
        }
        let mut row = Map::new();
        for (name, (text, quoted)) in names.iter().zip(record) {
            let column = table.columns.iter().find(|c| c.field_name == *name);
            let place = format!("body[{index}].{name}");
            row.insert(name.to_string(), typed_cell(column, &text, quoted, &place)?);
        }
        rows.push(Value::Object(row));
    }
    Ok(Value::Array(rows))
}

// ---------------------------------------------------------------------------------------------
// multipart/form-data
// ---------------------------------------------------------------------------------------------

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn boundary_of(headers: &HeaderMap) -> Result<String, ProblemDetails> {
    let raw = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    raw.split(';')
        .filter_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_string())
        .find(|b| !b.is_empty())
        .ok_or_else(|| {
            ProblemDetails::bad_request("A multipart body needs a `boundary` in its Content-Type")
        })
}

fn disposition_param(disposition: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let start = disposition.find(&marker)? + marker.len();
    let end = disposition[start..].find('"')? + start;
    Some(disposition[start..end].to_string())
}

/// A `multipart/form-data` body as one JSON object: text parts by their `name` (typed by
/// the column), file parts of a `bytea` column as Base64.
pub fn multipart_to_object(
    headers: &HeaderMap,
    body: &[u8],
    table: &TableMetadata,
) -> Result<Value, ProblemDetails> {
    use base64::Engine;
    let boundary = boundary_of(headers)?;
    let delimiter = format!("--{boundary}").into_bytes();
    let malformed = || ProblemDetails::bad_request("Invalid multipart body");
    let mut object = Map::new();
    let mut at = find(body, &delimiter, 0).ok_or_else(malformed)?;
    loop {
        at += delimiter.len();
        // `--` after a delimiter closes the body
        if body.get(at..at + 2) == Some(b"--") {
            break;
        }
        let head_start = find(body, b"\r\n", at).ok_or_else(malformed)? + 2;
        let head_end = find(body, b"\r\n\r\n", head_start - 2).ok_or_else(malformed)?;
        let head = std::str::from_utf8(&body[head_start.min(head_end)..head_end])
            .map_err(|_| malformed())?;
        let content_start = head_end + 4;
        let next = find(body, &delimiter, content_start).ok_or_else(malformed)?;
        // The line break before a delimiter belongs to the delimiter
        let content = &body[content_start..next.saturating_sub(2).max(content_start)];

        let disposition = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))
            .ok_or_else(malformed)?;
        let name = disposition_param(disposition, "name").ok_or_else(malformed)?;
        let column = table.columns.iter().find(|c| c.field_name == name);
        let is_file = disposition_param(disposition, "filename").is_some();
        let value = match column.map(|c| &c.pg_type) {
            Some(PgType::Bytea) if is_file || std::str::from_utf8(content).is_err() => {
                Value::String(base64::engine::general_purpose::STANDARD.encode(content))
            }
            _ => {
                let text = std::str::from_utf8(content).map_err(|_| {
                    ProblemDetails::invalid(
                        format!("Part `{name}` is not text"),
                        format!("body.{name}"),
                    )
                })?;
                typed_cell(column, text, true, &format!("body.{name}"))?
            }
        };
        object.insert(name, value);
        at = next;
    }
    Ok(Value::Object(object))
}

// ---------------------------------------------------------------------------------------------
// Binary
// ---------------------------------------------------------------------------------------------

/// A media type recognized from the first bytes of a document, else `application/octet-stream`.
pub fn sniff(bytes: &[u8]) -> &'static str {
    const SIGNATURES: [(&[u8], &str); 9] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
        (b"RIFF", "application/octet-stream"),
        (b"OggS", "audio/ogg"),
    ];
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp";
    }
    SIGNATURES
        .iter()
        .find(|(magic, _)| bytes.starts_with(magic))
        .map_or(OCTET_STREAM, |(_, media)| media)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(accept: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT, accept.parse().unwrap());
        h
    }

    #[test]
    fn accept_picks_the_best_supported_type() {
        assert_eq!(negotiate(&HeaderMap::new()).unwrap(), Media::Json);
        assert_eq!(negotiate(&headers("text/csv")).unwrap(), Media::Csv);
        assert_eq!(negotiate(&headers("*/*")).unwrap(), Media::Json);
        assert_eq!(
            negotiate(&headers("text/csv;q=0.9, application/json;q=0.5")).unwrap(),
            Media::Csv
        );
        assert_eq!(negotiate(&headers("text/*")).unwrap(), Media::Csv);
        assert!(negotiate(&headers("application/xml")).is_err());
        assert!(negotiate(&headers("text/csv;q=0")).is_err());
    }

    #[test]
    fn csv_round_trips_quotes_commas_and_line_breaks() {
        let rows = vec![
            serde_json::json!({ "id": 1, "note": "a, \"b\"\nc", "gone": null }),
            serde_json::json!({ "id": 2, "note": "plain", "gone": null }),
        ];
        let csv = rows_to_csv(&rows, None);
        assert_eq!(
            csv,
            "id,note,gone\r\n1,\"a, \"\"b\"\"\nc\",\r\n2,plain,\r\n"
        );
        let records = csv_records(&csv).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1][1].0, "a, \"b\"\nc");
        assert!(csv_records("a,\"b").is_err());
    }

    #[test]
    fn sniffing_knows_common_documents() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\0\0"), "image/png");
        assert_eq!(sniff(b"%PDF-1.7"), "application/pdf");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8"), "image/webp");
        assert_eq!(sniff(b"nothing known"), OCTET_STREAM);
    }
}
