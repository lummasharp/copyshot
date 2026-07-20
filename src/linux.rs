use ashpd::desktop::screenshot::Screenshot;
use std::fs;
use std::path::PathBuf;

const APP_NAME: &str = "Copyshot";

pub fn run() -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|err| format!("Could not start the async runtime: {err}"))?;
    let source = runtime.block_on(request_screenshot())?;
    let destination = screenshot_dir().join(format!("Screenshot {}.png", timestamp()));

    fs::create_dir_all(
        destination
            .parent()
            .ok_or_else(|| "Could not determine the screenshots directory.".to_string())?,
    )
    .map_err(|err| format!("Could not create the screenshots directory: {err}"))?;
    fs::copy(&source, &destination)
        .map_err(|err| format!("Could not save the screenshot: {err}"))?;

    println!("{APP_NAME}: saved {}", destination.display());
    Ok(())
}

async fn request_screenshot() -> Result<PathBuf, String> {
    let response = Screenshot::request()
        .interactive(true)
        .send()
        .await
        .map_err(|err| format!("Could not request a screenshot from the desktop portal: {err}"))?
        .response()
        .map_err(|err| format!("Screenshot request was cancelled or denied: {err}"))?;

    url::Url::parse(response.uri().as_str())
        .map_err(|err| format!("The desktop portal returned an invalid screenshot URI: {err}"))?
        .to_file_path()
        .map_err(|_| "The desktop portal returned a non-local screenshot URI.".to_string())
}

fn screenshot_dir() -> PathBuf {
    std::env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Pictures"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Screenshots")
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "capture".to_string())
}
