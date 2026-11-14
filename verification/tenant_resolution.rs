use vstd::prelude::*;

verus! {

/// `tenancy.source`.
pub enum Source {
    Session,
    Jwt,
    Subdomain,
    Header,
}

/// `tenancy.header_trust`. `Missing` is `None`.
pub enum Trust {
    Missing,
    TrustedProxy,
    AnyClient,
}

/// The shadow of one request. A tenant id is the `u64` rank of its string.
pub struct Req {
    pub source: Source,
    pub trust: Trust,
    /// The peer address is in `security.trusted_proxies.ranges`.
    pub peer_trusted: bool,
    /// The number of values of the tenant header.
    pub header_count: u64,
    /// The value of the tenant header.
    pub header_tenant: u64,
    /// The value of `tenancy.session_key`, or `None` when it is absent or empty.
    pub session_tenant: Option<u64>,
    /// The tenant from a token or a host name, or `None` when the lookup fails.
    pub other_tenant: Option<u64>,
}

/// `check_header_sender` in `autumn/src/tenancy.rs`.
pub open spec fn header_allowed(r: Req) -> bool {
    match r.trust {
        Trust::Missing => false,
        Trust::TrustedProxy => r.peer_trusted,
        Trust::AnyClient => true,
    }
}

/// The tenant that the configured source finds, before the session check.
pub open spec fn looked_up(r: Req) -> Option<u64> {
    match r.source {
        Source::Session => r.session_tenant,
        Source::Jwt => r.other_tenant,
        Source::Subdomain => r.other_tenant,
        Source::Header => if header_allowed(r) && r.header_count == 1 {
            Some(r.header_tenant)
        } else {
            None
        },
    }
}

/// `check_session_tenant`: a session that holds a tenant binds the request.
pub open spec fn bound(r: Req, t: u64) -> bool {
    match r.source {
        Source::Session => true,
        _ => match r.session_tenant {
            None => true,
            Some(s) => s == t,
        },
    }
}

/// The tenant that `extract_tenant_impl` returns. `None` is a rejection.
pub open spec fn outcome(r: Req) -> Option<u64> {
    match looked_up(r) {
        Some(t) => if bound(r, t) {
            Some(t)
        } else {
            None
        },
        None => None,
    }
}

/// The header source with no `header_trust` never selects a tenant.
pub proof fn missing_trust_never_accepts(r: Req)
    requires
        r.source is Header,
        r.trust is Missing,
    ensures
        outcome(r) is None,
{
}

/// With `trusted_proxy`, only a trusted peer selects a tenant by header.
pub proof fn trusted_proxy_needs_a_trusted_peer(r: Req)
    requires
        r.source is Header,
        r.trust is TrustedProxy,
        !r.peer_trusted,
    ensures
        outcome(r) is None,
{
}

/// Two or more header values never select a tenant.
pub proof fn many_header_values_never_accept(r: Req)
    requires
        r.source is Header,
        r.header_count != 1,
    ensures
        outcome(r) is None,
{
}

/// A session that holds a tenant binds every source.
pub proof fn the_session_tenant_binds(r: Req, s: u64, t: u64)
    requires
        r.session_tenant == Some(s),
        outcome(r) == Some(t),
    ensures
        t == s,
{
}

/// A tenant-A user cannot select tenant B by header, even with `any_client`.
pub proof fn tenant_a_cannot_select_tenant_b(r: Req, a: u64, b: u64)
    requires
        r.source is Header,
        r.session_tenant == Some(a),
        r.header_tenant == b,
        a != b,
    ensures
        outcome(r) is None,
{
}

/// Models `extract_tenant_impl` in `autumn/src/tenancy.rs`.
pub fn resolve(r: &Req) -> (o: Option<u64>)
    ensures
        o == outcome(*r),
{
    let found: Option<u64> = match r.source {
        Source::Session => r.session_tenant,
        Source::Jwt => r.other_tenant,
        Source::Subdomain => r.other_tenant,
        Source::Header => {
            let allowed = match r.trust {
                Trust::Missing => false,
                Trust::TrustedProxy => r.peer_trusted,
                Trust::AnyClient => true,
            };
            if allowed && r.header_count == 1 {
                Some(r.header_tenant)
            } else {
                None
            }
        },
    };
    match found {
        None => None,
        Some(t) => {
            let from_session = match r.source {
                Source::Session => true,
                _ => false,
            };
            if from_session {
                Some(t)
            } else {
                match r.session_tenant {
                    None => Some(t),
                    Some(s) => if s == t {
                        Some(t)
                    } else {
                        None
                    },
                }
            }
        },
    }
}

fn main() {}

} // verus!
