//! Human-readable time formatting for browser views.
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
