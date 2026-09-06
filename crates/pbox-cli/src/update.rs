use serde::Deserialize;
use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Deserialize, serde::Serialize)]
struct Cache {
    checked_at: u64,
    latest: String,
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    crate_info: CrateInfo,
}

#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_version: String,
}

pub(crate) fn maybe_notify(json: bool) {
    if json || !should_check() {
        return;
    }
    let Some(cache_path) = cache_path() else {
        return;
    };
    if let Some(cache) = read_cache(&cache_path)
        && now().saturating_sub(cache.checked_at) < CHECK_INTERVAL.as_secs()
    {
        notify(&cache.latest);
        return;
    }

    let result = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .user_agent(format!("pbox/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
        .and_then(|client| {
            client
                .get("https://crates.io/api/v1/crates/pbox")
                .send()
                .ok()
        })
        .and_then(|response| response.error_for_status().ok())
        .and_then(|response| response.json::<CratesResponse>().ok());

    let Some(response) = result else {
        return;
    };
    let cache = Cache {
        checked_at: now(),
        latest: response.crate_info.max_version,
    };
    let _ = fs::create_dir_all(
        cache_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    );
    let _ = fs::write(&cache_path, serde_json::to_vec(&cache).unwrap_or_default());
    notify(&cache.latest);
}

fn should_check() -> bool {
    std::env::var_os("PBOX_NO_UPDATE_CHECK").is_none()
}

fn cache_path() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|path| path.join("pbox/update-check.json"))
}

fn read_cache(path: &std::path::Path) -> Option<Cache> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn notify(latest: &str) {
    if newer_than_current(latest) {
        crate::ui::stderr().hint(&format!(
            "pbox {latest} is available; run `pbox update` to install it."
        ));
    }
}

fn newer_than_current(version: &str) -> bool {
    parse_version(version) > parse_version(env!("CARGO_PKG_VERSION"))
}

fn parse_version(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_published_versions() {
        assert!(parse_version("1.2.3") > parse_version("1.2.2"));
        assert_eq!(parse_version("0.1.1-beta.1"), (0, 1, 1));
    }
}
