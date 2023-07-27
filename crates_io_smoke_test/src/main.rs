#[macro_use]
extern crate tracing;

use anyhow::{anyhow, Context};
use clap::Parser;
use reqwest::blocking::Client;
use secrecy::{ExposeSecret, SecretString};
use std::fs::File;
use std::io::Write;
use std::process::Command;
use tempfile::tempdir;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[derive(clap::Parser, Debug)]
struct Options {
    /// name of the test crate that will be published to staging.crates.io
    #[arg(long, default_value = "crates-staging-test-tb")]
    crate_name: String,

    /// staging.crates.io API token that will be used to publish a new version
    #[arg(long, env = "CARGO_REGISTRY_TOKEN", hide_env_values = true)]
    token: SecretString,

    /// skip the publishing step and run the verifications for the highest
    /// uploaded version instead.
    #[arg(long)]
    skip_publish: bool,
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    let options = Options::parse();
    debug!(?options);

    let http_client = Client::builder()
        .user_agent("crates.io smoke test")
        .build()
        .context("Failed to initialize HTTP client")?;

    info!("Loading crate information from staging.crates.io…");
    let url = format!(
        "https://staging.crates.io/api/v1/crates/{}?include=versions",
        &options.crate_name
    );
    debug!(?url);

    let response = http_client
        .get(url)
        .send()
        .context("Failed to load crate information from staging.crates.io")?
        .error_for_status()
        .context("Failed to load crate information from staging.crates.io")?;

    #[derive(Debug, serde::Deserialize)]
    struct CrateResponse {
        #[serde(rename = "crate")]
        krate: Crate,
    }

    #[derive(Debug, serde::Deserialize)]
    struct Crate {
        max_version: semver::Version,
    }

    let json: CrateResponse = response
        .json()
        .context("Failed to deserialize crate information")?;
    debug!(?json);

    let old_version = json.krate.max_version;
    let mut new_version = old_version.clone();

    if options.skip_publish {
        info!("Skipping publish step");
    } else {
        new_version.patch += 1;
        info!(%old_version, %new_version, "Calculated new version number");

        info!("Creating temporary working folder…");
        let tempdir = tempdir().context("Failed to create temporary working folder")?;
        debug!(tempdir.path = %tempdir.path().display());

        info!("Creating `{}` project…", options.crate_name);
        let exit_status = Command::new("cargo")
            .args(["new", "--lib", &options.crate_name])
            .current_dir(tempdir.path())
            .env("CARGO_TERM_COLOR", "always")
            .status()
            .context("Failed to run `cargo new`")?;

        if !exit_status.success() {
            return Err(anyhow!("Failed to run `cargo new`"));
        }

        let project_path = tempdir.path().join(&options.crate_name);
        debug!(project_path = %project_path.display());

        {
            let manifest_path = project_path.join("Cargo.toml");
            info!(manifest_path = %manifest_path.display(), "Overriding `Cargo.toml` file…");
            let mut manifest_file =
                File::create(manifest_path).context("Failed to open `Cargo.toml` file")?;

            let new_content = format!(
                r#"[package]
name = "{}"
version = "{}"
edition = "2018"
license = "MIT"
description = "test crate"
"#,
                &options.crate_name, &new_version
            );

            manifest_file
                .write_all(new_content.as_bytes())
                .context("Failed to write `Cargo.toml` file content")?;
        }

        {
            let readme_path = project_path.join("README.md");
            info!(readme_path = %readme_path.display(), "Creating `README.md` file…");
            let mut readme_file =
                File::create(readme_path).context("Failed to open `README.md` file")?;

            let new_content = format!(
                "# {} v{}\n\n![](https://media1.giphy.com/media/Ju7l5y9osyymQ/200.gif)\n",
                &options.crate_name, &new_version
            );

            readme_file
                .write_all(new_content.as_bytes())
                .context("Failed to write `README.md` file content")?;
        }

        info!("Publishing to staging.crates.io…");
        let exit_status = Command::new("cargo")
            .args(["publish", "--registry", "staging", "--allow-dirty"])
            .current_dir(project_path)
            .env("CARGO_TERM_COLOR", "always")
            .env(
                "CARGO_REGISTRIES_STAGING_INDEX",
                "https://github.com/rust-lang/staging.crates.io-index",
            )
            .env(
                "CARGO_REGISTRIES_STAGING_TOKEN",
                options.token.expose_secret(),
            )
            .status()
            .context("Failed to run `cargo publish`")?;

        if !exit_status.success() {
            return Err(anyhow!("Failed to run `cargo publish`"));
        }
    }

    let version = new_version;
    info!(%version, "Checking staging.crates.io API for the new version…");

    let url = format!(
        "https://staging.crates.io/api/v1/crates/{}/{}",
        &options.crate_name, &version
    );
    debug!(?url);

    let response = http_client
        .get(url)
        .send()
        .context("Failed to load version information from staging.crates.io")?
        .error_for_status()
        .context("Failed to load version information from staging.crates.io")?;

    #[derive(Debug, serde::Deserialize)]
    struct VersionResponse {
        version: Version,
    }

    #[derive(Debug, serde::Deserialize)]
    struct Version {
        #[serde(rename = "crate")]
        krate: String,
        num: semver::Version,
    }

    let json: VersionResponse = response
        .json()
        .context("Failed to deserialize version information")?;
    debug!(?json);

    if json.version.krate != options.crate_name {
        return Err(anyhow!(
            "API returned an unexpected crate name; expected `{}` found `{}`",
            options.crate_name,
            json.version.krate
        ));
    }

    if json.version.num != version {
        return Err(anyhow!(
            "API returned an unexpected version number; expected `{}` found `{}`",
            version,
            json.version.num
        ));
    }

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let log_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_filter(env_filter);

    tracing_subscriber::registry().with(log_layer).init();
}
