//! Navidrome ListenBrainz loved-track sync and CritiqueBrainz rating sync.
//!
//! ListenBrainz, for each Navidrome user linked by token:
//! - inbound: a recording loved on ListenBrainz is starred on the matching local track;
//! - outbound: a locally starred recording is loved on ListenBrainz.
//!
//! CritiqueBrainz, one-way (ratings are read, never written), per entity type:
//! - the rating of a loved/rated artist, release group, or recording is copied onto the
//!   matching Navidrome artist, album, or song;
//! - a rating at or above a configured threshold also favourites (hearts) that item.
//!
//! Every pass is convergent and incremental: what a pass has already applied is recorded in
//! KVStore, so a repeat pass redoes only what is still outstanding. An applied state is skipped
//! without any lookup, but the CritiqueBrainz rating list is still paged to look for new ratings
//! (one request per 50 ratings per enabled entity type, at the politeness floor). Removals are
//! deliberately never propagated, so the plugin cannot clobber a change it did not make. The
//! ListenBrainz username is derived from the token via `/validate-token`; the CritiqueBrainz
//! username falls back to it.

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
/// CritiqueBrainz API root.
const CB_ROOT: &str = "https://critiquebrainz.org/ws/1";
/// Ratings requested per CritiqueBrainz page. The API rejects anything above 50.
const CB_PAGE_SIZE: i64 = 50;
/// Local items requested per CritiqueBrainz entity lookup. One entity can cover several local
/// items (a release group may hold duplicate albums, a recording can sit on many
/// compilations), so the lookup asks for all of them rather than just the first.
const MATCH_COUNT: i64 = 50;

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

/// A Navidrome user linked to a ListenBrainz account by token, and optionally to a
/// CritiqueBrainz username that differs from the ListenBrainz one.
struct UserLink {
    nd_username: String,
    token: String,
    cb_username: Option<String>,
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
    /// Enabled CritiqueBrainz entity rules, one per entity type the administrator opted into.
    cb_rules: Vec<(CbEntity, CbRule)>,
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
    #[serde(default)]
    critiquebrainz_username: String,
}

/// A CritiqueBrainz entity type that maps onto a Navidrome item.
#[derive(Clone, Copy)]
enum CbEntity {
    /// CritiqueBrainz `artist` → Navidrome artist.
    Artist,
    /// CritiqueBrainz `release_group` → Navidrome album.
    ReleaseGroup,
    /// CritiqueBrainz `recording` → Navidrome song.
    Recording,
}

impl CbEntity {
    /// The `entity_type` value the CritiqueBrainz API expects and returns.
    fn cb_name(self) -> &'static str {
        match self {
            CbEntity::Artist => "artist",
            CbEntity::ReleaseGroup => "release_group",
            CbEntity::Recording => "recording",
        }
    }

    /// The Subsonic `star` parameter carrying this entity's id. Only songs are named `id`.
    fn star_param(self) -> &'static str {
        match self {
            CbEntity::Artist => "artistId",
            CbEntity::ReleaseGroup => "albumId",
            CbEntity::Recording => "id",
        }
    }

    /// Whether Navidrome echoes back the same MusicBrainz id that was queried against it.
    ///
    /// Songs and artists do (`musicBrainzId` is the recording / artist MBID). Albums do not:
    /// Navidrome reports the *release* MBID, while CritiqueBrainz rates *release groups*, so
    /// for albums the two differ legitimately and must never be compared.
    fn echoes_queried_mbid(self) -> bool {
        match self {
            CbEntity::Artist | CbEntity::Recording => true,
            CbEntity::ReleaseGroup => false,
        }
    }
}

/// What to apply to one CritiqueBrainz entity type. `favorite_at == 0` disables favouriting,
/// so the two toggles independently cover rating-only, favourite-only, both, or neither.
#[derive(Clone, Copy, Deserialize)]
struct CbRule {
    #[serde(default)]
    sync_rating: bool,
    #[serde(default)]
    favorite_at: i64,
}

impl CbRule {
    /// True when the rule asks for no action, so the entity type is dropped entirely.
    fn is_disabled(self) -> bool {
        !self.sync_rating && self.favorite_at == 0
    }
}

/// Reads the per-entity CritiqueBrainz rules. Objects arrive JSON-encoded in the config map.
/// Disabled rules are dropped, so a pass touches CritiqueBrainz only once opted in.
fn read_cb_rules() -> Vec<(CbEntity, CbRule)> {
    let mut rules = Vec::new();
    for (entity, key) in [
        (CbEntity::Artist, "cb_sync_artists"),
        (CbEntity::ReleaseGroup, "cb_sync_albums"),
        (CbEntity::Recording, "cb_sync_recordings"),
    ] {
        let Some(raw) = config::get(key)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let mut rule: CbRule = match serde_json::from_str(&raw) {
            Ok(rule) => rule,
            Err(e) => {
                warn!("CritiqueBrainz sync: invalid {key} config: {e}");
                continue;
            }
        };
        if !(0..=5).contains(&rule.favorite_at) {
            warn!("CritiqueBrainz sync: {key} favorite_at must be 0-5, favouriting stays off");
            rule.favorite_at = 0;
        }
        if !rule.is_disabled() {
            rules.push((entity, rule));
        }
    }
    rules
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
            let cb_username = entry.critiquebrainz_username.trim().to_string();
            if nd_username.is_empty() || token.is_empty() {
                warn!("ListenBrainz sync: skipping incomplete user link");
                return None;
            }
            Some(UserLink {
                nd_username,
                token,
                cb_username: (!cb_username.is_empty()).then_some(cb_username),
            })
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
        cb_rules: read_cb_rules(),
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
        "Sync start (links={}, inbound={}, outbound={}, critiquebrainz_rules={}, dry_run={})",
        settings.links.len(),
        settings.sync_inbound,
        settings.sync_outbound,
        settings.cb_rules.len(),
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
        // The ListenBrainz username doubles as the CritiqueBrainz identity when no separate
        // one is configured, so it is resolved first. An invalid token only disables the
        // ListenBrainz half: CritiqueBrainz needs no token at all.
        let (lb_username, stop_pass) =
            match resolve_lb_username(&link.token, settings.request_delay_ms) {
                Ok(Some(lb_username)) => (Some(lb_username), false),
                Ok(None) => {
                    warn!(
                        "ListenBrainz sync: ListenBrainz token for '{}' is invalid",
                        link.nd_username
                    );
                    (None, false)
                }
                Err(Stop::RateLimited) => {
                    warn!("ListenBrainz sync: rate limited while validating tokens; stopping pass");
                    (None, true)
                }
                Err(Stop::Fatal(message)) => {
                    warn!(
                        "ListenBrainz sync: token validation failed for '{}': {message}",
                        link.nd_username
                    );
                    (None, false)
                }
            };
        if !settings.cb_rules.is_empty() {
            sync_critiquebrainz(
                &settings,
                link,
                lb_username.as_deref(),
                settings.max_per_run,
            );
        }
        if let Some(lb_username) = lb_username {
            sync_user(&settings, link, &lb_username);
        }
        if stop_pass {
            break;
        }
    }
    info!("Sync complete");
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
fn resolve_lb_username(token: &str, delay_ms: u64) -> Result<Option<String>, Stop> {
    let url = format!("{LB_ROOT}/validate-token");
    let auth = format!("Token {token}");
    let (body, _) = api_get(&url, Some(&auth), delay_ms)?;
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
        let auth = format!("Token {}", link.token);
        let (body, exhausted) = match api_get(&url, Some(&auth), settings.request_delay_ms) {
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
        let (status, exhausted) = match submit_love(&link.token, &mbid, settings.request_delay_ms) {
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
    }
}

/// Copies CritiqueBrainz ratings into Navidrome for one user.
///
/// CritiqueBrainz is independent of ListenBrainz: the ratings live on public reviews, so only
/// a username is needed and no token or OAuth flow is involved. A CritiqueBrainz user id is
/// resolved once per pass, then each enabled entity type is paged.
fn sync_critiquebrainz(
    settings: &Settings,
    link: &UserLink,
    lb_username: Option<&str>,
    budget: i64,
) {
    let username = match link.cb_username.as_deref().or(lb_username) {
        Some(username) => username,
        None => {
            warn!(
                "CritiqueBrainz sync: no username for '{}' and no ListenBrainz username to fall back on",
                link.nd_username
            );
            return;
        }
    };
    let user_id = match resolve_cb_user_id(username, settings.request_delay_ms) {
        Ok(Some(user_id)) => user_id,
        Ok(None) => {
            warn!("CritiqueBrainz sync: CritiqueBrainz user '{username}' has no id");
            return;
        }
        Err(Stop::RateLimited) => {
            warn!("CritiqueBrainz sync: rate limited while resolving '{username}'; stopping pass");
            return;
        }
        Err(Stop::Fatal(message)) => {
            warn!("CritiqueBrainz sync: could not resolve '{username}': {message}");
            return;
        }
    };
    // The username can be inherited from the ListenBrainz link, and binding the wrong account
    // would sync a stranger's public ratings into this library, so the binding is always logged.
    info!(
        "CritiqueBrainz sync: using account '{username}' ({user_id}) for '{}'",
        link.nd_username
    );

    let mut settled = load_cb_settled(&link.nd_username);
    let mut changes = 0i64;
    for (entity, rule) in &settings.cb_rules {
        if changes >= budget {
            break;
        }
        changes += sync_cb_entity(
            settings,
            link,
            &user_id,
            *entity,
            *rule,
            &mut settled,
            budget - changes,
        );
    }
}

/// Resolves a CritiqueBrainz username (or user id) to the id the reviews API filters on.
fn resolve_cb_user_id(username: &str, delay_ms: u64) -> Result<Option<String>, Stop> {
    let url = format!("{CB_ROOT}/user/{}", percent_encode(username));
    let (body, _) = api_get(&url, None, delay_ms)?;
    let parsed: CbUserEnvelope = serde_json::from_str(&body)
        .map_err(|e| Stop::Fatal(format!("malformed user response: {e}")))?;
    Ok(parsed.user.map(|user| user.id).filter(|id| !id.is_empty()))
}

/// Pages one entity type's ratings and applies every state that is not already settled.
///
/// Returns how many changes were attempted, which is what the per-pass budget caps. Skipping an
/// already-settled rating is free, and so is a rating with no local match, so the *walk* stays
/// bounded by the account's rating count rather than by the budget: one request per page, at the
/// politeness floor. Counting only changes is also what stops a permanent backlog of unmatched
/// ratings from pinning every pass to the same first page and starving everything behind it.
fn sync_cb_entity(
    settings: &Settings,
    link: &UserLink,
    user_id: &str,
    entity: CbEntity,
    rule: CbRule,
    settled: &mut HashSet<String>,
    budget: i64,
) -> i64 {
    let mut changes = 0i64;
    let mut examined = 0i64;
    let mut unmatched = 0i64;
    let mut offset = 0i64;
    loop {
        // Reaching here means a previous page did not exhaust the list, so there really is more
        // work waiting rather than the budget being spent on the last row.
        if changes >= budget {
            warn!(
                "CritiqueBrainz sync: {} change budget of {budget} reached; remaining ratings continue next pass",
                entity.cb_name()
            );
            break;
        }
        // `entity_type` is validated against concrete types only, so each type is paged
        // separately; `review_type=rating` keeps reviews that carry a rating.
        let url = format!(
            "{CB_ROOT}/review/?user_id={}&entity_type={}&review_type=rating&limit={CB_PAGE_SIZE}&offset={offset}",
            percent_encode(user_id),
            entity.cb_name()
        );
        let (body, exhausted) = match api_get(&url, None, settings.request_delay_ms) {
            Ok(page) => page,
            Err(Stop::RateLimited) => {
                warn!(
                    "CritiqueBrainz sync: {} pausing (rate limited or server error)",
                    entity.cb_name()
                );
                break;
            }
            Err(Stop::Fatal(message)) => {
                warn!(
                    "CritiqueBrainz sync: {} stopped: {message}",
                    entity.cb_name()
                );
                break;
            }
        };
        let page: CbReviewPage = match serde_json::from_str(&body) {
            Ok(page) => page,
            Err(e) => {
                warn!(
                    "CritiqueBrainz sync: malformed {} page: {e}",
                    entity.cb_name()
                );
                break;
            }
        };
        if page.reviews.is_empty() {
            break;
        }

        for review in &page.reviews {
            if changes >= budget {
                break;
            }
            // The server filters by entity type, but a mismatch would silently apply a
            // rating to the wrong kind of item, so it is verified rather than assumed.
            if review.entity_type != entity.cb_name() {
                warn!(
                    "CritiqueBrainz sync: skipping unexpected entity type '{}'",
                    review.entity_type
                );
                continue;
            }
            let Some(rating) = review.rating() else {
                continue;
            };
            if !(1..=5).contains(&rating) {
                warn!("CritiqueBrainz sync: skipping out-of-range rating {rating}");
                continue;
            }
            if !is_valid_mbid(&review.entity_id) {
                warn!(
                    "CritiqueBrainz sync: skipping malformed entity id '{}'",
                    review.entity_id
                );
                continue;
            }
            if settled.contains(&cb_state_key(entity, &review.entity_id, rating, rule)) {
                continue;
            }
            examined += 1;
            match apply_cb_state(
                settings,
                link,
                entity,
                &review.entity_id,
                rating,
                rule,
                settled,
            ) {
                None => unmatched += 1,
                Some(attempted) => changes += attempted,
            }
        }

        match cb_next_offset(offset, page.reviews.len() as i64, page.count) {
            Some(next) => offset = next,
            None => break,
        }
        if exhausted {
            warn!("CritiqueBrainz sync: rate-limit window exhausted");
            break;
        }
    }
    // A rating with no local match is normal (the library simply does not have it); a whole
    // entity type matching nothing is a bug. Either way the tally is reported, so silence is
    // never mistaken for "nothing to do".
    info!(
        "CritiqueBrainz sync: {} ratings for '{}' (examined {examined}, changes {changes}, no local match {unmatched})",
        entity.cb_name(),
        link.nd_username
    );
    changes
}

/// The next CritiqueBrainz page offset, or `None` when the list is exhausted. Advancing by the
/// number of rows actually returned, rather than the page size that was asked for, keeps a short
/// final page from skipping rows.
fn cb_next_offset(offset: i64, page_len: i64, count: i64) -> Option<i64> {
    if page_len <= 0 {
        return None;
    }
    let next = offset + page_len;
    if next >= count {
        None
    } else {
        Some(next)
    }
}

/// Applies one required state to every local Navidrome item matching the CritiqueBrainz entity.
/// A failed write leaves the key unsettled, so the next pass retries just that entity. Returns
/// the number of write attempts, or `None` when no local item matched.
fn apply_cb_state(
    settings: &Settings,
    link: &UserLink,
    entity: CbEntity,
    mbid: &str,
    rating: i64,
    rule: CbRule,
    settled: &mut HashSet<String>,
) -> Option<i64> {
    let state = cb_state_key(entity, mbid, rating, rule);
    let items = lookup_local_items(link, entity, mbid);
    if items.is_empty() {
        // Deliberately not settled: a later library scan may add the item.
        debug!(
            "CritiqueBrainz sync: no local match for {} {mbid}",
            entity.cb_name()
        );
        return None;
    }

    // Every local item of this entity gets the same state, but only the ones still missing it
    // are touched, so a library the user rated by hand converges instead of being rewritten.
    let rating_targets = if rule.sync_rating {
        pending_rating(&items, rating)
    } else {
        Vec::new()
    };
    let star_targets = if rule.favorite_at > 0 && rating >= rule.favorite_at {
        pending_star(&items)
    } else {
        Vec::new()
    };

    if settings.dry_run {
        if !rating_targets.is_empty() {
            info!(
                "CritiqueBrainz sync [dry-run]: would set {} '{mbid}' rating to {rating} on {} item(s)",
                entity.cb_name(),
                rating_targets.len()
            );
        }
        if !star_targets.is_empty() {
            info!(
                "CritiqueBrainz sync [dry-run]: would favourite {} '{mbid}' ({} item(s))",
                entity.cb_name(),
                star_targets.len()
            );
        }
        mark_cb_settled(settings, settled, &link.nd_username, &state);
        return Some(0);
    }

    // An attempted write counts against the budget even when it fails: a systemic failure would
    // otherwise walk the whole library in one pass, logging a warning per rating.
    let mut attempted = 0i64;
    for item in &rating_targets {
        attempted += 1;
        if !set_rating(link, &item.id, rating) {
            return Some(attempted);
        }
    }
    for item in &star_targets {
        attempted += 1;
        if !star_item(link, entity, &item.id) {
            return Some(attempted);
        }
    }
    if attempted > 0 {
        info!(
            "CritiqueBrainz sync: {} '{mbid}' -> {state} (rating on {}, favourite on {} item(s))",
            entity.cb_name(),
            rating_targets.len(),
            star_targets.len()
        );
    }
    mark_cb_settled(settings, settled, &link.nd_username, &state);
    Some(attempted)
}

/// The local items that do not hold the wanted rating yet.
fn pending_rating(items: &[SearchItem], rating: i64) -> Vec<&SearchItem> {
    items
        .iter()
        .filter(|item| item.user_rating != Some(rating))
        .collect()
}

/// The local items that are not favourited yet.
fn pending_star(items: &[SearchItem]) -> Vec<&SearchItem> {
    items.iter().filter(|item| item.starred.is_none()).collect()
}

/// Finds every local Navidrome item matching one CritiqueBrainz entity id.
///
/// `search3` matches a UUID query directly against Navidrome's MusicBrainz id columns
/// (recording / release group or release / artist), so every entity type resolves without
/// scanning the library. One entity can span several local items — a release group may hold
/// duplicate albums and a recording can appear on many compilations — and since they are all the
/// same entity, all of them are returned. Trusting that match means the returned ids may be
/// cross-checked only where Navidrome reports the same kind of MBID back.
fn lookup_local_items(link: &UserLink, entity: CbEntity, mbid: &str) -> Vec<SearchItem> {
    let uri = format!(
        "search3?query={}&artistCount={MATCH_COUNT}&albumCount={MATCH_COUNT}&songCount={MATCH_COUNT}&u={}",
        percent_encode(mbid),
        percent_encode(&link.nd_username)
    );
    let body = match subsonicapi::call(&uri) {
        Ok(body) => body,
        Err(e) => {
            warn!("CritiqueBrainz sync: search3 failed for {mbid}: {e}");
            return Vec::new();
        }
    };
    let envelope = match serde_json::from_str::<SubsonicEnvelope>(&body) {
        Ok(envelope) => envelope,
        Err(e) => {
            warn!("CritiqueBrainz sync: malformed search3 response for {mbid}: {e}");
            return Vec::new();
        }
    };
    let Some(result) = envelope.response.search_result3 else {
        return Vec::new();
    };
    let mut items = match entity {
        CbEntity::Artist => result.artist,
        CbEntity::ReleaseGroup => result.album,
        CbEntity::Recording => result.song,
    };
    items.retain(|item| {
        if item.id.is_empty() {
            return false;
        }
        if entity.echoes_queried_mbid()
            && !item.mbid.is_empty()
            && !item.mbid.eq_ignore_ascii_case(mbid)
        {
            debug!(
                "CritiqueBrainz sync: search3 returned {} '{}' for {mbid}, skipping",
                entity.cb_name(),
                item.mbid
            );
            return false;
        }
        true
    });
    items
}

/// Sets one Navidrome rating through Subsonic.
fn set_rating(link: &UserLink, id: &str, rating: i64) -> bool {
    let uri = format!(
        "setRating?id={}&rating={rating}&u={}",
        percent_encode(id),
        percent_encode(&link.nd_username)
    );
    subsonic_call_ok(&uri, "setRating")
}

/// Favourites (hearts) one Navidrome item through Subsonic.
fn star_item(link: &UserLink, entity: CbEntity, id: &str) -> bool {
    let uri = format!(
        "star?{}={}&u={}",
        entity.star_param(),
        percent_encode(id),
        percent_encode(&link.nd_username)
    );
    subsonic_call_ok(&uri, "star")
}

/// Runs one Subsonic write and reports whether it succeeded. Failures are logged, never fatal.
fn subsonic_call_ok(uri: &str, action: &str) -> bool {
    match subsonicapi::call(uri) {
        Ok(body) if subsonic_ok(&body) => true,
        Ok(body) => {
            warn!("CritiqueBrainz sync: {action} call failed: {body}");
            false
        }
        Err(e) => {
            warn!("CritiqueBrainz sync: {action} call error: {e}");
            false
        }
    }
}

/// KVStore key prefix for the CritiqueBrainz states already applied for one user. Kept apart
/// from the ListenBrainz `synced:` set: these keys encode a desired state rather than a
/// recording that is settled on both sides, so a changed rating produces a new key.
fn cb_settled_prefix(nd_username: &str) -> String {
    format!("cbsynced:{nd_username}:")
}

/// Loads a user's applied CritiqueBrainz states once per pass, stripping the key prefix.
fn load_cb_settled(nd_username: &str) -> HashSet<String> {
    let prefix = cb_settled_prefix(nd_username);
    match kvstore::list(&prefix) {
        Ok(keys) => keys
            .into_iter()
            .filter_map(|key| key.strip_prefix(&prefix).map(str::to_string))
            .collect(),
        Err(e) => {
            warn!("CritiqueBrainz sync: could not load settled state: {e}");
            HashSet::new()
        }
    }
}

/// Records one applied CritiqueBrainz state for a user. Persistence is skipped in dry-run mode.
fn mark_cb_settled(
    settings: &Settings,
    settled: &mut HashSet<String>,
    nd_username: &str,
    state: &str,
) {
    settled.insert(state.to_string());
    if settings.dry_run {
        return;
    }
    let key = format!("{}{state}", cb_settled_prefix(nd_username));
    if let Err(e) = kvstore::set(&key, Vec::new()) {
        warn!("CritiqueBrainz sync: could not persist settled '{state}': {e}");
    }
}

/// The applied state of one entity under one rule. It lists only the actions the rule asks
/// for, so a changed rating re-applies just that action, and a rule that only favourites is
/// never re-applied because of a rating change it does not care about.
fn cb_state_key(entity: CbEntity, mbid: &str, rating: i64, rule: CbRule) -> String {
    let mut key = format!("{}:{mbid}:", entity.cb_name());
    if rule.sync_rating {
        key.push_str(&format!("r{rating}:"));
    }
    if rule.favorite_at > 0 {
        key.push_str(if rating >= rule.favorite_at {
            "f1"
        } else {
            "f0"
        });
    }
    key
}

/// GETs a JSON endpoint, sending `auth` as the Authorization header when present. The
/// CritiqueBrainz rating endpoints are public, so they pass `None`. The bool is true when the
/// rate-limit window is exhausted, so the caller must make no further API calls.
///
/// Every request is followed by the configured gap, so no two ListenBrainz or CritiqueBrainz
/// calls in a pass can land closer together than the politeness floor, whatever the surrounding
/// loop does.
fn api_get(url: &str, auth: Option<&str>, delay_ms: u64) -> Result<(String, bool), Stop> {
    let mut headers = HashMap::new();
    if let Some(auth) = auth {
        headers.insert("Authorization".to_string(), auth.to_string());
    }
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
    sleep_ms(delay_ms);
    Ok((body, exhausted))
}

/// Submits love feedback for a single recording. One feedback per request is an API constraint.
/// The bool is true when the rate-limit window is exhausted.
fn submit_love(token: &str, mbid: &str, delay_ms: u64) -> Result<(Status, bool), String> {
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
    sleep_ms(delay_ms);
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

/// Accepts only UUID-shaped MusicBrainz IDs: 32 hex digits in 8-4-4-4-12 groups. A merely
/// 36-character hex-and-dash string would be looked up on every pass and could never match.
fn is_valid_mbid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
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
    #[serde(rename = "searchResult3", default)]
    search_result3: Option<SearchResult3>,
}

/// `search3` results, grouped by the kind of entity that matched the query.
#[derive(Debug, Deserialize)]
struct SearchResult3 {
    #[serde(default)]
    artist: Vec<SearchItem>,
    #[serde(default)]
    album: Vec<SearchItem>,
    #[serde(default)]
    song: Vec<SearchItem>,
}

/// One local item found by `search3`. `starred` is absent unless the user favourited it, and
/// `userRating` is absent unless it is non-zero.
#[derive(Debug, Deserialize)]
struct SearchItem {
    #[serde(default)]
    id: String,
    #[serde(rename = "musicBrainzId", default)]
    mbid: String,
    #[serde(default)]
    starred: Option<String>,
    #[serde(rename = "userRating", default)]
    user_rating: Option<i64>,
}

/// Response from `/user/<username>`.
#[derive(Debug, Deserialize)]
struct CbUserEnvelope {
    #[serde(default)]
    user: Option<CbUser>,
}

#[derive(Debug, Deserialize)]
struct CbUser {
    #[serde(default)]
    id: String,
}

/// One page of the CritiqueBrainz review list.
#[derive(Debug, Deserialize)]
struct CbReviewPage {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    reviews: Vec<CbReview>,
}

/// One rating or review. Unknown fields are ignored by design.
#[derive(Debug, Deserialize)]
struct CbReview {
    #[serde(default)]
    entity_id: String,
    #[serde(default)]
    entity_type: String,
    #[serde(default)]
    rating: Option<i64>,
    #[serde(default)]
    last_revision: Option<CbRevision>,
}

impl CbReview {
    /// The rating the CritiqueBrainz website shows. The newest revision wins, which is also
    /// what its own list endpoint filters on; the review row's copy is only a fallback.
    fn rating(&self) -> Option<i64> {
        self.last_revision
            .as_ref()
            .and_then(|revision| revision.rating)
            .or(self.rating)
    }
}

#[derive(Debug, Deserialize)]
struct CbRevision {
    #[serde(default)]
    rating: Option<i64>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_key_encodes_exactly_what_the_rule_applies() {
        let rating_only = CbRule {
            sync_rating: true,
            favorite_at: 0,
        };
        let star_only = CbRule {
            sync_rating: false,
            favorite_at: 4,
        };
        let both = CbRule {
            sync_rating: true,
            favorite_at: 4,
        };

        // A changed rating must produce a new key, or the change would never be re-applied.
        assert_ne!(
            cb_state_key(CbEntity::ReleaseGroup, "m", 4, rating_only),
            cb_state_key(CbEntity::ReleaseGroup, "m", 5, rating_only)
        );
        // The two toggles are encoded independently.
        assert_eq!(
            cb_state_key(CbEntity::Artist, "m", 3, star_only),
            "artist:m:f0"
        );
        assert_eq!(
            cb_state_key(CbEntity::Artist, "m", 4, star_only),
            "artist:m:f1"
        );
        assert_eq!(
            cb_state_key(CbEntity::Recording, "m", 4, rating_only),
            "recording:m:r4:"
        );
        // Crossing the threshold re-applies the favourite without a rating change.
        assert_ne!(
            cb_state_key(CbEntity::Recording, "m", 3, both),
            cb_state_key(CbEntity::Recording, "m", 4, both)
        );
        // Entity type and MBID keep different items apart.
        assert_ne!(
            cb_state_key(CbEntity::Artist, "a", 5, both),
            cb_state_key(CbEntity::Recording, "a", 5, both)
        );
    }

    #[test]
    fn only_the_items_missing_the_state_are_touched() {
        let item = |id: &str, starred: Option<&str>, user_rating: Option<i64>| SearchItem {
            id: id.to_string(),
            mbid: String::new(),
            starred: starred.map(str::to_string),
            user_rating,
        };
        let items = vec![
            item("unrated-unstarred", None, None),
            item(
                "already-5-and-starred",
                Some("2024-05-05T00:00:00Z"),
                Some(5),
            ),
            item("rated-4-unstarred", None, Some(4)),
            item("rated-5-unstarred", None, Some(5)),
        ];

        let rated: Vec<&str> = pending_rating(&items, 5)
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(rated, vec!["unrated-unstarred", "rated-4-unstarred"]);

        // Favouriting is independent of the rating, so only the already-starred item is skipped.
        let starred: Vec<&str> = pending_star(&items).iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            starred,
            vec![
                "unrated-unstarred",
                "rated-4-unstarred",
                "rated-5-unstarred"
            ]
        );
    }

    #[test]
    fn cb_next_offset_advances_by_the_rows_returned_and_stops_at_the_end() {
        // A full page mid-list advances by the page size.
        assert_eq!(cb_next_offset(0, 50, 165), Some(50));
        assert_eq!(cb_next_offset(100, 50, 165), Some(150));
        // A short final page must not skip rows, and the final page exhausts the list.
        assert_eq!(cb_next_offset(150, 15, 165), None);
        assert_eq!(cb_next_offset(0, 50, 50), None);
        // An empty page ends the walk whatever total the server claims.
        assert_eq!(cb_next_offset(0, 0, 10), None);
    }

    #[test]
    fn rule_deserializes_from_the_manifest_keys() {
        // Pins the names the manifest schema exposes: renaming either silently disables the
        // feature instead of failing, because both fields are `serde(default)`.
        let rule: CbRule = serde_json::from_str(r#"{"sync_rating":true,"favorite_at":4}"#)
            .expect("manifest key shape");
        assert!(rule.sync_rating);
        assert_eq!(rule.favorite_at, 4);
        let absent: CbRule = serde_json::from_str("{}").expect("absent keys default");
        assert!(absent.is_disabled());
    }

    #[test]
    fn star_param_matches_the_subsonic_endpoint() {
        assert_eq!(CbEntity::Recording.star_param(), "id");
        assert_eq!(CbEntity::ReleaseGroup.star_param(), "albumId");
        assert_eq!(CbEntity::Artist.star_param(), "artistId");
        assert_eq!(CbEntity::ReleaseGroup.cb_name(), "release_group");
        // Navidrome reports an album's release MBID, never the release group we query by, so
        // an album id must stay out of the cross-check or every album match is discarded.
        assert!(!CbEntity::ReleaseGroup.echoes_queried_mbid());
        assert!(CbEntity::Artist.echoes_queried_mbid());
        assert!(CbEntity::Recording.echoes_queried_mbid());
    }

    #[test]
    fn rule_is_disabled_only_when_both_actions_are_off() {
        assert!(CbRule {
            sync_rating: false,
            favorite_at: 0
        }
        .is_disabled());
        assert!(!CbRule {
            sync_rating: true,
            favorite_at: 0
        }
        .is_disabled());
        assert!(!CbRule {
            sync_rating: false,
            favorite_at: 5
        }
        .is_disabled());
    }

    #[test]
    fn review_rating_prefers_the_newest_revision() {
        // Trimmed from a real `review/?review_type=rating` response.
        let page: CbReviewPage = serde_json::from_str(
            r#"{"count":165,"limit":50,"offset":0,"reviews":[
                 {"entity_id":"63f689ce-a3bb-4c6c-b468-50a21e9c77a8","entity_type":"release_group","rating":4,
                  "last_revision":{"id":18060,"rating":5,"text":null}},
                 {"entity_id":"958b7ebf-163c-4d1b-a9bc-de394a2880be","entity_type":"event","rating":4,
                  "last_revision":{"id":18096,"rating":null,"text":null}},
                 {"entity_id":"e2bb3e11-2b48-4c71-b257-5e5ba79ce270","entity_type":"recording"}]}"#,
        )
        .expect("real CritiqueBrainz page shape");

        // `count` is the total for the entity type, not the size of this page (verified against a
        // live account: count 165 across four pages), which is what the paging loop relies on.
        assert_eq!(page.count, 165);
        assert_eq!(page.reviews.len(), 3);
        assert_eq!(page.reviews[0].rating(), Some(5));
        assert_eq!(page.reviews[1].rating(), Some(4));
        assert_eq!(page.reviews[2].rating(), None);
        assert!(page.reviews[0].entity_type == CbEntity::ReleaseGroup.cb_name());
    }

    #[test]
    fn search3_response_parses_each_entity_kind() {
        let envelope: SubsonicEnvelope = serde_json::from_str(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{
                 "artist":[{"id":"ar-1","name":"A","albumCount":1,"userRating":4,
                            "musicBrainzId":"2ef8e9b2-5f7d-4cfb-899a-bc4544755371"}],
                 "album":[{"id":"al-1","name":"B","songCount":2,"duration":3,
                           "created":"2024-01-01T00:00:00Z","starred":"2024-05-05T00:00:00Z",
                           "musicBrainzId":"63f689ce-a3bb-4c6c-b468-50a21e9c77a8"}],
                 "song":[]}}}"#,
        )
        .expect("real Subsonic search3 shape");

        let result = envelope
            .response
            .search_result3
            .expect("searchResult3 present");
        assert_eq!(result.artist.first().and_then(|a| a.user_rating), Some(4));
        assert_eq!(
            result.artist.first().map(|a| a.starred.is_none()),
            Some(true)
        );
        assert_eq!(
            result.album.first().and_then(|a| a.starred.as_deref()),
            Some("2024-05-05T00:00:00Z")
        );
        assert!(result.song.is_empty());
    }
}
