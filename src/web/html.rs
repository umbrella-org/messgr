use crate::sms::model::SmsMessage;

const PAGE: &str = include_str!("templates/index.html");
const ROW: &str = include_str!("templates/row.html");
const EMPTY_ROW: &str = include_str!("templates/empty-row.html");
const RESULTS: &str = include_str!("templates/results.html");
const PAGINATION: &str = include_str!("templates/pagination.html");
const PAGINATION_LINK: &str = include_str!("templates/pagination-link.html");
const PAGINATION_DISABLED: &str = include_str!("templates/pagination-disabled.html");
pub const STYLE: &str = include_str!("static/style.css");

pub fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn rows(messages: &[SmsMessage]) -> String {
    if messages.is_empty() {
        return EMPTY_ROW.to_string();
    }

    messages
        .iter()
        .map(|m| {
            ROW.replace("{sender}", &escape(&m.sender))
                .replace("{recipient}", &escape(&m.recipient))
                .replace("{body}", &escape(&m.body))
                .replace("{received_at_iso}", &m.received_at.to_rfc3339())
                .replace(
                    "{received_at_display}",
                    &m.received_at.format("%b %d, %H:%M").to_string(),
                )
        })
        .collect()
}

pub fn results(messages: &[SmsMessage], page: u32, has_next: bool) -> String {
    let pagination_html = if page == 1 && !has_next {
        String::new()
    } else {
        pagination(page, has_next)
    };

    RESULTS
        .replace("{rows}", &rows(messages))
        .replace("{pagination}", &pagination_html)
}

fn pagination(page: u32, has_next: bool) -> String {
    let prev = if page > 1 {
        PAGINATION_LINK
            .replace("{page}", &(page - 1).to_string())
            .replace("{label}", "Previous")
    } else {
        PAGINATION_DISABLED.replace("{label}", "Previous")
    };

    let next = if has_next {
        PAGINATION_LINK
            .replace("{page}", &(page + 1).to_string())
            .replace("{label}", "Next")
    } else {
        PAGINATION_DISABLED.replace("{label}", "Next")
    };

    PAGINATION
        .replace("{prev}", &prev)
        .replace("{next}", &next)
        .replace("{page}", &page.to_string())
}

pub fn page() -> &'static str {
    PAGE
}
