//! Human-readable time formatting for browser views.
use crate::overview::Age;
use leptos::wasm_bindgen::JsValue;

/// The current time in Unix milliseconds.
#[expect(
    clippy::cast_possible_truncation,
    reason = "Date::now returns integral Unix milliseconds within i64's range"
)]
pub(super) fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// Full local date and time, used in tooltips and detail views.
pub(super) fn timestamp(milliseconds: i64) -> String {
    date(milliseconds)
        .to_locale_string("default", &JsValue::UNDEFINED)
        .into()
}

/// Compact age such as "4 min ago" or "in 3 d"; from a week on, the date.
pub(super) fn relative(milliseconds: i64) -> String {
    Age::at(milliseconds, now_ms()).label().unwrap_or_else(|| {
        date(milliseconds)
            .to_locale_date_string("default", &JsValue::UNDEFINED)
            .into()
    })
}

#[expect(
    clippy::cast_precision_loss,
    reason = "browser timestamps fit within f64's exact integer range"
)]
fn date(milliseconds: i64) -> js_sys::Date {
    js_sys::Date::new(&JsValue::from_f64(milliseconds as f64))
}
