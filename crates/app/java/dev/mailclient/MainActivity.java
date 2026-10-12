package dev.mailclient;

import android.app.AlertDialog;
import android.app.NativeActivity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ContentValues;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.widget.Toast;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;
import java.util.List;

/**
 * The app's activity: NativeActivity runs the Rust UI; this subclass adds what needs Java
 * callbacks: the system file pickers (their results arrive in onActivityResult, which
 * NativeActivity drops), the sync when the app comes back, and the background mail check.
 *
 * Called from Rust on any thread. One picker at a time; its result goes to onResult.
 */
public final class MainActivity extends NativeActivity {
    private static final int PICK = 1;
    private static final int SAVE = 2;

    /** The picker's result: file paths (pick) or the saved file's name (save); null when cancelled or on error. */
    static native void onResult(String[] values, String error);

    private byte[] saving;

    /** Lets the user pick files; copies them into the cache, as the composer attaches by path. */
    public void pickFiles() {
        runOnUiThread(() -> {
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("*/*");
            intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
            startActivityForResult(intent, PICK);
        });
    }

    /** Saves `data` as `name` in Downloads and offers to open it; before Android 10 (no MediaStore.Downloads), lets the user choose where. */
    public void saveFile(String name, String mime, byte[] data) {
        String type = mime.isEmpty() ? "application/octet-stream" : mime;
        if (Build.VERSION.SDK_INT >= 29) {
            new Thread(() -> {
                try {
                    ContentValues values = new ContentValues();
                    values.put(MediaStore.MediaColumns.DISPLAY_NAME, name);
                    values.put(MediaStore.MediaColumns.MIME_TYPE, type);
                    Uri uri = getContentResolver().insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values);
                    if (uri == null) throw new java.io.IOException("Downloads is not available");
                    try (OutputStream out = getContentResolver().openOutputStream(uri, "wt")) {
                        out.write(data);
                    }
                    // Android renames on a clash ("x (1).pdf"): report the name it chose.
                    String saved = displayName(uri);
                    report(new String[] {saved + " in Downloads"}, null);
                    runOnUiThread(() -> offerToOpen(saved, uri, type));
                } catch (Exception e) {
                    report(null, e.toString());
                }
            }).start();
            return;
        }
        runOnUiThread(() -> {
            saving = data;
            Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType(type);
            intent.putExtra(Intent.EXTRA_TITLE, name);
            startActivityForResult(intent, SAVE);
        });
    }

    /**
     * A short, firm tick (long press, a swipe reaching its next stage). Through the vibrator: some
     * phones (OnePlus) play the view's LONG_PRESS feedback faintly or not at all.
     */
    public void haptic() {
        runOnUiThread(() -> {
            android.os.Vibrator vibrator = getSystemService(android.os.Vibrator.class);
            if (Build.VERSION.SDK_INT >= 29 && vibrator != null && vibrator.hasVibrator()) {
                vibrator.vibrate(android.os.VibrationEffect.createPredefined(android.os.VibrationEffect.EFFECT_HEAVY_CLICK));
            } else {
                getWindow().getDecorView().performHapticFeedback(android.view.HapticFeedbackConstants.LONG_PRESS);
            }
        });
    }

    private void offerToOpen(String name, Uri uri, String type) {
        new AlertDialog.Builder(this)
            .setMessage("Saved " + name + " in Downloads.")
            .setPositiveButton("Open", (dialog, which) -> {
                Intent view = new Intent(Intent.ACTION_VIEW).setDataAndType(uri, type).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                try {
                    startActivity(view);
                } catch (ActivityNotFoundException e) {
                    Toast.makeText(this, "No app can open " + name, Toast.LENGTH_LONG).show();
                }
            })
            .setNegativeButton("Close", null)
            .show();
    }

    /** Syncs every account (Rust side); registered once the Rust UI runs. */
    static native void onResumed();

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        try {
            SyncJob.schedule(this);
            SyncAlarm.schedule(this);
        } catch (RuntimeException e) {
            // The app works without background checks; the error lands in logcat.
            android.util.Log.w("mailclient", "cannot schedule the mail check", e);
        }
    }

    /** In the background Android freezes the app and its IMAP IDLE: catch up on what arrived. */
    @Override
    protected void onResume() {
        super.onResume();
        try {
            onResumed();
        } catch (UnsatisfiedLinkError e) {
            // First start: the Rust side is not up yet and syncs on its own.
        }
    }

    /** Back the app did not use: leave like Android 12+ launchers do, without finishing, so it reopens instantly. */
    @Override
    public void onBackPressed() {
        moveTaskToBack(true);
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        byte[] bytes = saving;
        saving = null;
        if (result != RESULT_OK || data == null) {
            report(null, null);
            return;
        }
        List<Uri> uris = new ArrayList<>();
        ClipData clip = data.getClipData();
        if (clip != null) {
            for (int i = 0; i < clip.getItemCount(); i++) {
                uris.add(clip.getItemAt(i).getUri());
            }
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        // Files can be large: copy off the UI thread.
        new Thread(() -> {
            try {
                if (request == PICK) {
                    report(copyToCache(uris), null);
                } else if (request == SAVE && bytes != null && !uris.isEmpty()) {
                    try (OutputStream out = getContentResolver().openOutputStream(uris.get(0), "wt")) {
                        out.write(bytes);
                    }
                    report(new String[] {displayName(uris.get(0))}, null);
                } else {
                    report(null, null);
                }
            } catch (Exception e) {
                report(null, e.toString());
            }
        }).start();
    }

    /** The process may have been restarted while the picker was open: then nobody waits, and onResult is not registered yet. */
    private static void report(String[] values, String error) {
        try {
            onResult(values, error);
        } catch (UnsatisfiedLinkError e) {
            // Nobody to tell.
        }
    }

    private String[] copyToCache(List<Uri> uris) throws Exception {
        // shortcut: copies are never deleted by us; Android trims the cache when storage runs low.
        File dir = new File(getCacheDir(), "attachments/" + System.nanoTime());
        String[] paths = new String[uris.size()];
        for (int i = 0; i < uris.size(); i++) {
            // A directory per file: two picked files may have the same name.
            File file = new File(new File(dir, String.valueOf(i)), displayName(uris.get(i)).replace('/', '_'));
            file.getParentFile().mkdirs();
            try (InputStream in = getContentResolver().openInputStream(uris.get(i));
                    OutputStream out = new FileOutputStream(file)) {
                byte[] buffer = new byte[64 * 1024];
                for (int n; (n = in.read(buffer)) > 0; ) {
                    out.write(buffer, 0, n);
                }
            }
            paths[i] = file.getPath();
        }
        return paths;
    }

    private String displayName(Uri uri) {
        try (Cursor c = getContentResolver().query(uri, new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (c != null && c.moveToFirst() && c.getString(0) != null) {
                return c.getString(0);
            }
        }
        String last = uri.getLastPathSegment();
        return last != null ? last : "attachment";
    }
}
