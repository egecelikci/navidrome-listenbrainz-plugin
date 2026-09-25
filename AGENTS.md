# Agent Instructions: Navidrome ListenBrainz Sync Plugin

You are an expert software engineer specializing in high-reliability, single-file Rust binaries targeting WebAssembly (`wasm32-wasip1`) via the Extism plugin framework, using Navidrome's Rust PDK. Your goal is to deliver highly readable, deterministic code optimized for a non-Rust developer to understand and manage.

## 0. Source of Truth (Read Before Coding)
- **Navidrome plugin platform:** `https://github.com/navidrome/navidrome/blob/master/plugins/README.md` — authoritative manifest schema, capability functions, host services, and permissions. Confirm exact names/signatures here before calling them.
- **Rust PDK:** `https://github.com/navidrome/navidrome/blob/master/plugins/pdk/rust/README.md`, with crates `nd-pdk` (umbrella), `nd-pdk-host` (host wrappers, `nd_pdk::host::*`), `nd-pdk-capabilities` (traits/macros), `nd-pdk-types` (`SongRef`, `Track`). Read the actual source when a wrapper's behavior matters.
- **ListenBrainz data shapes:** `https://github.com/kellnerd/listenbrainz-ts` — `api_types.ts` and `listen.ts` are authoritative for listens, track metadata, MBID mappings, and API responses.
- **ListenBrainz HTTP API:** `https://listenbrainz.readthedocs.io/` — authoritative for endpoints/payloads the TS files do not define (feedback). Do not invent endpoints; confirm against the docs.
- **Sample export data (read-only, deferred use):** a ListenBrainz data export (Settings → Export) is a ZIP whose shape informs the sidecar backfill (§8) and this phase's sync types. If a local sample is provided, treat it as read-only and never modify it:
  - `user.json` — `user_id` (integer in practice) and `username`.
  - `listens/<year>/<month>.jsonl` — one `InsertedListen` per line, including `track_metadata.mbid_mapping.recording_mbid`.
  - `feedback.jsonl` — `score`, `created`, `recording_mbid`, `recording_msid` (may be `null`).
  - `pinned_recording.jsonl` — `created`, `pinned_until`, `blurb_content` (may be `null`), `recording_mbid`, `recording_msid` (may be `null`).
- **Pristine handling:** Reference files are external and read-only. Never copy, vendor, translate, import, or modify them. Read them to derive exact field names, nullability, and nesting, then mirror the shape in Rust `serde` types. If a reference file and an assumption disagree, the reference file wins; if the *sample data* disagrees with the TS types on nullability, the sample wins (see §3).
- **Missing references:** If any reference URL or local path is unavailable, stop and ask the user. Never guess a schema, endpoint, or host-function signature.

## 1. Mission & Scope

### Vocabulary (binding — use these words in code, config, logs, and docs)

Three systems, three words for the same gestures. Mixing them is the most common source of confusion in this project, so they are fixed here:

| System | Word | Means |
| --- | --- | --- |
| ListenBrainz | **Love** | the binary feedback (`score: 1`) on a recording |
| Navidrome / OpenSubsonic | **Favorite** | the heart on a track, album, or artist |
| Navidrome / OpenSubsonic | **Rating** | the 1–5 value the UI draws as stars |
| CritiqueBrainz | **Rating** | the 1–5 value the site draws as stars |

So the inbound half is **Love → Favorite**, outbound is **Favorite → Love**, and both rating syncs are **Rating → Rating**.

Never use "star" or "heart" for the Navidrome Favorite in prose, log messages, config text, or new identifiers — they are synonyms for the same thing and are what made an early draft of this spec ambiguous. The only surviving uses of "star" are **OpenSubsonic protocol names**, which must stay verbatim because they are an external contract: the `star` endpoint, its `artistId`/`albumId`/`id` parameters, the `starred2` download list and `getStarred2` method, the `starred` response field, the `starred` annotation column, the matcher's PDK `Track.starred` field, and this codebase's mirrors of that payload (`Starred2`, `StarredSong`). When one of those appears, say which it is (e.g. "mark as a Favorite via the `star` endpoint"), so a reader never has to guess whether "star" is the protocol or our domain word.

**Current deliverable (Phase 1): two-way Love sync** between Navidrome Favorites and ListenBrainz Loves. It is not the whole product, but it is the first thing to build.
- **Inbound:** A new loved recording on ListenBrainz → mark the matching local track as a Favorite in Navidrome.
- **Outbound:** A newly favorited track in Navidrome → submit Love feedback for it to ListenBrainz.
- **Convergent and incremental:** A scheduled pass reconciles both sides toward the union of both love sets, then only does delta work on later passes.

**Already shipped on top of Phase 1: one-way CritiqueBrainz rating sync** (§7, last subsection). It reads public CritiqueBrainz ratings and applies them to the matching Navidrome artist, album, or song, optionally marking items at or above a configured rating as Favorites. It is inbound-only and does not write to CritiqueBrainz. Do not remove it as out of scope — it is implemented, configured through `cb_sync_*`, and needs the `critiquebrainz.org` HTTP host and the `search3`/`setRating` Subsonic calls that §10 records.

**Out of scope:** historical play backfill. ListenBrainz forbids paging its listens endpoint for a full history and this plugin cannot decompress the export archive, so the import is a separate sidecar, not a deferred plugin feature (§8).

**Explicitly out of scope for now:** live-play scrobbling to ListenBrainz, hate (`-1`) propagation, and removal propagation (un-favorite/un-love) — see §7. No speculative capabilities, daemons, or UI.

**API citizenship is a hard requirement:** never flood ListenBrainz. Every request must carry a contactable `User-Agent` (`AppName/<version> ( contact )`) or it may be blocked. The API allows **at most one request per second** per client application, so `request_delay_ms` is floored at 1000 and applied between *all* API calls in a pass, not just writes. Read the `X-RateLimit-Remaining` / `X-RateLimit-Reset-In` response headers, stop the pass when the window is exhausted or on 429/5xx, and never `Retry-After`-spin. A 450k-listen batch job must never be pointed at their API (this is why history import is a separate sidecar that uses the export API, not this plugin).

## 2. Structural Constraints
- **Single File Only:** All models, structs, config blocks, and execution loops live in one `src/lib.rs`. No extra modules, files, folders, or custom abstraction traits. A `Cargo.toml` and `manifest.json` are required packaging files, not code.

## 3. Type Fidelity (`serde` Strictness)
- **Explicit structs over loose values:** Model every ListenBrainz payload as a named `serde` struct. Never fall back to `serde_json::Value` for a field whose shape is known.
- **Mirror the TypeScript:** Use field names, casing, and nesting from `listen.ts` / `api_types.ts`. Required fields → concrete types; optional (`?`) → `Option<T>`; mirror unions (`T | U`, `0 | 1`) and arrays faithfully.
- **Strict on known fields, tolerant of extras:** Do **not** use `deny_unknown_fields` on inbound ListenBrainz payloads — the TS types intentionally allow unspecified extra fields, and rejecting them would violate the no-panic guarantee. Ignore extras safely.
- **Concrete data overrides TS nullability:** The sample exports prove the TS types are optimistic. Model these as `Option<T>` even where TS says required: `feedback.jsonl.recording_msid` (null observed), `user.json.user_id` (integer, not the TS `string`), and `track_metadata.mbid_mapping` (may be absent). In the feedback API payloads, `recording_mbid` may also be absent — skip such records.
- **Unions get explicit handling:** `AdditionalTrackInfo.tracknumber` is `number | string`. Model with an untagged enum, never with unchecked casts.
- **Validate after deserialization:** A successful parse is a floor, not a ceiling. Re-validate timestamps (positive Unix seconds) and MBIDs (non-empty, UUID-shaped) before use. Skip malformed records with a warning.

## 4. Safety & "No-Panic" Guarantee
- **Strictly No Panics:** No `.unwrap()`, `.expect()`, `panic!()`, `todo!()`, `unreachable!()`, or direct indexing that can crash (`list[0]`, `s[..n]`). Use `get`, `first`, `if let`, `match`, and `?` on `Result`/`Option`.
- **Explicit flow control:** Prefer `if let`, `match`, and fallbacks like `.unwrap_or_default()`.
- **Graceful fault tolerance:** A malformed record, bad config value, failed match, or failed HTTP call must log a warning via the host logger and `continue`/`return`. One bad record never halts the pass.

## 5. Idempotency & Sync State
- **Single "settled" set:** A recording is *settled* once both sides agree (it is a Favorite in Navidrome and Loved on ListenBrainz) or once we have just propagated it. Store settled recording MBIDs in KVStore under one prefix, e.g. `synced:<recording_mbid>`.
- **A second, differently-shaped prefix for CritiqueBrainz:** `cbsynced:<nd_username>:<entity>:<mbid>[:r<rating>][:f<0|1>]`. These keys encode the *state that was applied* rather than "both sides agree", because a CritiqueBrainz rating is not binary: a changed rating must produce a different key so it is re-applied. Only the actions the rule enables are encoded, so a favorite-only rule is not re-run when a rating it ignores changes. Keep the two prefixes separate — the ListenBrainz set is matched on a bare recording MBID, so mixing shapes into one prefix would corrupt that lookup.
- **Per pass:** Load the set once with `nd_pdk::host::kvstore::list("synced:")` into an in-memory set; add entries with `kvstore::set` only after a side effect has actually succeeded.
- **Why one set, not two:** A single set prevents ping-pong. When inbound marks a track as a Favorite, it is already Loved on LB, so recording it as settled stops outbound from re-loving it; when outbound Loves a track, it was already a Favorite locally, so settled stops inbound from re-favoriting it.
- **Never settle an unmatched recording:** If a loved MBID has no local track yet, do not store it — a later library scan should still be able to pick it up.
- **Keys:** `kvstore` keys must be ≤256 bytes UTF-8. Size math: ~50 B/key; assume up to ~50k loves/favorites → ~2.5 MB, so set `permissions.kvstore.maxSize` (e.g. `"32MB"`) rather than relying on the 1 MB default.

## 6. Platform Constraints (verified — do not assume otherwise)
- **There is no "track starred" event exposed to plugins** (OpenSubsonic's name for the Favorite action is "star"). The Scrobbler capability delivers *playback* events only. Outbound Love sync must **poll** `getStarred2` and diff against state.
- **Plugins cannot read arbitrary host paths.** The plugin storage mount `/storage` is **read-write** (it needs the `storage` permission, which Phase 1 and the CritiqueBrainz sync deliberately do not declare), and `/libraries/<id>` is read-only unless the administrator grants write access. Neither is needed today.
- **Every plugin function call is killed at 30 s** (`plugins/manager.go`: `defaultTimeout = 30 * time.Second`, applied in `manager_loader.go`). One scheduled pass is one such call, so the whole pass — every user, both halves — must fit. At the `request_delay_ms` floor of 1000 ms that is **≤ ~25 API calls per pass**, which is the real bound on any paging loop and the reason long backfills must be spread across passes rather than walked in one.
- **HTTP response bodies are capped at 10 MB and truncated _silently_** (`plugins/host_httpclient.go`: `httpClientMaxResponseBodyLen = 10 * 1024 * 1024`, read through `io.ReadAll(io.LimitReader(...))`). The plugin sees HTTP 200 and a short body with no error. Default per-request timeout is 10 s (`timeoutMs` can raise it, but never past the 30 s call budget). Never design a feature that assumes a response body arrived whole.
- **Phase 1 injects no plays**, so it cannot trigger the scrobble echo described in §8. Do not add the Scrobbler capability in this phase.

## 7. Two-Way Love Sync (Phase 1 — implement this)
Run one reconciliation pass inside the Scheduler callback (§10), once per configured user link. Read config first (§9); if `dry_run`, log intended actions and make no writes.

0. **Per-user linking:** Config is a `user_links` array of `{navidrome_username, listenbrainz_token}`. For each link: skip it unless the Navidrome user is assigned to the plugin (`host::users::get_users`), then derive the ListenBrainz username from the token with `GET https://api.listenbrainz.org/1/validate-token` (header `Authorization: Token <token>`; response `{valid, user_name}`). A separate ListenBrainz username is never configured.
1. **Inbound (ListenBrainz → Navidrome), if `sync_inbound`:**
   - Page `GET https://api.listenbrainz.org/1/feedback/user/<lb_username>/get-feedback?score=1&count=1000&offset=<n>` with header `Authorization: Token <token>` (confirm the exact path/params in the LB docs — §0). Read `count`, `offset`, and `total_count` to page; stop when the page is empty or `offset >= total_count`.
   - For each feedback entry with a non-empty `recording_mbid` not already settled: resolve it with `nd_pdk::host::matcher::match_songs(vec![SongRef { mbid, .. }], MatchOptions { username })`. The returned `Track` carries per-user annotations (`starred`), so skip if it is already a Favorite.
   - Collect matched media file IDs and mark them as Favorites in **one batched call**: `nd_pdk::host::subsonicapi::call("star?id=<id>&id=<id>&u=<navidrome_username>")` (the `star` endpoint is the Favorite action). Mark each MBID settled only after the call succeeds.
2. **Outbound (Navidrome → ListenBrainz), if `sync_outbound`:**
   - Fetch all Favorited songs: `nd_pdk::host::subsonicapi::call("getStarred2?u=<navidrome_username>")`; the JSON is `subsonic-response.starred2.song[]`, and the recording MBID is the `musicBrainzId` field on each song (verified: `childFromMediaFile` sets `MusicBrainzId` from `mbzRecordingID`).
   - For each Favorited song with a non-empty `musicBrainzId` not already settled: submit Love via `nd_pdk::host::http::send` → `POST https://api.listenbrainz.org/1/feedback/recording-feedback`, header `Authorization: Token <token>`, body `{"recording_mbid": "...", "score": 1}`. **One feedback per request** (API constraint). Mark settled after a 2xx.
3. **Politeness (mandatory):** send the `User-Agent` header on every ListenBrainz request; keep at least `request_delay_ms` (floored at 1000 ms) between all API calls in a pass; cap work per user per pass with `max_per_run`; read `X-RateLimit-Remaining` and stop the pass once it hits 0; on HTTP 429 or 5xx stop the pass (do not retry in a tight loop) and let the next scheduled pass continue. Never `Retry-After`-spin.
4. **Removals and hates are not propagated.** Un-favoriting locally or un-loving on LB is intentionally left untouched — we must not clobber a change the plugin did not make. If this is ever added, it must be snapshot-guarded to only revert what the plugin itself set, and it must be an explicit, opt-in feature.

### CritiqueBrainz rating sync (implemented — one-way, read-only)

Runs inside the same scheduled pass, once per enabled `cb_sync_*` rule. Nothing is ever written to CritiqueBrainz, so there is no OAuth flow and no token: the ratings sit on public reviews. The CritiqueBrainz username is inherited from the ListenBrainz link and may be overridden with `critiquebrainz_username`.

0. **Resolve, then page.** `GET {CB_ROOT}/user/<username>` → `user.id`, once per pass; log which account was bound, because inheriting the username can bind a different account than the administrator expects. Then `GET {CB_ROOT}/review/?user_id=<id>&entity_type=<type>&review_type=rating&limit=50&offset=<n>`, reading `count` (the total for that entity type, **not** the page size). Three API facts are load-bearing and were verified live: `limit` is capped at 50 (51 → 400); `entity_type=musicbrainz` is **rejected** with 400, so `artist` / `release_group` / `recording` are paged separately; and `?username=` is silently ignored, so only `user_id` filters.
1. **Mapping and resolution.** `artist` → Navidrome artist, `release_group` → Navidrome album, `recording` → Navidrome song. Resolve each `entity_id` with one `subsonicapi::call("search3?query=<mbid>&artistCount=N&albumCount=N&songCount=N")`: a UUID query is matched against Navidrome's MBID columns directly (it bypasses full-text search), so no library scan is needed. **One entity can span several local items** — a release group may hold duplicate albums, a recording may sit on many compilations — and every match receives the same state (fan-out).
2. **Two independent actions per rule.** `sync_rating` copies the rating (`setRating?id=…&rating=N`, one id per call); `favorite_at` ≥ 1 additionally marks items rated at least that high as Favorites (`star?artistId=…` / `albumId=…` / `id=…`, several ids per call). Both off ⇒ that entity type is ignored entirely, which is how rating-only, favorite-only, both, and neither are all expressed. Never un-favorite and never clear a rating.
3. **Never cross-check an album's `musicBrainzId`.** Navidrome reports the *release* MBID in that field while CritiqueBrainz rates *release groups*, so the two differ by design; the `search3` MBID match is authoritative for albums. Artists and songs do echo back the queried MBID and may be compared.
4. **Budget counts changes attempted, not rows.** Skipping an already-settled rating is free, and so is a rating with no local match, so the walk always reaches the end of the list. Do **not** "fix" the cost by charging settled rows to the budget: that pins every pass to the same first page and permanently starves the tail. The list is paged in full every pass (one request per 50 ratings per enabled type), which §12's README documents as a real cost.
5. **State.** `cbsynced:` keys, per §5. A rating with no local match is never settled so a later scan can still pick it up; a release group that *has* been settled is not re-examined, so a duplicate album appearing in a later scan is not marked as a Favorite until the rating changes. Documented, not fixed — re-checking would cost a `search3` per rating per pass.
6. **Known ceiling, not yet fixed.** Because the budget counts changes rather than rows, one pass walks the entity's whole rating list. The §6 30 s per-call kill therefore truncates the walk for an account with more than roughly **25 pages (~1250 unsettled ratings) in one entity type**, and the next pass restarts from offset 0. Settled rows are skipped without a lookup, so this converges for a rating history that is mostly matched — but a *permanent* backlog of more than ~1250 unmatched ratings ahead of newly added ones would starve the tail. The fix, when a library actually reaches that size, is a KVStore page cursor plus a per-pass page cap; it is deliberately not implemented while the largest real account is 4 pages.

## 8. Historical Backfill (out of scope — a separate sidecar)
Play-history import is **not a plugin feature**. It lives in a sidecar project (Forgejo `egecelikci/ndlbbf`), because every path that would put it here is blocked by something verified:

- **The listens endpoint must not be paged for a full history.** The API docs: "Do not scrape or paginate through the user's entire history using the listens endpoint. Use that endpoint for recent listens, bounded queries, and incremental updates after importing an export." `count` caps at 1000, `min_ts` and `max_ts` are mutually exclusive, and the docs put automated paging at "roughly ten pages or fewer" (~10k listens). The earlier "prefer `listens` paging" design is withdrawn.
- **The export API is the only sanctioned full-history path, and this plugin cannot consume it.** `POST /1/export/` (optional inclusive `{start_time, end_time}` in UNIX seconds; `{}` or no body for everything) → poll `GET /1/export/{id}` (≥30 s, ramping to ~120 s; one pending export per user, so a second POST is a 400) → `GET /1/export/{id}/download`, an `application/zip` whose entries are DEFLATE-compressed (`background/export.py` uses `ZIP_DEFLATED`). No decompressor is reachable: there is no host service for it, §10 forbids new crates, and `std` has no inflate. A readable archive would arrive corrupt anyway, through the 10 MB silent truncation in §6.
- **A sidecar has none of those limits:** it unzips with `zipfile`, streams, and takes as long as it needs.

Verified facts the sidecar must keep, so nobody re-derives them:

- **`scrobble` accepts many listens per call.** `server/subsonic/media_annotation.go` `Scrobble` reads repeated `id` plus a repeated `time` of equal count (a mismatch is an error), parsed by `time.UnixMilli` (`utils/req/req.go`), so `time` is **milliseconds** and one call carries a whole batch with per-listen timestamps.
- **Recording the play is not gated; forwarding is.** `core/scrobbler/play_tracker.go`: `Submit` always calls `incPlay` (play counts and history), and calls `dispatchScrobble` only when `player.ScrobbleEnabled`. Turning scrobbling off for the sidecar's player therefore suppresses the ListenBrainz echo — which would otherwise duplicate every imported play, since ListenBrainz dedupes on `(listened_at, user_id, recording_msid)` and Navidrome re-derives a different MSID — without losing the imported play.
- **That player is identifiable.** New players default to `ScrobbleEnabled: true` and are named `<client> [<userAgent>]` from the Subsonic `c` parameter (`core/players.go`), so the sidecar's own `c` value gives the administrator a row to toggle off. Deleting the row brings it back with scrobbling on.
- **Deduplicate on `(listened_at, recording_mbid)`,** persisted locally after each accepted batch. Both fields come from `listens/<year>/<month>.jsonl` under `track_metadata.mbid_mapping.recording_mbid`, which may be absent. Two traps in that archive: `listened_at` is emitted without the `::integer` cast `inserted_at` has, so it may be a fractional JSON number, and `feedback.jsonl` carries **all** scores, including `-1`.

## 9. Configuration Blueprint
Configuration is JSON Schema (draft-07) + optional JSONForms `uiSchema` in the manifest, surfaced in Navidrome's UI. Read via `nd_pdk::host::config::{get, get_int, keys}` (or `pdk.GetConfig`). **Config is read-only to the plugin.** Phase 1 fields:

```json
{
  "config": {
    "schema": {
      "type": "object",
      "properties": {
        "user_links": {
          "type": "array",
          "title": "Linked users",
          "minItems": 1,
          "items": {
            "type": "object",
            "properties": {
              "navidrome_username": { "type": "string", "title": "Navidrome username", "minLength": 1 },
              "listenbrainz_token": { "type": "string", "title": "ListenBrainz token", "minLength": 1 }
              "critiquebrainz_username": {
                "type": "string",
                "title": "CritiqueBrainz username",
                "description": "Optional. Defaults to the ListenBrainz username."
              }
            },
            "required": ["navidrome_username", "listenbrainz_token"]
          }
        },
        "cb_sync_artists": {
          "type": "object",
          "default": { "sync_rating": false, "favorite_at": 0 },
          "properties": {
            "sync_rating": { "type": "boolean", "default": false },
            "favorite_at": { "type": "integer", "default": 0, "minimum": 0, "maximum": 5 }
          }
        },
        "cb_sync_albums": {
          "type": "object",
          "default": { "sync_rating": false, "favorite_at": 0 },
          "properties": {
            "sync_rating": { "type": "boolean", "default": false },
            "favorite_at": { "type": "integer", "default": 0, "minimum": 0, "maximum": 5 }
          }
        },
        "cb_sync_recordings": {
          "type": "object",
          "default": { "sync_rating": false, "favorite_at": 0 },
          "properties": {
            "sync_rating": { "type": "boolean", "default": false },
            "favorite_at": { "type": "integer", "default": 0, "minimum": 0, "maximum": 5 }
          }
        },
        "sync_inbound": { "type": "boolean", "title": "Favorite tracks loved on ListenBrainz", "default": true },
        "sync_outbound": { "type": "boolean", "title": "Love tracks favorited in Navidrome", "default": true },
        "sync_schedule": { "type": "string", "title": "Sync cron expression", "default": "*/30 * * * *" },
        "dry_run": { "type": "boolean", "title": "Dry run (report only, write nothing)", "default": true },
        "batch_size": { "type": "integer", "title": "Items per matcher batch", "default": 100, "minimum": 1, "maximum": 1000 },
        "request_delay_ms": { "type": "integer", "title": "Delay between ListenBrainz API calls (ms, min 1000)", "default": 1000, "minimum": 1000 },
        "max_per_run": { "type": "integer", "title": "Max changes per user per pass", "default": 500, "minimum": 1 }
      },
      "required": ["user_links"]
    },
    "uiSchema": {
      "type": "VerticalLayout",
      "elements": [
        {
          "type": "Control",
          "scope": "#/properties/user_links",
          "options": {
            "elementLabelProp": "navidrome_username",
            "detail": {
              "type": "VerticalLayout",
              "elements": [
                { "type": "Control", "scope": "#/properties/navidrome_username" },
                { "type": "Control", "scope": "#/properties/listenbrainz_token", "options": { "format": "password" } }
              ]
            }
          }
        },
        { "type": "Control", "scope": "#/properties/sync_inbound" },
        { "type": "Control", "scope": "#/properties/sync_outbound" },
        { "type": "Control", "scope": "#/properties/sync_schedule" },
        { "type": "Control", "scope": "#/properties/dry_run" },
        { "type": "Control", "scope": "#/properties/batch_size" },
        { "type": "Control", "scope": "#/properties/request_delay_ms" },
        { "type": "Control", "scope": "#/properties/max_per_run" }
      ]
    }
  }
}
```

- **UI schema must be a real JSONForms layout:** Navidrome passes `uiSchema` straight to JSONForms (`ui/src/plugin/SchemaConfigEditor.jsx`), so a bare property map is invalid and renders "No applicable renderer found." Always use a root `VerticalLayout` of `Control`s; array item fields are configured via `options.detail`. The password mask is `options.format: "password"` (the custom renderer checks `format`, not `ui:widget`).
- **Non-string config values arrive JSON-encoded:** Navidrome stores config as `map[string]string`, marshalling arrays/objects to JSON (`plugins/manager_loader.go`). Parse `user_links` with `serde_json::from_str`.
- **Defaults must be safe:** `dry_run` defaults to `true`; a first run reports what it would do and writes nothing until the administrator opts in.
- **No backfill config:** play-history import is not a plugin feature (§8), so there are no `import_path` / `execute_import` / `import_start_year` / `import_end_year` fields and no `storage` permission.

## 10. Platform Contract (capabilities, host services, permissions)
Phase 1 uses only these. Confirm names/signatures against the PDK source before calling.
- **Capabilities:** Lifecycle (`nd_on_init`; Rust `nd_pdk::lifecycle::InitProvider` + `register_lifecycle_init!`) registers the recurring sync and schedules a one-shot reconciliation ~5 s after load (so a fresh install or update does not wait for the next cron tick); Scheduler callback (`nd_scheduler_callback`; Rust `nd_pdk::scheduler::CallbackProvider`) runs the pass. No Scrobbler, no TaskWorker.
- **Host services (Rust paths):** `host::http::send` (the only network path; Extism's built-in HTTP is disabled) — ListenBrainz feedback and token endpoints, plus the public CritiqueBrainz user and review endpoints; `host::subsonicapi::call` (star / getStarred2 / search3 / setRating); `host::matcher::match_songs`; `host::kvstore::{list, set, has, get_many}`; `host::scheduler::schedule_recurring`; `host::config::{get, get_int, keys}`; `host::users::{get_users, get_admins}`.
- **Manifest permissions (exact keys):** `http` (`requiredHosts: ["api.listenbrainz.org", "critiquebrainz.org"]`), `users`, `subsonicapi` (requires `users`), `matcher` (requires `library`), `library`, `kvstore` (`maxSize`), `scheduler`. Each needs a `reason`. Skeleton:
  ```json
  {
    "name": "Navidrome ListenBrainz Sync",
    "author": "egecelikci",
    "version": "1",
    "description": "Two-way love sync with ListenBrainz",
    "config": { "schema": { }, "uiSchema": { } },
    "permissions": {
      "http": { "reason": "Call the ListenBrainz API", "requiredHosts": ["api.listenbrainz.org", "critiquebrainz.org"] },
      "users": { "reason": "Act for the users assigned to this plugin" },
      "subsonicapi": { "reason": "Read favorited tracks, search for MBID matches, and favorite or rate matched items" },
      "matcher": { "reason": "Resolve recording MBIDs to local tracks" },
      "library": { "reason": "Required by the matcher permission" },
      "kvstore": { "reason": "Store settled sync state", "maxSize": "32MB" },
      "scheduler": { "reason": "Run the periodic sync pass" }
    }
  }
  ```
- **Dependencies:** `nd-pdk` (path to `pdk/rust/nd-pdk`), `extism-pdk = "1.2"`, `serde`, `serde_json`. No other crates. `[lib] crate-type = ["cdylib"]`, `edition = "2021"`.

## 11. Security & Data Safety
- **Secrets:** Treat the ListenBrainz token as a secret. Use the `password` UI widget, never log it, never echo it in errors, never write it to KVStore or `/storage`.
- **Least privilege:** Declare only the permissions the features use, each with a clear `reason`. Restrict HTTP to the ListenBrainz host.
- **User scoping:** Only operate on users assigned to the plugin; never touch other users' data.
- **Read-only library:** Never modify, move, or delete media files.
- **No data loss:** Additions only; never un-favorite, un-love, or clear feedback (see §7.4). Writes are idempotent by the settled set (§5).
- **Rate limiting:** Honor 429/`Retry-After`; cap per-pass work; avoid unbounded retries.
- **No clank-slop:** No dead code, placeholder `TODO`s, speculative abstractions, commented-out blocks, or comments that merely restate code. Comments explain *why*. Deterministic behavior only.

## 12. Build & Test
```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
zip -j navidrome-listenbrainz-plugin.ndp manifest.json target/wasm32-wasip1/release/plugin.wasm
navidrome plugin validate navidrome-listenbrainz-plugin.ndp
```
- The plugin ID is the `.ndp` filename (minus extension); it determines the storage/KV paths, so keep the filename stable.
- Verify the parser against real ListenBrainz feedback JSON before touching live data; run with `dry_run = true` first.

## 13. Commit Messages
- **Conventional Commits, lowercase:** every commit is `<type>[optional scope]: <description>` — e.g. `feat(critiquebrainz): one-way rating sync`. Types are lowercase (`feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `perf`, `build`, `ci`); a scope is an optional lowercase noun in parentheses; the description is lowercase, imperative, and has no trailing period.
- **Description only — no body, no footers:** the description is the entire message. Never write a body, a bullet list, `BREAKING CHANGE:`, or any other trailer. If a change needs explaining, it belongs in a code comment, a doc, or this file — not in the commit.
- **Why:** one grep-friendly line per commit keeps history readable and `git log --oneline` complete; the rationale for a design decision must be findable in the source, not only in `git show`.

## 14. Documentation Layout
- **Rustdoc over JSDoc:** Document all functions and struct declarations with `///` or file-level `//!`.
- **No verbose type comments:** Do not restate types in comments (e.g. avoid `@param {string} mbid`). Let the static types declare themselves; comments describe intent and logic paths.
