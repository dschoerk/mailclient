package dev.mailclient;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;

/**
 * A foreground service that does nothing but exist: while it runs, Android keeps the
 * process unfrozen, so the Rust side's IMAP IDLE keeps waiting for new mail in the
 * background. Android requires the ongoing notification it shows.
 */
public final class MailService extends Service {
    private static final String CHANNEL = "push";
    private static final int ID = 2; // 1 is the new-mail notification (platform/android.rs)

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager manager = getSystemService(NotificationManager.class);
        // Lowest importance: no sound, no status bar icon; it only sits in the shade.
        manager.createNotificationChannel(new NotificationChannel(CHANNEL, "Push", NotificationManager.IMPORTANCE_MIN));
        Intent launch = getPackageManager().getLaunchIntentForPackage(getPackageName());
        Notification notification = new Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_dialog_email)
            .setContentTitle("Waiting for new mail")
            .setContentIntent(PendingIntent.getActivity(this, 0, launch, PendingIntent.FLAG_IMMUTABLE))
            .setOngoing(true)
            .build();
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(ID, notification);
        }
        // Not sticky: restarted without the app, it would keep nothing alive.
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
