use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use log::{debug, warn};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSObject, NSObjectProtocol, NSSortDescriptor, NSString,
};
use objc2_image_io::CGImagePropertyOrientation;
use objc2_photos::{
    PHAccessLevel, PHAsset, PHAssetCollection, PHAssetCollectionSubtype, PHAssetCollectionType,
    PHAssetMediaType, PHAssetResource, PHAssetResourceManager, PHAssetResourceRequestOptions, PHAssetResourceType,
    PHAuthorizationStatus, PHChange, PHFetchOptions, PHFetchResult, PHFetchResultChangeDetails, PHImageErrorKey, PHImageManager,
    PHImageRequestOptions, PHImageRequestOptionsDeliveryMode, PHImageRequestOptionsVersion, PHPhotoLibrary,
    PHPhotoLibraryChangeObserver,
};
use objc2_uniform_type_identifiers::UTType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
    Limited,
}

impl AuthStatus {
    fn from_raw(status: PHAuthorizationStatus) -> Self {
        match status {
            PHAuthorizationStatus::Authorized => Self::Authorized,
            PHAuthorizationStatus::Limited => Self::Limited,
            PHAuthorizationStatus::Denied => Self::Denied,
            PHAuthorizationStatus::Restricted => Self::Restricted,
            _ => Self::NotDetermined,
        }
    }

    pub fn is_granted(self) -> bool {
        matches!(self, Self::Authorized | Self::Limited)
    }
}

pub fn authorization_status() -> AuthStatus {
    AuthStatus::from_raw(unsafe { PHPhotoLibrary::authorizationStatusForAccessLevel(PHAccessLevel::ReadWrite) })
}

pub fn request_authorization(timeout: Duration) -> AuthStatus {
    let (tx, rx) = mpsc::channel::<PHAuthorizationStatus>();
    let handler = RcBlock::new(move |status: PHAuthorizationStatus| {
        let _ = tx.send(status);
    });
    unsafe { PHPhotoLibrary::requestAuthorizationForAccessLevel_handler(PHAccessLevel::ReadWrite, &handler) };
    match rx.recv_timeout(timeout) {
        Ok(status) => AuthStatus::from_raw(status),
        Err(_) => authorization_status(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image,
    Video,
    Audio,
    Unknown,
}

impl MediaType {
    pub fn as_i64(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Image => 1,
            Self::Video => 2,
            Self::Audio => 3,
        }
    }
}

#[derive(Clone)]
pub struct AssetInfo {
    pub local_id: String,
    pub media_type: MediaType,
    pub creation_ts: Option<f64>,
    pub modification_ts: Option<f64>,
    pub is_favorite: bool,
    pub is_hidden: bool,
    pub has_adjustments: bool,
    pub duration_secs: f64,
    handle: Retained<PHAsset>,
}

impl AssetInfo {
    fn from_asset(asset: Retained<PHAsset>) -> Self {
        unsafe {
            let media_type = match asset.mediaType() {
                PHAssetMediaType::Image => MediaType::Image,
                PHAssetMediaType::Video => MediaType::Video,
                PHAssetMediaType::Audio => MediaType::Audio,
                _ => MediaType::Unknown,
            };
            Self {
                local_id: asset.localIdentifier().to_string(),
                media_type,
                creation_ts: asset.creationDate().map(|d| d.timeIntervalSince1970()),
                modification_ts: asset.modificationDate().map(|d| d.timeIntervalSince1970()),
                is_favorite: asset.isFavorite(),
                is_hidden: asset.isHidden(),
                has_adjustments: asset.hasAdjustments(),
                duration_secs: asset.duration(),
                handle: asset,
            }
        }
    }
}

pub struct AssetFetch {
    result: Retained<PHFetchResult<PHAsset>>,
}

impl AssetFetch {
    pub fn len(&self) -> usize {
        unsafe { self.result.count() }
    }

    pub fn get(&self, index: usize) -> AssetInfo {
        AssetInfo::from_asset(unsafe { self.result.objectAtIndex(index) })
    }
}

fn fetch_options(include_hidden: bool, newest_first: bool) -> Retained<PHFetchOptions> {
    unsafe {
        let opts = PHFetchOptions::new();
        opts.setIncludeHiddenAssets(include_hidden);
        let key = NSString::from_str("creationDate");
        let descriptor = NSSortDescriptor::sortDescriptorWithKey_ascending(Some(&key), !newest_first);
        opts.setSortDescriptors(Some(&NSArray::from_retained_slice(&[descriptor])));
        opts
    }
}

pub fn fetch_all_assets(include_hidden: bool, newest_first: bool) -> AssetFetch {
    let opts = fetch_options(include_hidden, newest_first);
    AssetFetch { result: unsafe { PHAsset::fetchAssetsWithOptions(Some(&opts)) } }
}

pub fn fetch_assets_by_ids(ids: &[String], include_hidden: bool) -> AssetFetch {
    let ns_ids: Vec<Retained<NSString>> = ids.iter().map(|s| NSString::from_str(s)).collect();
    let array = NSArray::from_retained_slice(&ns_ids);
    let opts = fetch_options(include_hidden, true);
    AssetFetch { result: unsafe { PHAsset::fetchAssetsWithLocalIdentifiers_options(&array, Some(&opts)) } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Photo,
    Video,
    FullSizePhoto,
    FullSizeVideo,
    PairedVideo,
    FullSizePairedVideo,
    Other,
}

#[derive(Debug, Clone)]
pub struct ResourceMeta {
    pub kind: ResourceKind,
    pub original_filename: String,
    pub uti: String,
}

pub struct ResourceInfo {
    pub meta: ResourceMeta,
    handle: Retained<PHAssetResource>,
}

pub fn resources_for(asset: &AssetInfo) -> Vec<ResourceInfo> {
    let list = unsafe { PHAssetResource::assetResourcesForAsset(&asset.handle) };
    list.iter()
        .map(|res| unsafe {
            let kind = match res.r#type() {
                PHAssetResourceType::Photo => ResourceKind::Photo,
                PHAssetResourceType::Video => ResourceKind::Video,
                PHAssetResourceType::FullSizePhoto => ResourceKind::FullSizePhoto,
                PHAssetResourceType::FullSizeVideo => ResourceKind::FullSizeVideo,
                PHAssetResourceType::PairedVideo => ResourceKind::PairedVideo,
                PHAssetResourceType::FullSizePairedVideo => ResourceKind::FullSizePairedVideo,
                _ => ResourceKind::Other,
            };
            ResourceInfo {
                meta: ResourceMeta {
                    kind,
                    original_filename: res.originalFilename().to_string(),
                    #[allow(deprecated)]
                    uti: res.uniformTypeIdentifier().to_string(),
                },
                handle: res,
            }
        })
        .collect()
}

pub fn extension_for_uti(uti: &str) -> Option<String> {
    let ut = UTType::typeWithIdentifier(&NSString::from_str(uti))?;
    ut.preferredFilenameExtension().map(|e| e.to_string().to_lowercase())
}

type FileSink = (File, Option<std::io::Error>);

// Waits for the completion signal, failing once no progress has been reported for `idle_timeout`.
fn wait_for_completion<T>(
    rx: &mpsc::Receiver<T>,
    last_progress: &Mutex<Instant>,
    idle_timeout: Duration,
    what: &str,
) -> Result<T> {
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Disconnected) => bail!("{what}: PhotoKit dropped the request"),
            Err(RecvTimeoutError::Timeout) => {
                let idle = last_progress.lock().map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
                if idle > idle_timeout {
                    bail!("{what}: no progress for {idle_timeout:?}, giving up");
                }
            }
        }
    }
}

pub fn export_resource(resource: &ResourceInfo, dest: &Path, idle_timeout: Duration) -> Result<()> {
    let what = format!("export {}", resource.meta.original_filename);
    let file = File::create(dest)?;
    let sink: Arc<Mutex<Option<FileSink>>> = Arc::new(Mutex::new(Some((file, None))));
    let last_progress = Arc::new(Mutex::new(Instant::now()));
    let (tx, rx) = mpsc::channel::<Option<String>>();

    let sink_for_data = Arc::clone(&sink);
    let progress_for_data = Arc::clone(&last_progress);
    let data_handler = RcBlock::new(move |data: NonNull<NSData>| {
        let bytes = unsafe { data.as_ref().as_bytes_unchecked() };
        if let Ok(mut guard) = sink_for_data.lock() {
            if let Some((file, err)) = guard.as_mut() {
                if err.is_none() {
                    if let Err(e) = file.write_all(bytes) {
                        *err = Some(e);
                    }
                }
            }
        }
        if let Ok(mut t) = progress_for_data.lock() {
            *t = Instant::now();
        }
    });
    let completion = RcBlock::new(move |err: *mut NSError| {
        let msg = unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(msg);
    });
    let progress_for_handler = Arc::clone(&last_progress);
    let progress_handler = RcBlock::new(move |_fraction: f64| {
        if let Ok(mut t) = progress_for_handler.lock() {
            *t = Instant::now();
        }
    });

    let manager = unsafe { PHAssetResourceManager::defaultManager() };
    let request_id = unsafe {
        let opts = PHAssetResourceRequestOptions::new();
        opts.setNetworkAccessAllowed(true);
        opts.setProgressHandler(RcBlock::as_ptr(&progress_handler));
        manager.requestDataForAssetResource_options_dataReceivedHandler_completionHandler(
            &resource.handle,
            Some(&opts),
            &data_handler,
            &completion,
        )
    };

    let outcome = wait_for_completion(&rx, &last_progress, idle_timeout, &what);
    let write_error = sink.lock().ok().and_then(|mut g| g.take()).and_then(|(file, err)| {
        drop(file);
        err
    });
    match outcome {
        Ok(None) => {}
        Ok(Some(msg)) => {
            let _ = std::fs::remove_file(dest);
            bail!("{what}: {msg}");
        }
        Err(e) => {
            unsafe { manager.cancelDataRequest(request_id) };
            let _ = std::fs::remove_file(dest);
            return Err(e);
        }
    }
    if let Some(err) = write_error {
        let _ = std::fs::remove_file(dest);
        bail!("{what}: writing file: {err}");
    }
    Ok(())
}

// Renders the current (edited) version of an image through PHImageManager; returns the file extension.
pub fn render_edited_image(asset: &AssetInfo, dest_without_ext: &Path, idle_timeout: Duration) -> Result<(PathBuf, String)> {
    let what = format!("render edited {}", asset.local_id);
    let last_progress = Arc::new(Mutex::new(Instant::now()));
    let (tx, rx) = mpsc::channel::<Result<(Vec<u8>, Option<String>), String>>();

    let handler = RcBlock::new(
        move |data: *mut NSData, uti: *mut NSString, _orientation: CGImagePropertyOrientation, info: *mut NSDictionary| {
            let error = unsafe { info.as_ref() }
                .and_then(|info| info.objectForKey(unsafe { PHImageErrorKey }))
                .and_then(|obj: Retained<AnyObject>| obj.downcast::<NSError>().ok())
                .map(|e| e.localizedDescription().to_string());
            let data = unsafe { data.as_ref() };
            let uti = unsafe { uti.as_ref() }.map(|s| s.to_string());
            let result = match (data, error) {
                (Some(d), _) => Ok((d.to_vec(), uti)),
                (None, Some(e)) => Err(e),
                (None, None) => Err("no image data returned".to_string()),
            };
            let _ = tx.send(result);
        },
    );
    let progress_for_handler = Arc::clone(&last_progress);
    let progress_handler = RcBlock::new(
        move |_fraction: f64, _err: *mut NSError, _stop: NonNull<Bool>, _info: *mut NSDictionary| {
            if let Ok(mut t) = progress_for_handler.lock() {
                *t = Instant::now();
            }
        },
    );

    let manager = unsafe { PHImageManager::defaultManager() };
    let request_id = unsafe {
        let opts = PHImageRequestOptions::new();
        opts.setNetworkAccessAllowed(true);
        opts.setSynchronous(false);
        opts.setDeliveryMode(PHImageRequestOptionsDeliveryMode::HighQualityFormat);
        opts.setVersion(PHImageRequestOptionsVersion::Current);
        opts.setProgressHandler(RcBlock::as_ptr(&progress_handler));
        manager.requestImageDataAndOrientationForAsset_options_resultHandler(&asset.handle, Some(&opts), &handler)
    };

    let result = match wait_for_completion(&rx, &last_progress, idle_timeout, &what) {
        Ok(r) => r,
        Err(e) => {
            unsafe { manager.cancelImageRequest(request_id) };
            return Err(e);
        }
    };
    let (bytes, uti) = result.map_err(|e| anyhow!("{what}: {e}"))?;
    let ext = uti.as_deref().and_then(extension_for_uti).unwrap_or_else(|| "jpg".to_string());
    let dest = dest_without_ext.with_extension(&ext);
    std::fs::write(&dest, &bytes)?;
    Ok((dest, ext))
}

#[derive(Debug, Clone)]
pub struct AlbumInfo {
    pub local_id: String,
    pub title: String,
    pub is_shared: bool,
    handle: Retained<PHAssetCollection>,
}

pub fn fetch_albums(include_shared: bool) -> Vec<AlbumInfo> {
    let result = unsafe {
        PHAssetCollection::fetchAssetCollectionsWithType_subtype_options(
            PHAssetCollectionType::Album,
            PHAssetCollectionSubtype::Any,
            None,
        )
    };
    let count = unsafe { result.count() };
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let collection = unsafe { result.objectAtIndex(i) };
        let subtype = unsafe { collection.assetCollectionSubtype() };
        let is_shared = subtype == PHAssetCollectionSubtype::AlbumCloudShared;
        if subtype == PHAssetCollectionSubtype::AlbumMyPhotoStream || (is_shared && !include_shared) {
            continue;
        }
        let title = unsafe { collection.localizedTitle() }
            .map(|t| t.to_string().trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Untitled Album".to_string());
        out.push(AlbumInfo { local_id: unsafe { collection.localIdentifier() }.to_string(), title, is_shared, handle: collection });
    }
    out
}

pub fn fetch_album_assets(album: &AlbumInfo, newest_first: bool) -> AssetFetch {
    let opts = fetch_options(false, newest_first);
    AssetFetch { result: unsafe { PHAsset::fetchAssetsInAssetCollection_options(&album.handle, Some(&opts)) } }
}

pub fn album_asset_ids(album: &AlbumInfo) -> Vec<String> {
    let result = unsafe { PHAsset::fetchAssetsInAssetCollection_options(&album.handle, None) };
    let count = unsafe { result.count() };
    (0..count).map(|i| unsafe { result.objectAtIndex(i).localIdentifier() }.to_string()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Assets(Vec<String>),
    Albums,
    Full,
    Shutdown,
}

pub struct ObserverIvars {
    tx: Sender<Trigger>,
    assets: Mutex<Retained<PHFetchResult<PHAsset>>>,
    albums: Mutex<Retained<PHFetchResult<PHAssetCollection>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ImmichPhotoSyncChangeObserver"]
    #[ivars = ObserverIvars]
    pub struct ChangeObserver;

    unsafe impl NSObjectProtocol for ChangeObserver {}

    unsafe impl PHPhotoLibraryChangeObserver for ChangeObserver {
        #[unsafe(method(photoLibraryDidChange:))]
        fn photo_library_did_change(&self, change: &PHChange) {
            self.handle_change(change);
        }
    }
);

impl ChangeObserver {
    fn handle_change(&self, change: &PHChange) {
        let ivars = self.ivars();
        let mut triggers = Vec::new();

        if let Ok(mut assets) = ivars.assets.lock() {
            let details = unsafe { change.changeDetailsForFetchResult(assets.cast_unchecked::<AnyObject>()) };
            if let Some(details) = details {
                unsafe {
                    let details: Retained<PHFetchResultChangeDetails<PHAsset>> = Retained::cast_unchecked(details);
                    if details.hasIncrementalChanges() {
                        let mut ids = Vec::new();
                        for asset in details.insertedObjects().iter().chain(details.changedObjects().iter()) {
                            ids.push(asset.localIdentifier().to_string());
                        }
                        if !ids.is_empty() {
                            triggers.push(Trigger::Assets(ids));
                        }
                    } else {
                        triggers.push(Trigger::Full);
                    }
                    *assets = details.fetchResultAfterChanges();
                }
            }
        }

        if let Ok(mut albums) = ivars.albums.lock() {
            let details = unsafe { change.changeDetailsForFetchResult(albums.cast_unchecked::<AnyObject>()) };
            if let Some(details) = details {
                triggers.push(Trigger::Albums);
                unsafe {
                    let details: Retained<PHFetchResultChangeDetails<PHAssetCollection>> = Retained::cast_unchecked(details);
                    *albums = details.fetchResultAfterChanges();
                }
            }
        }

        if triggers.is_empty() {
            debug!("photo library change without relevant details");
        }
        for trigger in triggers {
            if ivars.tx.send(trigger).is_err() {
                warn!("sync engine is gone; ignoring photo library change");
            }
        }
    }
}

pub struct Watcher {
    observer: Retained<ChangeObserver>,
}

impl Watcher {
    pub fn start(tx: Sender<Trigger>, include_hidden: bool) -> Self {
        let assets = unsafe { PHAsset::fetchAssetsWithOptions(Some(&fetch_options(include_hidden, true))) };
        let albums = unsafe {
            PHAssetCollection::fetchAssetCollectionsWithType_subtype_options(
                PHAssetCollectionType::Album,
                PHAssetCollectionSubtype::Any,
                None,
            )
        };
        let observer = ChangeObserver::alloc().set_ivars(ObserverIvars {
            tx,
            assets: Mutex::new(assets),
            albums: Mutex::new(albums),
        });
        let observer: Retained<ChangeObserver> = unsafe { msg_send![super(observer), init] };
        unsafe {
            PHPhotoLibrary::sharedPhotoLibrary().registerChangeObserver(ProtocolObject::from_ref(&*observer));
        }
        Self { observer }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            PHPhotoLibrary::sharedPhotoLibrary().unregisterChangeObserver(ProtocolObject::from_ref(&*self.observer));
        }
    }
}
