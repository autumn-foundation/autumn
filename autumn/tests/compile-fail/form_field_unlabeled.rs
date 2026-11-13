//! Issue #3395: the field helpers take the label as a positional argument.
//! `FieldOptions` adds a hint and attributes, but it has no label setter, so
//! a field with options and no label does not compile.

use autumn_web::form::{Changeset, FieldOptions, text_input_with};

fn main() {
    let changeset = Changeset::new(());
    let options = FieldOptions::new().hint("We never share it").placeholder("you@example.com");
    let _markup = text_input_with(&changeset, "email", &options);
}
