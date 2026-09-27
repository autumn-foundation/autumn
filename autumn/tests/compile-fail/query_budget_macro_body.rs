//! A macro body is opaque token soup to the analysis. One that names the
//! database handle could hide any number of queries, so it is reported rather
//! than assumed query-free — the difference between a conservative gate and a
//! false negative.
//!
//! The stand-in macro discards its tokens, which is what leaves `db` unused —
//! not something this fixture is testing, so the lint is silenced to keep the
//! golden output the macro diagnostic alone.
#![allow(unused_mut, unused_variables)]

use autumn_web::query_budget;

struct Db;

macro_rules! render {
    ($($tokens:tt)*) => {
        String::new()
    };
}

async fn fetch_title(_db: &mut Db) -> Result<String, ()> {
    Ok(String::new())
}

#[query_budget(5)]
async fn show(mut db: Db) -> Result<String, ()> {
    Ok(render! { title: fetch_title(&mut db).await? })
}

fn main() {}
