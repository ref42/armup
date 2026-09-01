use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, InvalidHeaderValue, USER_AGENT,
};
use reqwest::{Client, Proxy};
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;
use tokio::task::JoinSet;

use crate::tool::ToolKind;
use crate::types::{ArchiveChecksum, ChecksumAlgorithm, ResolvedTool, ToolVersionOptions};

const ARM_GUIDE_PAGE: &str = "https://learn.arm.com/install-guides/gcc/arm-gnu/";
// ponytail: GitLab package discovery returns WAF challenges here; keep scraping Arm's guide for the version.
const ARM_GITLAB_PACKAGE_BASE: &str = "https://gitlab.arm.com/api/v4/projects/tooling%2Fgnu-toolchains-for-arm/packages/generic/gnu-toolchain";
const LOCAL_PROXY_HOST: &str = "127.0.0.1";
const LOCAL_PROXY_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const LOCAL_PROXY_PORTS: &[u16] = &[10808, 10809, 7890, 7891, 8080, 1080];
const PROXY_ENV_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];
const GITHUB_TOKEN_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "GH_TOKEN"];

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

pub fn build_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, armup_user_agent()?);
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json, text/html;q=0.9, */*;q=0.8"),
    );
    if let Some(token) = github_token() {
        headers.insert(AUTHORIZATION, github_auth_header(&token)?);
    }

    let mut builder = Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15));

    if has_proxy_environment() {
        println!("Proxy: using proxy from environment.");
    } else if let Some(proxy_url) = detect_local_proxy() {
        println!("Proxy: auto-detected local proxy at {proxy_url}.");
        builder = builder.proxy(
            Proxy::all(&proxy_url)
                .with_context(|| format!("failed to configure proxy {proxy_url}"))?,
        );
    }

    builder.build().context("failed to build HTTP client")
}

fn armup_user_agent() -> Result<HeaderValue, InvalidHeaderValue> {
    HeaderValue::from_str(concat!("armup/", env!("CARGO_PKG_VERSION")))
}

fn github_auth_header(token: &str) -> Result<HeaderValue, InvalidHeaderValue> {
    HeaderValue::from_str(&format!("Bearer {token}"))
}

fn github_token() -> Option<String> {
    GITHUB_TOKEN_ENV_VARS.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn has_proxy_environment() -> bool {
    PROXY_ENV_VARS.iter().any(|name| {
        env::var_os(name)
            .and_then(|value| value.into_string().ok())
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn detect_local_proxy() -> Option<String> {
    for &port in LOCAL_PROXY_PORTS {
        if is_socks5_proxy(port) {
            return Some(format!("socks5h://{LOCAL_PROXY_HOST}:{port}"));
        }
        if is_http_proxy(port) {
            return Some(format!("http://{LOCAL_PROXY_HOST}:{port}"));
        }
    }
    None
}

fn is_socks5_proxy(port: u16) -> bool {
    let Ok(mut stream) = connect_local_port(port) else {
        return false;
    };

    if stream.write_all(&[0x05, 0x01, 0x00]).is_err() {
        return false;
    }

    let mut response = [0u8; 2];
    stream.read_exact(&mut response).is_ok() && response[0] == 0x05 && response[1] != 0xff
}

fn is_http_proxy(port: u16) -> bool {
    let Ok(mut stream) = connect_local_port(port) else {
        return false;
    };

    let request = b"CONNECT api.github.com:443 HTTP/1.1\r\nHost: api.github.com:443\r\n\r\n";
    if stream.write_all(request).is_err() {
        return false;
    }

    let mut response = [0u8; 5];
    stream.read_exact(&mut response).is_ok() && response == *b"HTTP/"
}

fn connect_local_port(port: u16) -> Result<TcpStream, std::io::Error> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&address, LOCAL_PROXY_PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(LOCAL_PROXY_PROBE_TIMEOUT))?;
    stream.set_write_timeout(Some(LOCAL_PROXY_PROBE_TIMEOUT))?;
    Ok(stream)
}

pub async fn resolve_tools(client: &Client, tools: &[ToolKind]) -> Result<Vec<ResolvedTool>> {
    let mut tasks = JoinSet::new();
    for &tool in tools {
        let client = client.clone();
        tasks.spawn(async move { resolve_tool(&client, tool).await });
    }

    let mut resolved = Vec::with_capacity(tools.len());
    while let Some(result) = tasks.join_next().await {
        resolved.push(result.context("tool resolution task failed")??);
    }

    resolved.sort_by_key(|tool| {
        tools
            .iter()
            .position(|candidate| *candidate == tool.kind)
            .unwrap_or(usize::MAX)
    });
    Ok(resolved)
}

async fn resolve_tool(client: &Client, tool: ToolKind) -> Result<ResolvedTool> {
    match tool {
        ToolKind::ArmNoneEabiGcc => resolve_arm_toolchain(client).await,
        ToolKind::Clangd => {
            resolve_github_latest(client, tool, "clangd", "clangd", normalize_version).await
        }
        ToolKind::Cmake => {
            resolve_github_latest(client, tool, "Kitware", "CMake", normalize_version).await
        }
        ToolKind::Ninja => {
            resolve_github_latest(client, tool, "ninja-build", "ninja", normalize_version).await
        }
        ToolKind::ProbeRs => {
            resolve_github_latest(client, tool, "probe-rs", "probe-rs", normalize_version).await
        }
        ToolKind::XpackOpenocd => {
            resolve_github_latest(
                client,
                tool,
                "xpack-dev-tools",
                "openocd-xpack",
                normalize_version,
            )
            .await
        }
    }
}

pub async fn resolve_tool_options(
    client: &Client,
    tools: &[ToolKind],
    limit: usize,
) -> Result<Vec<ToolVersionOptions>> {
    let mut tasks = JoinSet::new();
    for &tool in tools {
        let client = client.clone();
        tasks.spawn(async move { resolve_version_options_for_tool(&client, tool, limit).await });
    }

    let mut options = Vec::with_capacity(tools.len());
    while let Some(result) = tasks.join_next().await {
        options.push(result.context("tool version resolution task failed")??);
    }

    options.sort_by_key(|option| {
        tools
            .iter()
            .position(|candidate| *candidate == option.kind)
            .unwrap_or(usize::MAX)
    });
    Ok(options)
}

async fn resolve_version_options_for_tool(
    client: &Client,
    tool: ToolKind,
    limit: usize,
) -> Result<ToolVersionOptions> {
    let releases = match tool {
        ToolKind::ArmNoneEabiGcc => vec![resolve_arm_toolchain(client).await?],
        ToolKind::Clangd => {
            resolve_github_release_options(
                client,
                tool,
                "clangd",
                "clangd",
                normalize_version,
                limit,
            )
            .await?
        }
        ToolKind::Cmake => {
            resolve_github_release_options(
                client,
                tool,
                "Kitware",
                "CMake",
                normalize_version,
                limit,
            )
            .await?
        }
        ToolKind::Ninja => {
            resolve_github_release_options(
                client,
                tool,
                "ninja-build",
                "ninja",
                normalize_version,
                limit,
            )
            .await?
        }
        ToolKind::ProbeRs => {
            resolve_github_release_options(
                client,
                tool,
                "probe-rs",
                "probe-rs",
                normalize_version,
                limit,
            )
            .await?
        }
        ToolKind::XpackOpenocd => {
            resolve_github_release_options(
                client,
                tool,
                "xpack-dev-tools",
                "openocd-xpack",
                normalize_version,
                limit,
            )
            .await?
        }
    };

    Ok(ToolVersionOptions {
        kind: tool,
        releases,
    })
}

async fn resolve_github_latest(
    client: &Client,
    tool: ToolKind,
    owner: &str,
    repo: &str,
    normalize: fn(&str) -> String,
) -> Result<ResolvedTool> {
    let api_url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let response = client
        .get(&api_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("failed to query GitHub release API for {owner}/{repo}"))?;

    if github_api_response_is_rate_limited(&response) {
        println!("GitHub API rate-limited for {owner}/{repo}; using release page fallback.");
        return resolve_github_latest_from_release_page(client, tool, owner, repo, normalize).await;
    }

    let release = response
        .error_for_status()
        .with_context(|| format!("GitHub release API returned an error for {owner}/{repo}"))?
        .json::<GitHubRelease>()
        .await
        .with_context(|| {
            format!("failed to parse GitHub release API response for {owner}/{repo}")
        })?;

    let asset = release
        .assets
        .into_iter()
        .find(|asset| tool.matches_github_asset(&asset.name))
        .with_context(|| format!("no matching Windows zip asset found for {}", tool.id()))?;

    Ok(ResolvedTool {
        kind: tool,
        version: normalize(&release.tag_name),
        asset_name: asset.name,
        download_url: asset.browser_download_url,
        checksum: parse_github_digest(asset.digest.as_deref()),
    })
}

async fn resolve_github_latest_from_release_page(
    client: &Client,
    tool: ToolKind,
    owner: &str,
    repo: &str,
    normalize: fn(&str) -> String,
) -> Result<ResolvedTool> {
    let latest_url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let response = client
        .get(&latest_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("failed to query GitHub release page for {owner}/{repo}"))?
        .error_for_status()
        .with_context(|| format!("GitHub release page returned an error for {owner}/{repo}"))?;

    let tag = extract_github_release_tag(response.url(), owner, repo).with_context(|| {
        format!(
            "could not determine latest release tag from {}",
            response.url()
        )
    })?;
    let version = normalize(&tag);
    let asset_name = fallback_github_asset_name(tool, &version)
        .with_context(|| format!("no release page fallback asset pattern for {}", tool.id()))?;
    let download_url =
        format!("https://github.com/{owner}/{repo}/releases/download/{tag}/{asset_name}");

    Ok(ResolvedTool {
        kind: tool,
        version,
        asset_name,
        download_url,
        checksum: None,
    })
}

fn github_api_response_is_rate_limited(response: &reqwest::Response) -> bool {
    matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) && response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "0")
}

fn extract_github_release_tag(url: &reqwest::Url, owner: &str, repo: &str) -> Option<String> {
    let marker = format!("/{owner}/{repo}/releases/tag/");
    url.path()
        .strip_prefix(&marker)
        .and_then(|value| value.split('/').next())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn fallback_github_asset_name(tool: ToolKind, version: &str) -> Option<String> {
    match tool {
        ToolKind::Clangd => Some(format!("clangd-windows-{version}.zip")),
        ToolKind::Cmake => Some(format!("cmake-{version}-windows-x86_64.zip")),
        ToolKind::Ninja => Some("ninja-win.zip".to_string()),
        ToolKind::ProbeRs => Some("probe-rs-tools-x86_64-pc-windows-msvc.zip".to_string()),
        ToolKind::XpackOpenocd => Some(format!("xpack-openocd-{version}-win32-x64.zip")),
        ToolKind::ArmNoneEabiGcc => None,
    }
}

async fn resolve_github_release_options(
    client: &Client,
    tool: ToolKind,
    owner: &str,
    repo: &str,
    normalize: fn(&str) -> String,
    limit: usize,
) -> Result<Vec<ResolvedTool>> {
    let per_page = limit.clamp(1, 20);
    let api_url =
        format!("https://api.github.com/repos/{owner}/{repo}/releases?per_page={per_page}");
    let releases = client
        .get(&api_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("failed to query GitHub releases API for {owner}/{repo}"))?
        .error_for_status()
        .with_context(|| format!("GitHub releases API returned an error for {owner}/{repo}"))?
        .json::<Vec<GitHubRelease>>()
        .await
        .with_context(|| {
            format!("failed to parse GitHub releases API response for {owner}/{repo}")
        })?;

    let resolved = releases
        .into_iter()
        .filter_map(|release| {
            let asset = release
                .assets
                .into_iter()
                .find(|asset| tool.matches_github_asset(&asset.name))?;
            Some(ResolvedTool {
                kind: tool,
                version: normalize(&release.tag_name),
                asset_name: asset.name,
                download_url: asset.browser_download_url,
                checksum: parse_github_digest(asset.digest.as_deref()),
            })
        })
        .take(limit)
        .collect::<Vec<_>>();

    if resolved.is_empty() {
        anyhow::bail!("no matching Windows zip assets found for {}", tool.id());
    }

    Ok(resolved)
}

async fn resolve_arm_toolchain(client: &Client) -> Result<ResolvedTool> {
    let page = client
        .get(ARM_GUIDE_PAGE)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("failed to query the Arm GNU toolchain install guide")?
        .error_for_status()
        .context("Arm GNU toolchain install guide returned an error")?
        .text()
        .await
        .context("failed to read the Arm GNU toolchain install guide")?;

    let version = extract_arm_version(&page)
        .context("could not find the latest Arm GNU toolchain version on the Arm install guide")?;

    Ok(arm_toolchain_package(version))
}

fn arm_toolchain_package(version: String) -> ResolvedTool {
    let asset_name = format!("arm-gnu-toolchain-{version}-mingw-w64-x86_64-arm-none-eabi.zip");
    let download_url = format!("{ARM_GITLAB_PACKAGE_BASE}/{version}/{asset_name}");

    ResolvedTool {
        kind: ToolKind::ArmNoneEabiGcc,
        version,
        asset_name,
        download_url,
        checksum: None,
    }
}

fn parse_github_digest(raw: Option<&str>) -> Option<ArchiveChecksum> {
    let raw = raw?;
    let (algorithm, value) = raw.split_once(':')?;
    if algorithm.eq_ignore_ascii_case("sha256")
        && value.len() == 64
        && value.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        Some(ArchiveChecksum {
            algorithm: ChecksumAlgorithm::Sha256,
            value: value.to_ascii_lowercase(),
        })
    } else {
        None
    }
}

fn extract_arm_version(page: &str) -> Option<String> {
    let prefix = "arm-gnu-toolchain-";
    let mut offset = 0;
    let mut seen = HashSet::new();

    while let Some(found) = page[offset..].find(prefix) {
        let start = offset + found + prefix.len();
        let remainder = &page[start..];
        let Some(end) = remainder.find('-') else {
            offset = start;
            continue;
        };
        let candidate = &remainder[..end];
        offset = start;

        if candidate.contains('<') || candidate.contains('>') {
            continue;
        }
        if candidate.len() > 24 || candidate.is_empty() {
            continue;
        }
        let candidate = candidate.to_ascii_lowercase();
        if !candidate
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
        {
            continue;
        }
        if !looks_like_arm_version(&candidate) {
            continue;
        }
        if seen.insert(candidate.clone()) {
            return Some(candidate);
        }
    }

    None
}

fn looks_like_arm_version(candidate: &str) -> bool {
    let Some((base, rel)) = candidate.split_once(".rel") else {
        return false;
    };
    if base.is_empty() || rel.is_empty() {
        return false;
    }
    if !rel.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let segments: Vec<&str> = base.split('.').collect();
    segments.len() >= 2
        && segments
            .iter()
            .all(|segment| !segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit()))
}

fn normalize_version(tag: &str) -> String {
    tag.trim_start_matches(['v', 'V']).to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        ARM_GITLAB_PACKAGE_BASE, arm_toolchain_package, extract_arm_version,
        extract_github_release_tag, fallback_github_asset_name, parse_github_digest,
    };
    use crate::tool::ToolKind;
    use crate::types::ChecksumAlgorithm;

    #[test]
    fn parse_github_digest_accepts_sha256_digest() {
        let digest = parse_github_digest(Some(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ))
        .unwrap();

        assert_eq!(digest.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(
            digest.value,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn parse_github_digest_rejects_unknown_digest_format() {
        assert!(parse_github_digest(Some("sha512:abc")).is_none());
        assert!(parse_github_digest(Some("sha256:not-hex")).is_none());
        assert!(parse_github_digest(None).is_none());
    }

    #[test]
    fn extract_github_release_tag_reads_redirect_url() {
        let url = reqwest::Url::parse("https://github.com/ninja-build/ninja/releases/tag/v1.13.2")
            .unwrap();

        assert_eq!(
            extract_github_release_tag(&url, "ninja-build", "ninja").unwrap(),
            "v1.13.2"
        );
    }

    #[test]
    fn fallback_github_asset_name_matches_supported_windows_assets() {
        assert_eq!(
            fallback_github_asset_name(ToolKind::Clangd, "22.1.0").unwrap(),
            "clangd-windows-22.1.0.zip"
        );
        assert_eq!(
            fallback_github_asset_name(ToolKind::Cmake, "4.3.3").unwrap(),
            "cmake-4.3.3-windows-x86_64.zip"
        );
        assert_eq!(
            fallback_github_asset_name(ToolKind::Ninja, "1.13.2").unwrap(),
            "ninja-win.zip"
        );
        assert_eq!(
            fallback_github_asset_name(ToolKind::ProbeRs, "0.31.0").unwrap(),
            "probe-rs-tools-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            fallback_github_asset_name(ToolKind::XpackOpenocd, "0.12.0-7").unwrap(),
            "xpack-openocd-0.12.0-7-win32-x64.zip"
        );
    }

    #[test]
    fn arm_toolchain_package_uses_gitlab_package_url() {
        let tool = arm_toolchain_package("15.3.rel1".to_string());

        assert_eq!(tool.kind, ToolKind::ArmNoneEabiGcc);
        assert_eq!(tool.version, "15.3.rel1");
        assert_eq!(
            tool.asset_name,
            "arm-gnu-toolchain-15.3.rel1-mingw-w64-x86_64-arm-none-eabi.zip"
        );
        assert_eq!(
            tool.download_url,
            format!("{ARM_GITLAB_PACKAGE_BASE}/15.3.rel1/{}", tool.asset_name)
        );
        assert!(tool.checksum.is_none());
    }

    #[test]
    fn extract_arm_version_reads_toolchain_archive_name() {
        let page = r#"
            <a href="/gnu/15.3.rel1/binrel/arm-gnu-toolchain-15.3.rel1-mingw-w64-x86_64-arm-none-eabi.zip">
        "#;

        assert_eq!(extract_arm_version(page).unwrap(), "15.3.rel1");
    }
}
