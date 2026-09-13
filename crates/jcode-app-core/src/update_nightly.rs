use super::*;
use serde::Deserialize;

const FORK_REPO: &str = "7sedam7/jcode";
const NIGHTLY_TAG: &str = "nightly";

#[derive(Deserialize)]
struct GitHubCommit {
    sha: String,
}

#[derive(Deserialize)]
struct GitHubComparison {
    status: String,
}

fn fetch_json<T: serde::de::DeserializeOwned>(url: &str, context: &str) -> Result<T> {
    let client = reqwest::blocking::Client::builder()
        .timeout(UPDATE_CHECK_TIMEOUT)
        .user_agent("jcode-updater")
        .build()?;
    let response = github_api_request(&client, url)
        .send()
        .with_context(|| context.to_string())?;
    if let Some(error) = rate_limit_error(&response) {
        return Err(error);
    }
    if !response.status().is_success() {
        anyhow::bail!("GitHub API error: {}", response.status());
    }
    let value = response.json().with_context(|| context.to_string())?;
    clear_rate_limit_backoff();
    Ok(value)
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn hashes_match(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    !left.is_empty()
        && left != "unknown"
        && !right.is_empty()
        && right != "unknown"
        && (left.starts_with(right) || right.starts_with(left))
}

fn comparison_requires_update(status: &str) -> Result<bool> {
    match status {
        "ahead" => Ok(true),
        "behind" | "identical" => Ok(false),
        "diverged" => anyhow::bail!(
            "Current build and fork nightly have diverged; refusing to replace local commits"
        ),
        other => anyhow::bail!("Unknown GitHub comparison status: {other}"),
    }
}

fn validate_artifact_hash(actual: &str, expected: &str) -> Result<()> {
    if !hashes_match(actual, expected) {
        anyhow::bail!(
            "Fork nightly artifact was built from {}, expected {}",
            actual,
            short_sha(expected)
        );
    }
    Ok(())
}

fn fetch_nightly_release() -> Result<GitHubRelease> {
    let release_url =
        format!("https://api.github.com/repos/{FORK_REPO}/releases/tags/{NIGHTLY_TAG}");
    let commit_url = format!("https://api.github.com/repos/{FORK_REPO}/commits/{NIGHTLY_TAG}");
    let release: GitHubRelease = fetch_json(&release_url, "Failed to fetch fork nightly release")?;
    let commit: GitHubCommit = fetch_json(&commit_url, "Failed to resolve fork nightly commit")?;
    nightly_release_for_commit(release, &commit.sha)
}

fn nightly_release_for_commit(
    mut release: GitHubRelease,
    commit_sha: &str,
) -> Result<GitHubRelease> {
    if commit_sha.trim().is_empty() {
        anyhow::bail!("Fork nightly tag did not resolve to a commit");
    }
    release._target_commitish = commit_sha.to_string();
    release.tag_name = format!("nightly-{}", short_sha(commit_sha));
    Ok(release)
}

pub(super) fn verify_downloaded_binary(
    release: &GitHubRelease,
    binary: &std::path::Path,
) -> Result<()> {
    if !release.tag_name.starts_with("nightly-") {
        return Ok(());
    }
    let output = std::process::Command::new(binary)
        .args(["version", "--json"])
        .env("JCODE_NON_INTERACTIVE", "1")
        .output()
        .context("Failed to inspect downloaded fork nightly binary")?;
    if !output.status.success() {
        anyhow::bail!("Downloaded fork nightly binary failed its version check");
    }
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("Downloaded fork nightly binary returned invalid version JSON")?;
    let actual = report["git_hash"]
        .as_str()
        .context("Downloaded fork nightly binary did not report a git hash")?;
    validate_artifact_hash(actual, &release._target_commitish)
}

fn nightly_is_update(current_hash: &str, release: &GitHubRelease) -> Result<bool> {
    let nightly_hash = release._target_commitish.trim();
    if hashes_match(current_hash, nightly_hash) {
        return Ok(false);
    }
    if current_hash.trim().is_empty() || current_hash == "unknown" {
        return Ok(true);
    }
    let compare_url = format!(
        "https://api.github.com/repos/{FORK_REPO}/compare/{}...{}",
        current_hash.trim(),
        nightly_hash
    );
    let comparison: GitHubComparison = fetch_json(
        &compare_url,
        "Failed to compare current build with fork nightly",
    )?;
    comparison_requires_update(&comparison.status)
}

pub(super) fn check_manual_update_blocking() -> Result<Option<GitHubRelease>> {
    let release = fetch_nightly_release()?;
    if !nightly_is_update(jcode_build_meta::git_hash(), &release)? {
        return Ok(None);
    }
    platform_asset(&release).context(
        "Fork nightly currently publishes prebuilt Linux x86_64 and aarch64 assets only",
    )?;
    checksum_asset(&release).context("Fork nightly release is still publishing SHA256SUMS")?;
    Ok(Some(release))
}

pub(super) fn prepare_manual_update_blocking() -> Result<PreparedUpdate> {
    let current = jcode_build_meta::version();
    let Some(release) = check_manual_update_blocking()? else {
        return Ok(PreparedUpdate::None {
            current: current.to_string(),
        });
    };
    let asset = platform_asset(&release)?;
    let metadata = UpdateMetadata::load().unwrap_or_else(|error| {
        crate::logging::warn(&format!("Could not load update timing metadata: {error}"));
        UpdateMetadata::default()
    });
    let duration = estimate_release_update_duration(asset._size, metadata.last_release_update_secs);
    let size_mb = asset._size as f64 / (1024.0 * 1024.0);
    let summary = format!(
        "Fork nightly update {} → {} (~{:.0} MB, {}). {}",
        current,
        release.tag_name,
        size_mb,
        format_duration_estimate(duration),
        if duration >= BACKGROUND_UPDATE_THRESHOLD {
            "Running in the background and will reload when it is ready."
        } else {
            "This should be quick."
        }
    );
    Ok(PreparedUpdate::Stable {
        release,
        estimate: update_estimate(summary, duration),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_labels_are_immutable_and_hash_comparison_accepts_short_ids() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert!(hashes_match("0123456", "0123456789abcdef"));
        assert!(!hashes_match("unknown", "0123456789abcdef"));
    }

    #[test]
    fn comparison_updates_only_when_nightly_is_newer_or_diverged() {
        assert!(comparison_requires_update("ahead").unwrap());
        assert!(!comparison_requires_update("behind").unwrap());
        assert!(!comparison_requires_update("identical").unwrap());
        assert!(comparison_requires_update("diverged").is_err());
        assert!(comparison_requires_update("mystery").is_err());
    }

    #[test]
    fn downloaded_artifact_must_match_resolved_nightly_commit() {
        assert!(validate_artifact_hash("0123456", "0123456789abcdef").is_ok());
        assert!(validate_artifact_hash("abcdef0", "0123456789abcdef").is_err());
    }

    #[test]
    fn nightly_release_uses_resolved_commit_not_moving_tag_name() {
        let release: GitHubRelease = serde_json::from_value(serde_json::json!({
            "tag_name": "nightly",
            "name": "Nightly",
            "html_url": "https://github.com/7sedam7/jcode/releases/tag/nightly",
            "published_at": "2026-09-12T00:00:00Z",
            "target_commitish": "master",
            "assets": []
        }))
        .unwrap();
        let release = nightly_release_for_commit(release, "0123456789abcdef").unwrap();
        assert_eq!(release.tag_name, "nightly-0123456");
        assert_eq!(release._target_commitish, "0123456789abcdef");
        assert!(nightly_release_for_commit(release, "").is_err());
    }

    #[test]
    #[ignore = "live GitHub API probe"]
    fn live_fork_nightly_release_resolves_commit_and_platform_asset() {
        let release = fetch_nightly_release().unwrap();
        assert!(release.tag_name.starts_with("nightly-"));
        assert_eq!(release._target_commitish.len(), 40);
        platform_asset(&release).unwrap();
        assert!(
            release
                .assets
                .iter()
                .any(|asset| asset.name == "SHA256SUMS")
        );
    }

    #[test]
    #[ignore = "live GitHub API probe"]
    fn live_manual_update_check_is_non_destructive() {
        if let Some(release) = check_manual_update_blocking().unwrap() {
            assert!(release.tag_name.starts_with("nightly-"));
            platform_asset(&release).unwrap();
            checksum_asset(&release).unwrap();
        }
    }
}
