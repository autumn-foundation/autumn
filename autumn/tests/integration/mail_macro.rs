#![cfg(feature = "mail")]

use autumn_web::prelude::*;

struct AccountMailer;

#[mailer]
impl AccountMailer {
    fn reset_password(&self, to: String, token: String) -> Mail {
        let _ = std::mem::size_of_val(self);
        Mail::builder()
            .to(to)
            .subject("Reset your password")
            .html(html! { p { "Token: " (token) } })
            .text(token)
            .build()
            .expect("valid mail")
    }

    fn welcome<T>(&self, to: T) -> Mail
    where
        T: Into<String>,
    {
        let _ = std::mem::size_of_val(self);
        Mail::builder()
            .to(to.into())
            .subject("Welcome")
            .text("hello")
            .build()
            .expect("valid mail")
    }

    fn welcome_with_marker<T>(&self, to: String) -> Mail
    where
        T: Default,
    {
        let _ = std::mem::size_of_val(self);
        let _ = T::default();
        Mail::builder()
            .to(to)
            .subject("Welcome")
            .text("hello")
            .build()
            .expect("valid mail")
    }

    fn welcome_borrowed(&self, to: &str) -> Mail {
        let _ = std::mem::size_of_val(self);
        Mail::builder()
            .to(to)
            .subject("Welcome")
            .text("hello")
            .build()
            .expect("valid mail")
    }

    fn named_mailer_arg(&self, mailer: String) -> Mail {
        let _ = std::mem::size_of_val(self);
        Mail::builder()
            .to(mailer)
            .subject("Welcome")
            .text("hello")
            .build()
            .expect("valid mail")
    }
}

#[mailer_preview]
impl AccountMailer {
    fn reset_password_preview() -> Mail {
        Mail::builder()
            .to("preview@example.com")
            .subject("Preview reset")
            .html(html! { p { "Preview token" } })
            .text("Preview token")
            .build()
            .expect("valid preview mail")
    }
}

struct GenericAccountMailer<T> {
    marker: std::marker::PhantomData<T>,
}

#[mailer]
impl<T> GenericAccountMailer<T>
where
    T: Clone + Sync,
{
    fn welcome(&self, to: String) -> Mail {
        let _ = std::mem::size_of_val(self);
        Mail::builder()
            .to(to)
            .subject("Welcome")
            .text("hello")
            .build()
            .expect("valid mail")
    }
}

#[test]
fn mailer_macro_generates_send_helpers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = AccountMailer;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(account.send_reset_password(
            &mailer,
            "user@example.com".to_owned(),
            "abc123".to_owned(),
        ))
        .expect("send helper should work");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        1
    );
}

#[test]
fn mailer_preview_macro_registers_zero_arg_methods() {
    let previews = mail_previews![AccountMailer];

    assert_eq!(previews.len(), 1);
    assert_eq!(previews[0].mailer(), "AccountMailer");
    assert_eq!(previews[0].method(), "reset_password_preview");

    let mail = previews[0].render().expect("preview should render");
    assert_eq!(mail.subject, "Preview reset");
    assert_eq!(mail.to, ["preview@example.com"]);
}

#[test]
fn mailer_macro_supports_generic_mailers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = GenericAccountMailer::<String> {
        marker: std::marker::PhantomData,
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(account.send_welcome(&mailer, "user@example.com".to_owned()))
        .expect("generic send helper should work");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        1
    );
}

#[test]
fn mailer_macro_supports_generic_template_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = AccountMailer;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(account.send_welcome(&mailer, "user@example.com"))
        .expect("generic method send helper should work");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        1
    );
}

#[test]
fn mailer_macro_supports_non_inferable_generic_template_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = AccountMailer;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(
            account.send_welcome_with_marker::<String>(&mailer, "user@example.com".to_owned()),
        )
        .expect("non-inferable generic method send helper should work");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        1
    );
}

#[test]
fn mailer_macro_deliver_later_helper_does_not_panic_without_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = AccountMailer;
    account.deliver_later_reset_password(
        &mailer,
        "user@example.com".to_owned(),
        "abc123".to_owned(),
    );

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        0
    );
}

#[test]
fn mailer_macro_supports_lifetime_generic_template_methods() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = AccountMailer;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(account.send_welcome_borrowed(&mailer, "user@example.com"))
        .expect("lifetime-generic method send helper should work");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        1
    );
}

#[test]
fn mailer_macro_supports_template_methods_named_mailer_arg() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mailer = Mailer::builder()
        .from("noreply@example.com")
        .transport(Transport::File)
        .file_dir(dir.path())
        .build()
        .expect("mailer should build");

    let account = AccountMailer;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(account.send_named_mailer_arg(&mailer, "user@example.com".to_owned()))
        .expect("named mailer arg send helper should work");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("mail dir exists")
            .count(),
        1
    );
}
