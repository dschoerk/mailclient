#!/usr/bin/env bash
# Measures mail apps on the connected phone or emulator, this one and others to compare with.
#
#   scripts/measure-android.sh startup [runs] [package…]
#       Cold start: the app is stopped, then launched, `runs` times (default 10). Prints the median,
#       min and max time from the launch until the app's first screen is drawn ("Fully drawn",
#       which the app reports); for apps that report none, until their first frame (am start -W),
#       which is often only the splash screen.
#       Packages default to this app; add others to compare, e.g. com.google.android.gm (Gmail),
#       com.fsck.k9 (K-9 / Thunderbird), eu.faircode.email (FairEmail).
#   scripts/measure-android.sh power reset
#       Starts a battery measurement: clears Android's statistics and counts as on battery.
#   scripts/measure-android.sh power [package…]
#       Battery use since the reset, per app: Android's estimate in mAh from the device's power
#       profile (CPU, network, wakelocks, alarms). On the emulator the profile is made up, so
#       only the comparison between apps means something; on a phone the numbers are real.
#       `power done` hands the battery back to the charger's state afterwards.
set -euo pipefail
export PATH="${ANDROID_HOME:-$HOME/Android/Sdk}/platform-tools:$PATH"
own=dev.mailclient.app

launcher() {
  adb shell cmd package resolve-activity --brief -c android.intent.category.LAUNCHER "$1" | tail -1 | tr -d '\r'
}

case "${1:-}" in
  startup)
    runs="${2:-10}"
    shift $(($# > 1 ? 2 : 1))
    for pkg in "${@:-$own}"; do
      activity="$(launcher "$pkg")"
      times=()
      what="first frame"
      # Not counted: the first launch after an install also loads the new code from storage.
      adb shell am start -W -n "$activity" >/dev/null
      sleep 5
      for _ in $(seq "$runs"); do
        adb shell am force-stop "$pkg"
        adb logcat -c
        sleep 2
        first="$(adb shell am start -W -n "$activity" | tr -d '\r' | sed -n 's/^TotalTime: //p')"
        # Let it finish starting (and syncing) before the next round.
        sleep 5
        # "Fully drawn dev.mailclient.app/…: +1s234ms" (or "+834ms").
        full="$(adb logcat -d | tr -d '\r' | grep -m1 "Fully drawn $pkg/" | sed -E 's/.*: \+(([0-9]+)s)?([0-9]+)ms.*/\2:\3/' || true)"
        if [[ -n "$full" ]]; then
          IFS=: read -r sec ms <<<"$full"
          times+=($((${sec:-0} * 1000 + ms)))
          what="fully drawn"
        elif [[ -n "$first" ]]; then
          times+=("$first")
        fi
      done
      adb shell am force-stop "$pkg"
      mapfile -t sorted < <(printf '%s\n' "${times[@]}" | sort -n)
      echo "$pkg: median ${sorted[$((${#sorted[@]} / 2))]} ms, min ${sorted[0]} ms, max ${sorted[-1]} ms to $what (${#sorted[@]} of $runs cold starts timed)"
    done
    ;;
  power)
    case "${2:-}" in
      reset)
        adb shell dumpsys batterystats --reset >/dev/null
        adb shell dumpsys battery unplug
        echo "Measuring from now. Use the phone as usual, then: $0 power [package…]"
        ;;
      done) adb shell dumpsys battery reset ;;
      *)
        shift
        stats="$(adb shell dumpsys batterystats | tr -d '\r')"
        echo "$stats" | grep -m1 -E "^  Estimated power use|Computed drain" || true
        for pkg in "${@:-$own}"; do
          uid="$(adb shell pm list packages -U "$pkg" | tr -d '\r' | grep -m1 "package:$pkg " | sed 's/.*uid://')"
          # batterystats names app uids u0a<uid - 10000>.
          line="$(echo "$stats" | grep -m1 -E "^ +UID u0a$((uid - 10000)): " || true)"
          echo "$pkg: ${line:-no battery use recorded}"
        done
        ;;
    esac
    ;;
  *)
    sed -n '2,/^set /{/^#/s/^# \{0,1\}//p}' "$0"
    exit 1
    ;;
esac
