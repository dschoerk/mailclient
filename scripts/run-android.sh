#!/usr/bin/env bash
# Builds the Android app, installs it on the connected phone or the emulator (started if
# nothing is connected) and launches it. When the local test server has been set up
# (scripts/test-mailserver.sh), the device can reach it at localhost and trusts its CA.
# Usage: scripts/run-android.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export JAVA_HOME="${JAVA_HOME:-$HOME/Android/jdk}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_NDK_ROOT="${ANDROID_NDK_ROOT:-$ANDROID_HOME/ndk/28.2.13676358}"
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$PATH"
app=dev.mailclient.app

# shortcut: assumes at most one device; with several, set ANDROID_SERIAL.
if ! adb get-state >/dev/null 2>&1; then
  echo "Starting emulator…"
  # The host GPU mode crashes in the emulator's GL translator.
  mkdir -p "$root/target"
  nohup "$ANDROID_HOME/emulator/emulator" -avd mailclient -gpu swiftshader_indirect \
    >"$root/target/emulator.log" 2>&1 &
  adb wait-for-device
  until [[ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == 1 ]]; do sleep 2; done
fi

case "$(adb shell getprop ro.product.cpu.abi | tr -d '\r')" in
  x86_64) target=x86_64-linux-android ;;
  arm64-v8a) target=aarch64-linux-android ;;
  *) echo "unsupported device ABI" >&2; exit 1 ;;
esac

cd "$root/crates/app"
cargo apk build --lib --no-default-features --target "$target"
adb install -r "$root/target/debug/apk/mailclient_app.apk"

ca="$root/target/test-mailserver/ca.pem"
if [[ -f "$ca" ]]; then
  adb reverse tcp:3993 tcp:3993 >/dev/null
  adb reverse tcp:3465 tcp:3465 >/dev/null
  adb push "$ca" /data/local/tmp/ca.pem >/dev/null
  # run-as only works for debug builds.
  adb shell run-as "$app" mkdir -p files
  adb shell run-as "$app" cp /data/local/tmp/ca.pem files/extra-ca.pem
fi

adb shell am start -n "$app/android.app.NativeActivity" >/dev/null
echo "Running. Logs: adb logcat -s mailclient"
