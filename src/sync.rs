use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use log::{debug, error, info, warn};
use sha1::{Digest, Sha1};

use crate::immich::{ImmichClient, UploadRequest};
use crate::photos::{self, AssetInfo, MediaType, ResourceInfo, ResourceKind, ResourceMeta, Trigger};
use crate::state::{AssetRow, StateStore, VariantRow};

pub const VARIANT_ORIGINAL: &str = "original";
pub const VARIANT_PAIRED_VIDEO: &str = "pairedVideo";
pub const VARIANT_EDITED: &str = "edited";
pub const VARIANT_EDITED_PAIRED_VIDEO: &str = "editedPairedVideo";

const ALBUM_BATCH: usize = 500;
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFilter {
    All,
    Images,
    Videos,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub device_id: String,
    pub temp_dir: PathBuf,
    pub include_hidden: bool,
    pub include_shared_albums: bool,
    pub media: MediaFilter,
    pub originals: bool,
    pub edited: bool,
    pub albums: bool,
    pub favorites: bool,
    pub newest_first: bool,
    pub upload_concurrency: usize,
    pub export_idle_timeout: Duration,
    pub replace_stale_edits: bool,
    pub limit: Option<usize>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Full,
    Partial { asset_ids: Vec<String>, albums: bool },
}

#[derive(Debug, Default, Clone)]
pub struct PassStats {
    pub scanned: usize,
    pub unchanged: usize,
    pub jobs: usize,
    pub files_exported: usize,
    pub uploaded: usize,
    pub duplicates: usize,
    pub failed: usize,
    pub album_adds: usize,
    pub albums_created: usize,
}

pub struct Engine {
    cfg: Config,
    client: Arc<ImmichClient>,
    state: Arc<StateStore>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct AssetSummary {
    pub local_id: String,
    pub media_type: MediaType,
    pub creation_ts: Option<f64>,
    pub modification_ts: f64,
    pub is_favorite: bool,
    pub has_adjustments: bool,
    pub duration_secs: f64,
}

impl AssetSummary {
    fn from(asset: &AssetInfo) -> Self {
        Self {
            local_id: asset.local_id.clone(),
            media_type: asset.media_type,
            creation_ts: asset.creation_ts,
            modification_ts: asset.modification_ts.or(asset.creation_ts).unwrap_or(0.0),
            is_favorite: asset.is_favorite,
            has_adjustments: asset.has_adjustments,
            duration_secs: asset.duration_secs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Resource(usize),
    RenderEdited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantPlan {
    pub variant: &'static str,
    pub signature: String,
    pub source: Source,
    pub filename_stem: String,
    pub extension: Option<String>,
    pub pairs_with: Option<&'static str>,
}

#[derive(Debug)]
struct FileJob {
    variant: &'static str,
    device_asset_id: String,
    signature: String,
    path: PathBuf,
    filename: String,
    pairs_with: Option<&'static str>,
}

#[derive(Debug)]
struct AssetJob {
    asset: AssetSummary,
    files: Vec<FileJob>,
    existing: Vec<VariantRow>,
    db_row: Option<AssetRow>,
}

#[derive(Debug, Default)]
struct WorkerStats {
    uploaded: usize,
    duplicates: usize,
    failed: usize,
}

fn rfc3339(ts: Option<f64>) -> String {
    let dt = ts
        .and_then(|t| DateTime::<Utc>::from_timestamp_millis((t * 1000.0) as i64))
        .unwrap_or_else(Utc::now);
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn sha1_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn split_filename(name: &str) -> (String, Option<String>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem.to_string(), Some(ext.to_lowercase())),
        _ => (name.to_string(), None),
    }
}

fn safe_id(local_id: &str) -> String {
    local_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect()
}

fn device_asset_id(asset: &AssetSummary, variant: &str) -> String {
    match variant {
        VARIANT_ORIGINAL if asset.media_type == MediaType::Video => format!("{}:video", asset.local_id),
        VARIANT_ORIGINAL => asset.local_id.clone(),
        other => format!("{}:{}", asset.local_id, other),
    }
}

fn find(resources: &[ResourceMeta], kind: ResourceKind) -> Option<usize> {
    resources.iter().position(|r| r.kind == kind)
}

fn resource_extension(res: &ResourceMeta, fallback: Option<&str>) -> Option<String> {
    split_filename(&res.original_filename)
        .1
        .or_else(|| photos::extension_for_uti(&res.uti))
        .or_else(|| fallback.map(str::to_string))
}

// Decides which files an asset contributes to Immich. Originals never change, so their signature is
// constant; edited renditions are keyed on the asset modification date.
pub fn plan_variants(originals: bool, edited: bool, asset: &AssetSummary, resources: &[ResourceMeta]) -> Vec<VariantPlan> {
    let mut plans = Vec::new();
    let edit_signature = format!("mod:{:.3}", asset.modification_ts);

    let primary = match asset.media_type {
        MediaType::Video => find(resources, ResourceKind::Video).or_else(|| find(resources, ResourceKind::FullSizeVideo)),
        _ => find(resources, ResourceKind::Photo).or_else(|| find(resources, ResourceKind::FullSizePhoto)),
    };
    let stem = primary
        .map(|i| split_filename(&resources[i].original_filename).0)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| safe_id(&asset.local_id));

    let paired = find(resources, ResourceKind::PairedVideo);
    if originals {
        if let Some(i) = paired {
            plans.push(VariantPlan {
                variant: VARIANT_PAIRED_VIDEO,
                signature: "original".into(),
                source: Source::Resource(i),
                filename_stem: stem.clone(),
                extension: resource_extension(&resources[i], Some("mov")),
                pairs_with: None,
            });
        }
        if let Some(i) = primary {
            plans.push(VariantPlan {
                variant: VARIANT_ORIGINAL,
                signature: "original".into(),
                source: Source::Resource(i),
                filename_stem: stem.clone(),
                extension: resource_extension(&resources[i], None),
                pairs_with: paired.map(|_| VARIANT_PAIRED_VIDEO),
            });
        }
    }

    if edited && asset.has_adjustments {
        match asset.media_type {
            MediaType::Image => {
                let mut pairs_with = None;
                if paired.is_some() {
                    if let Some(i) = find(resources, ResourceKind::FullSizePairedVideo) {
                        plans.push(VariantPlan {
                            variant: VARIANT_EDITED_PAIRED_VIDEO,
                            signature: edit_signature.clone(),
                            source: Source::Resource(i),
                            filename_stem: format!("{stem}_edited"),
                            extension: resource_extension(&resources[i], Some("mov")),
                            pairs_with: None,
                        });
                        pairs_with = Some(VARIANT_EDITED_PAIRED_VIDEO);
                    } else if originals {
                        pairs_with = Some(VARIANT_PAIRED_VIDEO);
                    }
                }
                let (source, extension) = match find(resources, ResourceKind::FullSizePhoto) {
                    Some(i) => (Source::Resource(i), resource_extension(&resources[i], None)),
                    None => (Source::RenderEdited, None),
                };
                plans.push(VariantPlan {
                    variant: VARIANT_EDITED,
                    signature: edit_signature,
                    source,
                    filename_stem: format!("{stem}_edited"),
                    extension,
                    pairs_with,
                });
            }
            MediaType::Video => {
                if let Some(i) = find(resources, ResourceKind::FullSizeVideo) {
                    plans.push(VariantPlan {
                        variant: VARIANT_EDITED,
                        signature: edit_signature,
                        source: Source::Resource(i),
                        filename_stem: format!("{stem}_edited"),
                        extension: resource_extension(&resources[i], Some("mov")),
                        pairs_with: None,
                    });
                }
            }
            _ => {}
        }
    }
    plans
}

// Folds a library change notification into whatever is already waiting to run.
pub fn merge_trigger(pending: Option<Scope>, trigger: Trigger) -> Scope {
    match (pending, trigger) {
        (_, Trigger::Shutdown) => unreachable!("shutdown is handled before merging"),
        (Some(Scope::Full), _) | (_, Trigger::Full) => Scope::Full,
        (None, Trigger::Assets(ids)) => Scope::Partial { asset_ids: ids, albums: false },
        (None, Trigger::Albums) => Scope::Partial { asset_ids: vec![], albums: true },
        (Some(Scope::Partial { mut asset_ids, albums }), Trigger::Assets(ids)) => {
            asset_ids.extend(ids);
            asset_ids.sort();
            asset_ids.dedup();
            Scope::Partial { asset_ids, albums }
        }
        (Some(Scope::Partial { asset_ids, .. }), Trigger::Albums) => Scope::Partial { asset_ids, albums: true },
    }
}

impl Engine {
    pub fn new(cfg: Config, client: Arc<ImmichClient>, state: Arc<StateStore>, cancel: Arc<AtomicBool>) -> Self {
        Self { cfg, client, state, cancel }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn passes_filter(&self, asset: &AssetInfo) -> bool {
        if asset.is_hidden && !self.cfg.include_hidden {
            return false;
        }
        match (self.cfg.media, asset.media_type) {
            (_, MediaType::Audio | MediaType::Unknown) => false,
            (MediaFilter::All, _) => true,
            (MediaFilter::Images, MediaType::Image) => true,
            (MediaFilter::Videos, MediaType::Video) => true,
            _ => false,
        }
    }

    fn failure_backoff_active(&self, local_id: &str) -> Result<bool> {
        let Some((attempts, last_ts)) = self.state.failure_attempts(local_id)? else {
            return Ok(false);
        };
        let wait = Duration::from_secs(600).checked_mul(1u32 << (attempts.clamp(1, 12) - 1) as u32).unwrap_or(MAX_FAILURE_BACKOFF).min(MAX_FAILURE_BACKOFF);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        Ok(now - last_ts < wait.as_secs_f64())
    }

    fn export_plans(&self, asset: &AssetInfo, summary: &AssetSummary, resources: &[ResourceInfo], plans: Vec<VariantPlan>) -> Result<Vec<FileJob>> {
        let mut files = Vec::new();
        let base = self.cfg.temp_dir.join(safe_id(&asset.local_id));
        let result: Result<()> = (|| {
            for plan in plans {
                if self.cancelled() {
                    bail!("cancelled");
                }
                let (path, ext) = match plan.source {
                    Source::Resource(index) => {
                        let ext = plan.extension.clone().unwrap_or_else(|| "bin".into());
                        let path = base.with_file_name(format!("{}-{}.{}", base.file_name().unwrap().to_string_lossy(), plan.variant, ext));
                        photos::export_resource(&resources[index], &path, self.cfg.export_idle_timeout)?;
                        (path, ext)
                    }
                    Source::RenderEdited => {
                        let stem = base.with_file_name(format!("{}-{}", base.file_name().unwrap().to_string_lossy(), plan.variant));
                        photos::render_edited_image(asset, &stem, self.cfg.export_idle_timeout)?
                    }
                };
                files.push(FileJob {
                    variant: plan.variant,
                    device_asset_id: device_asset_id(summary, plan.variant),
                    signature: plan.signature,
                    path,
                    filename: format!("{}.{}", plan.filename_stem, ext),
                    pairs_with: plan.pairs_with,
                });
            }
            Ok(())
        })();
        if let Err(e) = result {
            for f in &files {
                let _ = std::fs::remove_file(&f.path);
            }
            return Err(e);
        }
        Ok(files)
    }

    pub fn run_pass(&self, scope: &Scope) -> Result<PassStats> {
        let started = Instant::now();
        let mut stats = PassStats::default();
        std::fs::create_dir_all(&self.cfg.temp_dir).with_context(|| format!("creating temp dir {}", self.cfg.temp_dir.display()))?;

        let (asset_ids, albums_requested) = match scope {
            Scope::Full => (None, true),
            Scope::Partial { asset_ids, albums } => (Some(asset_ids), *albums),
        };

        if asset_ids.map(|ids| !ids.is_empty()).unwrap_or(true) {
            // Shared album contents are not part of the library fetch, so they get their own fetches.
            let mut fetches: Vec<(photos::AssetFetch, String)> = match asset_ids {
                None => vec![(photos::fetch_all_assets(self.cfg.include_hidden, self.cfg.newest_first), "library".into())],
                Some(ids) => vec![(photos::fetch_assets_by_ids(ids, self.cfg.include_hidden), "changed assets".into())],
            };
            if asset_ids.is_none() && self.cfg.include_shared_albums {
                for album in photos::fetch_albums(true).iter().filter(|a| a.is_shared) {
                    fetches.push((photos::fetch_album_assets(album, self.cfg.newest_first), format!("shared album \"{}\"", album.title)));
                }
            }
            let total: usize = fetches.iter().map(|(f, _)| f.len()).sum();
            info!("scanning {total} asset(s) across {} source(s)", fetches.len());

            let (tx, rx) = sync_channel::<AssetJob>(self.cfg.upload_concurrency.max(1) * 2);
            let rx = Arc::new(Mutex::new(rx));
            let worker_stats = Arc::new(Mutex::new(WorkerStats::default()));
            let workers: Vec<_> = (0..self.cfg.upload_concurrency.max(1))
                .map(|n| {
                    let rx = Arc::clone(&rx);
                    let stats = Arc::clone(&worker_stats);
                    let ctx = WorkerContext {
                        client: Arc::clone(&self.client),
                        state: Arc::clone(&self.state),
                        cancel: Arc::clone(&self.cancel),
                        device_id: self.cfg.device_id.clone(),
                        replace_stale_edits: self.cfg.replace_stale_edits,
                        favorites: self.cfg.favorites,
                    };
                    thread::Builder::new()
                        .name(format!("upload-{n}"))
                        .spawn(move || ctx.run(rx, stats))
                        .expect("spawning upload worker")
                })
                .collect();

            let mut seen: HashSet<String> = HashSet::new();
            let mut last_report = Instant::now();
            let mut position = 0usize;
            'scan: for (fetch, source) in &fetches {
                debug!("scanning {source}: {} asset(s)", fetch.len());
                for index in 0..fetch.len() {
                    position += 1;
                    if self.cancelled() {
                        info!("cancelled during scan");
                        break 'scan;
                    }
                    if let Some(limit) = self.cfg.limit {
                        if stats.jobs >= limit {
                            info!("reached --limit {limit}");
                            break 'scan;
                        }
                    }
                    if last_report.elapsed() > Duration::from_secs(30) {
                        info!("scan progress: {position}/{total} (assets with work so far: {})", stats.jobs);
                        last_report = Instant::now();
                    }
                    let asset = fetch.get(index);
                    if !self.passes_filter(&asset) {
                        continue;
                    }
                    if fetches.len() > 1 && !seen.insert(asset.local_id.clone()) {
                        continue;
                    }
                    stats.scanned += 1;

                    match self.process_asset(&asset, &mut stats) {
                        Ok(Some(job)) => {
                            stats.jobs += 1;
                            stats.files_exported += job.files.len();
                            if tx.send(job).is_err() {
                                error!("upload workers stopped unexpectedly");
                                break 'scan;
                            }
                        }
                        Ok(None) => {}
                        Err(_) if self.cancelled() => break 'scan,
                        Err(e) => {
                            stats.failed += 1;
                            warn!("{}: {e:#}", asset.local_id);
                            if let Err(db_err) = self.state.record_failure(&asset.local_id, &format!("{e:#}")) {
                                error!("recording failure: {db_err:#}");
                            }
                        }
                    }
                }
            }
            drop(tx);
            for worker in workers {
                if let Err(e) = worker.join() {
                    error!("upload worker panicked: {e:?}");
                }
            }
            let ws = worker_stats.lock().map(|s| WorkerStats { uploaded: s.uploaded, duplicates: s.duplicates, failed: s.failed }).unwrap_or_default();
            stats.uploaded = ws.uploaded;
            stats.duplicates = ws.duplicates;
            stats.failed += ws.failed;
        }

        let sync_albums = self.cfg.albums && (albums_requested || stats.uploaded + stats.duplicates > 0);
        if sync_albums && !self.cancelled() {
            match self.sync_albums() {
                Ok((adds, created)) => {
                    stats.album_adds = adds;
                    stats.albums_created = created;
                }
                Err(e) => error!("album sync failed: {e:#}"),
            }
        }

        info!(
            "pass done in {:.1?}: scanned {}, unchanged {}, assets with work {}, files exported {}, uploaded {}, duplicates {}, failed {}, album adds {}, albums created {}",
            started.elapsed(), stats.scanned, stats.unchanged, stats.jobs, stats.files_exported, stats.uploaded, stats.duplicates, stats.failed, stats.album_adds, stats.albums_created
        );
        Ok(stats)
    }

    fn process_asset(&self, asset: &AssetInfo, stats: &mut PassStats) -> Result<Option<AssetJob>> {
        let summary = AssetSummary::from(asset);
        let db_row = self.state.get_asset(&asset.local_id)?;
        let has_failure = self.state.failure_attempts(&asset.local_id)?.is_some();
        let unchanged = db_row.as_ref().is_some_and(|row| {
            (row.modification_ts - summary.modification_ts).abs() < 0.001
                && row.has_adjustments == summary.has_adjustments
                && row.is_favorite == summary.is_favorite
                && (row.favorite_synced || !self.cfg.favorites)
        });
        if unchanged && !has_failure {
            stats.unchanged += 1;
            return Ok(None);
        }
        if has_failure && self.failure_backoff_active(&asset.local_id)? {
            debug!("{}: skipping, failure backoff active", asset.local_id);
            return Ok(None);
        }

        let resources = photos::resources_for(asset);
        let metas: Vec<ResourceMeta> = resources.iter().map(|r| r.meta.clone()).collect();
        let plans = plan_variants(self.cfg.originals, self.cfg.edited, &summary, &metas);
        if plans.is_empty() && !resources.is_empty() {
            warn!("{}: nothing exportable (resources: {:?})", asset.local_id, metas.iter().map(|r| r.kind).collect::<Vec<_>>());
        }
        let existing = self.state.variants_for(&asset.local_id)?;
        let pending: Vec<VariantPlan> = plans
            .into_iter()
            .filter(|p| !existing.iter().any(|e| e.variant == p.variant && e.signature == p.signature))
            .collect();

        if self.cfg.dry_run {
            for p in &pending {
                info!("[dry-run] {} would upload {} ({}.{})", asset.local_id, p.variant, p.filename_stem, p.extension.as_deref().unwrap_or("?"));
            }
            if pending.is_empty() && db_row.is_none() {
                info!("[dry-run] {} nothing to upload", asset.local_id);
            }
            return Ok(None);
        }

        if pending.is_empty() && existing.is_empty() {
            // Nothing exportable (for example an asset with no local resources); remember it so the scan stays cheap.
            self.state.upsert_asset(
                &summary.local_id,
                summary.media_type.as_i64(),
                summary.creation_ts,
                summary.modification_ts,
                summary.has_adjustments,
                summary.is_favorite,
                true,
            )?;
            self.state.clear_failure(&summary.local_id)?;
            return Ok(None);
        }

        let files = self.export_plans(asset, &summary, &resources, pending)?;
        Ok(Some(AssetJob { asset: summary, files, existing, db_row }))
    }

    fn sync_albums(&self) -> Result<(usize, usize)> {
        let albums = photos::fetch_albums(self.cfg.include_shared_albums);
        info!("syncing {} album(s)", albums.len());
        let mut title_counts: HashMap<&str, usize> = HashMap::new();
        for a in &albums {
            *title_counts.entry(a.title.as_str()).or_default() += 1;
        }
        let mut server_albums: Option<HashMap<String, String>> = None;
        let mut adds = 0usize;
        let mut created = 0usize;

        for album in &albums {
            if self.cancelled() {
                break;
            }
            let asset_ids = photos::album_asset_ids(album);
            if asset_ids.is_empty() {
                continue;
            }
            let name = if title_counts[album.title.as_str()] > 1 {
                format!("{} ({})", album.title, safe_id(&album.local_id).chars().take(8).collect::<String>())
            } else {
                album.title.clone()
            };
            let candidates = self.state.album_candidate_ids(&asset_ids)?;
            let mut wanted: Vec<String> = candidates.values().flatten().cloned().collect();
            if wanted.is_empty() {
                continue;
            }

            let mapping = self.state.get_album(&album.local_id)?;
            let immich_album_id = match mapping {
                Some(m) => {
                    if m.title != name {
                        match self.client.rename_album(&m.immich_album_id, &name) {
                            Ok(()) => {
                                info!("renamed Immich album \"{}\" -> \"{}\"", m.title, name);
                                self.state.upsert_album(&album.local_id, &name, &m.immich_album_id)?;
                            }
                            Err(e) => warn!("renaming album \"{}\": {e:#}", m.title),
                        }
                    }
                    m.immich_album_id
                }
                None => {
                    if server_albums.is_none() {
                        let list = self.client.list_albums().context("listing Immich albums")?;
                        server_albums = Some(list.into_iter().filter_map(|a| a.album_name.map(|n| (n, a.id))).collect());
                    }
                    let known = server_albums.as_mut().unwrap();
                    let id = match known.get(&name) {
                        Some(id) => {
                            info!("adopting existing Immich album \"{name}\"");
                            id.clone()
                        }
                        None => {
                            let dto = self.client.create_album(&name).with_context(|| format!("creating album \"{name}\""))?;
                            info!("created Immich album \"{name}\"");
                            created += 1;
                            known.insert(name.clone(), dto.id.clone());
                            dto.id
                        }
                    };
                    self.state.upsert_album(&album.local_id, &name, &id)?;
                    id
                }
            };

            let members: HashSet<String> = self.state.album_members(&album.local_id)?.into_iter().collect();
            wanted.retain(|id| !members.contains(id));
            wanted.sort();
            wanted.dedup();
            for chunk in wanted.chunks(ALBUM_BATCH) {
                if self.cancelled() {
                    break;
                }
                match self.client.add_assets_to_album(&immich_album_id, chunk) {
                    Ok(rejected) => {
                        for (id, err) in &rejected {
                            warn!("album \"{name}\": server rejected {id}: {err}");
                        }
                        let rejected_ids: HashSet<&String> = rejected.iter().map(|(id, _)| id).collect();
                        let accepted: Vec<String> = chunk.iter().filter(|id| !rejected_ids.contains(id)).cloned().collect();
                        adds += accepted.len();
                        self.state.add_album_members(&album.local_id, &accepted)?;
                    }
                    Err(e) => {
                        warn!("album \"{name}\": adding {} asset(s) failed: {e:#}", chunk.len());
                        break;
                    }
                }
            }
            if adds > 0 {
                debug!("album \"{name}\": {} new asset(s)", adds);
            }
        }
        Ok((adds, created))
    }
}

struct WorkerContext {
    client: Arc<ImmichClient>,
    state: Arc<StateStore>,
    cancel: Arc<AtomicBool>,
    device_id: String,
    replace_stale_edits: bool,
    favorites: bool,
}

impl WorkerContext {
    fn run(self, rx: Arc<Mutex<Receiver<AssetJob>>>, stats: Arc<Mutex<WorkerStats>>) {
        loop {
            let job = {
                let guard = match rx.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                guard.recv()
            };
            let Ok(job) = job else { return };
            let local_id = job.asset.local_id.clone();
            let paths: Vec<PathBuf> = job.files.iter().map(|f| f.path.clone()).collect();
            let outcome = self.process(job, &stats);
            for p in paths {
                let _ = std::fs::remove_file(p);
            }
            match outcome {
                Ok(()) => {
                    if let Err(e) = self.state.clear_failure(&local_id) {
                        error!("clearing failure for {local_id}: {e:#}");
                    }
                }
                Err(_) if self.cancel.load(Ordering::Relaxed) => {
                    debug!("{local_id}: left for the next pass (cancelled)");
                }
                Err(e) => {
                    warn!("{local_id}: {e:#}");
                    if let Ok(mut s) = stats.lock() {
                        s.failed += 1;
                    }
                    if let Err(db_err) = self.state.record_failure(&local_id, &format!("{e:#}")) {
                        error!("recording failure for {local_id}: {db_err:#}");
                    }
                }
            }
        }
    }

    fn process(&self, job: AssetJob, stats: &Arc<Mutex<WorkerStats>>) -> Result<()> {
        let asset = &job.asset;
        let mut ids_this_job: HashMap<&'static str, String> = HashMap::new();
        let mut freshly_created: HashSet<String> = HashSet::new();

        for file in &job.files {
            if self.cancel.load(Ordering::Relaxed) {
                bail!("cancelled before uploading {}", file.filename);
            }
            let live_photo_video_id = match file.pairs_with {
                Some(variant) => Some(
                    ids_this_job
                        .get(variant)
                        .cloned()
                        .or_else(|| job.existing.iter().find(|e| e.variant == variant).map(|e| e.immich_id.clone()))
                        .ok_or_else(|| anyhow!("{}: paired video {variant} has no Immich id", file.filename))?,
                ),
                None => None,
            };

            let sha1 = sha1_file(&file.path).with_context(|| format!("hashing {}", file.filename))?;
            let (immich_id, status) = match self.client.find_duplicate(&file.device_asset_id, &sha1)? {
                Some(id) => (id, "duplicate".to_string()),
                None => {
                    let req = UploadRequest {
                        file: &file.path,
                        filename: &file.filename,
                        sha1_hex: &sha1,
                        device_id: &self.device_id,
                        device_asset_id: &file.device_asset_id,
                        file_created_at: &rfc3339(asset.creation_ts),
                        file_modified_at: &rfc3339(Some(asset.modification_ts)),
                        duration_ms: (asset.media_type == MediaType::Video && file.variant != VARIANT_PAIRED_VIDEO)
                            .then(|| (asset.duration_secs * 1000.0).round().max(0.0) as u64),
                        is_favorite: asset.is_favorite,
                        live_photo_video_id: live_photo_video_id.as_deref(),
                    };
                    let resp = self.client.upload_asset(&req)?;
                    (resp.id, resp.status)
                }
            };
            info!("{}: {} {} -> {} ({})", asset.local_id, file.variant, file.filename, immich_id, status);
            if let Ok(mut s) = stats.lock() {
                if status == "duplicate" {
                    s.duplicates += 1;
                } else {
                    s.uploaded += 1;
                }
            }
            if status == "created" {
                freshly_created.insert(immich_id.clone());
            }

            let stale = job.existing.iter().find(|e| e.variant == file.variant && e.immich_id != immich_id).map(|e| e.immich_id.clone());
            self.state.upsert_variant(&asset.local_id, file.variant, &file.signature, &immich_id, &sha1)?;
            if let Some(old) = stale {
                let is_edit = file.variant == VARIANT_EDITED || file.variant == VARIANT_EDITED_PAIRED_VIDEO;
                if is_edit && self.replace_stale_edits {
                    match self.client.delete_assets(std::slice::from_ref(&old)) {
                        Ok(()) => info!("{}: trashed superseded {} {}", asset.local_id, file.variant, old),
                        Err(e) => warn!("{}: trashing superseded {} {old}: {e:#}", asset.local_id, file.variant),
                    }
                } else {
                    debug!("{}: previous {} {} left in place", asset.local_id, file.variant, old);
                }
            }
            ids_this_job.insert(file.variant, immich_id);
        }

        let mut favorite_synced = true;
        if self.favorites {
            let needs_push = asset.is_favorite
                || job.db_row.as_ref().is_some_and(|r| r.is_favorite != asset.is_favorite || !r.favorite_synced);
            if needs_push {
                let variants = self.state.variants_for(&asset.local_id)?;
                for v in variants.iter().filter(|v| !v.variant.ends_with("PairedVideo") && !freshly_created.contains(&v.immich_id)) {
                    if let Err(e) = self.client.set_favorite(&v.immich_id, asset.is_favorite) {
                        warn!("{}: setting favorite on {}: {e:#}", asset.local_id, v.immich_id);
                        favorite_synced = false;
                    }
                }
            }
        }

        self.state.upsert_asset(
            &asset.local_id,
            asset.media_type.as_i64(),
            asset.creation_ts,
            asset.modification_ts,
            asset.has_adjustments,
            asset.is_favorite,
            favorite_synced,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(kind: ResourceKind, name: &str, uti: &str) -> ResourceMeta {
        ResourceMeta { kind, original_filename: name.into(), uti: uti.into() }
    }

    fn image(has_adjustments: bool) -> AssetSummary {
        AssetSummary {
            local_id: "ABC-123/L0/001".into(),
            media_type: MediaType::Image,
            creation_ts: Some(1_700_000_000.0),
            modification_ts: 1_700_000_100.5,
            is_favorite: false,
            has_adjustments,
            duration_secs: 0.0,
        }
    }

    fn video(has_adjustments: bool) -> AssetSummary {
        AssetSummary { media_type: MediaType::Video, duration_secs: 12.3, ..image(has_adjustments) }
    }

    fn names(plans: &[VariantPlan]) -> Vec<(&str, String)> {
        plans.iter().map(|p| (p.variant, format!("{}.{}", p.filename_stem, p.extension.as_deref().unwrap_or("?")))).collect()
    }

    #[test]
    fn plain_image_uploads_only_the_original() {
        let plans = plan_variants(true, true, &image(false), &[res(ResourceKind::Photo, "IMG_0001.HEIC", "public.heic")]);
        assert_eq!(names(&plans), vec![(VARIANT_ORIGINAL, "IMG_0001.heic".to_string())]);
        assert_eq!(plans[0].signature, "original");
        assert_eq!(plans[0].source, Source::Resource(0));
        assert_eq!(plans[0].pairs_with, None);
    }

    #[test]
    fn edited_live_photo_with_rendered_paired_video() {
        let resources = [
            res(ResourceKind::Photo, "IMG_0002.HEIC", "public.heic"),
            res(ResourceKind::PairedVideo, "IMG_0002.MOV", "com.apple.quicktime-movie"),
            res(ResourceKind::FullSizePhoto, "FullSizeRender.jpg", "public.jpeg"),
            res(ResourceKind::FullSizePairedVideo, "FullSizeRender.mov", "com.apple.quicktime-movie"),
            res(ResourceKind::Other, "Adjustments.plist", "com.apple.property-list"),
        ];
        let plans = plan_variants(true, true, &image(true), &resources);
        assert_eq!(
            names(&plans),
            vec![
                (VARIANT_PAIRED_VIDEO, "IMG_0002.mov".to_string()),
                (VARIANT_ORIGINAL, "IMG_0002.heic".to_string()),
                (VARIANT_EDITED_PAIRED_VIDEO, "IMG_0002_edited.mov".to_string()),
                (VARIANT_EDITED, "IMG_0002_edited.jpg".to_string()),
            ]
        );
        assert_eq!(plans[1].pairs_with, Some(VARIANT_PAIRED_VIDEO));
        assert_eq!(plans[3].pairs_with, Some(VARIANT_EDITED_PAIRED_VIDEO));
        assert_eq!(plans[3].signature, "mod:1700000100.500");
        assert_eq!(plans[2].source, Source::Resource(3));
    }

    #[test]
    fn edited_live_photo_falls_back_to_original_paired_video() {
        let resources = [
            res(ResourceKind::Photo, "IMG_0003.HEIC", "public.heic"),
            res(ResourceKind::PairedVideo, "IMG_0003.MOV", "com.apple.quicktime-movie"),
        ];
        let plans = plan_variants(true, true, &image(true), &resources);
        let edited = plans.iter().find(|p| p.variant == VARIANT_EDITED).unwrap();
        assert_eq!(edited.source, Source::RenderEdited);
        assert_eq!(edited.extension, None);
        assert_eq!(edited.pairs_with, Some(VARIANT_PAIRED_VIDEO));
    }

    #[test]
    fn edited_only_mode_skips_originals_and_does_not_pair_with_missing_video() {
        let resources = [
            res(ResourceKind::Photo, "IMG_0004.HEIC", "public.heic"),
            res(ResourceKind::PairedVideo, "IMG_0004.MOV", "com.apple.quicktime-movie"),
        ];
        let plans = plan_variants(false, true, &image(true), &resources);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].variant, VARIANT_EDITED);
        assert_eq!(plans[0].pairs_with, None);
    }

    #[test]
    fn video_variants_and_device_ids() {
        let resources = [
            res(ResourceKind::Video, "IMG_0005.MOV", "com.apple.quicktime-movie"),
            res(ResourceKind::FullSizeVideo, "FullSizeRender.mov", "com.apple.quicktime-movie"),
        ];
        let asset = video(true);
        let plans = plan_variants(true, true, &asset, &resources);
        assert_eq!(
            names(&plans),
            vec![(VARIANT_ORIGINAL, "IMG_0005.mov".to_string()), (VARIANT_EDITED, "IMG_0005_edited.mov".to_string())]
        );
        assert_eq!(device_asset_id(&asset, VARIANT_ORIGINAL), "ABC-123/L0/001:video");
        assert_eq!(device_asset_id(&image(false), VARIANT_ORIGINAL), "ABC-123/L0/001");
        assert_eq!(device_asset_id(&asset, VARIANT_EDITED), "ABC-123/L0/001:edited");
    }

    #[test]
    fn unedited_video_has_no_edited_variant_even_with_full_size_resource() {
        let resources = [
            res(ResourceKind::Video, "IMG_0006.MOV", "com.apple.quicktime-movie"),
            res(ResourceKind::FullSizeVideo, "FullSizeRender.mov", "com.apple.quicktime-movie"),
        ];
        let plans = plan_variants(true, true, &video(false), &resources);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].variant, VARIANT_ORIGINAL);
    }

    #[test]
    fn extension_falls_back_to_uti() {
        let plans = plan_variants(true, false, &image(false), &[res(ResourceKind::Photo, "noext", "public.png")]);
        assert_eq!(plans[0].extension.as_deref(), Some("png"));
    }

    #[test]
    fn helpers() {
        assert_eq!(split_filename("IMG_1.HEIC"), ("IMG_1".into(), Some("heic".into())));
        assert_eq!(split_filename("noext"), ("noext".into(), None));
        assert_eq!(split_filename(".hidden"), (".hidden".into(), None));
        assert_eq!(safe_id("ABC-123/L0/001"), "ABC-123L0001");
        assert_eq!(rfc3339(Some(1_700_000_000.25)), "2023-11-14T22:13:20.250Z");
    }

    #[test]
    fn trigger_merging() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(merge_trigger(None, Trigger::Assets(ids(&["b", "a"]))), Scope::Partial { asset_ids: ids(&["b", "a"]), albums: false });
        assert_eq!(
            merge_trigger(Some(Scope::Partial { asset_ids: ids(&["b"]), albums: false }), Trigger::Assets(ids(&["a", "b"]))),
            Scope::Partial { asset_ids: ids(&["a", "b"]), albums: false }
        );
        assert_eq!(
            merge_trigger(Some(Scope::Partial { asset_ids: ids(&["a"]), albums: false }), Trigger::Albums),
            Scope::Partial { asset_ids: ids(&["a"]), albums: true }
        );
        assert_eq!(merge_trigger(None, Trigger::Albums), Scope::Partial { asset_ids: vec![], albums: true });
        assert_eq!(merge_trigger(Some(Scope::Partial { asset_ids: ids(&["a"]), albums: true }), Trigger::Full), Scope::Full);
        assert_eq!(merge_trigger(Some(Scope::Full), Trigger::Assets(ids(&["a"]))), Scope::Full);
    }
}
