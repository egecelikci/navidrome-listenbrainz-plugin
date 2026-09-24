# Agent Instructions: Navidrome ListenBrainz Sync Plugin

You are an expert software engineer specializing in high-reliability, single-file Rust binaries targeting WebAssembly (`wasm32-wasip1`) via the Extism plugin framework, using Navidrome's Rust PDK. Your goal is to deliver highly readable, deterministic code optimized for a non-Rust developer to understand and manage.

## 0. Source of Truth (Read Before Coding)
- **Navidrome plugin platform:** `https://github.com/navidrome/navidrome/blob/master/plugins/README.md` — authoritative manifest schema, capability functions, host services, and permissions. Confirm exact names/signatures here before calling them.
- **Rust PDK:** `https://github.com/navidrome/navidrome/blob/master/plugins/pdk/rust/README.md`, with crates `nd-pdk` (umbrella), `nd-pdk-host` (host wrappers, `nd_pdk::host::*`), `nd-pdk-capabilities` (traits/macros), `nd-pdk-types` (`SongRef`, `Track`). Read the actual source when a wrapper's behavior matters.
- **ListenBrainz data shapes:** `https://github.com/kellnerd/listenbrainz-ts` — `api_types.ts` and `listen.ts` are authoritative for listens, track metadata, MBID mappings, and API responses.
- **ListenBrainz HTTP API:** `https://listenbrainz.readthedocs.io/` — authoritative for endpoints/payloads the TS files do not define (feedback). Do not invent endpoints; confirm against the docs.
- **Sample export data (read-only, deferred use):** a ListenBrainz data export (Settings → Export) is a ZIP whose shape informs the deferred backfill (§8) and this phase's sync types. If a local sample is provided, treat it as read-only and never modify it:
  - `user.json` — `user_id` (integer in practice) and `username`.
  - `listens/<year>/<month>.jsonl` — one `InsertedListen` per line, including `track_metadata.mbid_mapping.recording_mbid`.
  - `feedback.jsonl` — `score`, `created`, `recording_mbid`, `recording_msid` (may be `null`).
  - `pinned_recording.jsonl` — `created`, `pinned_until`, `blurb_content` (may be `null`), `recording_mbid`, `recording_msid` (may be `null`).
- **Pristine handling:** Reference files are external and read-only. Never copy, vendor, translate, import, or modify them. Read them to derive exact field names, nullability, and nesting, then mirror the shape in Rust `serde` types. If a reference file and an assumption disagree, the reference file wins; if the *sample data* disagrees with the TS types on nullability, the sample wins (see §3).
- **Missing references:** If any reference URL or local path is unavailable, stop and ask the user. Never guess a schema, endpoint, or host-function signature.

## 1. Mission & Scope
**Current deliverable (Phase 1): two-way "loved track" sync** between Navidrome stars and ListenBrainz loves. It is not the whole product, but it is the first thing to build.
- **Inbound:** A new loved recording on ListenBrainz → star the matching local track in Navidrome.
- **Outbound:** A newly starred track in Navidrome → submit love feedback for it to ListenBrainz.
- **Convergent and incremental:** A scheduled pass reconciles both sides toward the union of both love sets, then only does delta work on later passes.

**Deferred:** historical play backfill is a future feature. Its verified design is preserved in §8 so we do not lose it — **do not implement it now**.

**Explicitly out of scope for now:** live-play scrobbling to ListenBrainz, hate (`-1`) propagation, and removal propagation (un-star/un-love) — see §7. No speculative capabilities, daemons, or UI.

**API citizenship is a hard requirement:** never flood ListenBrainz. Every request must carry a contactable `User-Agent` (`AppName/<version> ( contact )`) or it may be blocked. The API allows **at most one request per second** per client application, so `request_delay_ms` is floored at 1000 and applied between *all* API calls in a pass, not just writes. Read the `X-RateLimit-Remaining` / `X-RateLimit-Reset-In` response headers, stop the pass when the window is exhausted or on 429/5xx, and never `Retry-After`-spin. A 450k-listen batch job must never be pointed at their API (this is why backfill is deferred and gated).

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
- **Single "settled" set:** A recording is *settled* once both sides agree (it is starred in Navidrome and loved on ListenBrainz) or once we have just propagated it. Store settled recording MBIDs in KVStore under one prefix, e.g. `synced:<recording_mbid>`.
- **Per pass:** Load the set once with `nd_pdk::host::kvstore::list("synced:")` into an in-memory set; add entries with `kvstore::set` only after a side effect has actually succeeded.
- **Why one set, not two:** A single set prevents ping-pong. When inbound stars a track, it is already loved on LB, so recording it as settled stops outbound from re-loving it; when outbound loves a track, it was already starred locally, so settled stops inbound from re-starring it.
- **Never settle an unmatched recording:** If a loved MBID has no local track yet, do not store it — a later library scan should still be able to pick it up.
- **Keys:** `kvstore` keys must be ≤256 bytes UTF-8. Size math: ~50 B/key; assume up to ~50k loves/starred → ~2.5 MB, so set `permissions.kvstore.maxSize` (e.g. `"32MB"`) rather than relying on the 1 MB default.

## 6. Platform Constraints (verified — do not assume otherwise)
- **There is no "track starred" event exposed to plugins.** The Scrobbler capability delivers *playback* events only. Outbound like-sync must **poll** `getStarred2` and diff against state.
- **Plugins cannot read arbitrary host paths.** The only readable directories are the plugin storage mount `/storage` and (optionally, read-only) `/libraries/<id>`. Phase 1 does not need either.
- **Phase 1 injects no plays**, so it cannot trigger the scrobble echo described in §8. Do not add the Scrobbler capability in this phase.

## 7. Two-Way Loved-Track Sync (Phase 1 — implement this)
Run one reconciliation pass inside the Scheduler callback (§10), once per configured user link. Read config first (§9); if `dry_run`, log intended actions and make no writes.

0. **Per-user linking:** Config is a `user_links` array of `{navidrome_username, listenbrainz_token}`. For each link: skip it unless the Navidrome user is assigned to the plugin (`host::users::get_users`), then derive the ListenBrainz username from the token with `GET https://api.listenbrainz.org/1/validate-token` (header `Authorization: Token <token>`; response `{valid, user_name}`). A separate ListenBrainz username is never configured.
1. **Inbound (ListenBrainz → Navidrome), if `sync_inbound`:**
   - Page `GET https://api.listenbrainz.org/1/feedback/user/<lb_username>/get-feedback?score=1&count=1000&offset=<n>` with header `Authorization: Token <token>` (confirm the exact path/params in the LB docs — §0). Read `count`, `offset`, and `total_count` to page; stop when the page is empty or `offset >= total_count`.
   - For each feedback entry with a non-empty `recording_mbid` not already settled: resolve it with `nd_pdk::host::matcher::match_songs(vec![SongRef { mbid, .. }], MatchOptions { username })`. The returned `Track` carries per-user annotations (`starred`), so skip if it is already starred.
   - Collect matched media file IDs and star them in **one batched call**: `nd_pdk::host::subsonicapi::call("star?id=<id>&id=<id>&u=<navidrome_username>")`. Mark each MBID settled only after the call succeeds.
2. **Outbound (Navidrome → ListenBrainz), if `sync_outbound`:**
   - Fetch all starred songs: `nd_pdk::host::subsonicapi::call("getStarred2?u=<navidrome_username>")`; the JSON is `subsonic-response.starred2.song[]`, and the recording MBID is the `musicBrainzId` field on each song (verified: `childFromMediaFile` sets `MusicBrainzId` from `mbzRecordingID`).
   - For each starred song with a non-empty `musicBrainzId` not already settled: submit love via `nd_pdk::host::http::send` → `POST https://api.listenbrainz.org/1/feedback/recording-feedback`, header `Authorization: Token <token>`, body `{"recording_mbid": "...", "score": 1}`. **One feedback per request** (API constraint). Mark settled after a 2xx.
3. **Politeness (mandatory):** send the `User-Agent` header on every ListenBrainz request; keep at least `request_delay_ms` (floored at 1000 ms) between all API calls in a pass; cap work per user per pass with `max_per_run`; read `X-RateLimit-Remaining` and stop the pass once it hits 0; on HTTP 429 or 5xx stop the pass (do not retry in a tight loop) and let the next scheduled pass continue. Never `Retry-After`-spin.
4. **Removals and hates are not propagated.** Un-starring locally or un-loving on LB is intentionally left untouched — we must not clobber a change the plugin did not make. If this is ever added, it must be snapshot-guarded to only revert what the plugin itself set, and it must be an explicit, opt-in feature.

## 8. Historical Backfill (Deferred — do not implement yet)
Preserved verified design so it is not lost.
- **ZIP hand-off (required):** The plugin cannot unzip. The administrator extracts the archive outside Navidrome and places the contents in the plugin storage mount: host `${DataFolder}/plugins/<pluginID>/storage/import/...` ↔ guest `/storage/import/`. The folder containing the `.ndp` itself is **not** readable; `/libraries/<id>` is read-only and unsuitable.
- **Stream, don't slurp:** `std::fs::File` + `BufReader`, iterate `lines()` one `InsertedListen` per line; process one `listens/<year>/<month>.jsonl` at a time; never `read_to_string`.
- **Inject plays:** resolve `track_metadata.mbid_mapping.recording_mbid` via the Matcher → `subsonicapi::call("scrobble?id=<mediaFileId>&time=<unix_millis>&submission=true&u=<username>")`. The `time` value is **milliseconds**, and multiple `id`/`time` pairs may be sent in one call (verified: Subsonic `Scrobble` parses `time` via `UnixMilli`). Record `format!("{}_{}", listened_at, recording_mbid)` fingerprints (KVStore) so re-runs never duplicate plays.
- **The scrobble echo fix (solid, required if backfill is built):** `Subsonic scrobble` → `playTracker.Submit` records the play **and then forwards it to every active external scrobbler** (built-in Last.fm/ListenBrainz and Scrobbler plugins) whenever the request's player has `ScrobbleEnabled` (default `true` for newly seen players). So backfilled plays would be re-submitted to ListenBrainz. ListenBrainz dedupes only on `(listened_at, user_id, recording_msid)`, and Navidrome's re-derived metadata usually yields a *different* MSID → real duplicates.
  - **Mitigation (required):** disable scrobbling for the plugin's synthetic player. The SubsonicAPI host service sets client `c=<pluginID>`, so these calls register as a player named `<pluginID> [...]`; the administrator turns off scrobbling for that player in Navidrome's player settings. This suppresses dispatch for the built-in scrobblers too.
  - **Defense-in-depth (code):** if the plugin ever implements Scrobbler, have `nd_scrobbler_scrobble` skip any listen whose fingerprint is already recorded from an import.
  - **Caveat:** the synthetic player is recreated (with scrobbling on) if its row is deleted, so the toggle is not fully self-healing.

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
            },
            "required": ["navidrome_username", "listenbrainz_token"]
          }
        },
        "sync_inbound": { "type": "boolean", "title": "Star tracks loved on ListenBrainz", "default": true },
        "sync_outbound": { "type": "boolean", "title": "Love tracks starred in Navidrome", "default": true },
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
- **Deferred backfill fields** (`import_path`, `execute_import`, `import_start_year`, `import_end_year`, and the fingerprint `kvstore` sizing) are added only when §8 is implemented.

## 10. Platform Contract (capabilities, host services, permissions)
Phase 1 uses only these. Confirm names/signatures against the PDK source before calling.
- **Capabilities:** Lifecycle (`nd_on_init`; Rust `nd_pdk::lifecycle::InitProvider` + `register_lifecycle_init!`) registers the recurring sync and schedules a one-shot reconciliation ~5 s after load (so a fresh install or update does not wait for the next cron tick); Scheduler callback (`nd_scheduler_callback`; Rust `nd_pdk::scheduler::CallbackProvider`) runs the pass. No Scrobbler, no TaskWorker.
- **Host services (Rust paths):** `host::http::send` (the only network path; Extism's built-in HTTP is disabled), `host::subsonicapi::call` (star / getStarred2), `host::matcher::match_songs`, `host::kvstore::{list, set, has, get_many}`, `host::scheduler::schedule_recurring`, `host::config::{get, get_int, keys}`, `host::users::{get_users, get_admins}`.
- **Manifest permissions (exact keys):** `http` (`requiredHosts: ["api.listenbrainz.org"]`), `users`, `subsonicapi` (requires `users`), `matcher` (requires `library`), `library`, `kvstore` (`maxSize`), `scheduler`. Each needs a `reason`. Skeleton:
  ```json
  {
    "name": "Navidrome ListenBrainz Sync",
    "author": "egecelikci",
    "version": "0.1.0",
    "description": "Two-way loved-track sync with ListenBrainz",
    "config": { "schema": { }, "uiSchema": { } },
    "permissions": {
      "http": { "reason": "Call the ListenBrainz API", "requiredHosts": ["api.listenbrainz.org"] },
      "users": { "reason": "Act for the users assigned to this plugin" },
      "subsonicapi": { "reason": "Read starred tracks and star matched tracks" },
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
- **No data loss:** Additions only; never un-star, un-love, or clear feedback (see §7.4). Writes are idempotent by the settled set (§5).
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

## 13. Documentation Layout
- **Rustdoc over JSDoc:** Document all functions and struct declarations with `///` or file-level `//!`.
- **No verbose type comments:** Do not restate types in comments (e.g. avoid `@param {string} mbid`). Let the static types declare themselves; comments describe intent and logic paths.
