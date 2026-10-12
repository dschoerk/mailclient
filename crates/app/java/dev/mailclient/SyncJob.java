package dev.mailclient;

import android.app.job.JobInfo;
import android.app.job.JobParameters;
import android.app.job.JobScheduler;
import android.app.job.JobService;
import android.content.ComponentName;
import android.content.Context;

/**
 * Checks for new mail every 15 minutes or so while the app is in the background, when
 * Android lets it: the system batches jobs and holds them back in Doze, which costs far
 * less battery than keeping IMAP IDLE awake. Runs in its own process (":sync"), where the
 * Rust side syncs each inbox once without the UI.
 */
public final class SyncJob extends JobService {
    private static final int ID = 1;
    private static final long PERIOD = 15 * 60 * 1000; // JobScheduler's minimum

    static native void checkMail(Context context, String filesDir);

    /** Schedules the check unless it is already; it survives reboots. */
    static void schedule(Context context) {
        JobScheduler scheduler = context.getSystemService(JobScheduler.class);
        if (scheduler.getPendingJob(ID) != null) {
            return; // rescheduling would restart the period
        }
        scheduler.schedule(new JobInfo.Builder(ID, new ComponentName(context, SyncJob.class))
            .setPeriodic(PERIOD)
            .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
            .setPersisted(true)
            .build());
    }

    /** One check at a time: the alarm and the job may come together. */
    static synchronized void check(Context context) {
        System.loadLibrary("mailclient_app");
        checkMail(context, context.getFilesDir().getPath());
    }

    @Override
    public boolean onStartJob(JobParameters params) {
        // After a reboot the job is the first to run: it starts the alarms again.
        SyncAlarm.schedule(this);
        new Thread(() -> {
            check(getApplicationContext());
            jobFinished(params, false);
        }).start();
        return true;
    }

    /** Stopped early (network lost, time is up): the next period checks again. */
    @Override
    public boolean onStopJob(JobParameters params) {
        return false;
    }
}
