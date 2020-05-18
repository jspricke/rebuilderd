use rebuilderd_common::errors::*;
use rebuilderd_common::Distro;
use std::fs::File;
use std::process::Command;
use std::path::{Path};
use url::Url;

pub fn rebuild(distro: &Distro, url: &str) -> Result<bool> {
    let tmp = tempfile::Builder::new().prefix("rebuilderd").tempdir()?;

    let url = url.parse::<Url>()
        .context("Failed to parse input as url")?;

    let filename = url.path_segments()
        .ok_or_else(|| format_err!("Url doesn't seem to have a path"))?
        .last()
        .ok_or_else(|| format_err!("Failed to get filename from path"))?;
    if filename.is_empty() {
        bail!("Filename is empty");
    }

    let input = tmp.path().join(filename);
    download(&url, &input)
        .context("Failed to download original package")?;
    let input = input.to_str()
        .ok_or_else(|| format_err!("Input path contains invalid characters"))?;

    if !spawn_script(distro, &url.to_string(), input)? {
        return Ok(false);
    }
    info!("rebuilder script indicated success");

    let output = Path::new("./build/").join(filename);
    if !output.exists() {
        bail!("Rebuild script exited successfully but output package does not exist");
    }

    // TODO: diff files. this is already done by the rebuilder script right now, but we'd rather do it here

    Ok(true)
}

fn download(url: &Url, target: &Path) -> Result<()> {
    info!("Downloading {:?} to {:?}", url, target);
    let client = reqwest::blocking::Client::new();
    let mut response = client.get(&url.to_string())
        .send()?
        .error_for_status()?;

    let mut f = File::create(target)
        .context("Failed to create output file")?;
    let n = response.copy_to(&mut f)
        .context("Failed to download")?;
    info!("Downloaded {} bytes", n);

    Ok(())
}

fn spawn_script(distro: &Distro, url: &str, path: &str) -> Result<bool> {
    // TODO: establish a common interface to interface with distro rebuilders
    let bin = match distro {
        Distro::Archlinux => "rebuilder-archlinux.sh",
        Distro::Debian => "rebuilder-debian.sh",
    };

    for prefix in &[".", "/usr/libexec/rebuilderd", "/usr/local/libexec/rebuilderd"] {
        let bin = format!("{}/{}", prefix, bin);
        let bin = Path::new(&bin);

        if bin.exists() {
            info!("executing rebuilder script at {:?}", bin);
            let status = Command::new(&bin)
                .args(&[url, path])
                .status()?;

            info!("rebuilder script finished: {:?} (for {:?}, {:?})", status, url, path);
            return Ok(status.success());
        }
    }

    bail!("failed to find a rebuilder script")
}
