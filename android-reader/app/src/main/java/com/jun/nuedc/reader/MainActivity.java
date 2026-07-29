package com.jun.nuedc.reader;

import android.Manifest;
import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.os.Build;
import android.os.Bundle;
import android.view.Gravity;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.List;
import java.util.Locale;

public final class MainActivity extends Activity {
    private static final int REQUEST_PERMISSIONS = 42;
    private static final int PRIMARY = Color.rgb(0, 96, 168);
    private static final int ACCENT = Color.rgb(0, 145, 105);
    private static final int SURFACE = Color.rgb(244, 247, 250);
    private static final int TEXT = Color.rgb(28, 34, 40);
    private static final int MUTED = Color.rgb(92, 104, 116);

    private final SimpleDateFormat timeFormat =
            new SimpleDateFormat("MM-dd HH:mm:ss", Locale.CHINA);

    private MeterDatabase database;
    private TextView connectionStatus;
    private TextView modeStatus;
    private TextView addressText;
    private TextView currentText;
    private TextView readingStatus;
    private TextView readingTime;
    private TextView trendTitle;
    private TextView historySummary;
    private LinearLayout historyList;
    private TrendView trendView;
    private Button autoButton;
    private boolean autoMode;
    private int selectedAddress;

    private final BroadcastReceiver eventReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            if (MeterPollingService.ACTION_READING.equals(intent.getAction())) {
                refreshDataViews();
            } else if (MeterPollingService.ACTION_STATE.equals(intent.getAction())) {
                String detail = intent.getStringExtra(MeterPollingService.EXTRA_DETAIL);
                String state = intent.getStringExtra(MeterPollingService.EXTRA_STATE);
                autoMode = intent.getBooleanExtra(MeterPollingService.EXTRA_AUTO, false);
                connectionStatus.setText(detail == null ? "正在连接HC-42…" : detail);
                updateModeControls();
                if ("ERROR".equals(state)) {
                    Toast.makeText(MainActivity.this, detail, Toast.LENGTH_LONG).show();
                }
            }
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        database = new MeterDatabase(this);
        buildUi();
        registerEventReceiver();
        refreshDataViews();
        ensurePermissions(() -> startReaderAction(MeterPollingService.ACTION_CONNECT_BASIC));
    }

    @Override
    protected void onResume() {
        super.onResume();
        refreshDataViews();
    }

    @Override
    protected void onDestroy() {
        unregisterReceiver(eventReceiver);
        database.close();
        super.onDestroy();
    }

    private void buildUi() {
        ScrollView scrollView = new ScrollView(this);
        scrollView.setFillViewport(true);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(dp(18), dp(22), dp(18), dp(24));
        root.setBackgroundColor(SURFACE);
        scrollView.addView(root);

        TextView title = text("无线读表器", 27, TEXT);
        title.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        root.addView(title);

        TextView subtitle = text("启动后自动连接 HC-42", 14, MUTED);
        subtitle.setPadding(0, dp(4), 0, dp(18));
        root.addView(subtitle);

        connectionStatus = text("正在搜索并连接HC-42…", 15, PRIMARY);
        connectionStatus.setPadding(dp(15), dp(14), dp(15), dp(14));
        connectionStatus.setBackground(card(Color.WHITE, PRIMARY));
        root.addView(connectionStatus, matchWrap());

        modeStatus = text("基本模式", 20, TEXT);
        modeStatus.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        LinearLayout.LayoutParams modeParams = matchWrap();
        modeParams.topMargin = dp(24);
        root.addView(modeStatus, modeParams);

        TextView modeHint = text(
                "基本模式手动读取一次；自动模式每2分钟搜索并采集覆盖范围内的全部电流表。",
                14,
                MUTED
        );
        modeHint.setPadding(0, dp(6), 0, dp(15));
        root.addView(modeHint);

        Button manualButton = actionButton("手动读取", PRIMARY);
        manualButton.setOnClickListener(view ->
                ensurePermissions(() -> startReaderAction(
                        MeterPollingService.ACTION_MANUAL_READ
                )));
        root.addView(manualButton, matchWrap());

        autoButton = actionButton("一键启动自动模式", ACCENT);
        LinearLayout.LayoutParams autoParams = matchWrap();
        autoParams.topMargin = dp(10);
        autoButton.setOnClickListener(view -> ensurePermissions(() -> startReaderAction(
                autoMode
                        ? MeterPollingService.ACTION_STOP_AUTO
                        : MeterPollingService.ACTION_START_AUTO
        )));
        root.addView(autoButton, autoParams);

        TextView addressHeading = text("地址码切换", 20, TEXT);
        addressHeading.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        LinearLayout.LayoutParams addressHeadingParams = matchWrap();
        addressHeadingParams.topMargin = dp(28);
        root.addView(addressHeading, addressHeadingParams);

        TextView addressHint = text(
                "地址码由电流表拨码开关设置，此处只切换查看00～15号表。",
                14,
                MUTED
        );
        addressHint.setPadding(0, dp(6), 0, dp(12));
        root.addView(addressHint);

        LinearLayout addressRow = new LinearLayout(this);
        addressRow.setOrientation(LinearLayout.HORIZONTAL);
        addressRow.setGravity(Gravity.CENTER);
        Button previous = actionButton("－", PRIMARY);
        previous.setTextSize(24);
        previous.setOnClickListener(view -> switchAddress(-1));
        addressRow.addView(previous, new LinearLayout.LayoutParams(dp(72), dp(54)));

        addressText = text("00", 34, TEXT);
        addressText.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        addressText.setGravity(Gravity.CENTER);
        LinearLayout.LayoutParams addressParams = new LinearLayout.LayoutParams(0, dp(54), 1f);
        addressParams.leftMargin = dp(12);
        addressParams.rightMargin = dp(12);
        addressText.setBackground(card(Color.WHITE, Color.TRANSPARENT));
        addressRow.addView(addressText, addressParams);

        Button next = actionButton("＋", PRIMARY);
        next.setTextSize(24);
        next.setOnClickListener(view -> switchAddress(1));
        addressRow.addView(next, new LinearLayout.LayoutParams(dp(72), dp(54)));
        root.addView(addressRow, matchWrap());

        LinearLayout readingCard = new LinearLayout(this);
        readingCard.setOrientation(LinearLayout.VERTICAL);
        readingCard.setGravity(Gravity.CENTER);
        readingCard.setPadding(dp(18), dp(24), dp(18), dp(24));
        readingCard.setBackground(card(Color.WHITE, Color.TRANSPARENT));
        readingCard.setElevation(dp(6));
        LinearLayout.LayoutParams readingParams = matchWrap();
        readingParams.topMargin = dp(18);

        currentText = text("-- A", 42, TEXT);
        currentText.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        currentText.setGravity(Gravity.CENTER);
        readingCard.addView(currentText);

        readingStatus = text("暂无读数", 17, MUTED);
        readingStatus.setGravity(Gravity.CENTER);
        readingStatus.setPadding(0, dp(10), 0, 0);
        readingCard.addView(readingStatus);

        readingTime = text("", 13, MUTED);
        readingTime.setGravity(Gravity.CENTER);
        readingTime.setPadding(0, dp(5), 0, 0);
        readingCard.addView(readingTime);
        root.addView(readingCard, readingParams);

        trendTitle = text("00号表电流趋势", 20, TEXT);
        trendTitle.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        LinearLayout.LayoutParams trendTitleParams = matchWrap();
        trendTitleParams.topMargin = dp(28);
        root.addView(trendTitle, trendTitleParams);

        TextView trendHint = text("显示当前地址最近120条有效电流数据", 14, MUTED);
        trendHint.setPadding(0, dp(6), 0, dp(12));
        root.addView(trendHint);

        trendView = new TrendView(this);
        trendView.setBackground(card(Color.WHITE, Color.TRANSPARENT));
        trendView.setElevation(dp(5));
        root.addView(trendView, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                dp(260)
        ));

        TextView historyTitle = text("历史数据", 20, TEXT);
        historyTitle.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        LinearLayout.LayoutParams historyTitleParams = matchWrap();
        historyTitleParams.topMargin = dp(28);
        root.addView(historyTitle, historyTitleParams);

        historySummary = text("", 14, MUTED);
        historySummary.setPadding(0, dp(6), 0, dp(10));
        root.addView(historySummary);

        historyList = new LinearLayout(this);
        historyList.setOrientation(LinearLayout.VERTICAL);
        root.addView(historyList, matchWrap());

        setContentView(scrollView);
        updateModeControls();
    }

    private void switchAddress(int delta) {
        selectedAddress = (selectedAddress + delta + 16) % 16;
        addressText.setText(String.format(Locale.CHINA, "%02d", selectedAddress));
        refreshDataViews();
    }

    private void refreshDataViews() {
        refreshSelectedReading();
        if (trendView != null) {
            trendTitle.setText(String.format(
                    Locale.CHINA,
                    "%02d号表电流趋势",
                    selectedAddress
            ));
            trendView.setReadings(database.trendForAddress(selectedAddress, 120));
        }
        rebuildHistory();
    }

    private void refreshSelectedReading() {
        if (currentText == null) {
            return;
        }
        MeterReading selected = null;
        List<MeterReading> readings = database.latestByAddress();
        for (MeterReading reading : readings) {
            if (reading.address == selectedAddress) {
                selected = reading;
                break;
            }
        }

        if (selected == null) {
            currentText.setText("-- A");
            currentText.setTextColor(TEXT);
            readingStatus.setText("暂无读数");
            readingStatus.setTextColor(MUTED);
            readingTime.setText("请在基本模式手动读取，或启动自动模式");
            return;
        }

        currentText.setText(selected.currentText());
        currentText.setTextColor(statusColor(selected.status));
        readingStatus.setText(selected.statusText());
        readingStatus.setTextColor(statusColor(selected.status));
        readingTime.setText("采集时间  " + timeFormat.format(new Date(selected.timestamp)));
    }

    private void rebuildHistory() {
        if (historyList == null || historySummary == null) {
            return;
        }
        List<MeterReading> history = database.history(50);
        int storedCount = database.storedCount();
        historySummary.setText(String.format(
                Locale.CHINA,
                "已掉电保存 %d 条，容量 %d 条；下方显示最近 %d 条",
                storedCount,
                MeterDatabase.MAX_STORED_READINGS,
                Math.min(50, storedCount)
        ));
        historyList.removeAllViews();
        if (history.isEmpty()) {
            TextView empty = text("暂无历史数据", 14, MUTED);
            empty.setPadding(dp(14), dp(16), dp(14), dp(16));
            empty.setBackground(card(Color.WHITE, Color.TRANSPARENT));
            historyList.addView(empty, matchWrap());
            return;
        }

        for (MeterReading reading : history) {
            LinearLayout row = new LinearLayout(this);
            row.setOrientation(LinearLayout.VERTICAL);
            row.setPadding(dp(14), dp(11), dp(14), dp(11));
            row.setBackground(card(Color.WHITE, Color.TRANSPARENT));

            TextView value = text(
                    String.format(
                            Locale.CHINA,
                            "%02d号表    %s    %s",
                            reading.address,
                            reading.currentText(),
                            reading.statusText()
                    ),
                    15,
                    statusColor(reading.status)
            );
            value.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
            row.addView(value);

            TextView timestamp = text(
                    timeFormat.format(new Date(reading.timestamp)),
                    12,
                    MUTED
            );
            timestamp.setPadding(0, dp(4), 0, 0);
            row.addView(timestamp);

            LinearLayout.LayoutParams rowParams = matchWrap();
            rowParams.bottomMargin = dp(8);
            historyList.addView(row, rowParams);
        }
    }

    private void updateModeControls() {
        if (modeStatus == null || autoButton == null) {
            return;
        }
        modeStatus.setText(autoMode ? "自动模式" : "基本模式");
        modeStatus.setTextColor(autoMode ? ACCENT : PRIMARY);
        autoButton.setText(autoMode ? "停止自动模式，返回基本模式" : "一键启动自动模式");
    }

    private void ensurePermissions(Runnable action) {
        java.util.ArrayList<String> missing = new java.util.ArrayList<>();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            addIfMissing(missing, Manifest.permission.BLUETOOTH_SCAN);
            addIfMissing(missing, Manifest.permission.BLUETOOTH_CONNECT);
        } else {
            addIfMissing(missing, Manifest.permission.ACCESS_FINE_LOCATION);
        }

        if (missing.isEmpty()) {
            action.run();
        } else {
            pendingPermissionAction = action;
            requestPermissions(missing.toArray(new String[0]), REQUEST_PERMISSIONS);
        }
    }

    private Runnable pendingPermissionAction;

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
        boolean granted = true;
        for (int result : grantResults) {
            if (result != PackageManager.PERMISSION_GRANTED) {
                granted = false;
                break;
            }
        }
        if (granted && pendingPermissionAction != null) {
            Runnable action = pendingPermissionAction;
            pendingPermissionAction = null;
            action.run();
        } else {
            pendingPermissionAction = null;
            connectionStatus.setText("需要附近设备权限才能自动连接HC-42");
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
        filter.addAction(MeterPollingService.ACTION_READING);
        filter.addAction(MeterPollingService.ACTION_STATE);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(eventReceiver, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(eventReceiver, filter);
        }
    }

    private Button actionButton(String label, int color) {
        Button button = new Button(this);
        button.setText(label);
        button.setTextColor(Color.WHITE);
        button.setTextSize(16);
        button.setAllCaps(false);
        button.setMinHeight(0);
        button.setMinimumHeight(0);
        button.setBackground(card(color, Color.TRANSPARENT));
        return button;
    }

    private TextView text(String value, float size, int color) {
        TextView view = new TextView(this);
        view.setText(value);
        view.setTextSize(size);
        view.setTextColor(color);
        return view;
    }

    private GradientDrawable card(int fill, int stroke) {
        GradientDrawable drawable = new GradientDrawable();
        drawable.setColor(fill);
        drawable.setCornerRadius(dp(18));
        if (stroke != Color.TRANSPARENT) {
            drawable.setStroke(dp(1), stroke);
        }
        return drawable;
    }

    private int statusColor(String status) {
        switch (status) {
            case MeterReading.HIGH:
                return Color.rgb(183, 28, 28);
            case MeterReading.LOW:
                return Color.rgb(220, 112, 0);
            case MeterReading.OFFLINE:
                return MUTED;
            default:
                return ACCENT;
        }
    }

    private int dp(float value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    private LinearLayout.LayoutParams matchWrap() {
        return new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
        );
    }
}
