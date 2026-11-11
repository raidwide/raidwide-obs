# Bitstream fixtures

Real 32×32 keyframes from FFmpeg's synthetic `testsrc2` source (FFmpeg `n9.0.2`).
Tests independently parse metadata, check length boundaries, and verify original
packet bytes are preserved. FFmpeg is needed only to regenerate fixtures.

Run from `obs/raidwide-ffxiv-rs` (encoder versions may change the output):

```sh
ffmpeg -y -v error -f lavfi -i testsrc2=size=32x32:rate=1 -frames:v 1 -an -pix_fmt yuv420p \
  -c:v libx264 -preset medium -threads 1 \
  -x264-params 'aud=1:repeat-headers=1:keyint=1:min-keyint=1:scenecut=0:qp=28' \
  -f h264 tests/fixtures/h264-keyframe.h264

ffmpeg -y -v error -f lavfi -i testsrc2=size=32x32:rate=1 -frames:v 1 -an -pix_fmt yuv420p \
  -c:v libx265 -preset medium \
  -x265-params 'pools=none:frame-threads=1:wpp=0:aud=1:repeat-headers=1:keyint=1:min-keyint=1:scenecut=0:qp=28:info=1:log-level=error' \
  -f hevc tests/fixtures/hevc-keyframe.h265

ffmpeg -y -v error -f lavfi -i testsrc2=size=32x32:rate=1 -frames:v 1 -an -pix_fmt yuv420p \
  -c:v libaom-av1 -cpu-used 8 -threads 1 -row-mt 0 -tiles 1x1 \
  -lag-in-frames 0 -g 1 -crf 40 -b:v 0 -f obu tests/fixtures/av1-keyframe.obu
```
