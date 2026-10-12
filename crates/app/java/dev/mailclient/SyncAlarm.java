package dev.mailclient;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.SystemClock;

/**
 * Wakes the background check every 15 minutes in Doze too. Doze holds SyncJob back until its
 * maintenance windows, hours apart at night (a mail at 2am was announced at 9am). An alarm that
 * may fire while idle comes at most every 9 minutes and gives the app about 10 seconds of
 * network: enough to check the inboxes once. Runs in the ":sync" process like SyncJob.
 */
public final class SyncAlarm extends BroadcastReceiver {
    private static final long PERIOD = 15 * 60 * 1000;

    /** Sets the next alarm, replacing a pending one. Alarms end with a reboot; SyncJob sets them again. */
    static void schedule(Context context) {
        PendingIntent intent = PendingIntent.getBroadcast(context, 0, new Intent(context, SyncAlarm.class),
            PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        context.getSystemService(AlarmManager.class)
            .setAndAllowWhileIdle(AlarmManager.ELAPSED_REALTIME_WAKEUP, SystemClock.elapsedRealtime() + PERIOD, intent);
    }

    @Override
    public void onReceive(Context context, Intent intent) {
        // The next one first: a check that hangs must not end the chain.
        schedule(context);
        PendingResult result = goAsync();
        new Thread(() -> {
            SyncJob.check(context.getApplicationContext());
            result.finish();
        }).start();
    }
}
