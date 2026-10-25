use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{debug, warn};
use reqwest::blocking::multipart::{Form, Part};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const RETRY_ATTEMPTS: u32 = 4;

pub struct ImmichClient {
    http: Client,
    api_base: String,
    api_key: String,
    retry_base_delay: Duration,
}

#[derive(Debug, Clone)]
pub struct UploadRequest<'a> {
    pub file: &'a Path,
    pub filename: &'a str,
    pub sha1_hex: &'a str,
    pub device_id: &'a str,
    pub device_asset_id: &'a str,
    pub file_created_at: &'a str,
    pub file_modified_at: &'a str,
    pub duration_ms: Option<u64>,
    pub is_favorite: bool,
    pub live_photo_video_id: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlbumDto {
    pub id: String,
    #[serde(rename = "albumName")]
    pub album_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerVersion {
    pub major: i64,
    pub minor: i64,
    pub patch: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetStatistics {
    pub images: u64,
    pub videos: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct BulkCheckResult {
    id: String,
    action: String,
    reason: Option<String>,
    #[serde(rename = "assetId")]
    asset_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BulkCheckResponse {
    results: Vec<BulkCheckResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BulkIdResult {
    pub id: String,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BulkIds<'a> {
    ids: &'a [String],
}

impl ImmichClient {
    pub fn new(server_url: &str, api_key: &str, timeout: Duration, insecure: bool) -> Result<Self> {
        let trimmed = server_url.trim_end_matches('/');
        let api_base = if trimmed.ends_with("/api") {
            trimmed.to_string()
        } else {
            format!("{trimmed}/api")
        };
        let http = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(insecure)
            .user_agent(concat!("immich-photo-sync/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, api_base, api_key: api_key.to_string(), retry_base_delay: Duration::from_secs(2) })
    }

    #[cfg(test)]
    fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_base_delay = delay;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.api_base, path)
    }

    fn authed(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.header("x-api-key", &self.api_key).header("Accept", "application/json")
    }

    fn send_with_retry(&self, build: impl Fn() -> RequestBuilder, what: &str) -> Result<Response> {
        let mut delay = self.retry_base_delay;
        for attempt in 1..=RETRY_ATTEMPTS {
            let result = self.authed(build()).send();
            let retryable = match &result {
                Ok(resp) => resp.status().is_server_error() || resp.status() == StatusCode::TOO_MANY_REQUESTS,
                Err(err) => err.is_connect() || err.is_timeout() || err.is_request(),
            };
            if !retryable || attempt == RETRY_ATTEMPTS {
                return result.with_context(|| format!("{what}: request failed"));
            }
            match &result {
                Ok(resp) => warn!("{what}: HTTP {} (attempt {attempt}/{RETRY_ATTEMPTS}), retrying in {delay:?}", resp.status()),
                Err(err) => warn!("{what}: {err} (attempt {attempt}/{RETRY_ATTEMPTS}), retrying in {delay:?}"),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(60));
        }
        unreachable!()
    }

    fn ensure_ok(resp: Response, what: &str) -> Result<Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().unwrap_or_default();
        let body: String = body.chars().take(1000).collect();
        bail!("{what}: HTTP {status}: {body}")
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let what = format!("GET {path}");
        let resp = self.send_with_retry(|| self.http.get(self.url(path)), &what)?;
        Self::ensure_ok(resp, &what)?.json().with_context(|| format!("{what}: decoding response"))
    }

    fn send_json<T: for<'de> Deserialize<'de>>(&self, method: reqwest::Method, path: &str, body: &impl Serialize) -> Result<T> {
        let what = format!("{method} {path}");
        let resp = self.send_with_retry(|| self.http.request(method.clone(), self.url(path)).json(body), &what)?;
        Self::ensure_ok(resp, &what)?.json().with_context(|| format!("{what}: decoding response"))
    }

    pub fn server_version(&self) -> Result<ServerVersion> {
        self.get_json("server/version")
    }

    pub fn validate_api_key(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Me {
            email: Option<String>,
            name: Option<String>,
        }
        let me: Me = self.get_json("users/me")?;
        Ok(me.email.or(me.name).unwrap_or_else(|| "unknown user".into()))
    }

    pub fn asset_statistics(&self) -> Result<AssetStatistics> {
        self.get_json("assets/statistics")
    }

    // Returns the server-side asset id when an asset with this checksum already exists.
    pub fn find_duplicate(&self, device_asset_id: &str, sha1_hex: &str) -> Result<Option<String>> {
        #[derive(Serialize)]
        struct Item<'a> {
            id: &'a str,
            checksum: &'a str,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            assets: Vec<Item<'a>>,
        }
        let body = Body { assets: vec![Item { id: device_asset_id, checksum: sha1_hex }] };
        let resp: BulkCheckResponse = self.send_json(reqwest::Method::POST, "assets/bulk-upload-check", &body)?;
        for r in resp.results {
            if r.id == device_asset_id && r.action == "reject" && r.reason.as_deref() == Some("duplicate") {
                return Ok(r.asset_id);
            }
        }
        Ok(None)
    }

    pub fn upload_asset(&self, req: &UploadRequest<'_>) -> Result<UploadResponse> {
        let what = format!("upload {}", req.filename);
        let mime = mime_guess::from_path(req.filename).first_or_octet_stream();
        let build = || {
            let mut form = Form::new()
                .text("deviceId", req.device_id.to_string())
                .text("deviceAssetId", req.device_asset_id.to_string())
                .text("fileCreatedAt", req.file_created_at.to_string())
                .text("fileModifiedAt", req.file_modified_at.to_string())
                .text("filename", req.filename.to_string())
                .text("isFavorite", if req.is_favorite { "true" } else { "false" });
            if let Some(ms) = req.duration_ms {
                form = form.text("duration", ms.to_string());
            }
            if let Some(video_id) = req.live_photo_video_id {
                form = form.text("livePhotoVideoId", video_id.to_string());
            }
            let part = Part::file(req.file)
                .expect("upload file vanished")
                .file_name(req.filename.to_string())
                .mime_str(mime.as_ref())
                .expect("valid mime");
            form = form.part("assetData", part);
            self.http
                .post(self.url("assets"))
                .header("x-immich-checksum", req.sha1_hex)
                .multipart(form)
        };
        let resp = self.send_with_retry(build, &what)?;
        let resp = Self::ensure_ok(resp, &what)?;
        let parsed: UploadResponse = resp.json().with_context(|| format!("{what}: decoding response"))?;
        debug!("{what}: {} ({})", parsed.status, parsed.id);
        Ok(parsed)
    }

    pub fn set_favorite(&self, asset_id: &str, favorite: bool) -> Result<()> {
        #[derive(Serialize)]
        struct Body {
            #[serde(rename = "isFavorite")]
            is_favorite: bool,
        }
        let what = format!("PUT assets/{asset_id}");
        let path = format!("assets/{asset_id}");
        let resp = self.send_with_retry(|| self.http.put(self.url(&path)).json(&Body { is_favorite: favorite }), &what)?;
        Self::ensure_ok(resp, &what).map(|_| ())
    }

    pub fn delete_assets(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        #[derive(Serialize)]
        struct Body<'a> {
            ids: &'a [String],
            force: bool,
        }
        let what = "DELETE assets";
        let resp = self.send_with_retry(|| self.http.delete(self.url("assets")).json(&Body { ids, force: false }), what)?;
        Self::ensure_ok(resp, what).map(|_| ())
    }

    pub fn list_albums(&self) -> Result<Vec<AlbumDto>> {
        self.get_json("albums")
    }

    pub fn create_album(&self, name: &str) -> Result<AlbumDto> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "albumName")]
            album_name: &'a str,
        }
        self.send_json(reqwest::Method::POST, "albums", &Body { album_name: name })
    }

    pub fn rename_album(&self, album_id: &str, name: &str) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "albumName")]
            album_name: &'a str,
        }
        let what = format!("PATCH albums/{album_id}");
        let path = format!("albums/{album_id}");
        let resp = self.send_with_retry(|| self.http.patch(self.url(&path)).json(&Body { album_name: name }), &what)?;
        Self::ensure_ok(resp, &what).map(|_| ())
    }

    // Returns the ids the server rejected for a reason other than already being in the album.
    pub fn add_assets_to_album(&self, album_id: &str, ids: &[String]) -> Result<Vec<(String, String)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let path = format!("albums/{album_id}/assets");
        let results: Vec<BulkIdResult> = self.send_json(reqwest::Method::PUT, &path, &BulkIds { ids })?;
        Ok(results
            .into_iter()
            .filter(|r| !r.success && r.error.as_deref() != Some("duplicate"))
            .map(|r| (r.id, r.error.unwrap_or_else(|| "unknown".into())))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    fn client(server: &Server) -> ImmichClient {
        ImmichClient::new(&server.url(), "secret", Duration::from_secs(5), false)
            .unwrap()
            .with_retry_delay(Duration::from_millis(1))
    }

    #[test]
    fn api_base_accepts_with_and_without_api_suffix() {
        for suffix in ["", "/", "/api", "/api/"] {
            let mut server = Server::new();
            let mock = server
                .mock("GET", "/api/server/version")
                .match_header("x-api-key", "secret")
                .with_body(r#"{"major":2,"minor":5,"patch":6}"#)
                .create();
            let client = ImmichClient::new(&format!("{}{suffix}", server.url()), "secret", Duration::from_secs(5), false).unwrap();
            let v = client.server_version().unwrap();
            assert_eq!((v.major, v.minor, v.patch), (2, 5, 6));
            mock.assert();
        }
    }

    #[test]
    fn duplicate_check_reads_bulk_upload_check_result() {
        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/assets/bulk-upload-check")
            .match_body(Matcher::Json(serde_json::json!({"assets": [{"id": "dev-1", "checksum": "abc"}]})))
            .with_body(r#"{"results":[{"id":"dev-1","action":"reject","reason":"duplicate","assetId":"imm-1"}]}"#)
            .create();
        assert_eq!(client(&server).find_duplicate("dev-1", "abc").unwrap().as_deref(), Some("imm-1"));
        mock.assert();

        let mut server = Server::new();
        server
            .mock("POST", "/api/assets/bulk-upload-check")
            .with_body(r#"{"results":[{"id":"dev-1","action":"accept"}]}"#)
            .create();
        assert_eq!(client(&server).find_duplicate("dev-1", "abc").unwrap(), None);
    }

    #[test]
    fn upload_sends_multipart_fields_and_checksum_header() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("IMG_1.HEIC");
        std::fs::write(&file, b"not really a heic").unwrap();

        let mut server = Server::new();
        let mock = server
            .mock("POST", "/api/assets")
            .match_header("x-immich-checksum", "deadbeef")
            .match_header("content-type", Matcher::Regex("multipart/form-data; boundary=.*".into()))
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex(r#"name="deviceId"\r\n\r\ndev-host"#.into()),
                Matcher::Regex(r#"name="deviceAssetId"\r\n\r\nABC/L0/001"#.into()),
                Matcher::Regex(r#"name="fileCreatedAt"\r\n\r\n2023-11-14T22:13:20.000Z"#.into()),
                Matcher::Regex(r#"name="duration"\r\n\r\n12300"#.into()),
                Matcher::Regex(r#"name="isFavorite"\r\n\r\ntrue"#.into()),
                Matcher::Regex(r#"name="livePhotoVideoId"\r\n\r\nvid-9"#.into()),
                Matcher::Regex(r#"name="assetData"; filename="IMG_1.heic"\r\n[Cc]ontent-[Tt]ype: image/heic"#.into()),
                Matcher::Regex("not really a heic".into()),
            ]))
            .with_status(201)
            .with_body(r#"{"id":"imm-2","status":"created"}"#)
            .create();

        let resp = client(&server)
            .upload_asset(&UploadRequest {
                file: &file,
                filename: "IMG_1.heic",
                sha1_hex: "deadbeef",
                device_id: "dev-host",
                device_asset_id: "ABC/L0/001",
                file_created_at: "2023-11-14T22:13:20.000Z",
                file_modified_at: "2023-11-14T22:13:21.000Z",
                duration_ms: Some(12300),
                is_favorite: true,
                live_photo_video_id: Some("vid-9"),
            })
            .unwrap();
        assert_eq!((resp.id.as_str(), resp.status.as_str()), ("imm-2", "created"));
        mock.assert();
    }

    #[test]
    fn album_add_filters_already_present_assets() {
        let mut server = Server::new();
        let mock = server
            .mock("PUT", "/api/albums/alb-1/assets")
            .match_body(Matcher::Json(serde_json::json!({"ids": ["a", "b", "c"]})))
            .with_body(r#"[{"id":"a","success":true},{"id":"b","success":false,"error":"duplicate"},{"id":"c","success":false,"error":"no_permission"}]"#)
            .create();
        let rejected = client(&server).add_assets_to_album("alb-1", &["a".into(), "b".into(), "c".into()]).unwrap();
        assert_eq!(rejected, vec![("c".to_string(), "no_permission".to_string())]);
        mock.assert();
        assert!(client(&server).add_assets_to_album("alb-1", &[]).unwrap().is_empty());
    }

    #[test]
    fn albums_create_rename_and_favorite() {
        let mut server = Server::new();
        server
            .mock("GET", "/api/albums")
            .with_body(r#"[{"id":"alb-1","albumName":"Trip","assetCount":3}]"#)
            .create();
        let create = server
            .mock("POST", "/api/albums")
            .match_body(Matcher::Json(serde_json::json!({"albumName": "New"})))
            .with_status(201)
            .with_body(r#"{"id":"alb-2","albumName":"New"}"#)
            .create();
        let rename = server
            .mock("PATCH", "/api/albums/alb-2")
            .match_body(Matcher::Json(serde_json::json!({"albumName": "Renamed"})))
            .with_body(r#"{"id":"alb-2","albumName":"Renamed"}"#)
            .create();
        let fav = server
            .mock("PUT", "/api/assets/imm-1")
            .match_body(Matcher::Json(serde_json::json!({"isFavorite": true})))
            .with_body(r#"{"id":"imm-1"}"#)
            .create();
        let c = client(&server);
        let albums = c.list_albums().unwrap();
        assert_eq!(albums[0].album_name.as_deref(), Some("Trip"));
        assert_eq!(c.create_album("New").unwrap().id, "alb-2");
        c.rename_album("alb-2", "Renamed").unwrap();
        c.set_favorite("imm-1", true).unwrap();
        create.assert();
        rename.assert();
        fav.assert();
    }

    #[test]
    fn server_errors_are_retried_then_reported() {
        let mut server = Server::new();
        let mock = server.mock("GET", "/api/users/me").with_status(503).with_body("down").expect(4).create();
        let err = client(&server).validate_api_key().unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 503"), "{err:#}");
        mock.assert();

        let mut server = Server::new();
        let mock = server.mock("GET", "/api/users/me").with_status(401).with_body("bad key").expect(1).create();
        let err = client(&server).validate_api_key().unwrap_err();
        assert!(format!("{err:#}").contains("401"), "{err:#}");
        mock.assert();
    }
}
