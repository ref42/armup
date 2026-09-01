use anyhow::{Context, Result, anyhow, bail};
use console::Term;
use reqwest::Client;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, RANGE};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, TempPath};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use zip::ZipArchive;

use crate::environment::{apply_user_environment, build_env_plan};
use crate::state::{
    cleanup_staging_dirs, discover_installed_tools, installed_tool_from_dir, tool_version_dir,
    tool_versions_dir,
};
use crate::tool::EnvScope;
use crate::types::{ArchiveChecksum, ChecksumAlgorithm, InstalledTool, ResolvedTool};

const DEFAULT_MULTIPART_THRESHOLD_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MULTIPART_TARGET_PART_SIZE_BYTES: u64 = 16 * 1024 * 1024;
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_RENDER_INTERVAL: Duration = Duration::from_millis(200);
const RANGE_DOWNLOAD_MAX_ATTEMPTS: u32 = 8;
const PROGRESS_BAR_WIDTH: usize = 24;

#[derive(Debug, Clone, Copy)]
pub struct DownloadConfig {
    total_connection_limit: u8,
    max_connections_per_archive: u64,
    multipart_threshold_bytes: u64,
    multipart_target_part_size_bytes: u64,
}

impl DownloadConfig {
    pub const DEFAULT_CONNECTIONS: u8 = 12;
    pub const MAX_CONNECTIONS: u8 = 32;

    pub fn from_connections(connections: u8) -> Self {
        let connections = connections.clamp(1, Self::MAX_CONNECTIONS);
        Self {
            total_connection_limit: connections,
            max_connections_per_archive: u64::from(connections.min(8)),
            multipart_threshold_bytes: DEFAULT_MULTIPART_THRESHOLD_BYTES,
            multipart_target_part_size_bytes: DEFAULT_MULTIPART_TARGET_PART_SIZE_BYTES,
        }
    }

    fn total_connection_limit(self) -> u8 {
        self.total_connection_limit
    }
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self::from_connections(Self::DEFAULT_CONNECTIONS)
    }
}

#[derive(Clone)]
struct ProgressReporter {
    state: Arc<Mutex<ProgressState>>,
}

struct ProgressState {
    downloads: Vec<DownloadSnapshot>,
    download_indices: BTreeMap<String, usize>,
    rendered_lines: usize,
    last_render_at: Instant,
    term: Term,
}

struct DownloadSnapshot {
    label: String,
    version: String,
    total_bytes: u64,
    downloaded_bytes: u64,
    started_at: Option<Instant>,
    status: DownloadStatus,
}

#[derive(Clone, Copy)]
enum DownloadStatus {
    Pending,
    Installed,
    Downloading,
    Downloaded,
    Verifying,
    Extracting,
    Ready,
    Failed,
}

#[derive(Clone)]
struct DownloadTracker {
    inner: Arc<DownloadTrackerInner>,
}

struct DownloadTrackerInner {
    label: String,
    downloaded_bytes: AtomicU64,
    progress: ProgressReporter,
}

#[derive(Clone)]
struct DownloadLimiter {
    semaphore: Arc<Semaphore>,
}

impl DownloadLimiter {
    fn new(max_connections: u8) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_connections as usize)),
        }
    }

    async fn acquire(&self) -> Result<OwnedSemaphorePermit> {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .context("download connection limiter closed")
    }
}

pub async fn install_tools(
    client: &Client,
    root: &Path,
    packages: Vec<ResolvedTool>,
    scope: EnvScope,
    download_config: DownloadConfig,
) -> Result<()> {
    install_packages(client, root, packages, Some(scope), download_config, true).await
}

pub async fn install_tool_archives(
    client: &Client,
    root: &Path,
    packages: Vec<ResolvedTool>,
    download_config: DownloadConfig,
) -> Result<()> {
    install_packages(client, root, packages, None, download_config, false).await
}

async fn install_packages(
    client: &Client,
    root: &Path,
    packages: Vec<ResolvedTool>,
    scope: Option<EnvScope>,
    download_config: DownloadConfig,
    print_summary: bool,
) -> Result<()> {
    validate_install_root(root)?;
    let cleaned = cleanup_staging_dirs(root)?;
    for path in cleaned {
        println!("Removed stale staging directory: {}", path.display());
    }

    let progress = ProgressReporter::new(&packages);
    let limiter = DownloadLimiter::new(download_config.total_connection_limit());
    let mut tasks = JoinSet::new();
    for package in packages {
        let client = client.clone();
        let root = root.to_path_buf();
        let progress = progress.clone();
        let limiter = limiter.clone();
        tasks.spawn(async move {
            install_one(
                &client,
                &root,
                &package,
                &progress,
                &limiter,
                download_config,
            )
            .await
        });
    }

    let install_result = async {
        while let Some(result) = tasks.join_next().await {
            result.context("tool install task failed")??;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    progress.clear();
    install_result?;

    let installed_tools = discover_installed_tools(root)?;

    if let Some(EnvScope::User) = scope {
        let plan = build_env_plan(root, &installed_tools)?;
        print_env_preview(root, &plan)?;
        apply_user_environment(root, &plan)?;
    }

    if print_summary {
        println!("Installed tools under {}", root.display());
        for tool in &installed_tools {
            println!(
                "- {} {} -> {}",
                tool.kind,
                tool.version,
                tool.executable_path.display()
            );
        }

        match scope {
            Some(EnvScope::User) => {
                println!("Updated user PATH.");
                println!("Open a new terminal to pick up the changes.");
            }
            Some(EnvScope::None) | None => {
                println!("Skipped PATH registry changes.");
            }
        }
    }

    Ok(())
}

pub fn validate_install_root(root: &Path) -> Result<()> {
    if root.as_os_str().is_empty() {
        bail!("install root cannot be empty");
    }

    let mut components = root.components();
    match (components.next(), components.next()) {
        (Some(Component::Prefix(prefix)), Some(Component::RootDir)) => {
            let drive_root = PathBuf::from(format!("{}\\", prefix.as_os_str().to_string_lossy()));
            if !drive_root.exists() {
                bail!(
                    "install drive {} does not exist. Create the drive or pass --root with an existing drive.",
                    drive_root.display()
                );
            }
        }
        _ => bail!(
            "install root must be an absolute path like D:\\Embedded_Toolchain, got {}",
            root.display()
        ),
    }

    Ok(())
}

fn print_env_preview(root: &Path, plan: &crate::types::EnvPlan) -> Result<()> {
    let preview = crate::environment::preview_user_environment(root, plan)?;
    println!("PATH update preview:");
    if preview.removed_path_entries.is_empty()
        && preview.added_path_entries.is_empty()
        && preview.legacy_variables_present.is_empty()
    {
        println!("- No PATH changes needed.");
    } else {
        for entry in &preview.removed_path_entries {
            println!("- Remove managed old PATH entry: {entry}");
        }
        for entry in &preview.added_path_entries {
            println!("- Add PATH entry: {}", entry.display());
        }
        for variable in &preview.legacy_variables_present {
            println!("- Remove legacy environment variable: {variable}");
        }
    }
    println!(
        "- Final user PATH entries: {}",
        preview.final_path_entry_count
    );
    Ok(())
}

async fn install_one(
    client: &Client,
    root: &Path,
    package: &ResolvedTool,
    progress: &ProgressReporter,
    limiter: &DownloadLimiter,
    download_config: DownloadConfig,
) -> Result<InstalledTool> {
    let install_dir = tool_version_dir(root, package.kind, &package.version);

    if install_dir.exists() {
        let installed =
            installed_tool_from_dir(package.kind, package.version.clone(), install_dir)?;
        progress.mark_installed(package.kind.id());
        return Ok(installed);
    }

    let archive_path = download_archive(
        client,
        &package.download_url,
        &package.asset_name,
        package.kind.id(),
        progress,
        limiter,
        download_config,
    )
    .await?;
    progress.mark_verifying(package.kind.id());
    verify_archive_checksum(archive_path.as_ref(), package)?;

    let tool_root = tool_versions_dir(root, package.kind);
    fs::create_dir_all(&tool_root)
        .with_context(|| format!("failed to create {}", tool_root.display()))?;
    let staging_dir = tool_root.join(format!(".staging-{}", package.version));
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .with_context(|| format!("failed to remove {}", staging_dir.display()))?;
    }
    fs::create_dir_all(&staging_dir)
        .with_context(|| format!("failed to create {}", staging_dir.display()))?;

    progress.mark_extracting(package.kind.id());
    extract_zip_archive(archive_path.as_ref(), &staging_dir)?;

    if let Some(parent) = install_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::rename(&staging_dir, &install_dir).with_context(|| {
        format!(
            "failed to move {} into {}",
            staging_dir.display(),
            install_dir.display()
        )
    })?;

    let installed = installed_tool_from_dir(package.kind, package.version.clone(), install_dir)?;
    progress.mark_ready(package.kind.id());
    Ok(installed)
}

fn verify_archive_checksum(archive_path: &Path, package: &ResolvedTool) -> Result<()> {
    let Some(checksum) = &package.checksum else {
        return Ok(());
    };

    match checksum.algorithm {
        ChecksumAlgorithm::Sha256 => verify_sha256(archive_path, checksum).with_context(|| {
            format!(
                "checksum verification failed for {} {}",
                package.kind.id(),
                package.version
            )
        })?,
    }

    Ok(())
}

fn verify_sha256(path: &Path, checksum: &ArchiveChecksum) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let digest = hasher.finalize();
    let actual = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual != checksum.value {
        bail!(
            "expected sha256 {}, got {} for {}",
            checksum.value,
            actual,
            path.display()
        );
    }

    Ok(())
}

async fn download_archive(
    client: &Client,
    url: &str,
    asset_name: &str,
    tool_label: &str,
    progress: &ProgressReporter,
    limiter: &DownloadLimiter,
    download_config: DownloadConfig,
) -> Result<TempPath> {
    if let Some((content_length, multipart_parts)) =
        probe_multipart_download(client, url, asset_name, download_config).await?
    {
        let tracker = progress.start_download(tool_label, content_length);
        match download_archive_multipart(
            client,
            url,
            asset_name,
            content_length,
            multipart_parts,
            &tracker,
            limiter,
        )
        .await
        {
            Ok(path) => {
                progress.finish_download(&tracker);
                return Ok(path);
            }
            Err(error) => {
                progress_log(
                    progress,
                    format!("Retrying {tool_label} with one connection."),
                );
                tracker.reset_attempt_bytes();
                match download_archive_single_with_tracker(
                    client, url, asset_name, &tracker, limiter,
                )
                .await
                {
                    Ok(path) => {
                        progress.finish_download(&tracker);
                        return Ok(path);
                    }
                    Err(single_error) => {
                        progress.fail_download(&tracker);
                        return Err(single_error).with_context(|| {
                            format!("parallel download also failed for {asset_name}: {error:#}")
                        });
                    }
                }
            }
        }
    }

    download_archive_single(client, url, asset_name, tool_label, progress, limiter).await
}

async fn probe_multipart_download(
    client: &Client,
    url: &str,
    _asset_name: &str,
    download_config: DownloadConfig,
) -> Result<Option<(u64, u64)>> {
    let response = match client
        .head(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    if let Ok(response) = response.error_for_status() {
        let supports_ranges = response
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);
        if supports_ranges
            && let Some(content_length) = response.content_length()
            && let Some(result) = multipart_plan_for_length(content_length, download_config)
        {
            return Ok(Some(result));
        }
    }

    let probe = match client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .timeout(Duration::from_secs(20))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };

    if probe.status() != StatusCode::PARTIAL_CONTENT {
        return Ok(None);
    }

    let Some(total) = probe
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range_total)
    else {
        return Ok(None);
    };

    Ok(multipart_plan_for_length(total, download_config))
}

fn multipart_plan_for_length(
    content_length: u64,
    download_config: DownloadConfig,
) -> Option<(u64, u64)> {
    if download_config.max_connections_per_archive <= 1 {
        return None;
    }
    if content_length < download_config.multipart_threshold_bytes {
        return None;
    }

    let suggested_parts = content_length.div_ceil(download_config.multipart_target_part_size_bytes);
    let multipart_parts = suggested_parts.clamp(2, download_config.max_connections_per_archive);
    Some((content_length, multipart_parts))
}

fn parse_content_range_total(header: &str) -> Option<u64> {
    let (_, total) = header.split_once('/')?;
    total.parse().ok()
}

async fn download_archive_single(
    client: &Client,
    url: &str,
    asset_name: &str,
    tool_label: &str,
    progress: &ProgressReporter,
    limiter: &DownloadLimiter,
) -> Result<TempPath> {
    let _permit = limiter.acquire().await?;
    let response = open_download_response(client, url).await?;

    let total = response.content_length().unwrap_or(0);
    let tracker = progress.start_download(tool_label, total);

    match write_response_to_temp(response, asset_name, &tracker).await {
        Ok(path) => {
            progress.finish_download(&tracker);
            Ok(path)
        }
        Err(error) => {
            progress.fail_download(&tracker);
            Err(error)
        }
    }
}

async fn download_archive_single_with_tracker(
    client: &Client,
    url: &str,
    asset_name: &str,
    tracker: &DownloadTracker,
    limiter: &DownloadLimiter,
) -> Result<TempPath> {
    let _permit = limiter.acquire().await?;
    let response = open_download_response(client, url).await?;
    write_response_to_temp(response, asset_name, tracker).await
}

async fn open_download_response(client: &Client, url: &str) -> Result<reqwest::Response> {
    client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("download returned an error for {url}"))
}

async fn write_response_to_temp(
    mut response: reqwest::Response,
    asset_name: &str,
    tracker: &DownloadTracker,
) -> Result<TempPath> {
    let mut temp = NamedTempFile::new().context("failed to create a temporary download file")?;
    loop {
        let chunk = timeout(DOWNLOAD_IDLE_TIMEOUT, response.chunk())
            .await
            .with_context(|| {
                format!(
                    "download stalled for {asset_name}: no data received for {} seconds",
                    DOWNLOAD_IDLE_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("failed while reading downloaded data for {asset_name}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        temp.write_all(&chunk)
            .context("failed to write downloaded archive chunk")?;
        tracker.inc(chunk.len() as u64);
    }

    Ok(temp.into_temp_path())
}

async fn download_archive_multipart(
    client: &Client,
    url: &str,
    _asset_name: &str,
    content_length: u64,
    parts: u64,
    tracker: &DownloadTracker,
    limiter: &DownloadLimiter,
) -> Result<TempPath> {
    let temp_dir = tempfile::tempdir().context("failed to create temporary part directory")?;
    let mut tasks = JoinSet::new();
    let part_size = content_length.div_ceil(parts);

    for index in 0..parts {
        let start = index * part_size;
        if start >= content_length {
            break;
        }
        let end = (start + part_size).min(content_length) - 1;
        let client = client.clone();
        let url = url.to_string();
        let part_path = temp_dir.path().join(format!("part-{index:02}.bin"));
        let tracker = tracker.clone();
        let limiter = limiter.clone();
        tasks.spawn(async move {
            let _permit = limiter.acquire().await?;
            download_range_to_file(&client, &url, start, end, &part_path, &tracker).await?;
            Ok::<(u64, PathBuf), anyhow::Error>((index, part_path))
        });
    }

    let mut part_paths = Vec::new();
    while let Some(result) = tasks.join_next().await {
        part_paths.push(result.context("parallel download task failed")??);
    }
    part_paths.sort_by_key(|(index, _)| *index);

    assemble_archive_from_parts(part_paths)
}

async fn download_range_to_file(
    client: &Client,
    url: &str,
    start: u64,
    end: u64,
    part_path: &Path,
    progress: &DownloadTracker,
) -> Result<()> {
    File::create(part_path).with_context(|| format!("failed to create {}", part_path.display()))?;

    let expected_len = end - start + 1;
    let mut written = 0;
    let mut attempts = 0;
    let mut last_error = None;

    while written < expected_len {
        attempts += 1;
        let request_start = start + written;
        let range_value = format!("bytes={request_start}-{end}");
        let response = client.get(url).header(RANGE, range_value).send().await;
        let mut response = match response {
            Ok(response) => match response.error_for_status() {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(anyhow!(
                        "download range returned an error for bytes {request_start}-{end}: {error}"
                    ));
                    if attempts >= RANGE_DOWNLOAD_MAX_ATTEMPTS {
                        break;
                    }
                    sleep(range_retry_delay(attempts)).await;
                    continue;
                }
            },
            Err(error) => {
                last_error = Some(anyhow!(
                    "failed to request download range {request_start}-{end}: {error}"
                ));
                if attempts >= RANGE_DOWNLOAD_MAX_ATTEMPTS {
                    break;
                }
                sleep(range_retry_delay(attempts)).await;
                continue;
            }
        };

        if response.status() != StatusCode::PARTIAL_CONTENT {
            bail!(
                "server ignored ranged request for bytes {request_start}-{end} and returned {}",
                response.status()
            );
        }

        let mut output = OpenOptions::new()
            .append(true)
            .open(part_path)
            .with_context(|| format!("failed to open {}", part_path.display()))?;

        loop {
            let chunk = match timeout(DOWNLOAD_IDLE_TIMEOUT, response.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => {
                    if written < expected_len {
                        last_error = Some(anyhow!(
                            "download range {request_start}-{end} ended early after {} of {} bytes",
                            written,
                            expected_len
                        ));
                    }
                    break;
                }
                Ok(Err(error)) => {
                    last_error = Some(anyhow!(
                        "failed while reading byte range {request_start}-{end}: {error}"
                    ));
                    break;
                }
                Err(_) => {
                    last_error = Some(anyhow!(
                        "download stalled for byte range {request_start}-{end}: no data received for {} seconds",
                        DOWNLOAD_IDLE_TIMEOUT.as_secs()
                    ));
                    break;
                }
            };

            output
                .write_all(&chunk)
                .with_context(|| format!("failed to write {}", part_path.display()))?;
            written += chunk.len() as u64;
            progress.inc(chunk.len() as u64);

            if written >= expected_len {
                break;
            }
        }

        if written >= expected_len {
            return Ok(());
        }

        if attempts >= RANGE_DOWNLOAD_MAX_ATTEMPTS {
            break;
        }

        sleep(range_retry_delay(attempts)).await;
    }

    Err(last_error.unwrap_or_else(|| {
        anyhow!(
            "download range {start}-{end} incomplete after {} attempts",
            RANGE_DOWNLOAD_MAX_ATTEMPTS
        )
    }))
    .with_context(|| {
        format!(
            "failed to complete byte range {start}-{end} after {} attempts",
            RANGE_DOWNLOAD_MAX_ATTEMPTS
        )
    })
}

fn range_retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(250 * attempt.min(4) as u64)
}

impl ProgressReporter {
    fn new(packages: &[ResolvedTool]) -> Self {
        let now = Instant::now();
        let downloads = packages
            .iter()
            .map(|package| DownloadSnapshot {
                label: package.kind.id().to_string(),
                version: package.version.clone(),
                total_bytes: 0,
                downloaded_bytes: 0,
                started_at: None,
                status: DownloadStatus::Pending,
            })
            .collect::<Vec<_>>();
        let download_indices = downloads
            .iter()
            .enumerate()
            .map(|(index, download)| (download.label.clone(), index))
            .collect::<BTreeMap<_, _>>();

        Self {
            state: Arc::new(Mutex::new(ProgressState {
                downloads,
                download_indices,
                rendered_lines: 0,
                last_render_at: now.checked_sub(DOWNLOAD_RENDER_INTERVAL).unwrap_or(now),
                term: Term::stdout(),
            })),
        }
    }

    fn start_download(&self, label: &str, total: u64) -> DownloadTracker {
        {
            let mut state = self.state.lock().expect("progress state lock poisoned");
            if let Some(download) = download_mut(&mut state, label) {
                download.total_bytes = total;
                download.downloaded_bytes = 0;
                download.started_at = Some(Instant::now());
                download.status = DownloadStatus::Downloading;
            }
            render_downloads(&mut state, true);
        }

        DownloadTracker {
            inner: Arc::new(DownloadTrackerInner {
                label: label.to_string(),
                downloaded_bytes: AtomicU64::new(0),
                progress: self.clone(),
            }),
        }
    }

    fn finish_download(&self, tracker: &DownloadTracker) {
        let downloaded = tracker.downloaded_bytes();
        let mut state = self.state.lock().expect("progress state lock poisoned");
        if let Some(download) = download_mut(&mut state, &tracker.inner.label) {
            download.downloaded_bytes = downloaded;
            download.status = DownloadStatus::Downloaded;
        }
        render_downloads(&mut state, true);
    }

    fn fail_download(&self, tracker: &DownloadTracker) {
        let downloaded = tracker.downloaded_bytes();
        let mut state = self.state.lock().expect("progress state lock poisoned");
        if let Some(download) = download_mut(&mut state, &tracker.inner.label) {
            download.downloaded_bytes = downloaded;
            download.status = DownloadStatus::Failed;
        }
        clear_rendered_downloads(&mut state);
        let _ = state.term.write_line(&format!(
            "Download failed for {} ({} received).",
            tracker.inner.label,
            format_bytes(downloaded)
        ));
        let _ = state.term.flush();
        render_downloads(&mut state, true);
    }

    fn mark_verifying(&self, label: &str) {
        self.update_status(label, DownloadStatus::Verifying);
    }

    fn mark_extracting(&self, label: &str) {
        self.update_status(label, DownloadStatus::Extracting);
    }

    fn mark_ready(&self, label: &str) {
        self.update_status(label, DownloadStatus::Ready);
    }

    fn clear(&self) {
        let mut state = self.state.lock().expect("progress state lock poisoned");
        clear_rendered_downloads(&mut state);
    }

    fn log(&self, message: impl AsRef<str>) {
        let mut state = self.state.lock().expect("progress state lock poisoned");
        clear_rendered_downloads(&mut state);
        let _ = state.term.write_line(message.as_ref());
        let _ = state.term.flush();
        render_downloads(&mut state, true);
    }

    fn record_download(&self, label: &str, downloaded: u64) {
        let mut state = self.state.lock().expect("progress state lock poisoned");
        if let Some(download) = download_mut(&mut state, label) {
            download.downloaded_bytes = downloaded;
        }
        render_downloads(&mut state, false);
    }

    fn reset_download(&self, label: &str) {
        let mut state = self.state.lock().expect("progress state lock poisoned");
        if let Some(download) = download_mut(&mut state, label) {
            download.downloaded_bytes = 0;
            download.started_at = Some(Instant::now());
            download.status = DownloadStatus::Downloading;
        }
        render_downloads(&mut state, true);
    }

    fn mark_installed(&self, label: &str) {
        self.update_status(label, DownloadStatus::Installed);
    }

    fn update_status(&self, label: &str, status: DownloadStatus) {
        let mut state = self.state.lock().expect("progress state lock poisoned");
        if let Some(download) = download_mut(&mut state, label) {
            download.status = status;
        }
        render_downloads(&mut state, true);
    }
}

impl DownloadTracker {
    fn inc(&self, amount: u64) {
        let downloaded = self
            .inner
            .downloaded_bytes
            .fetch_add(amount, Ordering::Relaxed)
            .saturating_add(amount);
        self.inner
            .progress
            .record_download(&self.inner.label, downloaded);
    }

    fn downloaded_bytes(&self) -> u64 {
        self.inner.downloaded_bytes.load(Ordering::Relaxed)
    }

    fn reset_attempt_bytes(&self) {
        self.inner.downloaded_bytes.store(0, Ordering::Relaxed);
        self.inner.progress.reset_download(&self.inner.label);
    }
}

fn progress_log(progress: &ProgressReporter, message: impl AsRef<str>) {
    progress.log(message);
}

fn download_mut<'a>(state: &'a mut ProgressState, label: &str) -> Option<&'a mut DownloadSnapshot> {
    let index = *state.download_indices.get(label)?;
    state.downloads.get_mut(index)
}

fn render_downloads(state: &mut ProgressState, force: bool) {
    if state.downloads.is_empty() {
        clear_rendered_downloads(state);
        return;
    }
    if !force && state.last_render_at.elapsed() < DOWNLOAD_RENDER_INTERVAL {
        return;
    }

    clear_rendered_downloads(state);
    let terminal_width = state
        .term
        .size_checked()
        .map(|(_, width)| width as usize)
        .unwrap_or(100);
    for download in &state.downloads {
        let _ = state.term.write_line(&fit_terminal_line(
            &format_download_line(download),
            terminal_width,
        ));
    }
    let _ = state.term.flush();
    state.rendered_lines = state.downloads.len();
    state.last_render_at = Instant::now();
}

fn clear_rendered_downloads(state: &mut ProgressState) {
    if state.rendered_lines == 0 {
        return;
    }

    let _ = state.term.clear_last_lines(state.rendered_lines);
    let _ = state.term.flush();
    state.rendered_lines = 0;
}

fn format_download_line(download: &DownloadSnapshot) -> String {
    let percent = download_percent(download.downloaded_bytes, download.total_bytes);
    let elapsed = download
        .started_at
        .map(|started_at| started_at.elapsed())
        .unwrap_or_default();
    let bytes_per_second = average_bytes_per_second(download.downloaded_bytes, elapsed);
    let eta = format_eta(
        download.total_bytes,
        download.downloaded_bytes,
        bytes_per_second,
    );

    match download.status {
        DownloadStatus::Pending => format!(
            "{:<18} [{}] pending {}",
            download.label,
            progress_bar(0, 0, PROGRESS_BAR_WIDTH),
            download.version
        ),
        DownloadStatus::Installed => format!(
            "{:<18} [{}] installed {}",
            download.label,
            progress_bar(
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH
            ),
            download.version
        ),
        DownloadStatus::Downloading => format!(
            "{:<18} [{}] {percent:>3}% {}/{} {}/s eta {eta}",
            download.label,
            progress_bar(
                download.downloaded_bytes,
                download.total_bytes,
                PROGRESS_BAR_WIDTH
            ),
            format_bytes(download.downloaded_bytes),
            format_download_size(download.total_bytes),
            format_bytes(bytes_per_second),
        ),
        DownloadStatus::Downloaded => format!(
            "{:<18} [{}] downloaded {}",
            download.label,
            progress_bar(
                download.total_bytes.max(download.downloaded_bytes),
                download.total_bytes.max(download.downloaded_bytes),
                PROGRESS_BAR_WIDTH
            ),
            format_download_amount(download.downloaded_bytes, download.total_bytes)
        ),
        DownloadStatus::Verifying => format!(
            "{:<18} [{}] verifying",
            download.label,
            progress_bar(
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH
            )
        ),
        DownloadStatus::Extracting => format!(
            "{:<18} [{}] extracting",
            download.label,
            progress_bar(
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH
            )
        ),
        DownloadStatus::Ready => format!(
            "{:<18} [{}] ready {}",
            download.label,
            progress_bar(
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH as u64,
                PROGRESS_BAR_WIDTH
            ),
            download.version
        ),
        DownloadStatus::Failed => format!(
            "{:<18} [{}] failed {}",
            download.label,
            progress_bar(
                download.downloaded_bytes,
                download.total_bytes,
                PROGRESS_BAR_WIDTH
            ),
            format_bytes(download.downloaded_bytes)
        ),
    }
}

fn progress_bar(downloaded: u64, total: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if total == 0 {
        return "*".repeat(width);
    }

    let filled = downloaded
        .saturating_mul(width as u64)
        .checked_div(total)
        .unwrap_or(0)
        .min(width as u64) as usize;
    format!(
        "{}{}",
        "#".repeat(filled),
        "-".repeat(width.saturating_sub(filled))
    )
}

fn download_percent(downloaded: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }

    downloaded
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100)
}

fn average_bytes_per_second(downloaded: u64, elapsed: Duration) -> u64 {
    if elapsed.is_zero() {
        return 0;
    }

    (downloaded as f64 / elapsed.as_secs_f64()) as u64
}

fn format_eta(total: u64, downloaded: u64, bytes_per_second: u64) -> String {
    if total == 0 {
        return "?".to_string();
    }
    if downloaded >= total {
        return "0s".to_string();
    }
    if bytes_per_second == 0 {
        return "?".to_string();
    }

    format_duration(Duration::from_secs(
        total.saturating_sub(downloaded).div_ceil(bytes_per_second),
    ))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn fit_terminal_line(line: &str, terminal_width: usize) -> String {
    let width = terminal_width.saturating_sub(1).max(20);
    if line.len() <= width {
        return line.to_string();
    }

    line.chars().take(width).collect()
}

fn format_download_amount(downloaded: u64, total: u64) -> String {
    if total == 0 {
        return format!("{} received", format_bytes(downloaded));
    }

    format!("{}/{}", format_bytes(downloaded), format_bytes(total))
}

fn format_download_size(total: u64) -> String {
    if total == 0 {
        "size unknown".to_string()
    } else {
        format_bytes(total)
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{bytes:.0} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes / KIB)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes / MIB)
    } else {
        format!("{:.1} GiB", bytes / GIB)
    }
}

fn assemble_archive_from_parts(part_paths: Vec<(u64, PathBuf)>) -> Result<TempPath> {
    let mut archive = NamedTempFile::new().context("failed to create assembled archive")?;
    for (_, path) in part_paths {
        let mut part =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        io::copy(&mut part, archive.as_file_mut())
            .with_context(|| format!("failed to append {}", path.display()))?;
    }
    Ok(archive.into_temp_path())
}

fn extract_zip_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("failed to read zip archive {}", archive_path.display()))?;
    let prefix = archive_common_prefix(&mut archive)?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let raw_name = entry.name().replace('\\', "/");
        let stripped = strip_optional_prefix(&raw_name, prefix.as_deref());
        if stripped.is_empty() {
            continue;
        }

        let relative = sanitize_archive_path(&stripped).with_context(|| {
            format!("archive entry {raw_name:?} contained an invalid path component")
        })?;
        let output_path = destination.join(relative);

        if entry.is_dir() {
            fs::create_dir_all(&output_path)
                .with_context(|| format!("failed to create {}", output_path.display()))?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut output = File::create(&output_path)
            .with_context(|| format!("failed to create {}", output_path.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("failed to extract {}", output_path.display()))?;
    }

    Ok(())
}

fn archive_common_prefix<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<Option<String>> {
    let mut prefix: Option<String> = None;

    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let normalized = entry.name().replace('\\', "/");
        let trimmed = normalized.trim_matches('/');
        if trimmed.is_empty() {
            continue;
        }

        let mut segments = trimmed.split('/');
        let first = segments.next().unwrap_or_default();
        if segments.next().is_none() {
            return Ok(None);
        }

        match &prefix {
            Some(existing) if existing != first => return Ok(None),
            None => prefix = Some(first.to_string()),
            _ => {}
        }
    }

    Ok(prefix)
}

fn strip_optional_prefix(path: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) => path
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix('/'))
            .unwrap_or(path)
            .to_string(),
        None => path.to_string(),
    }
}

fn sanitize_archive_path(path: &str) -> Result<PathBuf> {
    let mut sanitized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            _ => bail!("unsupported path component"),
        }
    }
    Ok(sanitized)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MULTIPART_THRESHOLD_BYTES, DownloadConfig, multipart_plan_for_length};

    #[test]
    fn multipart_planner_uses_medium_archives() {
        let config = DownloadConfig::default();
        let (_, parts) =
            multipart_plan_for_length(DEFAULT_MULTIPART_THRESHOLD_BYTES, config).unwrap();

        assert_eq!(parts, 2);
    }

    #[test]
    fn multipart_planner_obeys_connection_limit() {
        let config = DownloadConfig::from_connections(4);
        let (_, parts) = multipart_plan_for_length(512 * 1024 * 1024, config).unwrap();

        assert_eq!(parts, 4);
    }

    #[test]
    fn multipart_planner_caps_each_archive_below_global_default() {
        let config = DownloadConfig::from_connections(16);
        let (_, parts) = multipart_plan_for_length(512 * 1024 * 1024, config).unwrap();

        assert_eq!(parts, 8);
    }

    #[test]
    fn multipart_planner_can_be_disabled_with_one_connection() {
        let config = DownloadConfig::from_connections(1);

        assert!(multipart_plan_for_length(512 * 1024 * 1024, config).is_none());
    }
}
