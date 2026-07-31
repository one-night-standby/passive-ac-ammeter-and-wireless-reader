package com.jun.nuedc.reader;

import android.Manifest;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;
import android.widget.Toast;

import java.util.ArrayList;
import java.util.List;

public final class MainActivity extends Activity implements MeterDashboardView.Listener {
    private static final int REQUEST_PERMISSIONS = 42;

    private MeterDatabase database;
    private ReaderPreferences preferences;
    private MeterDashboardView dashboard;
    private Runnable pendingPermissionAction;

    private final BroadcastReceiver eventReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent.getAction();
            if (MeterPollingService.ACTION_FRAME.equals(action)) {
                // 实时帧只进圆盘,不落库
                dashboard.onLiveFrame(
                        intent.getIntExtra(MeterPollingService.EXTRA_ADDRESS, -1),
                        intent.getIntExtra(MeterPollingService.EXTRA_CURRENT_MA, -1),
                        intent.getLongExtra(
                                MeterPollingService.EXTRA_TIMESTAMP,
                                System.currentTimeMillis()
                        ),
                        intent.getStringExtra(MeterPollingService.EXTRA_MAC),
                        intent.getStringExtra(MeterPollingService.EXTRA_NAME),
                        intent.getIntExtra(MeterPollingService.EXTRA_RSSI, -127)
                );
                return;
            }
            if (MeterPollingService.ACTION_OFFLINE.equals(action)) {
                dashboard.onOffline(
                        intent.getIntExtra(MeterPollingService.EXTRA_ADDRESS, -1));
                return;
            }
            if (MeterPollingService.ACTION_READING.equals(action)) {
                refreshDashboard();
                return;
            }
            if (!MeterPollingService.ACTION_STATE.equals(action)) {
                return;
            }
            String detail = intent.getStringExtra(MeterPollingService.EXTRA_DETAIL);
            String state = intent.getStringExtra(MeterPollingService.EXTRA_STATE);
            dashboard.setConnectionState(
                    detail == null ? "正在连接 HC-42…" : detail,
                    "ERROR".equals(state)
            );
            dashboard.setAutoMode(intent.getBooleanExtra(
                    MeterPollingService.EXTRA_AUTO,
                    false
            ));
            if ("ERROR".equals(state) && detail != null) {
                Toast.makeText(MainActivity.this, detail, Toast.LENGTH_LONG).show();
            }
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        requestWindowFeature(Window.FEATURE_NO_TITLE);
        getWindow().setFlags(
                WindowManager.LayoutParams.FLAG_FULLSCREEN,
                WindowManager.LayoutParams.FLAG_FULLSCREEN
        );
        database = new MeterDatabase(this);
        preferences = new ReaderPreferences(this);
        dashboard = new MeterDashboardView(this);
        dashboard.setListener(this);
        dashboard.setPollingIntervalSeconds(preferences.pollingIntervalSeconds());
        setContentView(dashboard);
        enterImmersiveMode();
        registerEventReceiver();
        refreshDashboard();
        ensurePermissions(() -> startReaderAction(MeterPollingService.ACTION_CONNECT_BASIC));
    }

    @Override
    protected void onResume() {
        super.onResume();
        enterImmersiveMode();
        refreshDashboard();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            enterImmersiveMode();
        }
    }

    @Override
    protected void onDestroy() {
        unregisterReceiver(eventReceiver);
        database.close();
        super.onDestroy();
    }

    /** 读条走完:把界面抓到的这一帧同步落库,返回前界面即可拿到新数据起飞。 */
    @Override
    public void onCaptureRecord(int address, int currentMa, String status,
                                String mac, String deviceName, int rssi) {
        if (currentMa < 0) {
            return;                                            // 没读到数不落库,第二道防线
        }
        database.insertReading(new MeterReading(
                0,
                System.currentTimeMillis(),
                address,
                currentMa,
                status,
                mac,
                deviceName,
                rssi,
                "MANUAL"
        ));
        refreshDashboard();
    }

    /** 基本模式的一对一读取:题目 2(2) 不允许碰电流表,读数只能由读表器要。 */
    @Override
    public void onReadRequest(int address) {
        Intent intent = new Intent(this, MeterPollingService.class);
        intent.setAction(MeterPollingService.ACTION_READ_NOW);
        intent.putExtra(MeterPollingService.EXTRA_ADDRESS, address);
        startService(intent);
    }

    @Override
    public void onAutoModeChanged(boolean enabled) {
        ensurePermissions(() -> startReaderAction(
                enabled
                        ? MeterPollingService.ACTION_START_AUTO
                        : MeterPollingService.ACTION_STOP_AUTO
        ));
    }

    /** 确认已经在界面里完成(「清空 → 确认清空」两次点击),这里直接清。 */
    @Override
    public void onClearHistory() {
        database.clearHistory();
        refreshDashboard();
        Toast.makeText(this, "记录已清空", Toast.LENGTH_SHORT).show();
    }

    @Override
    public void onIntervalRequested() {
        int[] values = {10, 30, 60, 120, 180, 240, 300};
        String[] labels = {
                "10 秒",
                "30 秒",
                "1 分钟",
                "2 分钟（默认）",
                "3 分钟",
                "4 分钟",
                "5 分钟"
        };
        new AlertDialog.Builder(this)
                .setTitle("自动采集间隔")
                .setItems(labels, (dialog, which) -> {
                    preferences.savePollingIntervalSeconds(values[which]);
                    dashboard.setPollingIntervalSeconds(values[which]);
                    startReaderAction(MeterPollingService.ACTION_UPDATE_INTERVAL);
                })
                .setNegativeButton("取消", null)
                .show();
    }

    private void refreshDashboard() {
        if (dashboard == null || database == null) {
            return;
        }
        dashboard.setData(
                database.latestByAddress(),
                database.history(MeterDatabase.MAX_STORED_READINGS),
                database.registeredAddresses()
        );
    }

    private void ensurePermissions(Runnable action) {
        ArrayList<String> missing = new ArrayList<>();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            addIfMissing(missing, Manifest.permission.BLUETOOTH_SCAN);
            addIfMissing(missing, Manifest.permission.BLUETOOTH_CONNECT);
        } else {
            addIfMissing(missing, Manifest.permission.ACCESS_FINE_LOCATION);
        }
        if (missing.isEmpty()) {
            action.run();
            return;
        }
        pendingPermissionAction = action;
        requestPermissions(missing.toArray(new String[0]), REQUEST_PERMISSIONS);
    }

    private void addIfMissing(List<String> missing, String permission) {
        if (checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
            missing.add(permission);
        }
    }

    @Override
    public void onRequestPermissionsResult(
            int requestCode,
            String[] permissions,
            int[] grantResults
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode != REQUEST_PERMISSIONS) {
            return;
        }
        boolean granted = grantResults.length > 0;
        for (int result : grantResults) {
            granted &= result == PackageManager.PERMISSION_GRANTED;
        }
        if (granted && pendingPermissionAction != null) {
            Runnable action = pendingPermissionAction;
            pendingPermissionAction = null;
            action.run();
        } else {
            pendingPermissionAction = null;
            dashboard.setConnectionState("需要附近设备权限才能连接 HC-42", true);
        }
    }

    private void startReaderAction(String action) {
        Intent intent = new Intent(this, MeterPollingService.class);
        intent.setAction(action);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent);
        } else {
            startService(intent);
        }
    }

    private void registerEventReceiver() {
        IntentFilter filter = new IntentFilter();
        filter.addAction(MeterPollingService.ACTION_FRAME);
        filter.addAction(MeterPollingService.ACTION_OFFLINE);
        filter.addAction(MeterPollingService.ACTION_READING);
        filter.addAction(MeterPollingService.ACTION_STATE);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(eventReceiver, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(eventReceiver, filter);
        }
    }

    @SuppressWarnings("deprecation")
    private void enterImmersiveMode() {
        getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        );
    }
}
