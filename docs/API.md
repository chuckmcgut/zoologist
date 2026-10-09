# HTTP API

Everything is under `/api/v1` on `server.bind` (default port 8090). Reading uses `GET`; the only
changes are marking an event wrong and naming its animal again (`POST`/`DELETE`, JSON bodies). The
dashboard is served at `/` (built into the program, or from `server.static_dir` if it holds an
`index.html`).

- Timestamps are RFC 3339 in UTC (`2026-09-19T12:47:26.847756Z`). Dates and hours in charts use the
  station time zone (`station.timezone`).
- Errors are JSON, `{"error": "…"}`, with status 400 (bad parameter), 404 (unknown or missing) or 500.
- CORS allows the origins in `server.cors_allow_origins` (`["*"]` by default), for `GET` only. The
  `POST` requests need `Content-Type: application/json`, so another web page cannot send them
  without the browser asking first, and that request is refused.

The examples use `Z=http://localhost:8090/api/v1`.

## Event JSON

Every route that returns events uses this shape: the stored record, plus links and `active`.

```json
{
  "id": 1,
  "camera_id": "yard",
  "label": "animal",                 // person | vehicle | animal | motion
  "raw_class": "animal",             // the detector's own class name
  "started_at": "2026-09-19T12:47:17.6Z",
  "ended_at": "2026-09-19T12:47:27.6Z",   // null while active
  "local_date": "2026-09-19",
  "local_hour": 12,
  "top_score": 0.91,
  "median_score": 0.84,
  "best_bbox": {"x1": 0.03, "y1": 0.6, "x2": 0.3, "y2": 0.9},   // 0..1 of the frame
  "species": {
    "scientific_name": "Vulpes vulpes",
    "common_name": "red fox",
    "score": 0.95,
    "model_id": "speciesnet-4.0.3a",
    "candidates": [["red fox", 0.95], ["gray fox", 0.02]]
  },
  "snapshot_path": "snapshots/2026-09-19/1.jpg",   // relative to the data directory
  "thumb_path": "thumbs/2026-09-19/1.jpg",
  "clip_path": "clips/2026-09-19/1.mp4",
  "clip_bytes": 287405,
  "clip_state": "ready",             // pending | ready | failed | purged
  "clip_url": "/api/v1/events/1/clip.mp4",          // null unless clip_state is ready
  "snapshot_url": "/api/v1/events/1/snapshot.jpg",
  "thumb_url": "/api/v1/events/1/thumb.jpg",
  "sharp_path": "sharp/2026-09-19/1.jpg",          // only when there is a sharp close-up
  "sharp_url": "/api/v1/events/1/sharp.jpg",        // null otherwise
  "active": false,
  "feedback": {                      // only when someone marked the event wrong
    "actual": null,                  // what was really there; null = nothing
    "species": null, "note": "dark stump", "at": "2026-09-28T20:51:14Z"
  }
}
```

`species` is `null` for non-animals, before the classifier has answered, and when it could not decide. It can
name a genus, family, etc. instead of a species when that is all it is sure of (e.g. "Procyonidae family").

## Status

### `GET /health`

Uptime, CPU features, and per-camera stream, decoder and analysis state. Also detector speed and disk use (measured every
5 minutes). `detect.state` / `record.state` is one of `connecting`, `streaming`, `backoff`, `unsupported` (see
`error`) or `ended` (file sources).

```sh
curl -s $Z/health | jq
```

```json
{
  "station": "Zoologist", "uptime_s": 16, "decoder": "rust",
  "cpu": {"avx2": true, "fma": true},
  "cameras": [{
    "id": "yard", "name": "Yard",
    "detect": {"state": "streaming", "error": null, "fps": 10.0, "analysed_fps": 5.0,
               "decode_ms": 0.64, "reconnects": 0, "last_frame_at": "…"},
    "record": {"state": "streaming", "error": null, "last_segment_at": "…"},
    "drops": {"decoded_frames": 0, "detector_jobs": 0}
  }],
  "hubs": [{
    "id": "home-hub", "state": "ok", "last_poll_at": "…", "last_import_at": "…", "imported": 17,
    "pending_files": 0, "errors": 0, "last_error": null,
    "cameras": {"trail": "…"}          // per battery camera: end of its newest imported recording
  }],
  "detector": {"id": "md-sorrel", "workers": 3, "busy_workers": 0, "mean_ms": 57.1, "p95_ms": 61.0, "queue_depth": 0},
  "species": {"id": "speciesnet-4.0.3a", "queue_depth": 0, "dropped": 0},
  "disk": {"recordings_mb": 5120, "clips_mb": 800, "free_mb": 420000, "measured_at": "…"}
}
```

`detector` and `species` are `null` when that model is not loaded. `species_problem` says why the species model is not running although it is enabled (`null` otherwise). `hubs` lists each Reolink Hub importer
(`state` is `starting`, `ok` or `error`, with `last_error`).

### `GET /config`

What the UI needs to know about the station.

```sh
curl -s $Z/config
# {"station":{"name":"Zoologist","timezone":"America/New_York"},"clips_days":30}
```

### `GET /cameras`

Every enabled camera from the config.

```sh
curl -s $Z/cameras
# [{"id":"yard","name":"Yard","kind":"stream","labels":["person","vehicle","animal","motion"],"record":true,"hub":null},
#  {"id":"trail","name":"Trail","kind":"hub_clips",…,"record":false,"hub":"home-hub"}]
```

### `GET /cameras/{id}/latest.jpg`

The newest analysed frame (detect stream resolution), `Cache-Control: no-store`. 404 for Hub cameras, unknown
cameras, or before the first frame.

```sh
curl -s -o yard.jpg $Z/cameras/yard/latest.jpg
```

### `GET /cameras/{id}/live.mp4`

The camera's live video as fragmented MP4, for the browser's MediaSource (the dashboard's live view). It is
the stream Zoologist already receives (the record stream, or the detect stream when the camera does not
record), passed through without decoding.

- **Full resolution:** `?full=1` gives the camera's full-resolution stream when the live view normally shows a
  smaller one (`"full": true` in `/cameras`). Zoologist opens that stream for this viewer only and closes it
  when they leave; it is often H.265. A stream sent on its side (Reolink panorama cameras) is turned by the
  page, by `full_rotate` from `/cameras` or by 270° when it is taller than wide. At most two at once (503). 404 for a camera without one, 502 when it
  does not start.
- **Start:** a new viewer starts at the latest keyframe.
- **Codec:** the `X-Codec` header gives the codec string, e.g. `avc1.4d401f`.
- **Slow viewers:** a viewer that falls behind skips to the next keyframe.
- **End of stream:** the stream ends when the camera reconnects with new settings, and the player reconnects.
- **Errors:** 404 before the camera has sent any video.

```sh
curl -s -m 5 -o live.mp4 $Z/cameras/nc200-color/live.mp4 && ffprobe live.mp4
```

## Events

### `GET /events`

| Parameter | Meaning |
|---|---|
| `limit` | 1..=500, default 50 |
| `order` | `desc` (newest first, default) or `asc` |
| `before_id` / `after_id` | only ids below / above this (paging) |
| `camera` | camera id |
| `label` | `person`, `vehicle`, `animal` or `motion` |
| `species` | common or scientific name, exact, ignoring case |
| `window` | `1h`, `6h`, `24h`, `7d` or `30d`: only events that started within it (default: all) |

Returns `{"items": [Event…], "next_before_id": 41, "next_after_id": 90}`. For the next page (newest first), pass
`before_id=<next_before_id>`. To poll for new events, pass `order=asc&after_id=<next_after_id>`.

```sh
curl -s "$Z/events?limit=20"
curl -s "$Z/events?limit=20&before_id=41"
curl -s "$Z/events?label=animal&camera=yard"
curl -s "$Z/events?species=red%20fox"
```

### `GET /events/{id}`

One event, or 404.

```sh
curl -s $Z/events/1
```

### `DELETE /events/{id}`

Deletes the event for good, with its snapshot, thumbnail, sharp view and clip. A Hub recording that is
also the clip of another event stays until that one is deleted too. Returns `{"deleted": 12}`; 400 while
the event is still going on. A deleted recording is not imported from the Hub again.

```bash
curl -s -X DELETE "$Z/events/12"
```

### `POST /events/{id}/feedback` and `DELETE /events/{id}/feedback`

Marks an event as wrong, or takes the mark back. The event is not changed otherwise: the feedback is
kept with it as a test case for tuning. Body: `{"actual": "nothing" | "person" | "vehicle" | "animal" |
"motion", "species": "American crow", "note": "…"}` (`species` and `note` are optional). Returns the
event. `GET /events?wrong=true` lists the marked events.

```bash
curl -s -X POST -H 'Content-Type: application/json' -d '{"actual":"nothing","note":"stump"}' "$Z/events/12/feedback"
```

### `POST /events/{id}/reclassify`

Names an animal event's animal again from its clip (detector and species classifier, as for a live
event, behind the live cameras; one at a time). Body `{"store": true}` stores the answer. Returns
`{"outcome": "named", "species": {…}}`, `{"outcome": "not_animal", "label": "person"}` (stored as a
relabel), `{"outcome": "still_unnamed"}` (it never moved and could not be named, or a second look finds
no animal: stored as motion, see `species.still_unnamed_as_motion`), `{"outcome": "unknown"}` (it
moved but could not be named: left as it is) or `{"outcome": "no_animal"}` (only with that rule off). 400 for a non-animal event or one
without a clip, 503 when the models are not loaded. Can take a minute. `zoologist reclassify` uses it
when Zoologist is running.

### `GET /events/{id}/clip.mp4`

The clip (H.264 MP4, fast start), with `Range` support so browsers can seek. 404 until the clip is ready.

```sh
curl -s -o fox.mp4 $Z/events/1/clip.mp4
curl -s -H "Range: bytes=0-99" -o /dev/null -w "%{http_code}\n" $Z/events/1/clip.mp4   # 206
```

### `GET /events/{id}/main.mp4`

The full-resolution recording of a battery camera's event (`kind = "hub_clips"`), as the Hub holds it:
usually H.265, which Safari and Chromium-based browsers play and Firefox may not. Zoologist stores only the
sub-stream clip; this one is downloaded from the Hub on the first request (a few seconds), kept in `tmp/main/` for an hour, and served with `Range` support.

- 404 for other cameras, and when the Hub no longer has the recording.
- 502 when the Hub does not answer.

```sh
curl -s -o main.mp4 $Z/events/1/main.mp4
```

### `GET /events/{id}/snapshot.jpg` and `GET /events/{id}/thumb.jpg`

The full frame with the box outlined, and a crop around the box. For events without a box (motion), `thumb.jpg`
returns the snapshot.

```sh
curl -s -o snap.jpg $Z/events/1/snapshot.jpg
curl -s -o thumb.jpg $Z/events/1/thumb.jpg
```

### `GET /events/{id}/sharp.jpg`

A close-up of the animal at the camera's full resolution: the area around it in the main-stream picture
the species was named from (`species.snapshots`; Hub cameras, with ffmpeg for battery cameras). 404 for
events without one (`sharp_url` is `null`).

```sh
curl -s -o sharp.jpg $Z/events/1/sharp.jpg
```

## Charts

`window` is `1h`, `6h`, `24h` (default), `7d` or `30d`, counted back from now by event start time. `camera`
limits every chart to one camera.

### `GET /stats/labels`

Every label in display order, including zeros.

```sh
curl -s "$Z/stats/labels?window=7d"
# {"window":"7d","items":[{"label":"person","count":12},{"label":"vehicle","count":30},
#                         {"label":"animal","count":7},{"label":"motion","count":2}]}
```

### `GET /stats/species`

Animal events per species, most seen first. `common_name: null` counts animals whose species is unknown.
`best_event_id` is the highest-scoring event with a clip, for a "play best clip" button.

```sh
curl -s "$Z/stats/species?window=30d&camera=yard"
# {"window":"30d","items":[{"common_name":"red fox","scientific_name":"Vulpes vulpes","count":5,
#   "last_seen":"…","best_event_id":17,"best_score":0.95}, …]}
```

### `GET /stats/hourly`

Counts per local hour and label for one local date (`date=YYYY-MM-DD`, default today).

```sh
curl -s "$Z/stats/hourly?date=2026-09-19"
# {"date":"2026-09-19","hours":[{"hour":0,"person":0,"vehicle":1,"animal":2,"motion":0}, … 24 entries]}
```

## Animals for other programs

Two routes for a home dashboard or a script that only wants to know which animals came by. They give names,
counts and times, and nothing else: no people, vehicles or motion, and no cameras, event ids, scores, pictures
or clips. The shape is the same as Birdsong's `stats/recent` and `stats/daily`.

A "Wrong" mark is taken into account: an "animal" marked as nothing or as a person is not counted, an event
marked as an animal is, and a species typed in with the mark replaces the classifier's (it has no scientific
name, so `scientific_name` is `""`). Names are grouped ignoring case, and can be a group ("Cervidae family")
when that is all the classifier was sure of. `unidentified` counts the animals without a name, and `total`
includes them.

### `GET /animals/recent`

The animals seen within `window` (`1h`, `6h`, `24h` (default), `7d` or `30d`), most seen first.

```sh
curl -s "$Z/animals/recent?window=7d"
# {"window":"7d","since":"2026-10-01T19:40:00Z","until":"2026-10-08T19:40:00Z","total":14,"unidentified":2,
#  "species":[{"common_name":"White-tailed deer","scientific_name":"Odocoileus virginianus","count":9,
#              "last_seen":"2026-10-08T11:02:31Z"}, …]}
```

### `GET /animals/daily`

The animals seen on one local date (`date=YYYY-MM-DD`, default today in the station's time zone). `by_hour`
has 24 counts, for the local hours 0 to 23.

```sh
curl -s "$Z/animals/daily?date=2026-10-08"
# {"date":"2026-10-08","total":5,"unidentified":1,
#  "species":[{"common_name":"White-tailed deer","scientific_name":"Odocoileus virginianus","total":4,
#              "by_hour":[0,0,0,0,0,1,2,0,0,0,0,0,0,0,0,0,0,0,1,0,0,0,0,0]}]}
```

## Live updates

### `GET /stream`

Server-Sent Events. Each message looks like this:

```
event: started            (or updated, ended, removed)
id: 17                    (the event id)
data: {…Event JSON…}
```

An event is `started` once, then `updated` whenever something changes: a better snapshot, a higher score, the clip becoming ready, or the species arriving. It is `ended` when the event finishes; updates can still follow (clip, species). `removed` means the event no longer exists: a "person" or "animal" that turned out to be nothing, on a camera whose `labels` leave out motion. A comment is sent every 15 s to keep proxies from closing the connection.

When a client reconnects with `Last-Event-ID: <id>` (browsers do this by themselves), it first gets every event with a higher id from the database, as `started` or `ended`. Then live messages follow.

```sh
curl -sN $Z/stream
```

```js
const es = new EventSource('/api/v1/stream');
es.addEventListener('started', e => addTile(JSON.parse(e.data)));
es.addEventListener('updated', e => replaceTile(JSON.parse(e.data)));
es.addEventListener('ended', e => replaceTile(JSON.parse(e.data)));
es.addEventListener('removed', e => removeTile(Number(e.lastEventId)));
```
