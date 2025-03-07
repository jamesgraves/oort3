use crate::{error, Error};
use http::StatusCode;
use lazy_static::lazy_static;
use regex::Regex;

pub fn check(text: &str) -> Result<(), Error> {
    lazy_static! {
        static ref RE: Regex =
            Regex::new(r#"\b(unsafe|extern|crate)\b|\b(macro_rules|include|include_bytes|include_str)(\b|!)|([^']static\b|^static\b)"#).unwrap();
    }
    if let Some(m) = RE.find(text) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            format!("Code did not pass sanitizer (found {:?})", m.as_str()),
        ));
    }

    lazy_static! {
        static ref ATTR_RE: Regex = Regex::new(r#"#!?\[([^\[\] ]*)"#).unwrap();
        static ref ALLOWED_RE: Regex =
            Regex::new(r#"derive|repr|inline|cfg\(test\)|test|must_use|default\b"#).unwrap();
    }
    for m in ATTR_RE.captures_iter(text) {
        if ALLOWED_RE.is_match(&m[1]) {
            continue;
        }
        return Err(error(
            StatusCode::BAD_REQUEST,
            format!("Code did not pass sanitizer (found {:?})", &m[0]),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unsafe() {
        assert!(check("... unsafe ...").is_err());
        assert!(check("... }unsafe{ ...").is_err());
    }

    #[test]
    fn test_static() {
        assert!(check("... static ...").is_err());
        assert!(check("static ...").is_err());
    }

    #[test]
    fn test_static_lifetime() {
        assert!(check("... 'static ...").is_ok());
    }

    #[test]
    fn test_extern() {
        assert!(check("... extern ...").is_err());
    }

    #[test]
    fn test_crate() {
        assert!(check("... crate ...").is_err());
    }

    #[test]
    fn test_macros() {
        assert!(check("... macro_rules! ...").is_err());
        assert!(check("... include! ...").is_err());
        assert!(check("... include_bytes! ...").is_err());
        assert!(check("... include_str! ...").is_err());
    }

    #[test]
    fn test_inside_words() {
        assert!(check("... foounsafe {} ...").is_ok());
        assert!(check("... unsafefoo {} ...").is_ok());
        assert!(check("... staticfoo {} ...").is_ok());
        assert!(check("... externfoo {} ...").is_ok());
        assert!(check("... cratefoo {} ...").is_ok());
    }

    #[test]
    fn derive_attr() {
        assert!(check("... #[derive(Clone)] ...").is_ok());
        assert!(check("... #[derive(Debug, Serialize, Deserialize)] ...").is_ok());
    }

    #[test]
    fn test_attr() {
        assert!(check("... #[test] ...").is_ok());
    }

    #[test]
    fn cfg_test_attr() {
        assert!(check("... #[cfg(test)] ...").is_ok());
    }

    #[test]
    fn repr_attr() {
        assert!(check("... #[repr(u32)] ...").is_ok());
    }

    #[test]
    fn inline_attr() {
        assert!(check("... #[inline(always)] ...").is_ok());
    }

    #[test]
    fn must_use_attr() {
        assert!(check("... #[must_use] ...").is_ok());
    }

    #[test]
    fn default_attr() {
        assert!(check("... #[default] ...").is_ok());
    }

    #[test]
    fn path_attr() {
        assert!(check("... #[path = \"/dev/random\"] ...").is_err());
        assert!(check("... #[\npath = \"/dev/random\"] ...").is_err());
        assert!(check("... #[\t  path\n= \"/dev/random\"] ...").is_err());
    }

    #[test]
    fn other_attrs() {
        assert!(check("... #[link] ...").is_err());
        assert!(check("... #![no_std] ...").is_err());
    }
}
