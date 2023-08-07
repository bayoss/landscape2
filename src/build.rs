//! This module defines the functionality of the build CLI subcommand.

#![allow(non_upper_case_globals)]

use crate::{
    cache::Cache,
    crunchbase::collect_crunchbase_data,
    data::{get_landscape_data, LandscapeData},
    datasets::Datasets,
    github::collect_github_data,
    logos::prepare_logo,
    settings::{get_landscape_settings, LandscapeSettings},
    tmpl, BuildArgs, LogosSource,
};
use anyhow::{format_err, Result};
use askama::Template;
use futures::stream::{self, StreamExt};
use rust_embed::RustEmbed;
use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::Write,
    path::Path,
    sync::Arc,
    time::Instant,
};
use tracing::{debug, error, info, instrument};
use uuid::Uuid;

/// Environment variable containing the Crunchbase API key.
const CRUNCHBASE_API_KEY: &str = "CRUNCHBASE_API_KEY";

/// Path where the datasets will be written to in the output directory.
const DATASETS_PATH: &str = "data";

/// Environment variable containing a comma separated list of GitHub tokens.
const GITHUB_TOKENS: &str = "GITHUB_TOKENS";

/// Path where the logos will be written to in the output directory.
const LOGOS_PATH: &str = "logos";

/// Maximum number of logos to prepare concurrently.
const PREPARE_LOGOS_MAX_CONCURRENCY: usize = 20;

/// External services credentials.
#[derive(Debug, Default)]
struct Credentials {
    crunchbase_api_key: Option<String>,
    github_tokens: Option<Vec<String>>,
}

/// Embed web application assets into binary.
/// (these assets will be built automatically from the build script)
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

/// Build landscape website.
#[instrument(skip_all)]
pub(crate) async fn build(args: &BuildArgs) -> Result<()> {
    info!("building landscape website..");
    let start = Instant::now();

    // Check required web assets are present
    check_web_assets()?;

    // Setup output directory, creating it when needed
    setup_output_dir(&args.output_dir)?;

    // Setup cache
    let cache = Cache::new(&args.cache_dir)?;

    // Get landscape data from the source provided
    let mut landscape_data = get_landscape_data(&args.data_source).await?;

    // Get landscape settings from the source provided
    let settings = get_landscape_settings(&args.settings_source).await?;

    // Add some extra information to the landscape based on the settings
    landscape_data.add_featured_items_data(&settings)?;
    landscape_data.add_member_subcategory(&settings.members_category);

    // Prepare logos and copy them to the output directory
    prepare_logos(&cache, &args.logos_source, &mut landscape_data, &args.output_dir).await?;

    // Collect data from external services
    let credentials = read_credentials();
    let (crunchbase_data, github_data) = tokio::try_join!(
        collect_crunchbase_data(&cache, &credentials.crunchbase_api_key, &landscape_data),
        collect_github_data(&cache, &credentials.github_tokens, &landscape_data)
    )?;

    // Add data collected from external services to the landscape data
    landscape_data.add_crunchbase_data(crunchbase_data)?;
    landscape_data.add_github_data(github_data)?;

    // Generate datasets for web application
    let datasets = generate_datasets(&landscape_data, &settings, &args.output_dir)?;

    // Render index file and write it to the output directory
    render_index(&datasets, &args.output_dir)?;

    // Copy web assets files to the output directory
    copy_web_assets(&args.output_dir)?;

    let duration = start.elapsed().as_secs_f64();
    info!("landscape website built! (took: {:.3}s)", duration);

    Ok(())
}

/// Check web assets are present, to make sure the web app has been built.
#[instrument(skip_all, err)]
fn check_web_assets() -> Result<()> {
    if !WebAssets::iter().any(|path| path.starts_with("assets/")) {
        return Err(format_err!(
            "web assets not found, please make sure they have been built"
        ));
    }

    Ok(())
}

/// Copy web assets files to the output directory.
#[instrument(skip_all, err)]
fn copy_web_assets(output_dir: &Path) -> Result<()> {
    for asset_path in WebAssets::iter() {
        // The index document is a template that we'll render, so we don't want
        // to copy it as is.
        if asset_path == "index.html" || asset_path == ".keep" {
            continue;
        }

        if let Some(embedded_file) = WebAssets::get(&asset_path) {
            debug!(?asset_path, "copying file");
            if let Some(parent_path) = Path::new(asset_path.as_ref()).parent() {
                fs::create_dir_all(output_dir.join(parent_path))?;
            }
            let mut file = File::create(output_dir.join(asset_path.as_ref()))?;
            file.write_all(&embedded_file.data)?;
        }
    }

    Ok(())
}

/// Generate datasets from the landscape data and settings, as well as from the
/// data collected from external services (GitHub, Crunchbase, etc). Some of
/// the datasets will be embedded in the index document, and the rest will be
/// written to the DATASETS_PATH in the output directory.
#[instrument(skip_all, err)]
fn generate_datasets(
    landscape_data: &LandscapeData,
    settings: &LandscapeSettings,
    output_dir: &Path,
) -> Result<Datasets> {
    debug!("generating datasets");
    let datasets = Datasets::new(landscape_data, settings)?;

    debug!("copying datasets to output directory");
    let datasets_path = output_dir.join(DATASETS_PATH);

    // Base
    let mut base_file = File::create(datasets_path.join("base.json"))?;
    base_file.write_all(&serde_json::to_vec(&datasets.base)?)?;

    // Full
    let mut full_file = File::create(datasets_path.join("full.json"))?;
    full_file.write_all(&serde_json::to_vec(&datasets.full)?)?;

    Ok(datasets)
}

/// Prepare logos and copy them to the output directory, updating the logo
/// reference on each landscape item.
#[instrument(skip_all, err)]
async fn prepare_logos(
    cache: &Cache,
    logos_source: &LogosSource,
    landscape_data: &mut LandscapeData,
    output_dir: &Path,
) -> Result<()> {
    debug!("preparing logos");

    // Get logos from the source and copy them to the output directory
    let mut concurrency = num_cpus::get();
    if concurrency > PREPARE_LOGOS_MAX_CONCURRENCY {
        concurrency = PREPARE_LOGOS_MAX_CONCURRENCY;
    }
    let http_client = reqwest::Client::new();
    let logos_source = Arc::new(logos_source.clone());
    let logos: HashMap<Uuid, Option<String>> = stream::iter(landscape_data.items.iter())
        .map(|item| async {
            // Prepare logo
            let cache = cache.clone();
            let http_client = http_client.clone();
            let logos_source = logos_source.clone();
            let file_name = item.logo.clone();
            let logo = match tokio::spawn(async move {
                prepare_logo(&cache, http_client, &logos_source, &file_name).await
            })
            .await
            {
                Ok(Ok(logo)) => logo,
                Ok(Err(err)) => {
                    error!(?err, ?item.logo, "error preparing logo");
                    return (item.id, None);
                }
                Err(err) => {
                    error!(?err, ?item.logo, "error executing prepare_logo task");
                    return (item.id, None);
                }
            };

            // Copy logo to output dir using the digest(+.svg) as filename
            let file_name = format!("{}.svg", logo.digest);
            let Ok(mut file) = fs::File::create(output_dir.join(LOGOS_PATH).join(&file_name)) else {
                error!(?file_name, "error creating logo file in output dir");
                return (item.id, None);
            };
            if let Err(err) = file.write_all(&logo.svg_data) {
                error!(?err, ?file_name, "error writing logo to file in output dir");
            };

            (item.id, Some(format!("{LOGOS_PATH}/{file_name}")))
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    // Update logo field in landscape items to logo digest path
    for item in &mut landscape_data.items {
        item.logo = if let Some(Some(logo)) = logos.get(&item.id) {
            logo.clone()
        } else {
            String::new()
        }
    }

    Ok(())
}

/// Read external services credentials from environment.
#[instrument]
fn read_credentials() -> Credentials {
    let mut credentials = Credentials::default();

    if let Ok(crunchbase_api_key) = env::var(CRUNCHBASE_API_KEY) {
        credentials.crunchbase_api_key = Some(crunchbase_api_key);
    }
    if let Ok(github_tokens) = env::var(GITHUB_TOKENS) {
        credentials.github_tokens = Some(github_tokens.split(',').map(ToString::to_string).collect());
    }

    credentials
}

/// Render index file and write it to the output directory.
#[instrument(skip_all, err)]
fn render_index(datasets: &Datasets, output_dir: &Path) -> Result<()> {
    debug!("rendering index.html file");
    let index = tmpl::Index { datasets }.render()?;
    let mut file = File::create(output_dir.join("index.html"))?;
    file.write_all(index.as_bytes())?;

    Ok(())
}

/// Setup output directory, creating it as well as any of the other required
/// paths inside it when needed.
#[instrument(fields(?output_dir), skip_all, err)]
fn setup_output_dir(output_dir: &Path) -> Result<()> {
    if !output_dir.exists() {
        debug!("creating output directory");
        fs::create_dir_all(output_dir)?;
    }

    let datasets_path = output_dir.join(DATASETS_PATH);
    if !datasets_path.exists() {
        fs::create_dir(datasets_path)?;
    }

    let logos_path = output_dir.join(LOGOS_PATH);
    if !logos_path.exists() {
        fs::create_dir(logos_path)?;
    }

    Ok(())
}
