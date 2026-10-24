#!/usr/bin/env bash
# Builds the 30 s test edit used in docs/design/shot-detection.md: hard cuts, a same-scene cut,
# a dissolve, a fade to black, a flash, a zoom and a pan, every event known to the frame.
# Usage: make-test-edit.sh <folder with frename's tests/folder images> <out dir>
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
CLIP=$HERE/../../../tests/clips/file_example_MP4_480_1_5MG.mp4
T=$(cd "$1" && pwd); O=$2; mkdir -p "$O"; cd "$O"
V="-c:v libx264 -pix_fmt yuv420p -r 30 -an -v error -y"
F="scale=480:270:force_original_aspect_ratio=increase,crop=480:270,setsar=1,fps=30"
ffmpeg -ss 2 -t 4 -i "$CLIP" -vf "$F" $V s1.mp4
ffmpeg -loop 1 -t 4 -i "$T/file_example_JPG_100kB.jpg" -vf "scale=960:540,zoompan=z='1+0.0015*on':d=1:s=480x270:fps=30,setsar=1" $V s2.mp4
ffmpeg -loop 1 -t 3.5 -i "$T/file_example_PNG_500kB.png" -f lavfi -t 4.5 -i "smptehdbars=s=480x270:r=30" -filter_complex "[0]$F[a];[1]$F[b];[a][b]xfade=transition=fade:duration=1:offset=2.5" $V s3.mp4
ffmpeg -f lavfi -t 3 -i "testsrc2=s=480x270:r=30" -vf "drawbox=c=white:t=fill:enable='between(n,45,46)',setsar=1" $V s4.mp4
ffmpeg -loop 1 -t 3 -i "$T/file_example_JPG_100kB.jpg" -vf "scale=1600:-2,crop=480:270:x='min(iw-480\,t*370)':y=(ih-270)/2,setsar=1,fps=30" $V s5.mp4
ffmpeg -loop 1 -t 3 -i "$T/file_example_JPG_100kB.jpg" -vf "$F,fade=t=out:st=2:d=1" $V s6.mp4
ffmpeg -loop 1 -t 2 -i "$T/HelloWorld. Nature. NonExisting. file_example_TIFF_1MB.tiff" -vf "$F" $V s7.mp4
ffmpeg -ss 20 -t 3 -i "$CLIP" -vf "$F" $V s8.mp4
ffmpeg -ss 26 -t 2 -i "$CLIP" -vf "$F" $V s9.mp4
for i in 1 2 3 4 5 6 7 8 9; do echo "file 's$i.mp4'"; done > list.txt
ffmpeg -v error -y -f concat -safe 0 -i list.txt -c copy test-edit.mp4
# 64x36 grey frames for prototype.py:
ffmpeg -v error -y -i test-edit.mp4 -vf "scale=64:36,format=gray" -f rawvideo luma64.raw
echo "python3 $HERE/prototype.py $(pwd)/luma64.raw 30"
