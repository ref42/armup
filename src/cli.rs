use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use dialoguer::{Confirm, Input, MultiSelect, Select, theme::ColorfulTheme};
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use crate::installer::DownloadConfig;
use crate::state::default_install_root;
use crate::tool::{EnvScope, ToolKind};
use crate::types::{ResolvedTool, ToolVersionOptions};

#[derive(Parser, Debug)]
#[command(
    name = "armup",
    bin_name = "armup",
    version,
    about = "Install embedded Cortex-M tools on Windows",
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Install tools")]
    Install(InstallArgs),
    #[command(about = "Update installed tools to the latest supported versions")]
    Update(UpdateArgs),
    #[command(about = "Show installed tools and managed PATH entries")]
    Status(StatusArgs),
    #[command(about = "Check network access, release resolution, and local state")]
    Doctor(DoctorArgs),
}

#[derive(Args, Debug, Default)]
pub struct InstallArgs {
    #[arg(
        short = 'a',
        long,
        help = "Install all supported tools without asking which tools to install"
    )]
    pub all: bool,

    #[arg(
        long = "tool",
        value_name = "TOOL",
        value_delimiter = ',',
        help = "Install selected tools. May be repeated or comma-separated."
    )]
    pub tools: Vec<ToolKind>,

    #[arg(long, value_name = "PATH", help = "Install root")]
    pub root: Option<PathBuf>,

    #[arg(
        long = "add-path",
        conflicts_with_all = ["no_add_path", "legacy_no_path"],
        help = "Add installed tools to the Windows user Path (default)"
    )]
    pub add_path: bool,

    #[arg(
        long = "no-add-path",
        conflicts_with_all = ["add_path", "legacy_path"],
        help = "Skip Windows user Path changes"
    )]
    pub no_add_path: bool,

    #[arg(long = "path", hide = true, conflicts_with_all = ["no_add_path", "legacy_no_path"])]
    pub legacy_path: bool,

    #[arg(long = "no-path", hide = true, conflicts_with_all = ["add_path", "legacy_path"])]
    pub legacy_no_path: bool,

    #[arg(long, help = "Use defaults for any missing interactive choices")]
    pub yes: bool,

    #[arg(
        long = "select-versions",
        help = "Choose versions from recent upstream releases instead of using latest"
    )]
    pub select_versions: bool,

    #[arg(
        short = 'j',
        long = "download-connections",
        value_name = "N",
        default_value_t = DownloadConfig::DEFAULT_CONNECTIONS,
        value_parser = clap::value_parser!(u8).range(1..=DownloadConfig::MAX_CONNECTIONS as i64),
        help = "Maximum total parallel download connections"
    )]
    pub download_connections: u8,
}

#[derive(Args, Debug, Default)]
pub struct StatusArgs {
    #[arg(long, value_name = "PATH", help = "Install root")]
    pub root: Option<PathBuf>,

    #[arg(long, short = 'v', help = "Show full executable and PATH entries")]
    pub verbose: bool,
}

#[derive(Args, Debug, Default)]
pub struct UpdateArgs {
    #[arg(
        short = 'a',
        long,
        conflicts_with = "tools",
        help = "Update all installed supported tools"
    )]
    pub all: bool,

    #[arg(
        long = "tool",
        value_name = "TOOL",
        value_delimiter = ',',
        help = "Update selected installed tools. May be repeated or comma-separated."
    )]
    pub tools: Vec<ToolKind>,

    #[arg(long, value_name = "PATH", help = "Install root")]
    pub root: Option<PathBuf>,

    #[arg(
        long = "add-path",
        conflicts_with_all = ["no_add_path", "legacy_no_path"],
        help = "Refresh the Windows user Path after updating"
    )]
    pub add_path: bool,

    #[arg(
        long = "no-add-path",
        conflicts_with_all = ["add_path", "legacy_path"],
        help = "Skip Windows user Path changes"
    )]
    pub no_add_path: bool,

    #[arg(long = "path", hide = true, conflicts_with_all = ["no_add_path", "legacy_no_path"])]
    pub legacy_path: bool,

    #[arg(long = "no-path", hide = true, conflicts_with_all = ["add_path", "legacy_path"])]
    pub legacy_no_path: bool,

    #[arg(
        short = 'j',
        long = "download-connections",
        value_name = "N",
        default_value_t = DownloadConfig::DEFAULT_CONNECTIONS,
        value_parser = clap::value_parser!(u8).range(1..=DownloadConfig::MAX_CONNECTIONS as i64),
        help = "Maximum total parallel download connections"
    )]
    pub download_connections: u8,
}

#[derive(Args, Debug, Default)]
pub struct DoctorArgs {
    #[arg(long, value_name = "PATH", help = "Install root")]
    pub root: Option<PathBuf>,
}

pub struct InstallRequest {
    pub tools: Vec<ToolKind>,
    pub root: PathBuf,
    pub scope: EnvScope,
    pub select_versions: bool,
    pub download_config: DownloadConfig,
}

pub struct UpdateRequest {
    pub requested_tools: Option<Vec<ToolKind>>,
    pub root: PathBuf,
    pub scope: EnvScope,
    pub download_config: DownloadConfig,
}

pub fn install_request(args: &InstallArgs) -> Result<InstallRequest> {
    Ok(InstallRequest {
        tools: choose_install_tools(args)?,
        root: choose_install_root(args)?,
        scope: choose_install_scope(args)?,
        select_versions: args.select_versions,
        download_config: DownloadConfig::from_connections(args.download_connections),
    })
}

pub fn update_request(args: &UpdateArgs) -> Result<UpdateRequest> {
    let requested_tools = if args.all || args.tools.is_empty() {
        None
    } else {
        Some(dedup_tools(args.tools.clone()))
    };

    Ok(UpdateRequest {
        requested_tools,
        root: normalize_root_path(args.root.clone().unwrap_or_else(default_install_root))?,
        scope: choose_update_scope(args),
        download_config: DownloadConfig::from_connections(args.download_connections),
    })
}

pub fn status_root(args: &StatusArgs) -> Result<PathBuf> {
    normalize_root_path(args.root.clone().unwrap_or_else(default_install_root))
}

pub fn doctor_root(args: &DoctorArgs) -> Result<PathBuf> {
    normalize_root_path(args.root.clone().unwrap_or_else(default_install_root))
}

pub fn choose_versions(options: &[ToolVersionOptions]) -> Result<Vec<ResolvedTool>> {
    if !is_interactive_terminal() {
        bail!("--select-versions requires an interactive terminal");
    }

    let theme = ColorfulTheme::default();
    let mut selected = Vec::with_capacity(options.len());
    for option in options {
        if option.releases.len() == 1 {
            let release = option.releases[0].clone();
            println!(
                "{}: using only discovered version {}",
                option.kind.id(),
                release.version
            );
            selected.push(release);
            continue;
        }

        let labels = option
            .releases
            .iter()
            .enumerate()
            .map(|(index, release)| {
                if index == 0 {
                    format!("latest: {}", release.version)
                } else {
                    release.version.clone()
                }
            })
            .collect::<Vec<_>>();

        let selection = Select::with_theme(&theme)
            .with_prompt(format!("Select version for {}", option.kind.id()))
            .items(&labels)
            .default(0)
            .report(false)
            .interact()
            .with_context(|| {
                format!("failed to read version selection for {}", option.kind.id())
            })?;
        selected.push(option.releases[selection].clone());
    }

    Ok(selected)
}

fn choose_install_tools(args: &InstallArgs) -> Result<Vec<ToolKind>> {
    if !args.tools.is_empty() {
        return Ok(dedup_tools(args.tools.clone()));
    }

    if args.all || args.yes || !is_interactive_terminal() {
        return Ok(ToolKind::all());
    }

    let theme = ColorfulTheme::default();
    let install_all = Confirm::with_theme(&theme)
        .with_prompt("Install all supported tools?")
        .default(true)
        .report(false)
        .interact()
        .context("failed to read interactive install selection")?;

    if install_all {
        println!("Installing all supported tools.");
        return Ok(ToolKind::all());
    }

    let all_tools = ToolKind::all();
    let labels = all_tools
        .iter()
        .map(|tool| tool.picker_label())
        .collect::<Vec<_>>();
    println!("Use Space to select tools, then press Enter to confirm.");
    let selections = MultiSelect::with_theme(&theme)
        .with_prompt("Select tools to install")
        .items(&labels)
        .report(false)
        .interact()
        .context("failed to read tool checklist selection")?;

    if selections.is_empty() {
        bail!("no tools selected");
    }

    let chosen = selections
        .into_iter()
        .map(|index| all_tools[index])
        .collect::<Vec<_>>();
    println!(
        "Selected tools: {}",
        chosen
            .iter()
            .map(|tool| tool.id())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(chosen)
}

fn choose_install_root(args: &InstallArgs) -> Result<PathBuf> {
    if let Some(root) = &args.root {
        return normalize_root_path(root.clone());
    }

    if args.yes || !is_interactive_terminal() {
        bail!("install root is required; pass --root <PATH>");
    }

    let theme = ColorfulTheme::default();
    println!("Enter the install root. Both / and \\ are accepted.");
    let raw = Input::<String>::with_theme(&theme)
        .with_prompt("Install root")
        .report(false)
        .interact_text()
        .context("failed to read install root path")?;

    let parsed = parse_root_path(&raw).map_err(anyhow::Error::msg)?;
    println!("Install root: {}", parsed.display());
    Ok(parsed)
}

fn choose_install_scope(args: &InstallArgs) -> Result<EnvScope> {
    if args.no_add_path || args.legacy_no_path {
        return Ok(EnvScope::None);
    }
    Ok(EnvScope::User)
}

fn choose_update_scope(args: &UpdateArgs) -> EnvScope {
    if args.no_add_path || args.legacy_no_path {
        EnvScope::None
    } else {
        EnvScope::User
    }
}

pub(crate) fn parse_root_path(raw: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("root path cannot be empty".to_string());
    }

    #[cfg(windows)]
    {
        normalize_windows_root_text(trimmed).map(PathBuf::from)
    }

    #[cfg(not(windows))]
    {
        Ok(PathBuf::from(trimmed))
    }
}

fn normalize_root_path(path: PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    parse_root_path(&raw).map_err(anyhow::Error::msg)
}

#[cfg(windows)]
fn normalize_windows_root_text(raw: &str) -> std::result::Result<String, String> {
    let normalized = raw.trim().replace('/', "\\");
    if normalized.is_empty() {
        return Err("root path cannot be empty".to_string());
    }

    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        if bytes.len() == 2 {
            return Ok(format!("{normalized}\\"));
        }
        if bytes[2] != b'\\' {
            let (drive, rest) = normalized.split_at(2);
            return Ok(format!("{drive}\\{rest}"));
        }
        return Ok(normalized);
    }

    if normalized.starts_with(r"\\") {
        return Ok(normalized);
    }

    Err(format!(
        "install root must be an absolute path like D:\\Embedded_Toolchain; got {raw:?}"
    ))
}

fn is_interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn dedup_tools(tools: Vec<ToolKind>) -> Vec<ToolKind> {
    let mut deduped = Vec::new();
    for tool in tools {
        if !deduped.contains(&tool) {
            deduped.push(tool);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::{InstallArgs, install_request, parse_root_path};
    use crate::tool::EnvScope;
    use crate::tool::ToolKind;
    use std::path::PathBuf;

    #[test]
    fn parse_root_path_rejects_empty_strings() {
        assert!(parse_root_path("   ").is_err());
    }

    #[test]
    fn parse_root_path_keeps_forward_slashes_usable() {
        let parsed = parse_root_path("D:/Embedded/armup").unwrap();
        assert_eq!(parsed, PathBuf::from(r"D:\Embedded\armup"));
    }

    #[test]
    fn parse_root_path_keeps_backslashes_usable() {
        let parsed = parse_root_path(r"D:\Embedded\armup").unwrap();
        assert_eq!(parsed, PathBuf::from(r"D:\Embedded\armup"));
    }

    #[test]
    fn parse_root_path_accepts_mixed_separators() {
        let parsed = parse_root_path(r"D:/Embedded\armup").unwrap();
        assert_eq!(parsed, PathBuf::from(r"D:\Embedded\armup"));
    }

    #[test]
    fn parse_root_path_repairs_drive_relative_paths_from_bash() {
        let parsed = parse_root_path(r"D:Embedded_Toolchain").unwrap();
        assert_eq!(parsed, PathBuf::from(r"D:\Embedded_Toolchain"));
    }

    #[test]
    fn parse_root_path_rejects_plain_relative_paths() {
        assert!(parse_root_path(r"Embedded_Toolchain").is_err());
    }

    #[test]
    fn tool_kind_accepts_user_friendly_names() {
        assert_eq!("ninja".parse::<ToolKind>().unwrap(), ToolKind::Ninja);
        assert_eq!(
            "openocd".parse::<ToolKind>().unwrap(),
            ToolKind::XpackOpenocd
        );
        assert_eq!(
            "arm-none-eabi-gcc".parse::<ToolKind>().unwrap(),
            ToolKind::ArmNoneEabiGcc
        );
        assert_eq!("probe-rs".parse::<ToolKind>().unwrap(), ToolKind::ProbeRs);
    }

    #[test]
    fn install_request_deduplicates_repeated_tools() {
        let args = InstallArgs {
            tools: vec![
                ToolKind::ArmNoneEabiGcc,
                ToolKind::Ninja,
                ToolKind::ArmNoneEabiGcc,
            ],
            root: Some(PathBuf::from(r"D:\Embedded\armup")),
            no_add_path: true,
            ..InstallArgs::default()
        };

        let request = install_request(&args).unwrap();

        assert_eq!(
            request.tools,
            vec![ToolKind::ArmNoneEabiGcc, ToolKind::Ninja]
        );
    }

    #[test]
    fn install_request_adds_user_path_by_default() {
        let args = InstallArgs {
            all: true,
            root: Some(PathBuf::from(r"D:\Embedded\armup")),
            ..InstallArgs::default()
        };

        let request = install_request(&args).unwrap();

        assert_eq!(request.scope, EnvScope::User);
    }

    #[test]
    fn install_request_can_skip_path_changes() {
        let args = InstallArgs {
            all: true,
            root: Some(PathBuf::from(r"D:\Embedded\armup")),
            no_add_path: true,
            ..InstallArgs::default()
        };

        let request = install_request(&args).unwrap();

        assert_eq!(request.scope, EnvScope::None);
    }

    #[test]
    fn install_request_requires_root_when_yes_is_used() {
        let args = InstallArgs {
            all: true,
            add_path: true,
            yes: true,
            ..InstallArgs::default()
        };

        let error = match install_request(&args) {
            Ok(_) => panic!("install request should require an explicit root"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("install root is required"));
    }
}
