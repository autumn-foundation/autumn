//! TEMPORARY review fixture — macro hygiene stress for #1747.
#![allow(dead_code, unused_variables, clippy::unused_async)]

use autumn_web::prelude::*;

// 1. Handler in a cfg-gated module (gated OFF).
#[cfg(feature = "never-enabled-xyz")]
pub mod gated_off {
    use autumn_web::prelude::*;
    #[get("/stress/gated")]
    pub async fn gated() -> String {
        String::new()
    }
}

// 2. Handler declared INSIDE a function body.
pub fn declares_a_handler() {
    #[get("/stress/inner")]
    pub async fn inner() -> String {
        String::new()
    }
}

// 3. Local `mod core` shadowing + local `file!`/`module_path!` macro_rules.
pub mod shadow {
    use autumn_web::prelude::*;

    pub mod core {
        pub struct NotTheRealCore;
    }

    macro_rules! file {
        () => {
            "hijacked"
        };
    }
    macro_rules! line {
        () => {
            0u32
        };
    }
    macro_rules! module_path {
        () => {
            "hijacked"
        };
    }
    macro_rules! concat {
        ($($t:tt)*) => {
            "hijacked"
        };
    }
    macro_rules! stringify {
        ($($t:tt)*) => {
            "hijacked"
        };
    }
    pub(crate) use {concat, file, line, module_path, stringify};

    #[get("/stress/shadow")]
    pub async fn shadowed() -> String {
        String::new()
    }

    #[autumn_web::static_get("/stress/shadow-static")]
    pub async fn shadowed_static() -> String {
        String::new()
    }
}

// 4. Stacked attributes, both orders.
#[secured]
#[get("/stress/secured-outer")]
pub async fn secured_outer(session: Session) -> String {
    String::new()
}

#[get("/stress/secured-inner")]
#[secured]
pub async fn secured_inner(session: Session) -> String {
    String::new()
}

#[throttle(limit = 5, per = "1m", key = "ip")]
#[get("/stress/throttle-outer")]
pub async fn throttle_outer() -> String {
    String::new()
}

#[get("/stress/throttle-inner")]
#[throttle(limit = 5, per = "1m", key = "ip")]
pub async fn throttle_inner() -> String {
    String::new()
}

#[feature_flag("beta_stress")]
#[get("/stress/flag-outer")]
pub async fn flag_outer() -> String {
    String::new()
}

#[get("/stress/flag-inner")]
#[feature_flag("beta_stress")]
pub async fn flag_inner() -> String {
    String::new()
}

// 5. static_get.
#[autumn_web::static_get("/stress/static")]
pub async fn stress_static() -> String {
    String::new()
}
