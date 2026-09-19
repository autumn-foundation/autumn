//! The grant is the whole point of the attribute: `#[agent_operable]` with no
//! `grant = ...` is a typo, not a request to be checked against nothing. One
//! purpose-written diagnostic, and no marker const referencing a grant that
//! was never named.

use autumn_web::agent_operable;

#[agent_operable]
async fn draft() -> Result<(), ()> {
    Ok(())
}

fn main() {}
