# Changelog

## 0.1.0 — 2026-11-03

First release.

- Reads the game clock on a Deadlock VOD's keyframes with a vision model on an OpenAI-compatible chat server, after a glyph gate picks the frames that show it.
- Aligns the clock's game starts with the account's deadlock-api.com match history, anchored by `--vod-start` or yt-dlp's `info.json` when given, and places each match on the VOD, pauses included.
- Groups kills near the streamer and objectives into windows, widens them by the streamer's combat, scores them 2 per own kill and 1 per assist, and cuts those at or above `--clips-min-score` (default 3) as stream-copied clips.
- Writes `highlights.json` with every match's status, the unmatched game starts and the windows, best first; exit code 3 when no match was placed.
- Caches clock readings and match metadata, so a rerun needs neither the model server nor new metadata requests.
