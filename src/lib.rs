//! Navidrome ListenBrainz two-way loved-track sync.
//!
//! Each configured Navidrome user is linked to their own ListenBrainz account by token.
//! A scheduled pass reconciles the union of the two "loved" sets per user:
//! - inbound: a recording loved on ListenBrainz is starred on the matching local track;
//! - outbound: a locally starred recording is loved on ListenBrainz.
//!
//! The pass is convergent and incremental. Once both sides agree on a recording it is
//! recorded in KVStore as `synced:<user>:<recording_mbid>` and skipped on later passes.
//! Removals are deliberately never propagated, so the plugin cannot clobber a change it
//! did not make. The ListenBrainz username is derived from the token via `/validate-token`,
//! so only the token is configured.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use extism_pdk::{debug, info, warn};
use nd_pdk::host::{config, http, kvstore, matcher, scheduler, subsonicapi, users};
use nd_pdk::lifecycle::{Error as LifecycleError, InitProvider};
use nd_pdk::scheduler::{CallbackProvider, Error as SchedulerError, SchedulerCallbackRequest};
use nd_pdk::types::SongRef;
use serde::Deserialize;

// Generating the capability exports. Both reference `SyncPlugin`, defined below.
nd_pdk::register_lifecycle_init!(SyncPlugin);
nd_pdk::register_scheduler_callback!(SyncPlugin);

/// Payload identifying the sync job in scheduler callbacks.
const SYNC_JOB: &str = "like-sync";
/// ListenBrainz API root.
const LB_ROOT: &str = "https://api.listenbrainz.org/1";
/// Feedback records requested per page. The API caps this value.
const PAGE_SIZE: i64 = 1000;
/// ListenBrainz requires a contactable User-Agent on every request.
const USER_AGENT: &str = concat!(
    "Navidrome-ListenBrainz-Plugin/",
    env!("CARGO_PKG_VERSION"),
    " ( ege@celikci.me )"
);
/// ListenBrainz allows at most one request per second per client application.
const MIN_REQUEST_DELAY_MS: i64 = 1000;

/// Plugin entry point. Kept stateless: every callback re-reads config and KVStore.
#[derive(Default)]
struct SyncPlugin;

impl InitProvider for SyncPlugin {
    fn on_init(&self) -> Result<(), LifecycleError> {
        let cron = config::get("sync_schedule")
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "*/30 * * * *".to_string());
        info!("ListenBrainz sync registering schedule: {cron}");
        scheduler::schedule_recurring(&cron, SYNC_JOB, SYNC_JOB)
            .map_err(|e| LifecycleError::new(format!("scheduling sync: {e}")))?;
        // Reconcile shortly after load so a fresh install or update does not wait for the
        // next cron tick. One-time, so it does not pile up on repeated reloads.
        let _ = scheduler::schedule_one_time(5, SYNC_JOB, "like-sync-startup");
        Ok(())
    }
}

impl CallbackProvider for SyncPlugin {
    fn on_callback(&self, req: SchedulerCallbackRequest) -> Result<(), SchedulerError> {
        if req.payload == SYNC_JOB {
            run_sync();
        }
        Ok(())
    }
}

/// A Navidrome user linked to a ListenBrainz account by token.
struct UserLink {
    nd_username: String,
    token: String,
}

/// Global settings shared by every linked user.
struct Settings {
    links: Vec<UserLink>,
    sync_inbound: bool,
    sync_outbound: bool,
    dry_run: bool,
    batch_size: usize,
    request_delay_ms: u64,
    max_per_run: i64,
}

/// Why an inbound call stopped the pass instead of continuing.
enum Stop {
    /// Transient: rate limited or server error. Retry on the next pass.
    RateLimited,
    /// Permanent for this pass: authentication, malformed input, network failure.
    Fatal(String),
}

/// Outcome of a single ListenBrainz write.
enum Status {
    /// Applied successfully.
    Ok,
    /// Rejected for this record only; continue with the rest.
    Skip,
    /// Stop the pass; retry later.
    Stop,
}

/// One entry of the `user_links` config array.
#[derive(Debug, Deserialize)]
struct LinkConfig {
    #[serde(default)]
    navidrome_username: String,
    #[serde(default)]
    listenbrainz_token: String,
}

/// Reads configuration and returns `None` (after logging) when nothing usable is set.
fn load_settings() -> Option<Settings> {
    let raw = match config::get("user_links").ok().flatten() {
        Some(value) if !value.is_empty() => value,
        _ => {
            warn!("ListenBrainz sync: no user_links configured");
            return None;
        }
    };
    let parsed: Vec<LinkConfig> = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!("ListenBrainz sync: invalid user_links config: {e}");
            return None;
        }
    };
    let links: Vec<UserLink> = parsed
        .into_iter()
        .filter_map(|entry| {
            let nd_username = entry.navidrome_username.trim().to_string();
            let token = entry.listenbrainz_token.trim().to_string();
            if nd_username.is_empty() || token.is_empty() {
                warn!("ListenBrainz sync: skipping incomplete user link");
                return None;
            }
            Some(UserLink { nd_username, token })
        })
        .collect();
    if links.is_empty() {
        warn!("ListenBrainz sync: no valid user links configured");
        return None;
    }
    Some(Settings {
        links,
        sync_inbound: read_bool("sync_inbound", true),
        sync_outbound: read_bool("sync_outbound", true),
        dry_run: read_bool("dry_run", true),
        batch_size: read_int("batch_size", 100).clamp(1, 1000) as usize,
        request_delay_ms: read_int("request_delay_ms", MIN_REQUEST_DELAY_MS)
            .clamp(MIN_REQUEST_DELAY_MS, 60_000) as u64,
        max_per_run: read_int("max_per_run", 500).max(1),
    })
}

fn read_bool(key: &str, default: bool) -> bool {
    config::get(key)
        .ok()
        .flatten()
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(default)
}

fn read_int(key: &str, default: i64) -> i64 {
    config::get_int(key).ok().flatten().unwrap_or(default)
}

/// Runs one reconciliation pass for every linked user.
fn run_sync() {
    let Some(settings) = load_settings() else {
        return;
    };
    let authorized: HashSet<String> = match users::get_users() {
        Ok(list) => list.into_iter().map(|user| user.user_name).collect(),
        Err(e) => {
            warn!("ListenBrainz sync: could not list assigned users: {e}");
            return;
        }
    };

    info!(
        "ListenBrainz sync start (links={}, inbound={}, outbound={}, dry_run={})",
        settings.links.len(),
        settings.sync_inbound,
        settings.sync_outbound,
        settings.dry_run
    );

    for link in &settings.links {
        if !authorized.contains(&link.nd_username) {
            warn!(
                "ListenBrainz sync: Navidrome user '{}' is not assigned to this plugin",
                link.nd_username
            );
            continue;
        }
        match resolve_lb_username(&link.token) {
            Ok(Some(lb_username)) => sync_user(&settings, link, &lb_username),
            Ok(None) => warn!(
                "ListenBrainz sync: ListenBrainz token for '{}' is invalid",
                link.nd_username
            ),
            Err(Stop::RateLimited) => {
                warn!("ListenBrainz sync: rate limited while validating tokens; stopping pass");
                break;
            }
            Err(Stop::Fatal(message)) => {
                warn!(
                    "ListenBrainz sync: token validation failed for '{}': {message}",
                    link.nd_username
                );
            }
        }
    }
    info!("ListenBrainz sync complete");
}

/// Reconciles one user's loves and stars.
fn sync_user(settings: &Settings, link: &UserLink, lb_username: &str) {
    let mut settled = load_settled(&link.nd_username);
    let mut budget = settings.max_per_run;
    if settings.sync_inbound && budget > 0 {
        budget -= sync_inbound(settings, link, lb_username, &mut settled, budget);
    }
    if settings.sync_outbound && budget > 0 {
        sync_outbound(settings, link, &mut settled, budget);
    }
}

/// Derives the ListenBrainz username from a user token. `None` means the token is invalid.
fn resolve_lb_username(token: &str) -> Result<Option<String>, Stop> {
    let url = format!("{LB_ROOT}/validate-token");
    let (body, _) = lb_get(&url, token)?;
    let parsed: TokenValidation = serde_json::from_str(&body)
        .map_err(|e| Stop::Fatal(format!("malformed validate-token response: {e}")))?;
    if !parsed.valid {
        return Ok(None);
    }
    match parsed.user_name {
        Some(name) if !name.is_empty() => Ok(Some(name)),
        _ => Ok(None),
    }
}

/// KVStore key prefix for a user's already-reconciled recordings.
fn settled_prefix(nd_username: &str) -> String {
    format!("synced:{nd_username}:")
}

/// Loads a user's settled set once per pass, stripping the key prefix.
fn load_settled(nd_username: &str) -> HashSet<String> {
    let prefix = settled_prefix(nd_username);
    match kvstore::list(&prefix) {
        Ok(keys) => keys
            .into_iter()
            .filter_map(|key| key.strip_prefix(&prefix).map(str::to_string))
            .collect(),
        Err(e) => {
            warn!("ListenBrainz sync: could not load settled state: {e}");
            HashSet::new()
        }
    }
}

/// Records a recording as reconciled for a user. Persistence is skipped in dry-run mode.
fn mark_settled(settings: &Settings, settled: &mut HashSet<String>, nd_username: &str, mbid: &str) {
    settled.insert(mbid.to_string());
    if settings.dry_run {
        return;
    }
    let key = format!("{}{mbid}", settled_prefix(nd_username));
    if let Err(e) = kvstore::set(&key, Vec::new()) {
        warn!("ListenBrainz sync: could not persist settled '{mbid}': {e}");
    }
}

/// Stars every ListenBrainz love that is not already settled. Returns how many were applied.
fn sync_inbound(
    settings: &Settings,
    link: &UserLink,
    lb_username: &str,
    settled: &mut HashSet<String>,
    budget: i64,
) -> i64 {
    let mut applied = 0i64;
    let mut offset = 0i64;
    while applied < budget {
        let url = format!(
            "{LB_ROOT}/feedback/user/{}/get-feedback?score=1&count={PAGE_SIZE}&offset={offset}",
            percent_encode(lb_username)
        );
        let (body, exhausted) = match lb_get(&url, &link.token) {
            Ok(page) => page,
            Err(Stop::RateLimited) => {
                warn!("ListenBrainz sync: inbound pausing (rate limited or server error)");
                break;
            }
            Err(Stop::Fatal(message)) => {
                warn!("ListenBrainz sync: inbound stopped: {message}");
                break;
            }
        };
        let page: FeedbackPage = match serde_json::from_str(&body) {
            Ok(page) => page,
            Err(e) => {
                warn!("ListenBrainz sync: malformed feedback page: {e}");
                break;
            }
        };
        if page.feedback.is_empty() {
            break;
        }

        let mut candidates = inbound_candidates(&page, settled);
        let room = (budget - applied) as usize;
        if candidates.len() > room {
            candidates.truncate(room);
        }
        if !candidates.is_empty() {
            applied += resolve_and_star(settings, link, settled, &candidates);
        }

        if exhausted {
            warn!("ListenBrainz sync: inbound rate-limit window exhausted");
            break;
        }
        offset += page.count.max(1);
        if page.count < PAGE_SIZE {
            break;
        }
        if page.total_count > 0 && offset >= page.total_count {
            break;
        }
        sleep_ms(settings.request_delay_ms);
    }
    applied
}

/// Selects the loved, not-yet-settled, valid recording MBIDs from a feedback page.
fn inbound_candidates(page: &FeedbackPage, settled: &HashSet<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for entry in &page.feedback {
        if entry.score != 1 {
            continue;
        }
        let Some(mbid) = entry.recording_mbid.as_deref() else {
            continue;
        };
        if !is_valid_mbid(mbid) {
            warn!("ListenBrainz sync: skipping malformed MBID '{mbid}'");
            continue;
        }
        if settled.contains(mbid) {
            continue;
        }
        if seen.insert(mbid.to_string()) {
            candidates.push(mbid.to_string());
        }
    }
    candidates
}

/// Resolves MBIDs to local tracks and stars the unstarred ones in a single batched call.
fn resolve_and_star(
    settings: &Settings,
    link: &UserLink,
    settled: &mut HashSet<String>,
    candidates: &[String],
) -> i64 {
    let mut to_star = Vec::new();
    let mut to_star_mbids = Vec::new();

    for chunk in candidates.chunks(settings.batch_size) {
        let songs: Vec<SongRef> = chunk
            .iter()
            .map(|mbid| SongRef {
                mbid: mbid.clone(),
                ..Default::default()
            })
            .collect();
        let options = matcher::MatchOptions {
            username: link.nd_username.clone(),
        };
        let results = match matcher::match_songs(songs, options) {
            Ok(results) => results,
            Err(e) => {
                warn!("ListenBrainz sync: matcher failed: {e}");
                continue;
            }
        };
        for (mbid, result) in chunk.iter().zip(results) {
            match result {
                Some(track) if !track.id.is_empty() => {
                    if track.starred {
                        mark_settled(settings, settled, &link.nd_username, mbid);
                    } else if settings.dry_run {
                        info!("ListenBrainz sync [dry-run]: would star '{}'", track.title);
                    } else {
                        to_star.push(track.id);
                        to_star_mbids.push(mbid.clone());
                    }
                }
                _ => debug!("ListenBrainz sync: no local match for {mbid}"),
            }
        }
    }

    if to_star.is_empty() || !star_tracks(link, &to_star) {
        return 0;
    }
    for mbid in &to_star_mbids {
        mark_settled(settings, settled, &link.nd_username, mbid);
    }
    to_star_mbids.len() as i64
}

/// Sends one batched Subsonic `star` call. Returns whether the API reported success.
fn star_tracks(link: &UserLink, media_ids: &[String]) -> bool {
    let mut query = String::from("star?");
    for id in media_ids {
        query.push_str("id=");
        query.push_str(&percent_encode(id));
        query.push('&');
    }
    query.push_str("u=");
    query.push_str(&percent_encode(&link.nd_username));

    match subsonicapi::call(&query) {
        Ok(body) if subsonic_ok(&body) => true,
        Ok(body) => {
            warn!("ListenBrainz sync: star call failed: {body}");
            false
        }
        Err(e) => {
            warn!("ListenBrainz sync: star call error: {e}");
            false
        }
    }
}

/// Loves every locally starred recording that is not already settled.
fn sync_outbound(
    settings: &Settings,
    link: &UserLink,
    settled: &mut HashSet<String>,
    budget: i64,
) {
    let uri = format!("getStarred2?u={}", percent_encode(&link.nd_username));
    let body = match subsonicapi::call(&uri) {
        Ok(body) => body,
        Err(e) => {
            warn!("ListenBrainz sync: getStarred2 failed: {e}");
            return;
        }
    };
    let songs = match serde_json::from_str::<SubsonicEnvelope>(&body) {
        Ok(envelope) => envelope
            .response
            .starred2
            .map(|starred| starred.song)
            .unwrap_or_default(),
        Err(e) => {
            warn!("ListenBrainz sync: malformed getStarred2 response: {e}");
            return;
        }
    };

    let mut pushed = 0i64;
    let mut seen = HashSet::new();
    for song in songs {
        if pushed >= budget {
            break;
        }
        let mbid = song.music_brainz_id;
        if mbid.is_empty() || !is_valid_mbid(&mbid) {
            continue;
        }
        if settled.contains(&mbid) || !seen.insert(mbid.clone()) {
            continue;
        }
        if settings.dry_run {
            info!("ListenBrainz sync [dry-run]: would love {mbid}");
            continue;
        }
        let (status, exhausted) = match submit_love(&link.token, &mbid) {
            Ok(result) => result,
            Err(e) => {
                warn!("ListenBrainz sync: feedback request failed: {e}");
                break;
            }
        };
        match status {
            Status::Ok => {
                mark_settled(settings, settled, &link.nd_username, &mbid);
                pushed += 1;
            }
            Status::Skip => {}
            Status::Stop => {
                warn!("ListenBrainz sync: outbound pausing (rate limited, server error, or auth failure)");
                break;
            }
        }
        if exhausted {
            warn!("ListenBrainz sync: outbound rate-limit window exhausted");
            break;
        }
        sleep_ms(settings.request_delay_ms);
    }
}

/// GETs a ListenBrainz endpoint with the user token. The bool is true when the
/// rate-limit window is exhausted, so the caller must make no further API calls.
fn lb_get(url: &str, token: &str) -> Result<(String, bool), Stop> {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Token {token}"));
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), USER_AGENT.to_string());

    let request = http::HTTPRequest {
        method: "GET".to_string(),
        url: url.to_string(),
        headers,
        timeout_ms: 30_000,
        ..Default::default()
    };
    let response = http::send(request)
        .map_err(|e| Stop::Fatal(e.to_string()))?
        .ok_or_else(|| Stop::Fatal("empty HTTP response".to_string()))?;

    if response.status_code == 429 {
        log_rate_limit(&response.headers);
        return Err(Stop::RateLimited);
    }
    if !(200..300).contains(&response.status_code) {
        return Err(classify_stop(response.status_code));
    }
    let exhausted = rate_limit_exhausted(&response.headers);
    let body = String::from_utf8(response.body)
        .map_err(|e| Stop::Fatal(format!("invalid UTF-8 response: {e}")))?;
    Ok((body, exhausted))
}

/// Submits love feedback for a single recording. One feedback per request is an API constraint.
/// The bool is true when the rate-limit window is exhausted.
fn submit_love(token: &str, mbid: &str) -> Result<(Status, bool), String> {
    let payload = serde_json::json!({ "recording_mbid": mbid, "score": 1 });
    let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Token {token}"));
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), USER_AGENT.to_string());

    let request = http::HTTPRequest {
        method: "POST".to_string(),
        url: format!("{LB_ROOT}/feedback/recording-feedback"),
        headers,
        body,
        timeout_ms: 30_000,
        ..Default::default()
    };
    let response = http::send(request)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "empty HTTP response".to_string())?;

    if response.status_code == 429 {
        log_rate_limit(&response.headers);
    }
    let exhausted = rate_limit_exhausted(&response.headers);
    let status = match response.status_code {
        200..=299 => Status::Ok,
        401 | 403 => Status::Stop,
        429 => Status::Stop,
        500..=599 => Status::Stop,
        400..=499 => Status::Skip,
        _ => Status::Skip,
    };
    Ok((status, exhausted))
}

/// Maps an HTTP status to the pass-stopping reason.
fn classify_stop(status: i32) -> Stop {
    if status == 429 || status >= 500 {
        Stop::RateLimited
    } else {
        Stop::Fatal(format!("HTTP {status}"))
    }
}

/// Case-insensitive header lookup; HTTP header casing is not guaranteed.
fn header_value<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// True when the response reports no requests left in the current window.
fn rate_limit_exhausted(headers: &HashMap<String, String>) -> bool {
    header_value(headers, "X-RateLimit-Remaining")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .map(|remaining| remaining <= 0)
        .unwrap_or(false)
}

/// Logs how long the rate-limit window has left, preferring the clock-skew-safe header.
fn log_rate_limit(headers: &HashMap<String, String>) {
    match header_value(headers, "X-RateLimit-Reset-In") {
        Some(reset) => warn!("ListenBrainz sync: rate limit reached, window resets in {reset}s"),
        None => warn!("ListenBrainz sync: rate limit reached"),
    }
}

/// Checks the Subsonic envelope status without assuming the body is well-formed.
fn subsonic_ok(body: &str) -> bool {
    serde_json::from_str::<SubsonicEnvelope>(body)
        .map(|envelope| envelope.response.status == "ok")
        .unwrap_or(false)
}

/// Sleeps between writes so ListenBrainz is never hammered. Backed by WASI `poll_oneoff`.
fn sleep_ms(ms: u64) {
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Percent-encodes a string for safe use in a URL path or query. RFC 3986 unreserved set.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Accepts only non-empty, UUID-shaped MusicBrainz IDs.
fn is_valid_mbid(value: &str) -> bool {
    value.len() == 36 && value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Response from `/validate-token`.
#[derive(Debug, Deserialize)]
struct TokenValidation {
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    user_name: Option<String>,
}

/// One page of the ListenBrainz feedback endpoint.
#[derive(Debug, Deserialize)]
struct FeedbackPage {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    total_count: i64,
    #[serde(default)]
    feedback: Vec<FeedbackEntry>,
}

/// A single love/hate feedback record. Unknown fields are ignored by design.
#[derive(Debug, Deserialize)]
struct FeedbackEntry {
    #[serde(default)]
    recording_mbid: Option<String>,
    #[serde(default)]
    score: i64,
}

/// Top-level Subsonic response envelope.
#[derive(Debug, Deserialize)]
struct SubsonicEnvelope {
    #[serde(rename = "subsonic-response", default)]
    response: SubsonicResponse,
}

#[derive(Debug, Default, Deserialize)]
struct SubsonicResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    starred2: Option<Starred2>,
}

#[derive(Debug, Deserialize)]
struct Starred2 {
    #[serde(default)]
    song: Vec<StarredSong>,
}

/// A starred song. `musicBrainzId` carries the recording MBID.
#[derive(Debug, Deserialize)]
struct StarredSong {
    #[serde(rename = "musicBrainzId", default)]
    music_brainz_id: String,
}
