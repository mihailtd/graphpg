//! Input values of scalar types: checked against their GraphQL scalar, and converted to the
//! text PostgreSQL parses for the column's type (still as JSON; parameters are bound as text).
//!
//! Values are converted deep inside planning, in code that has no error path of its own, so
//! invalid values are collected here and reported together once planning ends
//! ([`start`], [`finish`]). Planning runs on one thread, without awaiting.
use crate::catalog::{to_snake_case, PgType, TypeLimits};
use serde::Serialize;
use serde_json::Value;
use std::cell::RefCell;
use std::net::IpAddr;

/// An input value planning rejected: where it is in the operation's arguments
/// (`input.players.create[0].name`), why, and the error code clients can act on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputError {
    pub path: Option<String>,
    pub message: String,
    /// `BAD_USER_INPUT` for a value of the wrong form; `<FIELD>_TOO_LONG` or
    /// `<FIELD>_OUT_OF_RANGE` (values of the table's `<Type>ErrorCode`) for one exceeding
    /// its column's type modifier.
    pub code: String,
}

thread_local! {
    static ERRORS: RefCell<Vec<InputError>> = const { RefCell::new(Vec::new()) };
}

/// Starts collecting the errors of one operation's input values.
pub(crate) fn start() {
    ERRORS.with(|e| e.borrow_mut().clear());
}

/// The errors collected since [`start`].
pub(crate) fn finish() -> Vec<InputError> {
    ERRORS.with(|e| std::mem::take(&mut *e.borrow_mut()))
}

/// A `LIKE` pattern, refused when it ends with the escape character: PostgreSQL fails on it,
/// but only when a row's value matches up to it (a request that worked would fail once the
/// data changes).
pub(crate) fn like_pattern(pattern: String, label: &str) -> String {
    let trailing = pattern.chars().rev().take_while(|c| *c == '\\').count();
    if trailing % 2 == 1 {
        report(
            label,
            &Value::String(pattern.clone()),
            "a LIKE pattern (it ends with the escape character `\\`: write `\\\\` to match one)",
        );
    }
    pattern
}

/// Reports `value`, the global ID of a `type_name`, passed where the ID of `expected` goes.
pub(crate) fn wrong_node_type(type_name: &str, expected: &str, value: Value, label: &str) -> Value {
    report(
        label,
        &value,
        &format!("the ID of {expected} (it is the ID of a `{type_name}`)"),
    )
}

/// Reports `value` as not one of enum `type_name`'s values.
pub(crate) fn not_a_value_of(type_name: &str, value: Value, label: &str) -> Value {
    report(label, &value, &format!("a value of `{type_name}`"))
}

/// Whether `value` is a valid input of `pg_type`, leaving the errors collected so far as they
/// are.
pub(crate) fn is_valid(pg_type: &PgType, value: Value) -> bool {
    let saved = finish();
    convert(pg_type, value, "");
    let valid = finish().is_empty();
    ERRORS.with(|e| *e.borrow_mut() = saved);
    valid
}

fn shown(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

fn report(label: &str, value: &Value, expected: &str) -> Value {
    let message = format!("`{label}`: {} is not {expected}", shown(value));
    ERRORS.with(|e| {
        e.borrow_mut().push(InputError {
            path: Some(label.to_string()),
            message,
            code: "BAD_USER_INPUT".to_string(),
        })
    });
    value.clone()
}

/// `<FIELD>_<suffix>` for the field a label ends with (`input.dateOfBirth` → `DATE_OF_BIRTH_TOO_LONG`).
fn field_code(label: &str, suffix: &str) -> String {
    let field = label.rsplit('.').next().unwrap_or(label);
    let field = field.split('[').next().unwrap_or(field);
    format!("{}_{suffix}", to_snake_case(field).to_uppercase())
}

/// Checks a converted scalar value against the limits of its column's type modifier (for
/// writes: comparisons with longer values simply match nothing).
pub(crate) fn check_limits(value: &Value, limits: &TypeLimits, label: &str) {
    let Value::String(text) = value else { return };
    let reject = |message: String, suffix: &str| {
        let error = InputError {
            path: Some(label.to_string()),
            message,
            code: field_code(label, suffix),
        };
        ERRORS.with(|e| e.borrow_mut().push(error));
    };
    if let Some(max) = limits.max_length {
        // PostgreSQL silently drops excess characters that are all spaces
        let chars = text.chars().count();
        let excess_blank = text.chars().skip(max as usize).all(|c| c == ' ');
        if chars > max as usize && !excess_blank {
            reject(
                format!(
                    "`{label}`: {} characters, more than the {max} allowed",
                    chars
                ),
                "TOO_LONG",
            );
        }
    }
    if let Some((precision, scale)) = limits.precision {
        if !fits_numeric(text.trim(), precision, scale) {
            let limit = match precision as i64 - scale as i64 {
                n if n <= 0 => "less than 1 in absolute value".to_string(),
                n => format!("less than 10^{n} in absolute value"),
            };
            reject(
                format!("`{label}`: {} does not fit `numeric({precision}, {scale})` (it must round to {limit})", shown(value)),
                "OUT_OF_RANGE",
            );
        }
    }
}

/// Whether decimal `text` fits `numeric(precision, scale)`: rounded (half away from zero)
/// to `scale` decimal places, its absolute value is below `10^(precision - scale)`.
fn fits_numeric(text: &str, precision: u32, scale: i32) -> bool {
    let t = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(0)),
        None => (t, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits: Vec<u8> = int
        .bytes()
        .chain(frac.bytes())
        .filter(u8::is_ascii_digit)
        .collect();
    // Position of the decimal point within `digits`
    let mut point = int.len() as i64 + exponent;
    while digits.first() == Some(&b'0') {
        digits.remove(0);
        point -= 1;
    }
    if digits.is_empty() {
        return true;
    }
    let keep = point + scale as i64;
    let magnitude = if keep < 0 {
        return true;
    } else if keep as usize >= digits.len() {
        point
    } else {
        let kept = &digits[..keep as usize];
        let round_up = digits[keep as usize] >= b'5';
        match (round_up, kept.iter().all(|d| *d == b'9')) {
            (false, _) if kept.iter().all(|d| *d == b'0') => return true,
            (true, true) => point + 1,
            _ => point,
        }
    };
    magnitude <= precision as i64 - scale as i64
}

/// A non-null, non-list input value of `pg_type`, converted (or reported, and returned as it is).
pub(crate) fn convert(pg_type: &PgType, value: Value, label: &str) -> Value {
    let text = value.as_str();
    let ok = |v: Value| v;
    match pg_type {
        // GraphQL checks these already, but not behind a domain's custom scalar
        PgType::Int2 | PgType::Int4 => {
            let (min, max) = if *pg_type == PgType::Int2 { (i16::MIN as i64, i16::MAX as i64) } else { (i32::MIN as i64, i32::MAX as i64) };
            // `ID` arguments (keys) arrive as strings
            match value.as_i64().or_else(|| text.and_then(|t| t.trim().parse().ok())) {
                Some(n) if (min..=max).contains(&n) => value,
                _ => report(label, &value, if *pg_type == PgType::Int2 { "a 16-bit integer" } else { "an Int (a 32-bit integer)" }),
            }
        }
        PgType::Float4 | PgType::Float8 => match value.as_f64().or_else(|| text.and_then(|t| t.trim().parse::<f64>().ok())) {
            Some(_) => value,
            None => report(label, &value, "a Float"),
        },
        PgType::Bool => match value {
            Value::Bool(_) => value,
            _ => report(label, &value, "a Boolean"),
        },
        PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext => match value {
            Value::String(_) => value,
            // An `ID` given as a number
            Value::Number(n) => Value::String(n.to_string()),
            _ => report(label, &value, "a String"),
        },
        PgType::Int8 => match (&value, text) {
            (Value::Number(n), _) if n.as_i64().is_some() => Value::String(n.to_string()),
            (_, Some(t)) if t.trim().parse::<i64>().is_ok() => Value::String(t.trim().to_string()),
            _ => report(label, &value, "a BigInt (a 64-bit integer)"),
        },
        PgType::Numeric | PgType::Money => match (&value, text) {
            (Value::Number(n), _) => Value::String(n.to_string()),
            (_, Some(t)) if is_decimal(t.trim()) => Value::String(t.trim().to_string()),
            _ => report(label, &value, "a Decimal (a decimal number)"),
        },
        PgType::Uuid => match text {
            Some(t) if uuid::Uuid::parse_str(t).is_ok() => ok(value),
            _ => report(label, &value, "a UUID"),
        },
        PgType::TimestampTz => match text {
            Some(t) if chrono::DateTime::parse_from_rfc3339(&t.replacen(' ', "T", 1)).is_ok() || local_datetime(t) => ok(value),
            _ => report(label, &value, "a Datetime (ISO 8601, e.g. `2024-05-01T12:30:00+00:00`)"),
        },
        PgType::Timestamp => match text {
            Some(t) if local_datetime(t) => ok(value),
            _ => report(label, &value, "a LocalDatetime (ISO 8601 without an offset, e.g. `2024-05-01T12:30:00`)"),
        },
        PgType::Date => match text {
            Some(t) if chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").is_ok() => ok(value),
            _ => report(label, &value, "a Date (ISO 8601, e.g. `2024-05-01`)"),
        },
        PgType::Time => match text {
            Some(t) if time_of_day(t) => ok(value),
            _ => report(label, &value, "a Time (ISO 8601, e.g. `12:30:00`)"),
        },
        PgType::TimeTz => match text.and_then(split_offset) {
            Some((time, _)) if time_of_day(time) => ok(value),
            _ => report(label, &value, "a TimeTz (a time with a UTC offset, e.g. `12:30:00+02`)"),
        },
        PgType::Interval => match text {
            Some(t) if is_iso_duration(t) => ok(value),
            _ => report(label, &value, "an Interval (an ISO 8601 duration, e.g. `P1DT2H`)"),
        },
        // bytea's hex input format
        PgType::Bytea => {
            use base64::Engine;
            match text.map(|t| base64::engine::general_purpose::STANDARD.decode(t)) {
                Some(Ok(bytes)) => Value::String(format!("\\x{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())),
                _ => report(label, &value, "Base64 (RFC 4648, with padding)"),
            }
        }
        PgType::Inet => match text {
            Some(t) if network(t, false) => ok(value),
            _ => report(label, &value, "an InternetAddress (e.g. `192.168.0.1` or `192.168.0.1/24`)"),
        },
        PgType::Cidr => match text {
            Some(t) if network(t, true) => ok(value),
            _ => report(label, &value, "a CidrAddress (a network, e.g. `192.168.0.0/24`, with no bits set after the prefix)"),
        },
        PgType::MacAddr | PgType::MacAddr8 => match text {
            Some(t) if mac_address(t, *pg_type == PgType::MacAddr8) => ok(value),
            _ => report(label, &value, "a MacAddress (e.g. `08:00:2b:01:02:03`)"),
        },
        // `[1,2,3]` is also the vector's text form
        PgType::Vector => match &value {
            Value::Array(items) if items.iter().all(|v| v.as_f64().is_some_and(f64::is_finite)) => Value::String(value.to_string()),
            _ => report(label, &value, "a list of floats"),
        },
        _ => value,
    }
}

/// A GeoJSON geometry object, as its text (`ST_GeomFromGeoJSON` parses it).
pub(crate) fn geojson(value: Value, label: &str) -> Value {
    match &value {
        Value::Object(obj) if obj.get("type").is_some_and(Value::is_string) => {
            Value::String(value.to_string())
        }
        // Also accepted as its text
        Value::String(text)
            if serde_json::from_str::<Value>(text).is_ok_and(|v| v.get("type").is_some()) =>
        {
            value
        }
        _ => report(label, &value, "a GeoJSON geometry object (with a `type`)"),
    }
}

/// A range input `{ start: { value, inclusive }, end: { ... } }` as a range literal: a missing
/// bound is unbounded, `inclusive` defaults to true for `start` and false for `end`.
pub(crate) fn range_literal(element: &PgType, value: &Value, label: &str) -> Value {
    if let Value::String(s) = value {
        if (s.starts_with('[') || s.starts_with('(')) && (s.ends_with(']') || s.ends_with(')')) {
            return Value::String(s.clone());
        }
    }
    let Value::Object(obj) = value else {
        return report(label, value, "a range");
    };
    let bound = |name: &str, default_inclusive: bool| -> (String, bool) {
        match obj.get(name) {
            Some(Value::Object(b)) => {
                let converted = convert(
                    element,
                    b.get("value").cloned().unwrap_or(Value::Null),
                    &format!("{label}.{name}.value"),
                );
                let text = match converted {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                // Double quotes keep spaces, commas and colons of the value inside the bound
                let quoted = format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
                (
                    quoted,
                    b.get("inclusive")
                        .and_then(Value::as_bool)
                        .unwrap_or(default_inclusive),
                )
            }
            _ => (String::new(), false),
        }
    };
    let (lower, lower_inc) = bound("start", true);
    let (upper, upper_inc) = bound("end", false);
    Value::String(format!(
        "{}{lower},{upper}{}",
        if lower_inc { '[' } else { '(' },
        if upper_inc { ']' } else { ')' }
    ))
}

/// `[+-]digits[.digits][e[+-]digits]`
fn is_decimal(t: &str) -> bool {
    let t = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e.strip_prefix(['+', '-']).unwrap_or(e))),
        None => (t, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) || int.is_empty())
                && (digits(frac) || frac.is_empty())
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    mantissa_ok && exponent.is_none_or(digits)
}

/// ISO 8601 date and time without an offset (`T` or a space between them).
fn local_datetime(t: &str) -> bool {
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .any(|f| chrono::NaiveDateTime::parse_from_str(t, f).is_ok())
}

fn time_of_day(t: &str) -> bool {
    ["%H:%M:%S%.f", "%H:%M"]
        .iter()
        .any(|f| chrono::NaiveTime::parse_from_str(t, f).is_ok())
}

/// `12:30:00+02:00` → (`12:30:00`, `+02:00`): a `Z` or `±HH[[:]MM]` suffix.
fn split_offset(t: &str) -> Option<(&str, &str)> {
    if let Some(time) = t.strip_suffix(['Z', 'z']) {
        return Some((time, "Z"));
    }
    let at = t.rfind(['+', '-'])?;
    let (time, offset) = t.split_at(at);
    let digits: String = offset[1..].chars().filter(|c| *c != ':').collect();
    let valid = matches!(digits.len(), 2 | 4)
        && digits.bytes().all(|b| b.is_ascii_digit())
        && offset[1..].matches(':').count() <= 1;
    valid.then_some((time, offset))
}

/// `P[nY][nM][nW][nD][T[nH][nM][nS]]`, at least one part, numbers optionally signed and
/// fractional; `-P...` negates it all.
fn is_iso_duration(t: &str) -> bool {
    let t = t.strip_prefix('-').unwrap_or(t);
    let Some(rest) = t.strip_prefix('P') else {
        return false;
    };
    let (date, time) = match rest.split_once('T') {
        Some((d, tm)) => (d, Some(tm)),
        None => (rest, None),
    };
    let parts = |s: &str, units: &[char]| -> Option<usize> {
        let mut count = 0;
        let mut next_unit = 0;
        let mut number = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() || (c == '-' && number.is_empty()) || c == '.' {
                number.push(c);
                continue;
            }
            let at = units[next_unit..].iter().position(|u| *u == c)? + next_unit;
            if number.trim_start_matches('-').is_empty() || number.parse::<f64>().is_err() {
                return None;
            }
            number.clear();
            next_unit = at + 1;
            count += 1;
        }
        number.is_empty().then_some(count)
    };
    let date_parts = parts(date, &['Y', 'M', 'W', 'D']);
    let time_parts = time.map(|tm| parts(tm, &['H', 'M', 'S']));
    match (date_parts, time_parts) {
        (Some(d), None) => d > 0,
        // A `T` is followed by at least one time part
        (Some(_), Some(Some(tm))) => tm > 0,
        _ => false,
    }
}

/// An IP address with an optional prefix length; `network` also requires no host bits.
fn network(t: &str, network: bool) -> bool {
    let (addr, prefix) = match t.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (t, None),
    };
    let Ok(ip) = addr.parse::<IpAddr>() else {
        return false;
    };
    let bits = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix.map(str::parse::<u32>) {
        None => bits,
        Some(Ok(p)) if p <= bits => p,
        _ => return false,
    };
    if !network {
        return true;
    }
    let value: u128 = match ip {
        IpAddr::V4(v4) => u32::from(v4) as u128,
        IpAddr::V6(v6) => u128::from(v6),
    };
    let host_bits = bits - prefix;
    host_bits == 0 || value & ((1u128 << host_bits) - 1) == 0
}

/// Hex bytes separated by `:` or `-` (or not at all): 6, or also 8 for `macaddr8`.
fn mac_address(t: &str, eight: bool) -> bool {
    let hex: String = t
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect();
    let separators_ok = t.chars().filter(|c| matches!(c, ':' | '-' | '.')).count() <= 7;
    let sizes: &[usize] = if eight { &[12, 16] } else { &[12] };
    separators_ok && sizes.contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_limits() {
        for (text, p, s, fits) in [
            ("12.34", 4, 2, true),
            ("123.4", 4, 2, false),
            ("99.994", 4, 2, true),
            ("99.995", 4, 2, false),
            ("-99.99", 4, 2, true),
            ("0.001", 4, 2, true),
            ("0.005", 3, 3, true),
            ("0.9995", 3, 3, false),
            ("1e3", 4, 0, true),
            ("1e4", 4, 0, false),
            ("12345", 3, -2, true),
            ("99999", 3, -2, false),
            ("0", 1, 0, true),
            ("5", 2, 2, false),
        ] {
            assert_eq!(
                fits_numeric(text, p, s),
                fits,
                "{text} in numeric({p}, {s})"
            );
        }
    }

    #[test]
    fn decimals() {
        for ok in ["1", "-1.5", "+.5", "5.", "1e10", "1.5E-3"] {
            assert!(is_decimal(ok), "{ok}");
        }
        for bad in ["", ".", "abc", "1.2.3", "1e", "NaN", "1,5"] {
            assert!(!is_decimal(bad), "{bad}");
        }
    }

    #[test]
    fn durations() {
        for ok in [
            "P1D",
            "PT2H",
            "P1Y2M3DT4H5M6.5S",
            "P2W",
            "-P1D",
            "P-1DT-2H",
            "PT0S",
            "P1.5D",
        ] {
            assert!(is_iso_duration(ok), "{ok}");
        }
        for bad in ["P", "PT", "1 day", "P1H", "PT1D", "P1M1Y", "P1DT", "P1..5D"] {
            assert!(!is_iso_duration(bad), "{bad}");
        }
    }

    #[test]
    fn networks_and_macs() {
        assert!(network("10.0.0.1/24", false) && network("::1", false));
        assert!(network("10.0.0.0/24", true) && !network("10.0.0.1/24", true));
        assert!(!network("10.0.0.1/33", false) && !network("nope", false));
        assert!(mac_address("08:00:2b:01:02:03", false) && mac_address("08-00-2b-01-02-03", false));
        assert!(
            !mac_address("08:00:2b:01:02", false) && mac_address("08:00:2b:01:02:03:04:05", true)
        );
    }

    #[test]
    fn offsets_and_ranges() {
        assert_eq!(split_offset("12:30:00+02"), Some(("12:30:00", "+02")));
        assert_eq!(split_offset("12:30:00-05:30"), Some(("12:30:00", "-05:30")));
        assert_eq!(split_offset("12:30:00Z"), Some(("12:30:00", "Z")));
        assert_eq!(split_offset("12:30:00"), None);
        let range = range_literal(
            &PgType::Int4,
            &serde_json::json!({ "start": { "value": 1 }, "end": { "value": 5 } }),
            "r",
        );
        assert_eq!(range, Value::String("[\"1\",\"5\")".to_string()));
        let range = range_literal(
            &PgType::Int4,
            &serde_json::json!({ "end": { "value": 5, "inclusive": true } }),
            "r",
        );
        assert_eq!(range, Value::String("(,\"5\"]".to_string()));
    }

    #[test]
    fn like_patterns_cannot_end_with_the_escape_character() {
        start();
        for ok in [
            "abc",
            "a%",
            "50\\%",
            "ends with an escaped backslash \\\\",
            "",
        ] {
            assert_eq!(like_pattern(ok.to_string(), "p"), ok);
        }
        assert!(finish().is_empty());
        for bad in ["abc\\", "\\", "a\\\\\\"] {
            like_pattern(bad.to_string(), "p");
        }
        let errors = finish();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].message.contains("escape character"));
    }

    #[test]
    fn validity_is_checked_without_touching_the_errors_collected() {
        start();
        convert(&PgType::Uuid, Value::String("nope".to_string()), "first");
        assert!(is_valid(
            &PgType::Uuid,
            Value::String("00000000-0000-0000-0000-000000000001".to_string())
        ));
        assert!(!is_valid(&PgType::Int4, Value::String("x".to_string())));
        let errors = finish();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path.as_deref(), Some("first"));
    }
}
