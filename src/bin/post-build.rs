use std::{env, io, io::Write, process::Command};

#[derive(Debug)]
#[allow(dead_code)]
struct BuildVars {
    name: &'static str,
    version: &'static str,
    target: String,
}

impl BuildVars {
    fn new() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            target: env::var("TARGET").expect("Failed to provide build target"),
        }
    }
}

fn run(cmd: String) {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .expect("Failed to run command");
    io::stdout().write_all(&output.stdout).unwrap();
    io::stderr().write_all(&output.stderr).unwrap();
}

fn create_dmg(vars: BuildVars) {
    let build_title = format!("{}-{}-{}", vars.name, vars.version, vars.target);
    let dmg_file_name = format!("{}.dmg", build_title);
    let source = format!("./target/{}/release/bundle/osx", vars.target);

    let cmd = format!(
        "hdiutil create -volname {title} -srcfolder {source} -ov -format UDZO {file}",
        title = build_title,
        source = source,
        file = dmg_file_name
    );
    run(cmd);
}

fn main() {
    let build_vars = BuildVars::new();
    println!("Running with build vars: {:?}", build_vars);
    create_dmg(build_vars);
}
