# Test fixtures

Small files committed so tests run offline. Large or private files (the owner's camera captures,
Hub recordings) go in `owner/`, which is git-ignored (see `IMPLEMENTATION_PLAN.md` §4).

## Synthetic H.264 (Annex-B), 640×360, 10 fps, 10 s, no B-frames, keyframe every 2 s

| File | Profile | Content |
|---|---|---|
| `testsrc_main_640x360_10fps.h264` | Main | ffmpeg test pattern |
| `testsrc_high_640x360_10fps.h264` | High (CABAC) | ffmpeg test pattern |
| `moving_square_640x360_10fps.h264` | High (CABAC) | 40×40 white square moving right over black, for motion tests |

`testsrc_high_640x360_10fps.flv` is the High-profile file re-wrapped as FLV (`ffmpeg -r 10 -i testsrc_high_640x360_10fps.h264 -c copy -f flv testsrc_high_640x360_10fps.flv`). It is used by the FLV parser tests and by `scripts/fake-camera.sh`, which needs a container file to loop.

Regenerate the `.h264` files with (ffmpeg 7 or newer; libx264 output can differ slightly between versions):

```bash
ffmpeg -f lavfi -i testsrc=size=640x360:rate=10 -t 10 -an -c:v libx264 -bf 0 -g 20 \
  -pix_fmt yuv420p -profile:v main -f h264 testsrc_main_640x360_10fps.h264
ffmpeg -f lavfi -i testsrc=size=640x360:rate=10 -t 10 -an -c:v libx264 -bf 0 -g 20 \
  -pix_fmt yuv420p -profile:v high -f h264 testsrc_high_640x360_10fps.h264
ffmpeg -f lavfi -i color=c=black:size=640x360:rate=10 -f lavfi -i color=c=white:size=40x40:rate=10 \
  -filter_complex "[0][1]overlay=x='20+t*55':y='160+20*sin(t)'" -t 10 -an -c:v libx264 -bf 0 -g 20 \
  -pix_fmt yuv420p -profile:v high -f h264 moving_square_640x360_10fps.h264
```

Still to add (plan Step 0.2): a few CC-licensed photos (person, car, deer, fox, bird) with their sources.
