//! Reading inputs from files, stdin or http(s).

use std::io::Read;

use anyhow::{anyhow, Context, Result};

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Absolute path of an external tool. Only absolute PATH entries are searched:
/// empty, `.` or relative entries would resolve against the current directory,
/// letting a hostile checkout supply its own `./ffmpeg`.
pub fn tool(bin: &str) -> Option<std::path::PathBuf> {
    tool_in(bin, &std::env::var_os("PATH")?)
}

fn tool_in(bin: &str, path: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(bin))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

pub fn which(bin: &str) -> bool {
    tool(bin).is_some()
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
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent(concat!("shw/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent()
    });
    let mut resp = agent.get(url).call().map_err(|e| anyhow!("{url}: {e}"))?;
    let buf = resp
        .body_mut()
        .with_config()
        .limit(128 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("failed to download {url}"))?;
    Ok(buf)
}

/// Download something a document asked for (e.g. a markdown image). Unlike
/// [`fetch`], only public internet hosts are allowed — a document must not make
/// shw send requests into the local network — and every redirect hop is checked.
#[cfg(feature = "remote")]
pub fn fetch_untrusted(url: &str) -> Result<Vec<u8>> {
    use std::sync::OnceLock;
    use std::time::Duration;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(concat!("shw/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent()
    });
    let mut url = url.to_string();
    for _ in 0..6 {
        check_public_host(&url)?;
        let mut resp = agent.get(&url).call().map_err(|e| anyhow!("{url}: {e}"))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("{url}: redirect without location"))?;
            url = join_url(&url, loc);
            continue;
        }
        if status >= 400 {
            return Err(anyhow!("{url}: HTTP {status}"));
        }
        return resp
            .body_mut()
            .with_config()
            .limit(32 * 1024 * 1024)
            .read_to_vec()
            .with_context(|| format!("failed to download {url}"));
    }
    Err(anyhow!("{url}: too many redirects"))
}

#[cfg(not(feature = "remote"))]
pub fn fetch_untrusted(url: &str) -> Result<Vec<u8>> {
    fetch(url)
}

/// Resolve a redirect `Location` against the current URL.
#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn join_url(base: &str, loc: &str) -> String {
    if is_url(loc) {
        return loc.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    if let Some(rest) = loc.strip_prefix("//") {
        format!("{}{rest}", &base[..scheme_end])
    } else if loc.starts_with('/') {
        format!("{}{loc}", &base[..host_end])
    } else {
        let dir_end = base
            .rfind('/')
            .filter(|&i| i >= host_end)
            .map(|i| i + 1)
            .unwrap_or(base.len());
        format!("{}{loc}", &base[..dir_end])
    }
}

/// Host and port of an http(s) URL.
#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default);
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((authority.to_string(), default)),
    }
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn check_public_host(url: &str) -> Result<()> {
    use std::net::ToSocketAddrs;
    let (host, port) =
        host_port(url).ok_or_else(|| anyhow!("{url}: only http(s) URLs are allowed"))?;
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("{url}: cannot resolve host"))?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(anyhow!(
            "{url}: refusing to fetch from a local or private network address"
        ));
    }
    Ok(())
}

#[cfg_attr(not(feature = "remote"), allow(dead_code))]
fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64/10 carrier-grade NAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)) // benchmarking
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s0 = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s0 & 0xfe00) == 0xfc00 // unique local
                || (s0 & 0xffc0) == 0xfe80) // link local
        }
    }
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
    fn tool_ignores_relative_path_entries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("shw-tooltest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("shw-fake-tool");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // relative / empty entries never match, even if the file exists there
        let rel =
            std::ffi::OsString::from(format!(".::{}", dir.strip_prefix("/").unwrap().display()));
        assert_eq!(tool_in("shw-fake-tool", &rel), None);
        assert_eq!(tool_in("shw-fake-tool", dir.as_os_str()), Some(fake));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocks_private_hosts() {
        for url in [
            "http://127.0.0.1/x.png",
            "http://localhost:8080/x.png",
            "http://192.168.1.1/admin",
            "http://10.0.0.5/x",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/x",
            "http://[fd00::1]/x",
            "http://user@192.168.0.1/x",
            "ftp://example.com/x",
        ] {
            assert!(check_public_host(url).is_err(), "{url}");
        }
        assert!(is_public_ip("140.82.112.3".parse().unwrap()));
        assert!(!is_public_ip("100.100.1.1".parse().unwrap()));
    }

    #[test]
    fn joins_redirects() {
        assert_eq!(
            join_url("https://a.com/x/y.png", "/z.png"),
            "https://a.com/z.png"
        );
        assert_eq!(
            join_url("https://a.com/x/y.png", "z.png"),
            "https://a.com/x/z.png"
        );
        assert_eq!(join_url("https://a.com/x", "//b.com/q"), "https://b.com/q");
        assert_eq!(
            join_url("https://a.com/x", "https://c.com/"),
            "https://c.com/"
        );
    }

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
