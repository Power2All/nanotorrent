// Port of src/picotorrent/core/geoip.cpp - resolves peer IP addresses to
// countries with a MaxMind GeoLite2 database. The (gzipped) database is
// downloaded from the `geoip.database_url` setting, cached in the data
// folder and refreshed weekly, like the original.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::core::configuration::Configuration;
use crate::core::environment::Environment;

const REFRESH_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub struct GeoIp {
    reader: Mutex<Option<maxminddb::Reader<Vec<u8>>>>,
}

impl GeoIp {
    pub fn new() -> Arc<GeoIp> {
        Arc::new(GeoIp {
            reader: Mutex::new(None),
        })
    }

    /// Load the database in the background (cache first, then download).
    pub fn spawn_load(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        env: &Environment,
        cfg: &Configuration,
    ) {
        if !cfg.get_bool("geoip.enabled") {
            return;
        }
        let Some(url) = cfg.get_string("geoip.database_url").filter(|u| !u.is_empty()) else {
            return;
        };

        let cache = env.get_application_data_path().join("geoip.mmdb");
        let this = self.clone();

        handle.spawn(async move {
            match load_database(&url, &cache).await {
                Ok(data) => match maxminddb::Reader::from_source(data) {
                    Ok(reader) => {
                        tracing::info!("GeoIP database loaded");
                        *this.reader.lock().unwrap() = Some(reader);
                    }
                    Err(err) => tracing::warn!("failed to parse GeoIP database: {err}"),
                },
                Err(err) => tracing::warn!("failed to load GeoIP database: {err:#}"),
            }
        });
    }

    /// Country name for a peer address ("ip:port"), if known.
    pub fn country(&self, addr: &str) -> Option<String> {
        self.lookup(addr).map(|(_, name)| name)
    }

    /// ISO 3166-1 alpha-2 code and display name, for the flag icon plus its
    /// label. The code is what the flag table is keyed by; either half can be
    /// missing from the database, so both are optional.
    pub fn lookup(&self, addr: &str) -> Option<(Option<String>, String)> {
        let ip = addr.parse::<SocketAddr>().ok()?.ip();
        let reader = self.reader.lock().ok()?;
        let lookup = reader.as_ref()?.lookup(ip).ok()?;
        let country = lookup.decode::<maxminddb::geoip2::Country>().ok()??;
        let country = country.country;
        let iso = country.iso_code.map(str::to_string);
        let name = country
            .names
            .english
            .map(str::to_string)
            .or_else(|| iso.clone())?;
        Some((iso, name))
    }
}

async fn load_database(url: &str, cache: &PathBuf) -> anyhow::Result<Vec<u8>> {
    // Fresh enough cache?
    if let Ok(meta) = tokio::fs::metadata(cache).await
        && meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < REFRESH_AFTER)
        && let Ok(data) = tokio::fs::read(cache).await
    {
        return Ok(data);
    }

    let fresh = download(url).await;
    match fresh {
        Ok(data) => {
            let _ = tokio::fs::write(cache, &data).await;
            Ok(data)
        }
        // Download failed - fall back to a stale cache if one exists.
        Err(err) => match tokio::fs::read(cache).await {
            Ok(data) => {
                tracing::warn!("GeoIP download failed ({err:#}), using cached database");
                Ok(data)
            }
            Err(_) => Err(err),
        },
    }
}

async fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    // Month-stamped URLs (DB-IP publishes one file per month). If the
    // current month isn't up yet, fall back to the previous one.
    if url.contains("{YYYY-MM}") {
        let now = chrono::Utc::now();
        let this_month = url.replace("{YYYY-MM}", &now.format("%Y-%m").to_string());
        match download_raw(&this_month).await {
            Ok(data) => return Ok(data),
            Err(err) => tracing::debug!("GeoIP download {this_month} failed: {err:#}"),
        }
        let previous = now - chrono::Duration::days(28);
        let last_month = url.replace("{YYYY-MM}", &previous.format("%Y-%m").to_string());
        return download_raw(&last_month).await;
    }

    download_raw(url).await
}

async fn download_raw(url: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;

    let response = reqwest::get(url).await?.error_for_status()?;
    let bytes = response.bytes().await?;

    // The database ships gzipped (…mmdb.gz); accept plain files too.
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}
