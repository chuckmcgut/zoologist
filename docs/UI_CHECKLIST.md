# Web UI checklist

Run this after changing anything in `static/`. Use seeded data and at least one live camera, which can be a file
camera playing `tools/fixtures/fox_walk_640x360_10fps.h264`:

```sh
zoologist seed-demo --config demo.toml    # needs an empty server.data_dir
zoologist run --config demo.toml
```

Items marked ✅ were checked on 2026-09-19 in Chromium (desktop width and 400 px, light and dark).
Items marked ☐ are implemented but not checked by hand yet. Items marked ⬜ need a real camera or the Hub importer
(Step 7.2).

## Header and filters

- ✅ The header and the tab title both read "Zoologist".
- ✅ The page works without a `static/` folder: the dashboard is built into the program.
- ✅ The status line reads "Watching N cameras · M events today · detector X ms".
- ⬜ The status turns amber when one camera is down, and red when all are down or the server is unreachable.
- ⬜ The status says "detector falling behind" (amber) when detector drops increase between two polls.
- ✅ The status says "species model not loaded: animals are not named" (amber) when the species model failed to
  load, and hovering it shows why.
- ✅ The filter bar stays at the top while scrolling.
- ✅ The camera select lists "All cameras" plus every enabled camera. Choosing one filters all charts and events.
- ✅ The window buttons (1h 6h 24h 7d 30d) switch Activity, Animals and the Recent events list. 24h is selected at
  first.

## Charts

- ✅ Activity shows all four labels in fixed colours with icons, including zero counts.
- ☐ Clicking an Activity bar filters Recent events to that label, and clicking it again clears the filter.
- ✅ Animals shows up to 12 species plus "Other (n)". Animals without a species appear as "Unidentified animal".
- ✅ Clicking a species bar filters events to that species and shows a removable species chip.
- ✅ By hour draws 24 stacked columns in label colours, with an axis, a legend and `<title>` tooltips.
- ☐ The date picker cannot go past today in the station time zone.
- ✅ The chart is drawn at its container's width and is never scaled up: 120 px tall on a wide screen, 150 px on a
  phone.

## Recent events

- ✅ Tiles show the thumbnail (lazy-loaded), label badge, species or label, camera, duration, and time with "n min ago".
- ✅ Active events show a pulsing LIVE badge.
- ☐ Filter chips (All / Person / Vehicle / Animal / Motion) filter the list.
- ✅ "Load more" appends the next page, still newest first.
- ☐ A new live event appears at the top, briefly highlighted.
- ✅ `updated`/`ended` messages replace the tile in place (e.g. "Unidentified animal" turns into "Red fox" when the species arrives).
- ☐ Chart refreshes from live updates happen at most once per 5 s.
- ☐ The live indicator shows "live" when connected and "reconnecting" when the server is gone.

## Viewer

- ✅ Clicking a tile opens the viewer, showing the title, then camera · start · duration · score, then the scientific name and top-3 species.
- ✅ The clip autoplays (muted, inline) with controls, and the snapshot is shown below it.
- ✅ The viewer and the live view use the whole window: full width, with the video short enough that the title
  and controls stay in view (checked at 1024×768, 1600×640 and 375×812).
- ⬜ A pending clip shows the snapshot and "Clip is being prepared…", polls every 3 s, and plays once it is ready.
- ✅ A failed or purged clip shows "Clip not available."
- ✅ Esc and × close the viewer and pause the video.
- ✅ "✗ Wrong" opens a form (Nothing / Person / Vehicle / Animal / Just motion, species for an animal, note).
  Saving shows the verdict, puts a "✗ wrong" badge on the tile, and "Undo" takes it back. The "✗ Marked wrong"
  chip lists only marked events.
- ✅ "Name again" on an animal event runs the classifier on its clip and shows the answer.
- ✅ ←/→ and the Newer/Older buttons step through the tiles in order.

## Cameras and species

- ✅ Clicking a stream camera's picture (or pressing Enter on it) opens its live view, and × or Esc closes it and
  stops the stream. In 8 open/close cycles alternating three cameras (1536×432, 1280×720 and 512×384), every one
  started within 0.1–0.6 s and drew frames at the camera's full rate (18 / 27 / 23 fps), measured with
  `requestVideoFrameCallback`, not just an advancing clock.
- ✅ Battery (Hub) cameras have no live view (it would wake them).

- ✅ Each camera tile shows its latest picture (refreshed every 10 s), a state dot, detect/record state and analysed fps.
- ✅ Each camera tile shows today's counts per label.
- ✅ Hub cameras show 🔋 and the newest event's snapshot with "Last event n min ago".
- ⬜ Hub cameras show the Hub importer's status and last import time (Step 7.2).
- ✅ The Species table lists species, detections, last seen (with "ago"), best score and ▶, which opens the best clip.

## General

- ✅ No horizontal page scroll at 400 px.
- ✅ Light and dark mode are both readable.
- ☐ Every control can be reached with the keyboard and shows a focus ring. The charts have `aria-label`s.
- ✅ No console errors and no failed requests on a fresh load.
