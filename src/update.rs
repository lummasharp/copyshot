//! Checks GitHub for a newer Copyshot release and installs it silently.
//! Uses WinHTTP so the app keeps its windows-sys-only footprint.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::*;

use crate::wide;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/lummasharp/copyshot/releases/latest";
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    download_url: String,
    sha256: String,
}

pub fn check() -> Result<Option<Update>, String> {
    let body = http_get(LATEST_RELEASE_URL)?;
    let release: Release =
        serde_json::from_slice(&body).map_err(|err| format!("Bad release data: {err}"))?;
    Ok(update_from_release(release, CURRENT_VERSION))
}

/// Downloads the installer, checks it against GitHub's SHA-256 digest and starts it silently.
/// The installer closes this process and relaunches the new version.
pub fn download_and_launch(update: &Update) -> Result<(), String> {
    let bytes = http_get(&update.download_url)?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(&update.sha256) {
        return Err("The downloaded update failed its security check.".to_string());
    }
    let path = std::env::temp_dir().join(format!("Copyshot-Setup-{}.exe", update.version));
    std::fs::write(&path, &bytes).map_err(|err| format!("Could not save update: {err}"))?;
    std::process::Command::new(&path)
        .args([
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/CLOSEAPPLICATIONS",
            "/AUTOLAUNCH=1",
        ])
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Could not start the update installer: {err}"))
}

fn update_from_release(release: Release, current: &str) -> Option<Update> {
    let latest = parse_version(&release.tag_name)?;
    if latest <= parse_version(current)? {
        return None;
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name.to_ascii_lowercase().ends_with(".exe"))?;
    let sha256 = asset.digest?.strip_prefix("sha256:")?.to_owned();
    Some(Update {
        version: format!("{}.{}.{}", latest.0, latest.1, latest.2),
        download_url: asset.browser_download_url,
        sha256,
    })
}

/// Parses `v1.2.3` / `1.2.3`. Pre-release tags don't parse, so they are never offered.
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.trim().trim_start_matches(['v', 'V']).split('.');
    let mut next = || parts.next()?.parse().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

fn split_https_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    (!host.is_empty()).then_some((host, if path.is_empty() { "/" } else { path }))
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let (host, path) = split_https_url(url).ok_or("Only HTTPS URLs are supported.")?;
    let failed = |step: &str| format!("{step} failed (error {}).", unsafe { GetLastError() });
    unsafe {
        let agent = wide(&format!("Copyshot/{CURRENT_VERSION}"));
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ));
        if session.0.is_null() {
            return Err(failed("Opening a connection"));
        }
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 60_000);

        let connection = Handle(WinHttpConnect(session.0, wide(host).as_ptr(), 443, 0));
        if connection.0.is_null() {
            return Err(failed("Connecting"));
        }
        // WinHTTP follows the https -> https redirect GitHub uses for release downloads.
        let request = Handle(WinHttpOpenRequest(
            connection.0,
            wide("GET").as_ptr(),
            wide(path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(failed("Creating the request"));
        }
        if WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0
        {
            return Err(failed("Downloading"));
        }

        let mut status = 0u32;
        let mut status_len = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut status_len,
            null_mut(),
        );
        if status != 200 {
            return Err(format!("The server responded with HTTP {status}."));
        }

        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err(failed("Reading the response"));
            }
            if read == 0 {
                return Ok(body);
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_DOWNLOAD_BYTES {
                return Err("The download is unexpectedly large.".to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, digest: Option<&str>) -> Release {
        Release {
            tag_name: tag.to_string(),
            assets: vec![
                Asset {
                    name: "notes.txt".to_string(),
                    browser_download_url: "https://example.com/notes.txt".to_string(),
                    digest: None,
                },
                Asset {
                    name: "Copyshot-Setup-9.0.0.EXE".to_string(),
                    browser_download_url: "https://example.com/setup.exe".to_string(),
                    digest: digest.map(str::to_string),
                },
            ],
        }
    }

    #[test]
    fn parses_plain_and_prefixed_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-beta"), None);
    }

    #[test]
    fn offers_only_newer_releases_with_a_checksummed_installer() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let update = update_from_release(release("v9.0.0", Some(&digest)), "0.2.0").unwrap();
        assert_eq!(update.version, "9.0.0");
        assert_eq!(update.download_url, "https://example.com/setup.exe");
        assert_eq!(update.sha256, "a".repeat(64));

        assert_eq!(
            update_from_release(release("v0.2.0", Some(&digest)), "0.2.0"),
            None
        );
        assert_eq!(
            update_from_release(release("v0.1.9", Some(&digest)), "0.2.0"),
            None
        );
        assert_eq!(update_from_release(release("v9.0.0", None), "0.2.0"), None);
    }

    #[test]
    fn splits_https_urls() {
        assert_eq!(
            split_https_url("https://api.github.com/repos/a/b"),
            Some(("api.github.com", "/repos/a/b"))
        );
        assert_eq!(
            split_https_url("https://example.com"),
            Some(("example.com", "/"))
        );
        assert_eq!(split_https_url("http://example.com/x"), None);
    }

    #[test]
    #[ignore = "hits the network"]
    fn fetches_the_latest_release() {
        let body = http_get(LATEST_RELEASE_URL).unwrap();
        let release: Release = serde_json::from_slice(&body).unwrap();
        assert!(parse_version(&release.tag_name).is_some());
    }
}
