//! autumn generate plugin — plan the generation of a conformant plugin crate.

use std::fs;
use std::path::Path;

use super::emit::Plan;
use super::{Flags, GenerateError};

#[derive(Debug)]
pub struct PluginPlan {
    pub plan: Plan,
    pub name_kebab: String,
    pub name_snake: String,
    pub struct_name: String,
    pub target_dir_relative: String,
}

#[allow(clippy::too_many_lines)]
pub fn plan_plugin(
    project_root: &Path,
    name: &str,
    path: Option<&Path>,
    flags: Flags,
) -> Result<PluginPlan, GenerateError> {
    // Call ensure_project_root first
    super::ensure_project_root(project_root)?;

    // Strip prefixes/suffixes
    let mut clean_name = name;
    if let Some(stripped) = clean_name.strip_prefix("autumn-") {
        clean_name = stripped;
    } else if let Some(stripped) = clean_name.strip_prefix("autumn_") {
        clean_name = stripped;
    }

    if let Some(stripped) = clean_name.strip_suffix("-plugin") {
        clean_name = stripped;
    } else if let Some(stripped) = clean_name.strip_suffix("_plugin") {
        clean_name = stripped;
    } else if let Some(stripped) = clean_name.strip_suffix("Plugin") {
        clean_name = stripped;
    } else if let Some(stripped) = clean_name.strip_suffix("plugin") {
        clean_name = stripped;
    }

    // Validate clean_name
    if clean_name.is_empty() {
        return Err(GenerateError::InvalidName(
            name.to_string(),
            "Name cannot be empty".to_string(),
        ));
    }

    let mut chars = clean_name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(GenerateError::InvalidName(
            name.to_string(),
            "Name must start with a letter or underscore".to_string(),
        ));
    }

    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(GenerateError::InvalidName(
            name.to_string(),
            "Name must contain only alphanumeric characters, dashes, or underscores".to_string(),
        ));
    }

    let name_kebab = super::naming::snake(clean_name).replace('_', "-");
    let name_snake = name_kebab.replace('-', "_");
    let struct_name = format!("{}Plugin", super::naming::pascal(&name_snake));

    // Validate path components
    if let Some(p) = path {
        for component in p.components() {
            match component {
                std::path::Component::ParentDir => {
                    return Err(GenerateError::Config(
                        "Path cannot contain parent directory traversal ('..')".to_string(),
                    ));
                }
                std::path::Component::RootDir => {
                    return Err(GenerateError::Config(
                        "Path cannot be an absolute path".to_string(),
                    ));
                }
                std::path::Component::Prefix(_) => {
                    return Err(GenerateError::Config(
                        "Path cannot contain prefix components (e.g. drive letters)".to_string(),
                    ));
                }
                _ => {}
            }
        }
    }

    let target_dir = path.map_or_else(
        || project_root.join(format!("autumn-{name_kebab}-plugin")),
        |p| project_root.join(p),
    );

    // Prevent target_dir == project_root
    let clean_target: std::path::PathBuf = target_dir
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    let clean_root: std::path::PathBuf = project_root
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if clean_target == clean_root {
        return Err(GenerateError::Config(
            "Target directory cannot be the project root itself".to_string(),
        ));
    }

    if !flags.dry_run && target_dir.exists() && !flags.force {
        let is_empty_dir = target_dir.is_dir() && fs::read_dir(&target_dir)?.next().is_none();
        if !is_empty_dir {
            return Err(GenerateError::Config(format!(
                "target directory '{}' already exists and is not empty. Use --force to override.",
                target_dir.display().to_string().replace('\\', "/")
            )));
        }
    }

    let plan_root = target_dir.parent().unwrap_or(project_root);
    let mut plan = Plan::new(plan_root);

    let cargo_version = env!("CARGO_PKG_VERSION");
    let major_minor = cargo_version
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".");

    let cargo_toml_content = format!(
        r#"[package]
name = "autumn-{name_kebab}-plugin"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = {{ version = "{major_minor}" }}
serde = {{ version = "1", features = ["derive"] }}
"#
    );

    let lib_rs_content = format!(
        r#"//! autumn-{name_kebab}-plugin

use std::borrow::Cow;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

pub struct {struct_name};

impl {struct_name} {{
    #[must_use]
    pub fn new() -> Self {{
        Self
    }}
}}

impl Default for {struct_name} {{
    fn default() -> Self {{
        Self::new()
    }}
}}

impl Plugin for {struct_name} {{
    fn name(&self) -> Cow<'static, str> {{
        Cow::Borrowed("autumn-{name_kebab}-plugin")
    }}

    fn build(self, app: AppBuilder) -> AppBuilder {{
        // Wire a commented example route contribution:
        // let app = app.routes(autumn_web::routes![index]);
        app
    }}
}}

// Commented index route function:
// #[autumn_web::get("/autumn-{name_kebab}-plugin")]
// async fn index() -> &'static str {{
//     "Hello from plugin!"
// }}
"#
    );

    let relative_path = target_dir.strip_prefix(project_root).map_or_else(
        |_| target_dir.display().to_string().replace('\\', "/"),
        |p| format!("./{}", p.display().to_string().replace('\\', "/")),
    );

    let readme_content = format!(
        r#"# autumn-{name_kebab}-plugin

An Autumn plugin for {name}.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
autumn-{name_kebab}-plugin = {{ path = "{relative_path}" }}
```

## Setup

Register the plugin with your Autumn application in `src/main.rs`:

```rust
use autumn_{name_snake}_plugin::{struct_name};

#[autumn_web::main]
async fn main() {{
    autumn_web::app()
        .plugin({struct_name}::new())
        .run()
        .await;
}}
```
"#
    );

    let conformance_rs_content = format!(
        r#"#[cfg(test)]
mod conformance_tests {{
    use autumn_web::plugin_conformance::{{ConformanceConfig, run_conformance}};
    use autumn_web::route_listing::{{RouteInfo, RouteSource}};

    #[test]
    fn plugin_passes_conformance() {{
        // Simulate the routes your plugin contributes
        let routes = vec![
            RouteInfo {{
                method: "GET".to_owned(),
                path: "/autumn-{name_kebab}-plugin".to_owned(),
                handler: "autumn_{name_snake}_plugin::index".to_owned(),
                source: RouteSource::Plugin("autumn-{name_kebab}-plugin".to_owned()),
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
            }},
        ];

        let config = ConformanceConfig::new("autumn-{name_kebab}-plugin")
            .prefix("/autumn-{name_kebab}-plugin")
            .sensitive_route("/autumn-{name_kebab}-plugin", "Role: admin required");

        let report = run_conformance(&config, &routes);
        assert!(
            report.passed(),
            "conformance failed:\n{{}}",
            report.to_text_report()
        );
    }}
}}
"#
    );

    plan.create(target_dir.join("Cargo.toml"), cargo_toml_content);
    plan.create(target_dir.join("src/lib.rs"), lib_rs_content);
    plan.create(target_dir.join("README.md"), readme_content);
    plan.create(
        target_dir.join("tests/conformance.rs"),
        conformance_rs_content,
    );

    let target_dir_relative = target_dir.strip_prefix(project_root).map_or_else(
        |_| target_dir.display().to_string().replace('\\', "/"),
        |p| p.display().to_string().replace('\\', "/"),
    );

    Ok(PluginPlan {
        plan,
        name_kebab,
        name_snake,
        struct_name,
        target_dir_relative,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn plan_creates_plugin_files() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path();
        fs::write(project_root.join("Cargo.toml"), "").unwrap();
        let target_dir = project_root.join("autumn-foo-plugin");

        let plugin_plan = plan_plugin(project_root, "Foo", None, Flags::default()).unwrap();
        let plan = plugin_plan.plan;

        // Verify Cargo.toml, src/lib.rs, README.md, and tests/conformance.rs are planned for creation
        let paths: std::collections::HashSet<PathBuf> = plan
            .actions
            .iter()
            .map(|a| a.path().to_path_buf())
            .collect();

        assert!(
            paths.contains(&target_dir.join("Cargo.toml")),
            "Should plan Cargo.toml"
        );
        assert!(
            paths.contains(&target_dir.join("src/lib.rs")),
            "Should plan src/lib.rs"
        );
        assert!(
            paths.contains(&target_dir.join("README.md")),
            "Should plan README.md"
        );
        assert!(
            paths.contains(&target_dir.join("tests/conformance.rs")),
            "Should plan tests/conformance.rs"
        );

        // Verify crate name and struct name
        let cargo_action = plan
            .actions
            .iter()
            .find(|a| a.path() == target_dir.join("Cargo.toml"))
            .unwrap();
        let super::super::emit::Action::Create {
            contents: cargo_content,
            ..
        } = cargo_action
        else {
            panic!("Expected Create action");
        };
        assert!(
            cargo_content.contains("name = \"autumn-foo-plugin\""),
            "Expected Cargo.toml to contain the lowercase kebab-case crate name"
        );

        let lib_action = plan
            .actions
            .iter()
            .find(|a| a.path() == target_dir.join("src/lib.rs"))
            .unwrap();
        let super::super::emit::Action::Create {
            contents: lib_content,
            ..
        } = lib_action
        else {
            panic!("Expected Create action");
        };
        assert!(
            lib_content.contains("pub struct FooPlugin;"),
            "Expected src/lib.rs to define FooPlugin"
        );
    }

    #[test]
    fn plan_includes_correct_contents_and_conformance_run() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path();
        fs::write(project_root.join("Cargo.toml"), "").unwrap();
        let target_dir = project_root.join("autumn-foo-plugin");

        let plugin_plan = plan_plugin(project_root, "foo", None, Flags::default()).unwrap();
        let plan = plugin_plan.plan;

        // Check Cargo.toml content
        let cargo_action = plan
            .actions
            .iter()
            .find(|a| a.path() == target_dir.join("Cargo.toml"))
            .unwrap();
        let super::super::emit::Action::Create {
            contents: cargo_content,
            ..
        } = cargo_action
        else {
            panic!("Expected Create action")
        };
        assert!(cargo_content.contains("[package]"));
        assert!(cargo_content.contains("name = \"autumn-foo-plugin\""));
        assert!(cargo_content.contains("edition = \"2024\""));
        let expected_version = env!("CARGO_PKG_VERSION")
            .split('.')
            .take(2)
            .collect::<Vec<_>>()
            .join(".");
        assert!(cargo_content.contains(&format!(
            "autumn-web = {{ version = \"{expected_version}\" }}"
        )));
        assert!(cargo_content.contains("serde = { version = \"1\", features = [\"derive\"] }"));

        // Check src/lib.rs content
        let lib_action = plan
            .actions
            .iter()
            .find(|a| a.path() == target_dir.join("src/lib.rs"))
            .unwrap();
        let super::super::emit::Action::Create {
            contents: lib_content,
            ..
        } = lib_action
        else {
            panic!("Expected Create action")
        };
        assert!(lib_content.contains("impl Plugin for FooPlugin"));
        assert!(lib_content.contains("fn name(&self) -> Cow<'static, str>"));
        assert!(lib_content.contains("Cow::Borrowed(\"autumn-foo-plugin\")"));
        assert!(lib_content.contains("// let app = app.routes("));
        assert!(lib_content.contains("index"));

        // Check README.md content
        let readme_action = plan
            .actions
            .iter()
            .find(|a| a.path() == target_dir.join("README.md"))
            .unwrap();
        let super::super::emit::Action::Create {
            contents: readme_content,
            ..
        } = readme_action
        else {
            panic!("Expected Create action")
        };
        assert!(readme_content.contains("autumn-foo-plugin = { path = \"./autumn-foo-plugin\" }"));
        assert!(readme_content.contains(".plugin(FooPlugin::new())"));

        // Check tests/conformance.rs content
        let conformance_action = plan
            .actions
            .iter()
            .find(|a| a.path() == target_dir.join("tests/conformance.rs"))
            .unwrap();
        let super::super::emit::Action::Create {
            contents: conformance_content,
            ..
        } = conformance_action
        else {
            panic!("Expected Create action")
        };
        assert!(conformance_content.contains("run_conformance"));
        assert!(conformance_content.contains("ConformanceConfig::new"));

        // Test non-empty directory collision check
        let collision_dir = project_root.join("autumn-bar-plugin");
        fs::create_dir_all(&collision_dir).unwrap();
        fs::write(collision_dir.join("some-file.txt"), "hello").unwrap();

        let err = plan_plugin(project_root, "bar", None, Flags::default()).unwrap_err();
        match err {
            GenerateError::Config(msg) => {
                let expected = format!(
                    "target directory '{}' already exists and is not empty. Use --force to override.",
                    collision_dir.display().to_string().replace('\\', "/")
                );
                assert_eq!(msg, expected);
            }
            _ => panic!("Expected GenerateError::Config, got {err:?}"),
        }

        // If force is true, it should bypass the check and succeed
        let plan_forced = plan_plugin(
            project_root,
            "bar",
            None,
            Flags {
                force: true,
                ..Default::default()
            },
        );
        assert!(plan_forced.is_ok());
    }

    #[test]
    fn name_normalization_and_validation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path();
        fs::write(project_root.join("Cargo.toml"), "").unwrap();

        for name in &["autumn-foo-plugin", "FooPlugin", "autumn_foo_plugin"] {
            let res = plan_plugin(project_root, name, None, Flags::default()).unwrap();
            assert_eq!(res.name_kebab, "foo");
            assert_eq!(res.name_snake, "foo");
            assert_eq!(res.struct_name, "FooPlugin");
        }

        for invalid in &["", "1foo", "-foo", "../foo", "foo/bar", "foo..bar"] {
            let res = plan_plugin(project_root, invalid, None, Flags::default());
            assert!(
                matches!(res, Err(GenerateError::InvalidName(..))),
                "expected InvalidName for '{invalid}', got {res:?}"
            );
        }
    }

    #[test]
    fn path_traversal_validation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path();
        fs::write(project_root.join("Cargo.toml"), "").unwrap();

        let invalid_paths = &[
            Path::new("foo/../bar"),
            Path::new("/abs/path"),
            Path::new(""),
            Path::new("."),
        ];

        for p in invalid_paths {
            let res = plan_plugin(project_root, "foo", Some(p), Flags::default());
            assert!(
                matches!(res, Err(GenerateError::Config(..))),
                "expected Config error for path '{p:?}', got {res:?}"
            );
        }
    }

    #[test]
    fn dry_run_bypasses_collision_check() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path();
        fs::write(project_root.join("Cargo.toml"), "").unwrap();

        let collision_dir = project_root.join("autumn-foo-plugin");
        fs::create_dir_all(&collision_dir).unwrap();
        fs::write(collision_dir.join("some-file.txt"), "hello").unwrap();

        let res = plan_plugin(
            project_root,
            "foo",
            None,
            Flags {
                dry_run: false,
                force: false,
            },
        );
        assert!(res.is_err());

        let res_dry = plan_plugin(
            project_root,
            "foo",
            None,
            Flags {
                dry_run: true,
                force: false,
            },
        );
        assert!(res_dry.is_ok());
    }
}
