mod immich;
mod launchd;
mod photos;
mod state;
mod sync;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use log::{error, info, warn};

use crate::immich::ImmichClient;
use crate::photos::{AuthStatus, Trigger, Watcher};
use crate::state::StateStore;
use crate::sync::{Config, Engine, MediaFilter, Scope};

#[derive(Parser, Debug)]
#[command(
    name = "immich-photo-sync",
    version,
    about = "Headless macOS daemon that syncs the Apple Photos library to an Immich server",
    long_about = "Uploads originals and edited renditions of every photo and video in the Photos library to Immich, \
mirrors Photos albums as Immich albums, and keeps doing so incrementally: either live via PhotoKit change \
notifications, on a fixed interval, or both. All settings are command line flags (or environment variables) so it \
can be run as a launchd agent."
)]
struct Cli {
    #[command(flatten)]
    opts: Options,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as a daemon: initial full sync, then watch for changes and/or run on an interval (default)
    Run,
    /// Run a single full sync pass and exit
    Once,
    /// Request Photos library access and report the current authorization status
    Auth,
    /// Show state database counts and recent failures
    Status,
    /// Print a launchd agent plist that runs this binary with the current flags
    LaunchdPlist {
        /// launchd label for the agent
        #[arg(long, default_value = "com.immich-photo-sync.agent")]
        label: String,
        /// Directory for stdout/stderr log files
        #[arg(long)]
        log_dir: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum MediaArg {
    All,
    Images,
    Videos,
}

#[derive(Args, Debug, Clone)]
struct Options {
    /// Immich server URL, e.g. https://photos.example.com or http://192.168.1.10:2283
    #[arg(long, env = "IMMICH_SERVER", value_name = "URL")]
    server: Option<String>,

    /// Immich API key (User Settings -> API Keys)
    #[arg(long, env = "IMMICH_API_KEY", value_name = "KEY", conflicts_with = "api_key_file")]
    api_key: Option<String>,

    /// Read the Immich API key from this file instead of passing it on the command line
    #[arg(long, env = "IMMICH_API_KEY_FILE", value_name = "PATH")]
    api_key_file: Option<PathBuf>,

    /// Directory for the state database (default: ~/Library/Application Support/immich-photo-sync)
    #[arg(long, env = "IMMICH_PHOTO_SYNC_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// Directory for exported files awaiting upload (default: <state-dir>/tmp)
    #[arg(long, value_name = "DIR")]
    temp_dir: Option<PathBuf>,

    /// Device id reported to Immich (default: immich-photo-sync:<hostname>)
    #[arg(long, value_name = "ID")]
    device_id: Option<String>,

    /// Run a full reconciliation pass this often (0 disables the timer)
    #[arg(long, default_value = "1h", value_name = "DURATION")]
    interval: humantime::Duration,

    /// Do not react to live Photos library change notifications; rely on --interval only
    #[arg(long)]
    no_watch: bool,

    /// Quiet period after a library change before the incremental pass starts
    #[arg(long, default_value = "30s", value_name = "DURATION")]
    debounce: humantime::Duration,

    /// Which media types to sync
    #[arg(long, value_enum, default_value_t = MediaArg::All)]
    media: MediaArg,

    /// Skip original files (upload edited renditions only)
    #[arg(long)]
    no_originals: bool,

    /// Skip edited renditions (upload originals only)
    #[arg(long)]
    no_edited: bool,

    /// Do not mirror Photos albums to Immich albums
    #[arg(long)]
    no_albums: bool,

    /// Do not push favorite changes to Immich
    #[arg(long)]
    no_favorites: bool,

    /// Include hidden photos
    #[arg(long)]
    include_hidden: bool,

    /// Also mirror iCloud Shared Albums (contents owned by others are uploaded too)
    #[arg(long)]
    include_shared_albums: bool,

    /// Process oldest assets first instead of newest first
    #[arg(long)]
    oldest_first: bool,

    /// Parallel uploads
    #[arg(long, default_value_t = 3, value_name = "N")]
    upload_concurrency: usize,

    /// Fail an export when PhotoKit reports no download progress for this long
    #[arg(long, default_value = "2m", value_name = "DURATION")]
    export_idle_timeout: humantime::Duration,

    /// HTTP request timeout (uploads of large videos need a generous value)
    #[arg(long, default_value = "30m", value_name = "DURATION")]
    http_timeout: humantime::Duration,

    /// When an edit changes, move the previously uploaded edited rendition to the Immich trash
    #[arg(long)]
    replace_stale_edits: bool,

    /// Accept invalid TLS certificates
    #[arg(long)]
    insecure: bool,

    /// Process at most N assets that need work per pass (useful for testing)
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Scan and report what would be uploaded without exporting or uploading anything
    #[arg(long)]
    dry_run: bool,

    /// Log level: error, warn, info, debug, trace
    #[arg(long, env = "IMMICH_PHOTO_SYNC_LOG", default_value = "info", value_name = "LEVEL")]
    log_level: String,
}

impl Options {
    fn state_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.state_dir {
            return Ok(dir.clone());
        }
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join("Library/Application Support/immich-photo-sync"))
    }

    fn api_key(&self) -> Result<String> {
        if let Some(key) = &self.api_key {
            return Ok(key.trim().to_string());
        }
        if let Some(path) = &self.api_key_file {
            let key = std::fs::read_to_string(path).with_context(|| format!("reading API key file {}", path.display()))?;
            return Ok(key.trim().to_string());
        }
        bail!("an Immich API key is required: pass --api-key, --api-key-file, or set IMMICH_API_KEY")
    }

    fn server(&self) -> Result<&str> {
        self.server.as_deref().context("an Immich server URL is required: pass --server or set IMMICH_SERVER")
    }

    fn sync_config(&self, state_dir: &std::path::Path) -> Config {
        Config {
            device_id: self.device_id.clone().unwrap_or_else(|| {
                format!("immich-photo-sync:{}", gethostname::gethostname().to_string_lossy())
            }),
            temp_dir: self.temp_dir.clone().unwrap_or_else(|| state_dir.join("tmp")),
            include_hidden: self.include_hidden,
            include_shared_albums: self.include_shared_albums,
            media: match self.media {
                MediaArg::All => MediaFilter::All,
                MediaArg::Images => MediaFilter::Images,
                MediaArg::Videos => MediaFilter::Videos,
            },
            originals: !self.no_originals,
            edited: !self.no_edited,
            albums: !self.no_albums,
            favorites: !self.no_favorites,
            newest_first: !self.oldest_first,
            upload_concurrency: self.upload_concurrency.max(1),
            export_idle_timeout: self.export_idle_timeout.into(),
            replace_stale_edits: self.replace_stale_edits,
            limit: self.limit,
            dry_run: self.dry_run,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .parse_filters(&cli.opts.log_level)
        .format_timestamp_secs()
        .format_target(false)
        .init();

    let result = match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(&cli.opts, true),
        Command::Once => run(&cli.opts, false),
        Command::Auth => auth(),
        Command::Status => status(&cli.opts),
        Command::LaunchdPlist { label, log_dir } => launchd::print_plist(&label, log_dir.as_deref()),
    };
    if let Err(e) = result {
        error!("{e:#}");
        std::process::exit(1);
    }
}

fn auth() -> Result<()> {
    let status = photos::authorization_status();
    println!("current Photos authorization: {status:?}");
    if status == AuthStatus::NotDetermined {
        println!("requesting access (a system dialog should appear)...");
        let status = photos::request_authorization(Duration::from_secs(300));
        println!("Photos authorization: {status:?}");
    }
    if !photos::authorization_status().is_granted() {
        bail!(
            "Photos access not granted. Enable it in System Settings > Privacy & Security > Photos. \
Note that macOS attributes access to the responsible process: when started from a terminal that is the terminal app, \
when started by launchd it is this binary."
        );
    }
    Ok(())
}

fn ensure_photos_access() -> Result<()> {
    let mut status = photos::authorization_status();
    if status == AuthStatus::NotDetermined {
        info!("requesting Photos library access");
        status = photos::request_authorization(Duration::from_secs(600));
    }
    if !status.is_granted() {
        bail!("Photos access is {status:?}; grant it in System Settings > Privacy & Security > Photos");
    }
    if status == AuthStatus::Limited {
        warn!("Photos access is limited to a selection; only selected items will sync");
    }
    Ok(())
}

fn status(opts: &Options) -> Result<()> {
    let state_dir = opts.state_dir()?;
    let db_path = state_dir.join("state.sqlite");
    if !db_path.exists() {
        println!("no state database at {}", db_path.display());
        return Ok(());
    }
    let state = StateStore::open(&db_path)?;
    let counts = state.counts()?;
    println!("state database: {}", db_path.display());
    println!("Photos authorization: {:?}", photos::authorization_status());
    println!("assets tracked:   {}", counts.assets);
    println!("files uploaded:   {}", counts.variants);
    println!("albums mirrored:  {}", counts.albums);
    println!("album entries:    {}", counts.album_members);
    println!("failing assets:   {}", counts.failures);
    if let Some(ts) = state.get_meta("last_full_pass")? {
        println!("last full pass:   {ts}");
    }
    if let Some(ts) = state.get_meta("last_pass")? {
        println!("last pass:        {ts}");
    }
    let failures = state.recent_failures(10)?;
    if !failures.is_empty() {
        println!("recent failures:");
        for (id, attempts, err) in failures {
            println!("  {id} ({attempts} attempt(s)): {err}");
        }
    }
    Ok(())
}

// In daemon mode the agent may start before the network or the server is up, so keep trying.
fn connect_to_immich(client: &ImmichClient, server: &str, daemon: bool) -> Result<()> {
    let mut delay = Duration::from_secs(15);
    loop {
        match client.validate_api_key() {
            Ok(user) => {
                match client.server_version() {
                    Ok(v) => info!("Immich server {server} is v{}.{}.{}, authenticated as {user}", v.major, v.minor, v.patch),
                    Err(e) => warn!("could not read Immich server version: {e:#}"),
                }
                if let Ok(stats) = client.asset_statistics() {
                    info!("Immich library has {} assets ({} images, {} videos)", stats.total, stats.images, stats.videos);
                }
                return Ok(());
            }
            Err(e) if daemon && !format!("{e:#}").contains("HTTP 401") => {
                warn!("Immich not reachable ({e:#}); retrying in {delay:?}");
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(300));
            }
            Err(e) => return Err(e.context("connecting to Immich")),
        }
    }
}

fn run(opts: &Options, daemon: bool) -> Result<()> {
    let server = opts.server()?;
    let api_key = opts.api_key()?;
    let state_dir = opts.state_dir()?;
    std::fs::create_dir_all(&state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let cfg = opts.sync_config(&state_dir);

    ensure_photos_access()?;

    let client = Arc::new(ImmichClient::new(server, &api_key, opts.http_timeout.into(), opts.insecure)?);
    connect_to_immich(&client, server, daemon)?;

    let state = Arc::new(StateStore::open(&state_dir.join("state.sqlite"))?);
    let cancel = Arc::new(AtomicBool::new(false));
    let engine = Engine::new(cfg.clone(), client, Arc::clone(&state), Arc::clone(&cancel));

    let (tx, rx) = mpsc::channel::<Trigger>();
    {
        let cancel = Arc::clone(&cancel);
        let tx = tx.clone();
        ctrlc::set_handler(move || {
            if cancel.swap(true, Ordering::SeqCst) {
                warn!("second signal received, exiting now");
                std::process::exit(130);
            }
            info!("shutdown requested, finishing the current item");
            let _ = tx.send(Trigger::Shutdown);
        })
        .context("installing signal handler")?;
    }

    info!(
        "config: device_id={} originals={} edited={} albums={} favorites={} media={:?} hidden={} shared_albums={} concurrency={} temp={}",
        cfg.device_id, cfg.originals, cfg.edited, cfg.albums, cfg.favorites, cfg.media, cfg.include_hidden,
        cfg.include_shared_albums, cfg.upload_concurrency, cfg.temp_dir.display()
    );

    let run_full = |engine: &Engine| {
        info!("starting full pass");
        match engine.run_pass(&Scope::Full) {
            Ok(_) => {
                let now = chrono::Local::now().to_rfc3339();
                let _ = state.set_meta("last_full_pass", &now);
                let _ = state.set_meta("last_pass", &now);
            }
            Err(e) => error!("full pass failed: {e:#}"),
        }
    };

    if !daemon {
        run_full(&engine);
        return if cancel.load(Ordering::Relaxed) { bail!("interrupted") } else { Ok(()) };
    }

    let watch = !opts.no_watch;
    let _watcher = watch.then(|| Watcher::start(tx.clone(), cfg.include_hidden));
    let interval: Duration = opts.interval.into();
    let debounce: Duration = opts.debounce.into();
    if watch {
        info!("watching the Photos library for changes (debounce {debounce:?})");
    }
    if interval.is_zero() {
        if !watch {
            bail!("--interval 0 together with --no-watch would never sync; pick one");
        }
        info!("periodic full passes disabled");
    } else {
        info!("full pass every {interval:?}");
    }

    run_full(&engine);

    let mut next_full = (!interval.is_zero()).then(|| Instant::now() + interval);
    let mut pending: Option<(Scope, Instant)> = None;

    while !cancel.load(Ordering::Relaxed) {
        let now = Instant::now();
        let deadline = [next_full, pending.as_ref().map(|(_, due)| *due)].into_iter().flatten().min();
        let wait = deadline.map(|d| d.saturating_duration_since(now)).unwrap_or(Duration::from_secs(3600));

        match rx.recv_timeout(wait) {
            Ok(Trigger::Shutdown) => break,
            Ok(trigger) => {
                pending = Some((sync::merge_trigger(pending.take().map(|(scope, _)| scope), trigger), Instant::now() + debounce));
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();
        if next_full.is_some_and(|t| t <= now) {
            pending = None;
            run_full(&engine);
            next_full = Some(Instant::now() + interval);
            continue;
        }
        if pending.as_ref().is_some_and(|(_, due)| *due <= now) {
            let (scope, _) = pending.take().unwrap();
            match &scope {
                Scope::Full => run_full(&engine),
                Scope::Partial { asset_ids, albums } => {
                    info!("starting incremental pass ({} changed asset(s), albums={albums})", asset_ids.len());
                    match engine.run_pass(&scope) {
                        Ok(_) => {
                            let _ = state.set_meta("last_pass", &chrono::Local::now().to_rfc3339());
                        }
                        Err(e) => error!("incremental pass failed: {e:#}"),
                    }
                }
            }
        }
    }

    info!("exiting");
    Ok(())
}
