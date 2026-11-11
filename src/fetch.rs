//! Reading inputs from files, stdin or http(s).

use std::io::Read;

use anyhow::{anyhow, Context, Result};

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Read the whole input: `-` is stdin, URLs are fetched, everything else is a path.
pub fn read_source(src: &str) -> Result<Vec<u8>> {
    if src == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("failed to read stdin")?;
        return Ok(buf);
    }
    if is_url(src) {
        return fetch(src);
    }
    let path = src.strip_prefix("file://").unwrap_or(src);
    std::fs::read(path).with_context(|| format!("cannot read {path}"))
}

#[cfg(feature = "remote")]
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    use std::sync::OnceLock;
    use std::time::Duration;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("shw/", env!("CARGO_PKG_VERSION")))
            .build()
    });
    let resp = agent.get(url).call().map_err(|e| anyhow!("{url}: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(128 * 1024 * 1024)
        .read_to_end(&mut buf)
        .with_context(|| format!("failed to download {url}"))?;
    Ok(buf)
}

#[cfg(not(feature = "remote"))]
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    Err(anyhow!("{url}: shw was built without the `remote` feature"))
}

/// Turn GitHub page URLs into raw content URLs:
/// `github.com/u/r` → its README, `github.com/u/r/blob/b/p` → raw file.
pub fn normalize_url(url: &str) -> String {
    let Some(rest) = url.strip_prefix("https://github.com/") else {
        return url.to_string();
    };
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        [user, repo] => format!("https://raw.githubusercontent.com/{user}/{repo}/HEAD/README.md"),
        [user, repo, "blob", tail @ ..] if !tail.is_empty() => {
            format!(
                "https://raw.githubusercontent.com/{user}/{repo}/{}",
                tail.join("/")
            )
        }
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls() {
        assert_eq!(
            normalize_url("https://github.com/a/b"),
            "https://raw.githubusercontent.com/a/b/HEAD/README.md"
        );
        assert_eq!(
            normalize_url("https://github.com/a/b/blob/main/docs/x.md"),
            "https://raw.githubusercontent.com/a/b/main/docs/x.md"
        );
        assert_eq!(
            normalize_url("https://example.com/x.png"),
            "https://example.com/x.png"
        );
    }
}
