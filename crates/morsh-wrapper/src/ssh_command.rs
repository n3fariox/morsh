//! Match mosh.pl's shellwords parsing of --ssh without invoking a shell.

pub(crate) fn parse(value: &str) -> Result<Vec<String>, String> {
    let args = shell_words::split(value).map_err(|e| format!("invalid --ssh command: {e}"))?;
    if args.first().is_none_or(String::is_empty) {
        return Err("--ssh requires a nonempty command".into());
    }
    Ok(args)
}

pub(crate) fn validate(value: &str) -> Result<String, String> {
    parse(value)?;
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_executable_and_option_values_are_single_arguments() {
        assert_eq!(
            parse("'/opt/my ssh' -o 'IdentityFile=/home/me/key file'").unwrap(),
            ["/opt/my ssh", "-o", "IdentityFile=/home/me/key file"]
        );
    }

    #[test]
    fn whitespace_and_unterminated_quotes_are_errors() {
        for value in ["", " ", "''", "ssh 'unfinished"] {
            assert!(parse(value).is_err());
        }
    }
}
