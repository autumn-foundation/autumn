use autumn_web::collaboration::{ActorId, OperationId, TextOperation, TextState};

fn main() {
    let mut note = TextState::default();
    for (actor, value) in [("alice", 'A'), ("bob", 'B')] {
        let applied = note.apply(TextOperation::Insert {
            id: OperationId {
                actor: ActorId(actor.into()),
                sequence: 1,
            },
            after: None,
            value,
        });
        if let Err(error) = applied {
            eprintln!("could not apply example operation: {error}");
            return;
        }
    }
    println!("Collaborative note: {}", note.text());
}
