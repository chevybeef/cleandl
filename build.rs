use std::error::Error;
use vergen_gitcl::{Emitter, GitclBuilder};

fn main() -> Result<(), Box<dyn Error>> {
    let gitcl = GitclBuilder::default().sha(true).dirty(true).build()?;

    Emitter::default()
        .fail_on_error()
        .add_instructions(&gitcl)?
        .emit()?;

    // Fix: Force Cargo to re-evaluate build.rs when files change
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");

    Ok(())
}
