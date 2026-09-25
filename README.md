# Navidrome ListenBrainz Sync

> [!WARNING]
> This plugin was fully written by an LLM (DeepSeek V4.1 Flash), not by hand. No person has thoroughly reviewed the design or tested it yet. It is **beta** software: expect rough edges and use at your own risk.

Two-way sync between Navidrome Favorites (the heart) and ListenBrainz Loves, plus a one-way Rating sync from CritiqueBrainz into Navidrome.

## ListenBrainz

- ListenBrainz Love → mark the matching track as a Favorite in Navidrome
- Navidrome Favorite → Love the recording on ListenBrainz

## CritiqueBrainz

- CritiqueBrainz artist / release group / recording rating → Navidrome artist / album / song rating
- optionally, a rating at or above a threshold → also mark that item as a Favorite in Navidrome

Each of those two CritiqueBrainz actions is toggled per entity type, so you can copy ratings only, Favorite only, both or neither—and only for the entity types you actually rate.

Runs on a schedule and only propagates additions. Un-favoriting and un-loving are ignored, and a rating is never cleared.

Each Navidrome user is linked to a ListenBrainz account by token. The ListenBrainz username is read from the token; the CritiqueBrainz username defaults to it and can be overridden.

## Install

Download `navidrome-listenbrainz-plugin.ndp` from the latest release and put it in your Navidrome plugins folder (`DataFolder/plugins`), then rescan plugins from the Plugins page in the UI. See the [Navidrome plugin docs](https://www.navidrome.org/docs/usage/features/plugins/#installing-plugins) for details.

Then:

1. Enable the plugin and assign the user(s) it may act for.
2. Add a user link: Navidrome username and ListenBrainz token.
3. Leave `dry_run` on for the first pass, review the log, then turn it off.

## Configuration

| key | default | description |
| --- | --- | --- |
| `user_links` | | required, `[{navidrome_username, listenbrainz_token, critiquebrainz_username?}]` |
| `sync_inbound` | `true` | favorite tracks loved on ListenBrainz |
| `sync_outbound` | `true` | love tracks favorited in Navidrome |
| `sync_schedule` | `*/30 * * * *` | cron for the sync pass |
| `dry_run` | `true` | log only, write nothing |
| `batch_size` | `100` | matcher batch size |
| `request_delay_ms` | `1000` | delay between API calls (min 1000) |
| `max_per_run` | `500` | max changes per user per pass, counted separately for each of the two halves |
| `cb_sync_artists` | both off | `{sync_rating, favorite_at}` applied to artist ratings |
| `cb_sync_albums` | both off | `{sync_rating, favorite_at}` applied to release group (album) ratings |
| `cb_sync_recordings` | both off | `{sync_rating, favorite_at}` applied to track ratings |

In each `cb_sync_*` rule, `sync_rating` copies the CritiqueBrainz rating onto the Navidrome item and `favorite_at` marks it as a Favorite once the rating is at least that value (`0` never favorites). Examples:

- favorite albums rated 4 or 5, without touching ratings: `{"sync_rating": false, "favorite_at": 4}`
- copy every album rating, never favorite: `{"sync_rating": true, "favorite_at": 0}`
- both, favoriting albums rated 5: `{"sync_rating": true, "favorite_at": 5}`
- ignore albums entirely: `{"sync_rating": false, "favorite_at": 0}`

Tracking which ratings have already been applied is keyed on the rating itself, so changing a rating on CritiqueBrainz re-applies it on the next pass. Copying a rating overwrites a rating you set by hand in Navidrome for that item; favoriting only ever adds.

`max_per_run` counts *changes*, not items: favoriting one release group can cover several local albums, so a single change may write to more than one item. Ratings with no local match cost no budget, which is what keeps a large unmatched backlog from blocking everything behind it.

## Build

    rustup target add wasm32-wasip1
    cargo build --release --target wasm32-wasip1
    zip -j navidrome-listenbrainz-plugin.ndp manifest.json target/wasm32-wasip1/release/plugin.wasm

The `nd-pdk` dependency is fetched from the Navidrome repository, pinned to the commit for the v0.64.2 release ([1011457](https://github.com/navidrome/navidrome/commit/101145742f4762164202cb4858c9126258bdc463)).

## Notes

Sync is keyed on MusicBrainz IDs. A love or rating with no local match is retried on later passes, so importing a missing album picks it up without a reset. Nothing is ever un-favorited, un-loved, or left unrated. The one exception is a copied rating, which overwrites a hand-set Navidrome rating for that item.

One CritiqueBrainz entity can cover several local items: a release group may hold multiple releases. Every one of them receives the same rating or Favorite, up to 50 per entity.

Two costs worth knowing before pointing this at a large library:

- The CritiqueBrainz rating list is paged on every pass to find new ratings — one request per 50 ratings per enabled entity type, at the politeness floor. A few thousand ratings therefore costs tens of seconds per pass even when nothing changed. Already-applied ratings are skipped without a lookup.
- A release group that has already been synced is not looked at again, so a duplicate album that a *later* library scan adds for it will not be marked as a Favorite until your CritiqueBrainz rating for that release group changes.
