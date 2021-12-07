use serde::Deserialize;
use std::io::{Read, Write};
use std::{env, fs, io}; // is it still necesarry to include io with the next use?

use anyhow::{bail, Context};
use reqwest;
use structopt::StructOpt;
use zip;

#[derive(Debug, StructOpt)]
pub struct UpdateCommand {
    #[structopt(long)]
    pub version: Option<String>,
}

impl UpdateCommand {
    pub fn run(self) -> Result<(), anyhow::Error> {
        let version = self.version.or_else(get_latest_version).context(
			"Rojo could not determine the latest binary version. This is most likely an issue with Rojo. Please try again later."
		)?;

        println!(
            "Rojo will download binary version {}. Continue? [Y/n]",
            version
        );
        let mut approval = String::new();

        io::stdin()
            .read_line(&mut approval)
            .expect("Invalid response. Expected Y or n");

        match approval.to_lowercase().as_str().trim() {
            "y" => do_update(&version),
            _ => bail!("Did not choose to continue."),
        }
    }
}

#[derive(Deserialize)]
struct ReleaseAsset {
    tag_name: String,
}

fn get_latest_version() -> Option<String> {
    let url = "https://api.github.com/repos/rojo-rbx/rojo/releases/latest";
    let mut response = reqwest::get(url).ok()?;
    // println!("{}", response.json::<Vec<ReleaseAsset>>().ok()?["name"]);
    let json: ReleaseAsset = response.json().ok()?;
    let release_with_v = json.tag_name;
    let targ_len = release_with_v.chars().count();
    let release = &release_with_v[1..targ_len];
    //println!("{}", json.tag_name);
    // let data:Vec<ReleaseAsset> = response.json();
    // println!("{:?}", data);
    //println!("Latest Release: {}", data.tag_Name);

    return Some(release.to_string());
}

fn do_update(version: &str) -> anyhow::Result<()> {
    let platform = get_platform()
        .context("Unable to determine platform. There should be further context listed above.")?;
    let filename = format!("rojo-{}-{}.zip", version, &platform);
    println!("Downloading {}...", filename);
    download_file(version, &filename)?;
    println!("Update Done. run rojo --version to validate.");
    Ok(())
}

fn download_file(version: &str, file_name: &str) -> anyhow::Result<()> {
    let url = format!(
        "https://github.com/rojo-rbx/rojo/releases/download/v{}/{}",
        version, file_name
    );

    let asset_client = reqwest::Client::new();
    let build_asset_request = move || asset_client.get(&url);

    log::debug!("Getting release asset from github...");
    let mut asset_response = build_asset_request().send()?;

    let status = asset_response.status();
    if status.is_success() {
        let release_url = asset_response.url();
        let response = reqwest::get(release_url.as_str())?;
        let path = env::current_dir()?;
        let mut file = std::fs::File::create(path.join(file_name))?;
        let data: Result<Vec<_>, _> = response.bytes().collect();
        let data = data.expect("Unable to read data from github");
        file.write_all(&data).expect("Unable to write data to .zip");

        println!("Unzipping...");
        let rojo_file_name = format!("Rojo{}", get_file_extension());
        // We now have .zip, extract the (should be) only file in the zip.
        let zip_file = fs::File::open(path.join(file_name)).unwrap();
        let mut archive = zip::ZipArchive::new(zip_file).unwrap();
        let mut update_file = archive.by_index(0).unwrap();
        let mut out_file = fs::File::create(path.join(rojo_file_name)).unwrap();
        io::copy(&mut update_file, &mut out_file).unwrap();
    } else {
        bail!(
            "Error getting release asset from github: {}",
            asset_response.text()?
        );
    }
    Ok(())
}

fn get_platform() -> Option<String> {
    const OS: &str = env::consts::OS;
    if OS == "linux" || OS == "macos" {
        return Some(OS.to_string());
    } else if OS == "windows" {
        return Some("win64".to_string());
    } else {
        println!("Unsupported platform. Please create a github issue with the following information:\nPlatform {} not listed in get_platform() of update.rs", OS);
        return None;
    }
}

fn get_file_extension() -> &'static str {
    const OS: &str = env::consts::OS;
    if OS == "windows" {
        return ".exe";
    } else {
        return "";
    }
}
