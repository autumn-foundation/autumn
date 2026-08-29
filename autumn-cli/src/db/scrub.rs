//! `autumn db scrub` — RED phase (issue #1602).
//!
//! Tests are written first, against the intended API. The implementation lands
//! in the GREEN commit.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use autumn_schema_core::{Backend, Column, ColumnType, ForeignKey, Index, Table};

    use super::*;

    // ── Fixtures ────────────────────────────────────────────────────────────

    fn text_col(name: &str) -> Column {
        Column::new(name, ColumnType::Text)
    }

    fn pk_col(name: &str) -> Column {
        let mut c = Column::new(name, ColumnType::Int64);
        c.primary_key = true;
        c
    }

    /// `users(id PK, email TEXT UNIQUE, full_name TEXT, bio TEXT NULL,
    /// created_at TIMESTAMP)`.
    fn users_table() -> Table {
        let mut t = Table::new("users", Backend::Postgres);
        t.primary_key = vec!["id".to_owned()];
        let mut email = text_col("email");
        email.unique = true;
        let mut bio = text_col("bio");
        bio.nullable = true;
        t.columns = vec![
            pk_col("id"),
            email,
            text_col("full_name"),
            bio,
            Column::new("created_at", ColumnType::Timestamp),
        ];
        t
    }

    fn empty_encrypted() -> BTreeMap<String, BTreeSet<String>> {
        BTreeMap::new()
    }

    fn no_anonymize() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn plan_for(
        tables: &[Table],
        config: &ScrubConfig,
        encrypted: &BTreeMap<String, BTreeSet<String>>,
        anonymize: &BTreeSet<String>,
    ) -> Result<ScrubPlan, ScrubError> {
        build_plan(&ClassificationInputs {
            tables,
            config,
            encrypted,
            anonymize_tables: anonymize,
        })
    }

    // ── Config parsing ──────────────────────────────────────────────────────

    #[test]
    fn config_parses_defaults_safe_and_pii() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]

            [tables.users]
            safe = ["role"]

            [tables.users.pii]
            email = "email"
            full_name = "name"
            "#,
        )
        .expect("config must parse");

        assert_eq!(config.defaults.safe_columns, vec!["id", "created_at"]);
        let users = config.tables.get("users").expect("users rule");
        assert_eq!(users.safe, vec!["role"]);
        assert_eq!(users.pii.get("email"), Some(&Strategy::Email));
        assert_eq!(users.pii.get("full_name"), Some(&Strategy::Name));
    }

    #[test]
    fn config_rejects_unknown_strategy() {
        let err = parse_config_str(
            r#"
            [tables.users.pii]
            email = "obfuscate"
            "#,
        )
        .expect_err("unknown strategy must be rejected");
        assert!(
            err.to_string().contains("obfuscate"),
            "error should name the bad strategy: {err}"
        );
    }

    #[test]
    fn config_rejects_unknown_keys() {
        let err = parse_config_str(
            r#"
            [tables.users]
            saf = ["role"]
            "#,
        )
        .expect_err("a typo'd key must not be silently ignored");
        assert!(err.to_string().contains("saf"), "error: {err}");
    }

    #[test]
    fn empty_config_parses_to_default() {
        assert_eq!(parse_config_str("").unwrap(), ScrubConfig::default());
    }

    // ── Fail-closed classification (AC #3) ──────────────────────────────────

    #[test]
    fn unclassified_columns_are_refused_and_listed() {
        let tables = vec![users_table()];
        let err = plan_for(
            &tables,
            &ScrubConfig::default(),
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("an all-unclassified schema must be refused");

        let ScrubError::Unclassified { columns } = &err else {
            panic!("expected Unclassified, got {err:?}");
        };
        assert_eq!(
            columns,
            &vec![
                "users.bio".to_owned(),
                "users.created_at".to_owned(),
                "users.email".to_owned(),
                "users.full_name".to_owned(),
                "users.id".to_owned(),
            ]
        );
        // The message must be actionable: it names the columns and the file.
        let rendered = err.to_string();
        assert!(rendered.contains("users.email"), "{rendered}");
        assert!(rendered.contains(SCRUB_CONFIG_FILE), "{rendered}");
    }

    #[test]
    fn a_newly_added_column_flips_a_previously_clean_config_to_failure() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            [tables.users]
            safe = []
            [tables.users.pii]
            email = "email"
            full_name = "name"
            bio = "redact"
            "#,
        )
        .unwrap();
        let tables = vec![users_table()];
        plan_for(&tables, &config, &empty_encrypted(), &no_anonymize())
            .expect("the fully-declared schema must pass");

        // Someone adds `users.ssn` and forgets the declaration.
        let mut with_new_column = users_table();
        with_new_column.columns.push(text_col("ssn"));
        let err = plan_for(
            &[with_new_column],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a new undeclared column must fail the scrub");
        assert!(matches!(
            err,
            ScrubError::Unclassified { ref columns } if columns == &vec!["users.ssn".to_owned()]
        ));
    }

    #[test]
    fn stale_config_entries_are_refused() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            [tables.users.pii]
            emial = "email"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a config naming a column that no longer exists must fail");
        assert!(
            matches!(err, ScrubError::StaleConfig { ref entries } if entries.contains(&"users.emial".to_owned())),
            "got {err:?}"
        );
    }

    #[test]
    fn stale_config_table_is_refused() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            [tables.legacy_users]
            safe = ["x"]
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a config table that no longer exists must fail");
        assert!(
            matches!(err, ScrubError::StaleConfig { ref entries } if entries.contains(&"legacy_users".to_owned())),
            "got {err:?}"
        );
    }

    #[test]
    fn a_column_cannot_be_both_safe_and_pii() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users]
            safe = ["email"]
            [tables.users.pii]
            email = "email"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a contradictory declaration must fail");
        assert!(matches!(err, ScrubError::Contradiction { .. }), "{err:?}");
    }

    // ── Automatic classification (AC #2) ────────────────────────────────────

    #[test]
    fn encrypted_columns_are_pii_without_any_declaration() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users]
            safe = []
            "#,
        )
        .unwrap();
        let mut encrypted = BTreeMap::new();
        encrypted.insert(
            "users".to_owned(),
            BTreeSet::from(["email".to_owned()]),
        );

        let plan = plan_for(&[users_table()], &config, &encrypted, &no_anonymize())
            .expect("an #[encrypted] column needs no declaration");
        let column = plan
            .column("users", "email")
            .expect("email must be in the plan");
        assert_eq!(column.source, ClassSource::Encrypted);
    }

    #[test]
    fn safe_cannot_override_an_encrypted_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users]
            safe = ["email"]
            "#,
        )
        .unwrap();
        let mut encrypted = BTreeMap::new();
        encrypted.insert("users".to_owned(), BTreeSet::from(["email".to_owned()]));

        let err = plan_for(&[users_table()], &config, &encrypted, &no_anonymize())
            .expect_err("marking an #[encrypted] column safe must be refused");
        assert!(
            matches!(err, ScrubError::SafeOverridesEncrypted { ref columns } if columns == &vec!["users.email".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn gdpr_anonymize_table_classifies_its_columns_as_pii() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            "#,
        )
        .unwrap();
        let anonymize = BTreeSet::from(["users".to_owned()]);
        let plan = plan_for(&[users_table()], &config, &empty_encrypted(), &anonymize)
            .expect("a GDPR-anonymize table needs no per-column declaration");

        for column in ["email", "full_name", "bio"] {
            let decision = plan
                .column("users", column)
                .unwrap_or_else(|| panic!("{column} must be scrubbed"));
            assert_eq!(decision.source, ClassSource::GdprAnonymize);
        }
        // `id`/`created_at` were explicitly declared safe, so they are untouched.
        assert!(plan.column("users", "id").is_none());
        assert!(plan.column("users", "created_at").is_none());
    }

    #[test]
    fn safe_may_narrow_a_gdpr_anonymize_table() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            [tables.users]
            safe = ["full_name"]
            "#,
        )
        .unwrap();
        let anonymize = BTreeSet::from(["users".to_owned()]);
        let plan = plan_for(&[users_table()], &config, &empty_encrypted(), &anonymize).unwrap();
        assert!(plan.column("users", "full_name").is_none());
        assert!(plan.column("users", "email").is_some());
    }

    #[test]
    fn explicit_pii_wins_over_the_auto_strategy() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "redact"
            "#,
        )
        .unwrap();
        let mut encrypted = BTreeMap::new();
        encrypted.insert("users".to_owned(), BTreeSet::from(["email".to_owned()]));
        let plan = plan_for(&[users_table()], &config, &encrypted, &no_anonymize()).unwrap();
        let column = plan.column("users", "email").unwrap();
        assert_eq!(column.strategy, Strategy::Redact);
        assert_eq!(column.source, ClassSource::Config);
    }

    // ── Constraint safety (AC #4) ───────────────────────────────────────────

    #[test]
    fn pii_on_a_primary_key_is_refused() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["created_at", "email", "full_name", "bio"]
            [tables.users.pii]
            id = "zero"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("scrubbing a primary key would break referencing rows");
        assert!(
            matches!(err, ScrubError::PiiOnKeyColumn { ref columns } if columns == &vec!["users.id".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn pii_on_a_foreign_key_is_refused() {
        let mut posts = Table::new("posts", Backend::Postgres);
        posts.primary_key = vec!["id".to_owned()];
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        posts.columns = vec![pk_col("id"), author];

        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id"]
            [tables.posts.pii]
            author_id = "zero"
            "#,
        )
        .unwrap();
        let err = plan_for(&[posts], &config, &empty_encrypted(), &no_anonymize())
            .expect_err("scrubbing a foreign key would break referential integrity");
        assert!(
            matches!(err, ScrubError::PiiOnKeyColumn { ref columns } if columns == &vec!["posts.author_id".to_owned()]),
            "got {err:?}"
        );
    }

    #[test]
    fn a_gdpr_anonymize_table_never_auto_classifies_its_key_columns() {
        let mut posts = Table::new("posts", Backend::Postgres);
        posts.primary_key = vec!["id".to_owned()];
        let mut author = Column::new("author_id", ColumnType::Int64);
        author.references = Some(ForeignKey::new("users", "id"));
        posts.columns = vec![pk_col("id"), author, text_col("body")];

        let plan = plan_for(
            &[posts],
            &ScrubConfig::default(),
            &empty_encrypted(),
            &BTreeSet::from(["posts".to_owned()]),
        )
        .expect("key columns are structurally safe under a table-level inference");
        assert!(plan.column("posts", "id").is_none());
        assert!(plan.column("posts", "author_id").is_none());
        assert!(plan.column("posts", "body").is_some());
    }

    #[test]
    fn null_strategy_is_refused_on_a_not_null_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "bio"]
            [tables.users.pii]
            full_name = "null"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("NULL into a NOT NULL column must be refused at plan time");
        assert!(matches!(err, ScrubError::NullOnNotNull { .. }), "{err:?}");
    }

    #[test]
    fn null_strategy_is_allowed_on_a_nullable_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name"]
            [tables.users.pii]
            bio = "null"
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .unwrap();
        assert_eq!(plan.column("users", "bio").unwrap().strategy, Strategy::Null);
    }

    #[test]
    fn a_non_injective_strategy_is_refused_on_a_unique_column() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "json"
            "#,
        )
        .unwrap();
        let err = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .expect_err("a constant replacement would violate the unique index");
        assert!(
            matches!(err, ScrubError::NonUniqueStrategy { .. }),
            "got {err:?}"
        );
    }

    // ── Replacement expressions ─────────────────────────────────────────────

    #[test]
    fn row_key_uses_the_primary_key_when_present() {
        assert_eq!(
            row_key_expr(&users_table()),
            r#"coalesce("id"::text, '')"#
        );
    }

    #[test]
    fn row_key_falls_back_to_ctid_without_a_primary_key() {
        let mut t = Table::new("legacy", Backend::Postgres);
        t.columns = vec![text_col("note")];
        assert_eq!(row_key_expr(&t), "ctid::text");
    }

    #[test]
    fn row_key_concatenates_a_composite_primary_key() {
        let mut t = Table::new("memberships", Backend::Postgres);
        t.primary_key = vec!["user_id".to_owned(), "team_id".to_owned()];
        let mut user_id = Column::new("user_id", ColumnType::Int64);
        user_id.primary_key = true;
        let mut team_id = Column::new("team_id", ColumnType::Int64);
        team_id.primary_key = true;
        t.columns = vec![user_id, team_id];
        assert_eq!(
            row_key_expr(&t),
            r#"coalesce("user_id"::text, '') || '|' || coalesce("team_id"::text, '')"#
        );
    }

    #[test]
    fn token_is_salted_per_column_so_two_columns_never_match() {
        let table = users_table();
        assert_ne!(
            token_expr(&table, "email"),
            token_expr(&table, "full_name"),
            "two PII columns of one row must not receive the same fake value"
        );
    }

    #[test]
    fn email_expression_is_unique_per_row_and_uses_a_reserved_domain() {
        let expr = replacement_expr(Strategy::Email, &text_col("email"), "TOK").unwrap();
        assert!(expr.contains("TOK"), "must vary per row: {expr}");
        assert!(
            expr.contains("@example.invalid"),
            "must use a reserved, undeliverable domain: {expr}"
        );
    }

    #[test]
    fn varchar_length_bounds_the_generated_value() {
        let column = Column::new(
            "email",
            ColumnType::Opaque {
                pg_type: "varchar(40)".to_owned(),
            },
        );
        let expr = replacement_expr(Strategy::Email, &column, "TOK").unwrap();
        // `scrubbed+` (9) + token + `@example.invalid` (16) must fit in 40.
        assert!(
            expr.contains("substr(TOK, 1, 15)"),
            "token must be narrowed to fit varchar(40): {expr}"
        );
    }

    #[test]
    fn a_too_narrow_column_is_refused_rather_than_silently_truncated() {
        let column = Column::new(
            "email",
            ColumnType::Opaque {
                pg_type: "varchar(28)".to_owned(),
            },
        );
        let err = replacement_expr(Strategy::Email, &column, "TOK")
            .expect_err("a column too narrow for a unique fake must be refused");
        assert!(matches!(err, ScrubError::ColumnTooNarrow { .. }), "{err:?}");
    }

    #[test]
    fn char_length_is_parsed_from_the_opaque_pg_type() {
        assert_eq!(
            char_max_len(&ColumnType::Opaque {
                pg_type: "varchar(64)".to_owned()
            }),
            Some(64)
        );
        assert_eq!(
            char_max_len(&ColumnType::Opaque {
                pg_type: "char(2)".to_owned()
            }),
            Some(2)
        );
        assert_eq!(char_max_len(&ColumnType::Text), None);
        assert_eq!(
            char_max_len(&ColumnType::Opaque {
                pg_type: "citext".to_owned()
            }),
            None
        );
    }

    #[test]
    fn auto_strategy_is_derived_from_the_column_type() {
        assert_eq!(
            auto_strategy(&text_col("email")).unwrap(),
            Strategy::Email,
            "an email-named text column gets a syntactically valid address"
        );
        assert_eq!(auto_strategy(&text_col("bio")).unwrap(), Strategy::Redact);
        assert_eq!(
            auto_strategy(&Column::new("token", ColumnType::Uuid)).unwrap(),
            Strategy::Uuid
        );
        assert_eq!(
            auto_strategy(&Column::new("blob", ColumnType::Bytes)).unwrap(),
            Strategy::Bytes
        );
        assert_eq!(
            auto_strategy(&Column::new("meta", ColumnType::Json)).unwrap(),
            Strategy::Json
        );
        assert_eq!(
            auto_strategy(&Column::new("age", ColumnType::Int32)).unwrap(),
            Strategy::Zero
        );
        assert_eq!(
            auto_strategy(&Column::new("seen_at", ColumnType::TimestampTz)).unwrap(),
            Strategy::Epoch
        );
    }

    #[test]
    fn auto_strategy_refuses_to_guess_for_a_closed_set_or_exotic_type() {
        assert!(matches!(
            auto_strategy(&Column::new(
                "status",
                ColumnType::Enum {
                    variants: vec!["draft".to_owned()]
                }
            )),
            Err(ScrubError::NoAutoStrategy { .. })
        ));
        assert!(matches!(
            auto_strategy(&Column::new(
                "addr",
                ColumnType::Opaque {
                    pg_type: "inet".to_owned()
                }
            )),
            Err(ScrubError::NoAutoStrategy { .. })
        ));
    }

    // ── Statement generation ────────────────────────────────────────────────

    #[test]
    fn update_preserves_nulls_and_batches_a_table_into_one_statement() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at"]
            [tables.users.pii]
            email = "email"
            full_name = "name"
            bio = "redact"
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .unwrap();
        assert_eq!(plan.tables.len(), 1, "one statement per table");
        let sql = &plan.tables[0].sql;
        assert!(sql.starts_with(r#"UPDATE "users" SET "#), "{sql}");
        assert!(
            sql.contains(r#""bio" = CASE WHEN "bio" IS NULL THEN NULL ELSE"#),
            "a nullable column must keep its NULLs: {sql}"
        );
        assert!(
            !sql.contains(r#""full_name" = CASE"#),
            "a NOT NULL column needs no CASE: {sql}"
        );
        assert!(sql.contains(r#""email" = "#) && sql.contains(r#""full_name" = "#));
    }

    #[test]
    fn a_table_with_no_pii_produces_no_statement() {
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "email", "full_name", "bio"]
            "#,
        )
        .unwrap();
        let plan = plan_for(
            &[users_table()],
            &config,
            &empty_encrypted(),
            &no_anonymize(),
        )
        .unwrap();
        assert!(plan.tables.is_empty(), "nothing to scrub, nothing emitted");
    }

    #[test]
    fn identifiers_with_quotes_are_escaped_in_the_statement() {
        let mut t = Table::new(r#"we"ird"#, Backend::Postgres);
        t.primary_key = vec!["id".to_owned()];
        t.columns = vec![pk_col("id"), text_col(r#"na"me"#)];
        let mut config = ScrubConfig::default();
        config.defaults.safe_columns = vec!["id".to_owned()];
        config.tables.insert(
            r#"we"ird"#.to_owned(),
            TableRule {
                safe: Vec::new(),
                pii: BTreeMap::from([(r#"na"me"#.to_owned(), Strategy::Redact)]),
            },
        );
        let plan = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize()).unwrap();
        assert!(
            plan.tables[0].sql.starts_with(r#"UPDATE "we""ird" SET "na""me" = "#),
            "{}",
            plan.tables[0].sql
        );
    }

    // ── GDPR anonymize extraction from app source ───────────────────────────

    #[test]
    fn anonymize_registrations_are_extracted_from_source() {
        let src = r#"
            use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
            fn registry() -> GdprRegistry {
                GdprRegistry::new()
                    .register(ModelRegistration::hard_delete("posts"))
                    .register(ModelRegistration::anonymize("comments"))
                    .register(ModelRegistration::retain("invoices", "legal hold"))
                    .register(autumn_web::gdpr::ModelRegistration::anonymize("profiles"))
            }
        "#;
        let found = extract_anonymize_tables(src).expect("source must parse");
        assert_eq!(
            found,
            BTreeSet::from(["comments".to_owned(), "profiles".to_owned()])
        );
    }

    #[test]
    fn a_commented_out_registration_is_not_extracted() {
        let src = r#"
            fn registry() {
                // ModelRegistration::anonymize("ghosts")
                let _ = ModelRegistration::anonymize("comments");
            }
        "#;
        let found = extract_anonymize_tables(src).unwrap();
        assert_eq!(found, BTreeSet::from(["comments".to_owned()]));
    }

    #[test]
    fn a_non_literal_registration_argument_is_reported_not_ignored() {
        let src = r#"
            fn registry() {
                let _ = ModelRegistration::anonymize(table_name());
            }
        "#;
        let err = extract_anonymize_tables(src)
            .expect_err("a table name the scanner cannot resolve must not pass silently");
        assert!(matches!(err, ScrubError::UnresolvableAnonymize { .. }), "{err:?}");
    }

    // ── Production guard (AC #5) ────────────────────────────────────────────

    #[test]
    fn scrub_refuses_a_production_profile_without_force() {
        for profile in ["prod", "production", "staging"] {
            assert!(
                matches!(
                    guard_scrub_target(profile, false),
                    Err(ScrubError::ProductionRefused { .. })
                ),
                "{profile} must be refused"
            );
            assert!(guard_scrub_target(profile, true).is_ok());
        }
        for profile in ["dev", "development", "test"] {
            assert!(guard_scrub_target(profile, false).is_ok());
        }
    }

    #[test]
    fn same_database_compares_host_port_and_name_ignoring_credentials() {
        // Same server + database, different credentials: still the same target.
        assert!(same_database(
            "postgres://app:pw@db.example.com:5432/myapp",
            "postgres://readonly:other@db.example.com:5432/myapp"
        ));
        assert!(!same_database(
            "postgres://app:pw@db.example.com:5432/myapp",
            "postgres://app:pw@db.example.com:5432/myapp_staging"
        ));
        assert!(!same_database(
            "postgres://app:pw@db.example.com:5432/myapp",
            "postgres://app:pw@staging.example.com:5432/myapp"
        ));
        // An unparsable URL never claims a match.
        assert!(!same_database("not a url", "not a url"));
    }

    #[test]
    fn errors_never_leak_credentials() {
        let err = ScrubError::ProductionRefused {
            profile: "prod".to_owned(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("prod"));
        assert!(!rendered.contains("postgres://"));
        assert!(!rendered.contains("hunter2"));
    }

    // ── Report ──────────────────────────────────────────────────────────────

    #[test]
    fn check_report_prints_a_paste_ready_stanza_for_unclassified_columns() {
        let stanza = suggested_config_stanza(&[
            "users.email".to_owned(),
            "users.full_name".to_owned(),
            "posts.body".to_owned(),
        ]);
        assert!(stanza.contains("[tables.users.pii]"), "{stanza}");
        assert!(stanza.contains("email = \"auto\""), "{stanza}");
        assert!(stanza.contains("[tables.posts.pii]"), "{stanza}");
        assert!(stanza.contains("body = \"auto\""), "{stanza}");
    }

    #[test]
    fn index_backed_uniqueness_is_recognized() {
        // A single-column unique INDEX (not a column flag) still forbids a
        // constant replacement.
        let mut t = users_table();
        t.columns[1].unique = false;
        t.indexes = vec![Index::new("idx_users_email", vec!["email".to_owned()], true)];
        let config = parse_config_str(
            r#"
            [defaults]
            safe_columns = ["id", "created_at", "full_name", "bio"]
            [tables.users.pii]
            email = "json"
            "#,
        )
        .unwrap();
        let err = plan_for(&[t], &config, &empty_encrypted(), &no_anonymize())
            .expect_err("a unique index must be honored like a unique column");
        assert!(matches!(err, ScrubError::NonUniqueStrategy { .. }), "{err:?}");
    }
}
