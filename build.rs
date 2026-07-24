use std::fs;
use std::path::Path;

fn main() {
    let build_file = Path::new(".build_number");

    // Read current build number or default to 0
    let current_build: u64 = if build_file.exists() {
        fs::read_to_string(build_file)
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    } else {
        0
    };

    let next_build = current_build + 1;
    fs::write(build_file, next_build.to_string()).unwrap();

    // Pass BUILD_NUMBER to cargo as an environment variable
    println!("cargo:rustc-env=BUILD_NUMBER={next_build}");

    // Tell Cargo to re-run this script if .build_number changes
    println!("cargo:rerun-if-changed=.build_number");
}
