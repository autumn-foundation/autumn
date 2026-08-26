//! The invite email, sent through the shipped Mail stack (`#[mailer]`).
//!
//! A scaffolded, overridable template (issue #1261 AC4): edit
//! [`InvitationMailer::invite`] to change copy/branding, or replace `.html`
//! with your own Maud partial.

use autumn_web::prelude::*;

pub struct InvitationMailer;

#[mailer]
impl InvitationMailer {
    /// `accept_url` is the fully-qualified `/invitations/{token}` link; the
    /// token itself is never logged or persisted in the clear (see
    /// `routes::invitations::create_invitation`).
    pub fn invite(
        &self,
        to: String,
        organization_name: String,
        role: String,
        accept_url: String,
    ) -> Mail {
        Mail::builder()
            .to(to)
            .subject(format!("You're invited to join {organization_name}"))
            .html(html! {
                p {
                    "You've been invited to join " strong { (organization_name) }
                    " as " (role) "."
                }
                p {
                    a href=(accept_url) { "Accept the invitation" }
                }
                p class="text-sm text-gray-500" { "This invitation expires in 7 days." }
            })
            .text(format!(
                "You've been invited to join {organization_name} as {role}.\n\n\
                 Accept: {accept_url}\n\n\
                 This invitation expires in 7 days."
            ))
            .build()
            .expect("static invite template should be valid")
    }
}

#[mailer_preview]
impl InvitationMailer {
    fn invite_preview() -> Mail {
        InvitationMailer.invite(
            "preview@example.com".to_owned(),
            "Acme, Inc.".to_owned(),
            "admin".to_owned(),
            "https://example.com/invitations/preview-token".to_owned(),
        )
    }
}

pub fn mail_previews() -> Vec<MailPreview> {
    mail_previews![InvitationMailer]
}
