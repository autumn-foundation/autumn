// An unrelated attribute that happens to share #[authorize]'s exact argument
// grammar ("action", resource = Type) is just as ambiguous as an alias would
// be -- Autumn cannot tell them apart from tokens alone, so both are refused
// rather than one being silently accepted (Codex review on #2628, second
// finding: a first fix attempt that tried to disambiguate by requiring a
// matching parameter binding still collided with exactly this shape).
use autumn_web::post;

struct Note;

#[post("/notes/{id}")]
#[audit("update", resource = Note)]
async fn update_note(note: Note) -> &'static str {
    let _ = note;
    "ok"
}

fn main() {}
