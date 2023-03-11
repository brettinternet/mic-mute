fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

pub struct AppVars {
    pub name: String,
    pub shortname: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub repository: &'static str,
    pub license: &'static str,
    pub authors: Vec<String>,
}

impl AppVars {
    pub fn new() -> Self {
        let shortname = env!("CARGO_PKG_NAME");
        let name = shortname
            .split("-")
            .map(capitalize)
            .collect::<Vec<String>>()
            .join(" ");
        let authors: Vec<String> = env!("CARGO_PKG_AUTHORS")
            .split(":")
            .map(|s| s.to_string())
            .collect();

        println!("NAME: {}", name);
        Self {
            name,
            shortname,
            version: env!("CARGO_PKG_VERSION"),
            description: env!("CARGO_PKG_DESCRIPTION"),
            repository: env!("CARGO_PKG_REPOSITORY"),
            license: env!("CARGO_PKG_LICENSE"),
            authors,
        }
    }
}
