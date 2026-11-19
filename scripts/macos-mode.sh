#!/usr/bin/env bash
# macos-mode.sh — switch the macOS server's streaming mode.
#
# Everything happens on the Mac (display refresh, fps, bitrate); the client
# needs no change. Fewer frames also means less CPU on the client when its
# decoder cannot hand out dma-bufs and NV12 has to be uploaded from the CPU.
#
# 1920x1200 is the panel's usable mode; below that macOS only offers HiDPI
# modes (twice the pixels), so the cheaper modes lower the frame rate instead
# of the resolution.
#
#   ./scripts/macos-mode.sh performance   # 1920x1200 @60,   30 Mbit/s
#   ./scripts/macos-mode.sh balanced      # 1920x1200 @120,  80 Mbit/s
#   ./scripts/macos-mode.sh quality       # 1920x1200 @120, 120 Mbit/s
set -euo pipefail

MODE="${1:-balanced}"
HOST="${LGX_HOST:-192.168.10.161}"
case "$MODE" in
    performance) HZ=60;  FPS=60;  BITRATE=30000000 ;;
    balanced)    HZ=120; FPS=120; BITRATE=80000000 ;;
    quality)     HZ=120; FPS=120; BITRATE=120000000 ;;
    *) echo "usage: $0 performance|balanced|quality" >&2; exit 2 ;;
esac

echo "==> $MODE: 1920x1200 @${FPS}fps (${HZ} Hz), $((BITRATE / 1000000)) Mbit/s"
ssh "$HOST" "export PATH=/opt/homebrew/bin:\$PATH
  id=\$(displayplacer list | sed -n 's/^Persistent screen id: //p')
  displayplacer \"id:\$id enabled:true res:1920x1200 hz:$HZ color_depth:8 scaling:off origin:(0,0) degree:0\" >/dev/null
  PL=\$HOME/Library/LaunchAgents/dev.b612s.lgx.server.plist
  /usr/libexec/PlistBuddy -c 'Set :ProgramArguments:6 $FPS' \$PL
  /usr/libexec/PlistBuddy -c 'Set :ProgramArguments:8 $BITRATE' \$PL
  # kickstart -k keeps the cached plist, so a changed fps/bitrate would be
  # ignored; bootout + bootstrap re-reads it.
  launchctl bootout gui/\$(id -u)/dev.b612s.lgx.server 2>/dev/null || true
  sleep 1
  launchctl bootstrap gui/\$(id -u) \$PL
  echo 'mode applied'"
