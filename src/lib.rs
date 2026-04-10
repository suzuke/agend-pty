pub mod config;
pub mod vterm;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_backend_function() {
        let yaml = "backend: claude\nmodel: opus";
        let defaults: config::Defaults = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(defaults.backend, "claude");
    }

    #[test]
    fn test_empty_defaults() {
        let yaml = "{}";
        let defaults: config::Defaults = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(defaults.backend, "claude");
    }
}
