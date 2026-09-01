mod cli;
mod environment;
mod installer;
mod resolver;
mod state;
mod tool;
mod types;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, UpdateRequest};
use environment::{apply_user_environment, build_env_plan, preview_user_environment};
use resolver::{resolve_tool_options, resolve_tools};
use state::{
    cleanup_old_tool_versions, cleanup_staging_dirs, default_install_root, discover_installed_tools,
};
use tool::{EnvScope, ToolKind};
use types::{InstalledTool, ResolvedTool};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Install(args) => {
            let request = cli::install_request(&args)?;
            let client = resolver::build_client()?;
            println!("Resolving tool versions...");
            let packages = if request.select_versions {
                let options = resolve_tool_options(&client, &request.tools, 6).await?;
                cli::choose_versions(&options)?
            } else {
                resolve_tools(&client, &request.tools).await?
            };
            installer::install_tools(
                &client,
                &request.root,
                packages,
                request.scope,
                request.download_config,
            )
            .await?;
        }
        Commands::Update(args) => {
            let request = cli::update_request(&args)?;
            update_tools(request).await?;
        }
        Commands::Status(args) => {
            let root = cli::status_root(&args)?;
            print_status(&root, args.verbose)?;
        }
        Commands::Doctor(args) => {
            let root = cli::doctor_root(&args)?;
            run_doctor(&root).await?;
        }
    }

    Ok(())
}

async fn update_tools(request: UpdateRequest) -> Result<()> {
    let installed = discover_installed_tools(&request.root)?;
    if installed.is_empty() {
        println!("No tools installed under {}", request.root.display());
        return Ok(());
    }

    let selected = select_update_tools(&installed, request.requested_tools)?;
    if selected.is_empty() {
        println!("No matching installed tools to update.");
        return Ok(());
    }

    let client = resolver::build_client()?;
    println!("Resolving latest versions...");
    let latest = resolve_tools(&client, &selected).await?;
    let keep_versions = update_keep_versions(&installed, &latest);
    let outdated = outdated_packages(&installed, latest);

    if outdated.is_empty() {
        let removed = cleanup_old_tool_versions(&request.root, &keep_versions)?;
        if !removed.is_empty() {
            println!("Removed old versions:");
            for path in removed {
                println!("- {}", path.display());
            }
        }
        println!("All selected tools are already latest.");
        refresh_user_path_after_update(&request.root, request.scope)?;
        return Ok(());
    }

    println!("Updates:");
    for package in &outdated {
        if let Some(current) = installed.iter().find(|tool| tool.kind == package.kind) {
            println!(
                "- {} {} -> {}",
                package.kind, current.version, package.version
            );
        }
    }

    installer::install_tool_archives(&client, &request.root, outdated, request.download_config)
        .await?;

    let removed = cleanup_old_tool_versions(&request.root, &keep_versions)?;
    if !removed.is_empty() {
        println!("Removed old versions:");
        for path in removed {
            println!("- {}", path.display());
        }
    }
    refresh_user_path_after_update(&request.root, request.scope)?;

    Ok(())
}

fn refresh_user_path_after_update(root: &std::path::Path, scope: EnvScope) -> Result<()> {
    if scope != EnvScope::User {
        return Ok(());
    }

    let installed = discover_installed_tools(root)?;
    let plan = build_env_plan(root, &installed)?;
    apply_user_environment(root, &plan)?;
    println!("Updated user PATH.");
    println!("Open a new terminal to pick up the changes.");
    Ok(())
}

fn select_update_tools(
    installed: &[InstalledTool],
    requested: Option<Vec<ToolKind>>,
) -> Result<Vec<ToolKind>> {
    let installed_kinds = installed.iter().map(|tool| tool.kind).collect::<Vec<_>>();
    let Some(requested) = requested else {
        return Ok(installed_kinds);
    };

    let mut selected = Vec::new();
    let mut missing = Vec::new();
    for kind in requested {
        if installed_kinds.contains(&kind) {
            selected.push(kind);
        } else {
            missing.push(kind);
        }
    }

    if !missing.is_empty() {
        println!(
            "Skipping tools not installed under this root: {}",
            missing
                .iter()
                .map(|tool| tool.id())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(selected)
}

fn outdated_packages(installed: &[InstalledTool], latest: Vec<ResolvedTool>) -> Vec<ResolvedTool> {
    latest
        .into_iter()
        .filter(|package| {
            installed
                .iter()
                .find(|tool| tool.kind == package.kind)
                .is_some_and(|current| version_is_newer(&package.version, &current.version))
        })
        .collect()
}

fn update_keep_versions(
    installed: &[InstalledTool],
    latest: &[ResolvedTool],
) -> Vec<(ToolKind, String)> {
    latest
        .iter()
        .filter_map(|package| {
            let current = installed.iter().find(|tool| tool.kind == package.kind)?;
            let keep_version = if version_is_newer(&package.version, &current.version) {
                package.version.clone()
            } else {
                current.version.clone()
            };
            Some((package.kind, keep_version))
        })
        .collect()
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    if latest.eq_ignore_ascii_case(current) {
        return false;
    }

    compare_version_numbers(latest, current).is_gt()
}

fn compare_version_numbers(left: &str, right: &str) -> std::cmp::Ordering {
    let left_numbers = version_numbers(left);
    let right_numbers = version_numbers(right);
    for index in 0..left_numbers.len().max(right_numbers.len()) {
        let left = left_numbers.get(index).copied().unwrap_or(0);
        let right = right_numbers.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }

    left.cmp(right)
}

fn version_numbers(version: &str) -> Vec<u64> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for ch in version.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(number) = current.parse() {
                numbers.push(number);
            }
            current.clear();
        }
    }
    if !current.is_empty()
        && let Ok(number) = current.parse()
    {
        numbers.push(number);
    }
    numbers
}

fn print_status(root: &std::path::Path, verbose: bool) -> Result<()> {
    let installed_tools = discover_installed_tools(root)?;
    if installed_tools.is_empty() {
        println!("No tools installed under {}", root.display());
        return Ok(());
    }

    println!("Installed: {}", installed_tools.len());
    for tool in &installed_tools {
        println!("- {} {}", tool.kind, tool.version);
        if verbose {
            println!("  exe: {}", tool.executable_path.display());
        }
    }

    let plan = build_env_plan(root, &installed_tools)?;
    let preview = preview_user_environment(root, &plan)?;
    let missing_count = preview.added_path_entries.len();
    let stale_count = preview.removed_path_entries.len();

    if missing_count == 0 && stale_count == 0 {
        println!("PATH: ok");
    } else {
        println!("PATH: needs update ({missing_count} missing, {stale_count} stale)");
        println!("Run: armup install --all --root <PATH> --add-path --yes");
    }

    if verbose {
        println!("Install root: {}", root.display());
        if !preview.removed_path_entries.is_empty() {
            println!("Stale managed PATH entries:");
            for entry in &preview.removed_path_entries {
                println!("- {entry}");
            }
        }
        if !preview.added_path_entries.is_empty() {
            println!("Missing managed PATH entries:");
            for entry in &preview.added_path_entries {
                println!("- {}", entry.display());
            }
        }
    }

    Ok(())
}

async fn run_doctor(root: &std::path::Path) -> Result<()> {
    println!("Doctor checks:");
    print_check("Install root", installer::validate_install_root(root));

    let cleanup_result = cleanup_staging_dirs(root).map(|removed| {
        if removed.is_empty() {
            "no stale staging directories".to_string()
        } else {
            format!("removed {} stale staging directories", removed.len())
        }
    });
    match cleanup_result {
        Ok(message) => println!("[ok] Staging cleanup: {message}"),
        Err(error) => println!("[fail] Staging cleanup: {error:#}"),
    }

    let client = match resolver::build_client() {
        Ok(client) => {
            println!("[ok] HTTP client");
            client
        }
        Err(error) => {
            println!("[fail] HTTP client: {error:#}");
            return Ok(());
        }
    };

    match resolve_tools(&client, &ToolKind::all()).await {
        Ok(packages) => {
            println!("[ok] Release resolution");
            for package in packages {
                let checksum = if package.checksum.is_some() {
                    "checksum available"
                } else {
                    "no checksum advertised"
                };
                println!("- {} {} ({checksum})", package.kind, package.version);
            }
        }
        Err(error) => println!("[fail] Release resolution: {error:#}"),
    }

    let installed = discover_installed_tools(root)?;
    let plan = build_env_plan(root, &installed)?;
    match preview_user_environment(root, &plan) {
        Ok(preview) => {
            println!("[ok] User PATH registry access");
            println!(
                "- {} managed entries would be refreshed, {} entries would be added",
                preview.removed_path_entries.len(),
                preview.added_path_entries.len()
            );
        }
        Err(error) => println!("[fail] User PATH registry access: {error:#}"),
    }

    if root == default_install_root() {
        println!("Default root policy: Windows-only, D: drive by design.");
    }

    Ok(())
}

fn print_check<T>(label: &str, result: Result<T>) {
    match result {
        Ok(_) => println!("[ok] {label}"),
        Err(error) => println!("[fail] {label}: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{outdated_packages, select_update_tools, update_keep_versions, version_is_newer};
    use crate::tool::ToolKind;
    use crate::types::{InstalledTool, ResolvedTool};
    use std::path::PathBuf;

    #[test]
    fn version_comparison_handles_common_tool_versions() {
        assert!(version_is_newer("1.13.2", "1.12.0"));
        assert!(version_is_newer("15.2.rel1", "14.3.rel1"));
        assert!(version_is_newer("0.12.0-7", "0.12.0-6"));
        assert!(!version_is_newer("1.13.2", "1.13.2"));
        assert!(!version_is_newer("1.12.0", "1.13.2"));
    }

    #[test]
    fn update_all_selects_only_installed_tools() {
        let installed = vec![installed_tool(ToolKind::Ninja, "1.12.0")];

        let selected = select_update_tools(&installed, None).unwrap();

        assert_eq!(selected, vec![ToolKind::Ninja]);
    }

    #[test]
    fn outdated_packages_ignores_current_tools() {
        let installed = vec![
            installed_tool(ToolKind::Ninja, "1.12.0"),
            installed_tool(ToolKind::Cmake, "4.3.3"),
        ];
        let latest = vec![
            resolved_tool(ToolKind::Ninja, "1.13.2"),
            resolved_tool(ToolKind::Cmake, "4.3.3"),
        ];

        let outdated = outdated_packages(&installed, latest);

        assert_eq!(outdated.len(), 1);
        assert_eq!(outdated[0].kind, ToolKind::Ninja);
        assert_eq!(outdated[0].version, "1.13.2");
    }

    #[test]
    fn keep_versions_tracks_latest_or_current_versions() {
        let installed = vec![
            installed_tool(ToolKind::Ninja, "1.12.0"),
            installed_tool(ToolKind::Cmake, "4.3.3"),
        ];
        let latest = vec![
            resolved_tool(ToolKind::Ninja, "1.13.2"),
            resolved_tool(ToolKind::Cmake, "4.3.3"),
        ];

        let keep_versions = update_keep_versions(&installed, &latest);

        assert_eq!(
            keep_versions,
            vec![
                (ToolKind::Ninja, "1.13.2".to_string()),
                (ToolKind::Cmake, "4.3.3".to_string())
            ]
        );
    }

    fn installed_tool(kind: ToolKind, version: &str) -> InstalledTool {
        InstalledTool {
            kind,
            version: version.to_string(),
            executable_path: PathBuf::from("tool.exe"),
            executable_dir: PathBuf::from("."),
        }
    }

    fn resolved_tool(kind: ToolKind, version: &str) -> ResolvedTool {
        ResolvedTool {
            kind,
            version: version.to_string(),
            asset_name: "tool.zip".to_string(),
            download_url: "https://example.com/tool.zip".to_string(),
            checksum: None,
        }
    }
}
