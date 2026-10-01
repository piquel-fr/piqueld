//! Human-readable time, duration, and size formatting for browser views.
// JavaScript numbers are doubles. Browser timestamps and durations stay far
// below 2^53, so converting between them and integers is exact in practice.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "browser timestamps fit within f64's exact integer range"
)]
use leptos::wasm_bindgen::JsValue;

/// The current time in Unix milliseconds.
pub(super) fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// Full local date and time, used in tooltips and detail views.
pub(super) fn timestamp(milliseconds: i64) -> String {
    js_sys::Date::new(&JsValue::from_f64(milliseconds as f64))
        .to_locale_string("default", &JsValue::UNDEFINED)
        .into()
}

/// Compact age such as "4 min ago" or "in 3 d"; older than a week shows the date.
pub(super) fn relative(milliseconds: i64) -> String {
    let delta = now_ms() - milliseconds;
    let future = delta < 0;
    let seconds = delta.unsigned_abs() / 1000;
    let text = match seconds {
        0..=44 => {
            return if future {
                "in a moment".into()
            } else {
                "just now".into()
            };
        }
        45..=3_569 => format!("{} min", (seconds + 30) / 60),
        3_570..=86_399 => format!("{} h", (seconds / 3_600).max(1)),
        86_400..=604_799 => format!("{} d", seconds / 86_400),
        _ => {
            return js_sys::Date::new(&JsValue::from_f64(milliseconds as f64))
                .to_locale_date_string("default", &JsValue::UNDEFINED)
                .into();
        }
    };
    if future {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

/// Elapsed time such as "850 ms", "3.2 s", or "4 min 12 s".
pub(super) fn duration(milliseconds: i64) -> String {
    let milliseconds = milliseconds.max(0);
    match milliseconds {
        0..=999 => format!("{milliseconds} ms"),
        1_000..=59_949 => format!("{:.1} s", milliseconds as f64 / 1_000.0),
        59_950..=3_599_999 => {
            let seconds = (milliseconds + 500) / 1_000;
            format!("{} min {} s", seconds / 60, seconds % 60)
        }
        _ => format!(
            "{} h {} min",
            milliseconds / 3_600_000,
            (milliseconds % 3_600_000) / 60_000
        ),
    }
}

/// Elapsed time from a fractional millisecond measurement.
pub(super) fn duration_f64(milliseconds: f64) -> String {
    duration(milliseconds as i64)
}

/// Uptime-style durations such as "3 d 4 h" or "12 min".
pub(super) fn duration_secs(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        (seconds % 86_400) / 3_600,
        (seconds % 3_600) / 60,
    );
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} min")
    } else if minutes > 0 {
        format!("{minutes} min")
    } else {
        format!("{seconds} s")
    }
}

/// Binary-prefixed sizes with one decimal above a kibibyte.
pub(super) fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
