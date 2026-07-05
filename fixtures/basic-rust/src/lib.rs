// dangerous comment with utf8: привет
fn dangerous_parser() -> usize {
    1
}

fn safe_helper() -> &'static str {
    "dangerous runtime string should stay real"
}

#[cfg(test)]
mod tests {
    #[test]
    fn string_fixture_is_sanitized() {
        let case = "dangerous fixture text";
        assert!(case.contains("dangerous"));
    }
}
