#!/usr/bin/env bash
# Builds the Android APK: cargo-apk compiles the native library, then the APK is packed again
# with crates/app/android/AndroidManifest.xml and the Java classes (crates/app/java/), which
# cargo-apk cannot add. Signed with the debug key. Output: target/<profile>/apk/mailclient_app.apk
# Usage: scripts/build-android.sh <cargo apk build arguments>, e.g. --no-default-features --target x86_64-linux-android
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export JAVA_HOME="${JAVA_HOME:-$HOME/Android/jdk}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_NDK_ROOT="${ANDROID_NDK_ROOT:-$ANDROID_HOME/ndk/28.2.13676358}"
export PATH="$JAVA_HOME/bin:$PATH"
tools="$ANDROID_HOME/build-tools/35.0.0"
android_jar="$ANDROID_HOME/platforms/android-35/android.jar"

(cd "$root/crates/app" && cargo apk build --lib "$@")
profile=debug
[[ " $* " == *" --release "* ]] && profile=release
apk="$root/target/$profile/apk"

# Java → dex
work="$apk/java"
rm -rf "$work" && mkdir -p "$work/classes" "$work/dex"
javac --release 11 -Xlint:-options -classpath "$android_jar" -d "$work/classes" "$root"/crates/app/java/dev/mailclient/*.java
"$tools/d8" --min-api 28 --release --lib "$android_jar" --output "$work/dex" $(find "$work/classes" -name '*.class')

# Version and debuggable as cargo-apk worked them out.
version_code=$(grep -o 'android:versionCode="[0-9]*"' "$apk/AndroidManifest.xml" | grep -o '[0-9]*')
version_name=$(grep -o 'android:versionName="[^"]*"' "$apk/AndroidManifest.xml" | cut -d'"' -f2)
debuggable=false
grep -q 'android:debuggable="true"' "$apk/AndroidManifest.xml" && debuggable=true
sed -e "s/VERSION_CODE/$version_code/" -e "s/VERSION_NAME/$version_name/" -e "s/DEBUGGABLE/$debuggable/" \
  "$root/crates/app/android/AndroidManifest.xml" >"$work/AndroidManifest.xml"

unaligned="$work/unaligned.apk"
"$tools/aapt" package -f -M "$work/AndroidManifest.xml" -I "$android_jar" -F "$unaligned"
(cd "$apk" && "$tools/aapt" add "$unaligned" lib/*/*.so >/dev/null)
(cd "$work/dex" && "$tools/aapt" add "$unaligned" classes.dex >/dev/null)
"$tools/zipalign" -f -p 4 "$unaligned" "$apk/mailclient_app.apk"
# ANDROID_KEYSTORE/ANDROID_KEYSTORE_PASS sign releases; otherwise the debug key, created like Gradle does.
ks="${ANDROID_KEYSTORE:-$HOME/.android/debug.keystore}"
if [[ -z "${ANDROID_KEYSTORE:-}" && ! -f "$ks" ]]; then
  mkdir -p "$(dirname "$ks")"
  keytool -genkeypair -keystore "$ks" -storepass android -keypass android -alias androiddebugkey \
    -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
fi
"$tools/apksigner" sign --ks "$ks" --ks-pass "pass:${ANDROID_KEYSTORE_PASS:-android}" "$apk/mailclient_app.apk"
echo "Built $apk/mailclient_app.apk"
