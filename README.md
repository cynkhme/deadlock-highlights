# Deadlock Highlights

Finds the fights in a Deadlock VOD and cuts the best ones as clips. It reads the game clock on screen with a vision model on an OpenAI-compatible server and takes the kills, deaths and objectives of each match from [deadlock-api.com](https://deadlock-api.com). The command is `dlhl`.

## Usage

Deadlock Highlights needs Rust 1.98 or newer, `ffmpeg` and `ffprobe` on `PATH`, and an OpenAI-compatible chat server with a vision model. The measured setup is [Ollama](https://ollama.com) with `gemma4:e4b`:

```sh
ollama serve    # in another terminal, unless the Ollama app is running
ollama pull gemma4:e4b
cargo run --release -- stream.mp4 --account 167576350 \
  --server http://localhost:11434/v1 --model gemma4:e4b
```

`--account` is the streamer's 32-bit Steam account id. Without `--vod-start`, the VOD's wall-clock start is read from yt-dlp's `stream.info.json` beside the VOD, if it is there and its duration is within 10 s of the VOD's. If not, it comes from the clock alone; on the measured VOD that was accepted for every span of 8 or more matches and for no span of 5 or fewer. For a shorter VOD, pass `--vod-start` with unix seconds or RFC 3339 with an offset (`2026-11-02T18:00:00+01:00`).

Each read sends `temperature: 0`, `max_tokens: 64` and `reasoning_effort: "none"`. A server that refuses any of them stops the run at a test read, before the keyframe pass. When frames show the clock but no answer holds one, the run stops with the model's first answer.

`--parallel` sets how many requests go to the server at once (default 1). On Ollama 0.40.13, 4 made `qwen3-vl:4b-instruct` faster (84 s against 194 s for 120 frames) and `gemma4:e4b` slower (15 s against 8 s).

Results go to `<VOD>.dlhl/`, or to `--out`:

- `highlights.json`: every match with its status, the game starts that matched no match, and every window, best first. It is removed when a run starts and written when the run completes.
- `clips/<match id>-<VOD second>.<ext>`: windows with a score of at least `--clips-min-score` (default 3: for example an own kill and an assist, or three assists), stream-copied. `--clips-max` caps the count. Every run cuts its clips again and replaces files of the same name, since a window may have changed. A clip that ffmpeg cannot cut is left out with a warning, and a file left from an earlier cut stays as it was. dlhl never deletes clips.
- `readings/`: clock readings per VOD and settings, so a rerun skips the model and needs no server. A read without any clock is not kept. It may be deleted.
- `matches/`: match metadata from deadlock-api.com, so a rerun does not request it again.
- `history.json`: the account's match history from this run.

Exit codes: 0 when some match was placed; 3 when none was (the reasons are in `highlights.json`, and `retryable` says whether a rerun may help); 2 for a usage error; 1 for any other failure.

`DEADLOCK_API_KEY` is sent to deadlock-api.com when set. `DLHL_SERVER_API_KEY` is sent to `--server`, and only over https or to a loopback address.

## Windows

A window holds events at most 12 s apart: kills near the streamer and objectives. It starts 10 s before its first event and ends 5 s after its last. While the streamer stays in combat, the start moves up to 20 s before the first event, and the end follows the fight to 5 s after its last combat second, at most 35 s after the last event. A fight that ends the match runs to 5 s after the match's last second, within the same limit. A window scores 2 per own kill and 1 per assist; other events score 0. Windows are listed best first: by score, then by seconds in combat, then earlier.

## HUD profile

The clock is looked for in a 360x64 band at the top centre of the frame, after the frame is scaled to height 1080. For another HUD layout, `--profile` takes a JSON file:

```json
{"version": 1, "band": {"center_x": 0.5, "top": 0.0}, "t": 0.4}
```

`center_x` is the band's horizontal centre as a fraction of the frame width, `top` its top edge as a fraction of the frame height. Frames whose gate score is at least `t` are read. All three lie in [0, 1]; the values above are the defaults.

## Limits

- The game capture must fill the video frame. Inset or letterboxed captures are not supported.
- Read accuracy was measured only with `gemma4:e4b` on Ollama 0.40.13 on an Apple M5 Max, at 0.13 s per frame. On a 6-hour VOD, 470 of the 482 frames it read inside the placed matches agreed within 2 s with each match's clock; 11 got no answer and 1 was wrong. Other servers and models are unmeasured.
- An assist is counted when the streamer was alive within 2500 units of the victim in the 3 s before a teammate's kill. Checked against demo files of 9 matches of one player, it finds 83% of the assists, and 93% of those it counts are real.
- Combat is read from `combat_type`, a path field deadlock-api does not document; value 1 is taken as combat with players. On the 6 matches of the measured VOD, 92% of the health drops of 10 or more between two path samples fell on value 1, and in 85% of a sample of value-1 seconds a live enemy was within 2500 units.
- A duo partner's account gives the same matches, seen from the partner's slot. The alignment cannot tell the two apart, so `--account` must be the streamer's.
- Only standard-mode matches are processed: in Street Brawl, for one, the clock restarts every round. Matches under 3 minutes are remakes and are skipped.
- A match deadlock-api.com has not ingested yet, or that it rate-limits, is listed as such. A later rerun requests it again.

## License

MIT
