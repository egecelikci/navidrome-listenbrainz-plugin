# Navidrome ListenBrainz Sync

> [!WARNING]
> This plugin was fully written by an LLM (DeepSeek V4.1 Flash), not by hand. No person has thoroughly reviewed the design or tested it yet. It is **beta** software: expect rough edges and use at your own risk.

Two-way sync between Navidrome favorites (hearts) and ListenBrainz loves.

- ListenBrainz love → heart the matching track in Navidrome
- Navidrome heart → love the recording on ListenBrainz

Runs on a schedule and only propagates additions. Un-hearting and un-loving are ignored.

Each Navidrome user is linked to a ListenBrainz account by token. The ListenBrainz username is read from the token.

## Install

Download `navidrome-listenbrainz-plugin.ndp` from the latest release and put it in your Navidrome plugins folder (`DataFolder/plugins`), then rescan plugins from the Plugins page in the UI. See the [Navidrome plugin docs](https://www.navidrome.org/docs/usage/features/plugins/#installing-plugins) for details.

Then:

1. Enable the plugin and assign the user(s) it may act for.
2. Add a user link: Navidrome username and ListenBrainz token.
3. Leave `dry_run` on for the first pass, review the log, then turn it off.

## Configuration

| key | default | description |
| --- | --- | --- |
| `user_links` | | required, `[{navidrome_username, listenbrainz_token}]` |
| `sync_inbound` | `true` | heart tracks loved on ListenBrainz |
| `sync_outbound` | `true` | love tracks hearted in Navidrome |
| `sync_schedule` | `*/30 * * * *` | cron for the sync pass |
| `dry_run` | `true` | log only, write nothing |
| `batch_size` | `100` | matcher batch size |
| `request_delay_ms` | `1000` | delay between ListenBrainz calls (min 1000) |
| `max_per_run` | `500` | max changes per user per pass |

## Build

    rustup target add wasm32-wasip1
    cargo build --release --target wasm32-wasip1
    zip -j navidrome-listenbrainz-plugin.ndp manifest.json target/wasm32-wasip1/release/plugin.wasm

The `nd-pdk` dependency is fetched from the Navidrome repository, pinned to the commit for the v0.64.2 release ([1011457](https://github.com/navidrome/navidrome/commit/101145742f4762164202cb4858c9126258bdc463)).

## Notes

Sync is keyed on the MusicBrainz Recording ID. A love with no local match is retried on later passes.
