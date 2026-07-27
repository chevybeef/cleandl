use std::error::Error;
use vergen_gitcl::{Emitter, GitclBuilder};

fn main() -> Result<(), Box<dyn Error>> {
    let gitcl = GitclBuilder::default()
        .sha(true) // VERGEN_GIT_SHA (short by default in recent versions; see note)
        .dirty(true) // VERGEN_GIT_DIRTY
        .build()?;

    Emitter::default().add_instructions(&gitcl)?.emit()?;

    Ok(())
}
