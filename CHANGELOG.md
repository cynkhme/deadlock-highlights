# Changelog

## Unreleased

- Every run cuts its clips again and replaces files of the same name, so a clip whose window grew is no longer kept as first cut.
- A VOD that starts after a match's first pause no longer marks that match's pauses as mismatched in `highlights.json`.
- The rejected-offset hint and the no-overlap error name where the VOD's start came from, `--vod-start` or `info.json`, instead of always telling to pass `--vod-start`.
- After one failed Steam names request, the run names account ids for the remaining matches instead of waiting out the retries again for each.
- Readings are cached under one server name for every loopback address, so switching between `localhost` and `127.0.0.1` reads no frame again; a user name or password in `--server` is no longer written into the cache.

## 0.1.0 — 2026-11-03

First release.

- Reads the game clock on a Deadlock VOD's keyframes with a vision model on an OpenAI-compatible chat server, after a glyph gate picks the frames that show it.
- Aligns the clock's game starts with the account's deadlock-api.com match history, anchored by `--vod-start` or yt-dlp's `info.json` when given, and places each match on the VOD, pauses included.
- Groups kills near the streamer and objectives into windows, widens them by the streamer's combat, scores them 2 per own kill and 1 per assist, and cuts those at or above `--clips-min-score` (default 3) as stream-copied clips.
- Writes `highlights.json` with every match's status, the unmatched game starts and the windows, best first; exit code 3 when no match was placed.
- Caches clock readings and match metadata, so a rerun needs neither the model server nor new metadata requests.
