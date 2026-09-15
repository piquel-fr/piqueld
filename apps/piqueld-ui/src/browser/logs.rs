use leptos::*;

/// Shared, accessible presentation for application and build output.
#[component]
pub(super) fn LogViewer(
    text: String,
    #[prop(into)] label: String,
    #[prop(into)] empty: String,
) -> impl IntoView {
    view! {
        <pre class="application-logs" tabindex="0" role="region" aria-label=label>
            {if text.is_empty() { empty } else { text }}
        </pre>
    }
}
