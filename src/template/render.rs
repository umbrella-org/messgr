use std::collections::HashMap;

/// A template body cannot be rendered as-is (T-010 decision 3): either a
/// `{{key}}` placeholder has no matching entry in the caller's variable map,
/// or a `{{` is never closed by a `}}`. Neither is passed through as literal
/// text — a bank must not send customer-facing content with an
/// unsubstituted placeholder.
#[derive(Debug)]
pub enum RenderError {
    MissingVariable(String),
    UnterminatedPlaceholder(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingVariable(key) => {
                write!(f, "template render failed: no value supplied for {key:?}")
            }
            Self::UnterminatedPlaceholder(rest) => write!(
                f,
                "template render failed: unterminated placeholder starting at {rest:?}"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// Substitutes every `{{key}}` token in `body` with `variables[key]`
/// (surrounding whitespace inside the braces is trimmed, so `{{ key }}`
/// matches too). A key referenced by the body but absent from `variables` is
/// a hard error; a key present in `variables` but never referenced by the
/// body is silently ignored (T-010 decision 3).
pub fn render(
    body: &str,
    variables: &HashMap<String, String>,
) -> Result<String, RenderError> {
    let mut result = String::with_capacity(body.len());
    let mut rest = body;

    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];

        let Some(end) = after_open.find("}}") else {
            return Err(RenderError::UnterminatedPlaceholder(
                rest[start..].to_string(),
            ));
        };

        let key = after_open[..end].trim();
        let value = variables
            .get(key)
            .ok_or_else(|| RenderError::MissingVariable(key.to_string()))?;
        result.push_str(value);

        rest = &after_open[end + 2..];
    }

    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn no_placeholders_returns_body_unchanged() {
        let body = "Hi there, nothing to substitute.";
        assert_eq!(render(body, &vars(&[])).unwrap(), body);
    }

    #[test]
    fn whitespace_inside_braces_is_trimmed() {
        let rendered = render("Hi {{ name }}.", &vars(&[("name", "Jordan")])).unwrap();
        assert_eq!(rendered, "Hi Jordan.");
    }

    #[test]
    fn unreferenced_extra_variable_is_ignored() {
        let rendered = render(
            "Hi {{name}}.",
            &vars(&[("name", "Jordan"), ("unused", "x")]),
        )
        .unwrap();
        assert_eq!(rendered, "Hi Jordan.");
    }

    #[test]
    fn missing_variable_is_a_hard_error() {
        let err = render("Hi {{name}}.", &vars(&[])).unwrap_err();
        assert!(matches!(err, RenderError::MissingVariable(key) if key == "name"));
    }

    #[test]
    fn unterminated_placeholder_is_a_hard_error() {
        let err = render("Hi {{name.", &vars(&[("name", "Jordan")])).unwrap_err();
        assert!(matches!(err, RenderError::UnterminatedPlaceholder(_)));
    }
}
