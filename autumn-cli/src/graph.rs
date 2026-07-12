use crate::routes::{compile_binary, find_binary};
use autumn_web::route_listing::RouteInfo;
use std::process::Command;

pub fn run(package: Option<&str>, bin: Option<&str>, output_file: &str) {
    eprintln!("🌟 autumn graph");
    compile_binary(package, bin);
    let binary = find_binary(package, bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("✗ Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    if !output.status.success() {
        eprintln!(
            "✗ Binary exited with status {} while dumping routes",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let routes: Vec<RouteInfo> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        eprintln!("✗ Failed to parse route listing JSON: {e}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    let mmd = generate_mermaid_graph(&routes);
    std::fs::write(output_file, mmd).unwrap_or_else(|e| {
        eprintln!("✗ Failed to write output file {output_file}: {e}");
        std::process::exit(1);
    });
    println!("✓ Wrote route graph to {output_file}");
}

pub fn generate_mermaid_graph(routes: &[RouteInfo]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str("graph TD\n");
    out.push_str("    Client((Client))\n");

    // Simple hierarchy approach: just connect Client to Paths to Handlers.
    for (i, route) in routes.iter().enumerate() {
        let route_node = format!("R{i}");
        let handler_node = format!("H{i}");

        // Sanitize path for Mermaid
        let clean_path = route.path.replace('"', "\\\"");
        // Sanitize handler for Mermaid
        let clean_handler = route.handler.replace('"', "\\\"");

        let middleware_label = if route.middleware.is_empty() {
            " ".to_owned()
        } else {
            format!("|{}| ", route.middleware.join(", "))
        };

        let _ = writeln!(
            out,
            "    Client -- \"{}\" --> {route_node}(\"{clean_path}\")",
            route.method
        );
        let _ = writeln!(
            out,
            "    {route_node} -->{middleware_label}{handler_node}(\"{clean_handler}\")"
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::route_listing::RouteSource;

    #[test]
    fn generate_mermaid_graph_creates_valid_mmd() {
        let routes = vec![
            RouteInfo {
                method: "GET".to_owned(),
                path: "/api/posts".to_owned(),
                handler: "get_posts".to_owned(),
                source: RouteSource::User,
                middleware: vec![],
                api_version: None,
                status: None,
                sunset_opt_out: None,
            },
            RouteInfo {
                method: "POST".to_owned(),
                path: "/api/posts".to_owned(),
                handler: "create_post".to_owned(),
                source: RouteSource::User,
                middleware: vec!["auth".to_owned()],
                api_version: None,
                status: None,
                sunset_opt_out: None,
            },
        ];

        let mmd = generate_mermaid_graph(&routes);
        assert!(mmd.contains("graph TD"));
        assert!(mmd.contains("Client -- \"GET\" --> R0(\"/api/posts\")"));
        assert!(mmd.contains("R0 --> H0(\"get_posts\")"));
        assert!(mmd.contains("Client -- \"POST\" --> R1(\"/api/posts\")"));
        assert!(mmd.contains("R1 -->|auth| H1(\"create_post\")"));
    }
}
