fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

pub struct AppVars {
    pub name: String,
}

impl AppVars {
    pub fn new() -> Self {
        let shortname = env!("CARGO_PKG_NAME");
        let name = shortname
            .split('-')
            .map(capitalize)
            .collect::<Vec<String>>()
            .join(" ");

        Self { name }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capitalize_empty() {
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn test_capitalize_single() {
        assert_eq!(capitalize("a"), "A");
    }

    #[test]
    fn test_capitalize_word() {
        assert_eq!(capitalize("hello"), "Hello");
    }

    #[test]
    fn test_capitalize_already_capitalized() {
        assert_eq!(capitalize("Hello"), "Hello");
    }

    #[test]
    fn test_app_vars_name() {
        let vars = AppVars::new();
        // CARGO_PKG_NAME = "mic-mute" -> "Mic Mute"
        assert_eq!(vars.name, "Mic Mute");
    }
}
