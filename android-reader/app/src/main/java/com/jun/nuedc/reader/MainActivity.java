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
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Date;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

public final class MainActivity extends Activity {
    private static final int REQUEST_PERMISSIONS = 42;
    private static final long DEVICE_LIST_REFRESH_DELAY_MS = 200L;
    private static final int PRIMARY = Color.rgb(10, 132, 255);
    private static final int PRIMARY_DARK = Color.rgb(0, 64, 128);
    private static final int ACCENT = Color.rgb(48, 176, 199);
    private static final int SURFACE = Color.rgb(241, 244, 248);
    private static final int TEXT = Color.rgb(28, 28, 30);
    private static final int GLASS = Color.argb(218, 255, 255, 255);
    private static final int GLASS_STROKE = Color.argb(145, 255, 255, 255);

    private final Map<String, DeviceInfo> devices = new LinkedHashMap<>();
    private final List<RawRecord> rawRecords = new ArrayList<>();
    private final List<Button> navigationButtons = new ArrayList<>();
    private final Handler uiHandler = new Handler(Looper.getMainLooper());
    private final SimpleDateFormat timeFormat =
            new SimpleDateFormat("HH:mm:ss", Locale.CHINA);
    private final SimpleDateFormat dateTimeFormat =
            new SimpleDateFormat("MM-dd HH:mm:ss", Locale.CHINA);
    private final Runnable deviceListRefreshTask = () -> {
        deviceListRefreshScheduled = false;
        rebuildDeviceList();
    };

    private MeterDatabase database;
    private ReaderPreferences preferences;
    private FrameLayout content;
    private TextView headerStatus;
    private LinearLayout deviceList;
    private TextView dialogLog;
    private CheckBox dialogHex;
    private CheckBox appendCrLf;
    private EditText dialogInput;
    private LinearLayout latestList;
    private LinearLayout historyList;
    private TextView displaySummary;
    private TrendView trendView;
    private View[] pages;
    private Runnable pendingPermissionAction;
    private boolean deviceListRefreshScheduled;
    private int selectedPage;

    private final BroadcastReceiver eventReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            if (MeterPollingService.ACTION_DEVICE.equals(intent.getAction())) {
                String mac = intent.getStringExtra(MeterPollingService.EXTRA_MAC);
                String name = intent.getStringExtra(MeterPollingService.EXTRA_NAME);
                int rssi = intent.getIntExtra(MeterPollingService.EXTRA_RSSI, -127);
                if (mac != null) {
                    devices.put(mac, new DeviceInfo(name, mac, rssi));
                    scheduleDeviceListRefresh();
                }
            } else if (MeterPollingService.ACTION_RAW.equals(intent.getAction())) {
                String direction =
                        intent.getStringExtra(MeterPollingService.EXTRA_DIRECTION);
                byte[] payload =
                        intent.getByteArrayExtra(MeterPollingService.EXTRA_PAYLOAD);
                long timestamp = intent.getLongExtra(
                        MeterPollingService.EXTRA_TIMESTAMP,
                        System.currentTimeMillis()
                );
                if (payload != null) {
                    rawRecords.add(new RawRecord(timestamp, direction, payload));
                    while (rawRecords.size() > 500) {
                        rawRecords.remove(0);
                    }
                    rebuildDialogLog();
                }
            } else if (MeterPollingService.ACTION_READING.equals(intent.getAction())) {
                refreshDisplay();
            } else if (MeterPollingService.ACTION_STATE.equals(intent.getAction())) {
                String detail = intent.getStringExtra(MeterPollingService.EXTRA_DETAIL);
                String state = intent.getStringExtra(MeterPollingService.EXTRA_STATE);
                headerStatus.setText(detail == null ? "就绪" : detail);
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
        preferences = new ReaderPreferences(this);
        buildUi();
        registerEventReceiver();
        showPage(0);
    }

    @Override
    protected void onResume() {
        super.onResume();
        refreshDisplay();
    }

    @Override
    protected void onDestroy() {
        uiHandler.removeCallbacks(deviceListRefreshTask);
        unregisterReceiver(eventReceiver);
        database.close();
        super.onDestroy();
    }

    private void buildUi() {
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(SURFACE);

        LinearLayout header = new LinearLayout(this);
        header.setOrientation(LinearLayout.VERTICAL);
        header.setPadding(dp(20), dp(16), dp(20), dp(13));
        header.setBackground(rounded(Color.argb(226, 255, 255, 255), 0, Color.TRANSPARENT));
        header.setElevation(dp(8));

        TextView title = text("“无源”交流电流表 · 无线读表器", 20, TEXT);
        title.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        header.addView(title);

        headerStatus = text("准备就绪", 13, Color.rgb(99, 99, 102));
        headerStatus.setPadding(0, dp(5), 0, 0);
        header.addView(headerStatus);
        root.addView(header, matchWrap());

        content = new FrameLayout(this);
        content.addView(new GlassBackgroundView(this), new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT
        ));
        root.addView(content, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                0,
                1f
        ));

        pages = new View[]{
                buildDevicePage(),
                buildDialogPage(),
                buildDisplayPage(),
                buildSettingsPage()
        };
        for (View page : pages) {
            content.addView(page, new FrameLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.MATCH_PARENT
            ));
        }

        LinearLayout navigation = new LinearLayout(this);
        navigation.setOrientation(LinearLayout.HORIZONTAL);
        navigation.setPadding(dp(4), dp(5), dp(4), dp(6));
        navigation.setBackground(rounded(Color.argb(226, 255, 255, 255), 20, GLASS_STROKE));
        navigation.setElevation(dp(12));
        String[] labels = {"设备连接", "对话模式", "显示界面", "设置"};
        for (int i = 0; i < labels.length; i++) {
            final int page = i;
            Button button = new Button(this);
            button.setText(labels[i]);
            button.setTextSize(13);
            button.setAllCaps(false);
            button.setMinHeight(0);
            button.setMinimumHeight(0);
            button.setPadding(dp(4), dp(8), dp(4), dp(8));
            button.setOnClickListener(view -> showPage(page));
            navigationButtons.add(button);
            navigation.addView(button, new LinearLayout.LayoutParams(0, dp(50), 1f));
        }
        FrameLayout navigationHost = new FrameLayout(this);
        FrameLayout.LayoutParams navigationParams = new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                dp(58)
        );
        navigationParams.setMargins(dp(12), dp(5), dp(12), dp(7));
        navigationHost.addView(navigation, navigationParams);
        root.addView(navigationHost, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                dp(70)
        ));
        setContentView(root);
    }

    private View buildDevicePage() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout page = pageContainer();
        scroll.addView(page);

        page.addView(sectionTitle("设备连接"));
        page.addView(bodyText(
                "搜索附近BLE设备，选择HC-42后保持连接。连接成功后可在“对话模式”查看ASCII数据。"
        ));

        LinearLayout actions = horizontal();
        Button scan = actionButton("扫描设备", PRIMARY);
        scan.setOnClickListener(view -> ensurePermissions(() -> {
            uiHandler.removeCallbacks(deviceListRefreshTask);
            deviceListRefreshScheduled = false;
            devices.clear();
            rebuildDeviceList();
            startReaderAction(MeterPollingService.ACTION_SCAN_ONCE, null, null);
        }));
        Button disconnect = actionButton("断开连接", Color.rgb(110, 118, 124));
        disconnect.setOnClickListener(view ->
                startReaderAction(MeterPollingService.ACTION_STOP, null, null));
        actions.addView(scan, weighted());
        actions.addView(disconnect, weightedWithLeftMargin());
        page.addView(actions, matchWrap());

        deviceList = new LinearLayout(this);
        deviceList.setOrientation(LinearLayout.VERTICAL);
        deviceList.setPadding(0, dp(12), 0, dp(24));
        page.addView(deviceList, matchWrap());
        rebuildDeviceList();
        return scroll;
    }

    private View buildDialogPage() {
        LinearLayout page = pageContainer();
        page.addView(sectionTitle("对话模式"));

        LinearLayout options = horizontal();
        dialogHex = new CheckBox(this);
        dialogHex.setText("HEX显示/发送");
        dialogHex.setChecked(preferences.dialogHex());
        dialogHex.setOnCheckedChangeListener((button, checked) -> {
            saveDialogOptions();
            rebuildDialogLog();
        });
        appendCrLf = new CheckBox(this);
        appendCrLf.setText("发送加CRLF");
        appendCrLf.setChecked(preferences.appendCrLf());
        appendCrLf.setOnCheckedChangeListener((button, checked) -> saveDialogOptions());
        options.addView(dialogHex, weighted());
        options.addView(appendCrLf, weighted());
        page.addView(options, matchWrap());

        ScrollView logScroll = new ScrollView(this);
        dialogLog = text("等待连接和数据…", 14, TEXT);
        dialogLog.setTypeface(Typeface.MONOSPACE);
        dialogLog.setTextIsSelectable(true);
        dialogLog.setPadding(dp(12), dp(12), dp(12), dp(12));
        dialogLog.setBackground(rounded(GLASS, 20, GLASS_STROKE));
        dialogLog.setElevation(dp(7));
        logScroll.addView(dialogLog);
        page.addView(logScroll, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                0,
                1f
        ));

        LinearLayout sendRow = horizontal();
        sendRow.setPadding(0, dp(10), 0, 0);
        dialogInput = new EditText(this);
        dialogInput.setHint("输入要发送的数据");
        dialogInput.setSingleLine(true);
        dialogInput.setTextSize(15);
        sendRow.addView(dialogInput, new LinearLayout.LayoutParams(
                0,
                dp(50),
                1f
        ));
        Button send = actionButton("发送", ACCENT);
        send.setOnClickListener(view -> sendDialogData());
        LinearLayout.LayoutParams sendParams =
                new LinearLayout.LayoutParams(dp(84), dp(50));
        sendParams.leftMargin = dp(8);
        sendRow.addView(send, sendParams);
        page.addView(sendRow, matchWrap());

        Button clear = actionButton("清空接收区", Color.rgb(110, 118, 124));
        LinearLayout.LayoutParams clearParams = matchWrap();
        clearParams.topMargin = dp(8);
        clear.setOnClickListener(view -> {
            rawRecords.clear();
            rebuildDialogLog();
        });
        page.addView(clear, clearParams);
        return page;
    }

    private View buildDisplayPage() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout page = pageContainer();
        scroll.addView(page);

        page.addView(sectionTitle("电流表显示"));
        displaySummary = bodyText("暂无读表记录");
        displaySummary.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        page.addView(displaySummary);

        LinearLayout actions = horizontal();
        Button auto = actionButton("一键自动读表", PRIMARY);
        auto.setOnClickListener(view -> ensurePermissions(() ->
                startReaderAction(MeterPollingService.ACTION_START_AUTO, null, null)));
        Button stop = actionButton("停止", Color.rgb(183, 57, 43));
        stop.setOnClickListener(view ->
                startReaderAction(MeterPollingService.ACTION_STOP, null, null));
        actions.addView(auto, weighted());
        actions.addView(stop, weightedWithLeftMargin());
        page.addView(actions, matchWrap());

        TextView latestTitle = subheading("最新读数");
        page.addView(latestTitle);
        latestList = new LinearLayout(this);
        latestList.setOrientation(LinearLayout.VERTICAL);
        page.addView(latestList, matchWrap());

        page.addView(subheading("电流趋势"));
        trendView = new TrendView(this);
        trendView.setBackground(rounded(GLASS, 20, GLASS_STROKE));
        trendView.setElevation(dp(7));
        page.addView(trendView, new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                dp(260)
        ));

        page.addView(subheading("最近30条记录"));
        historyList = new LinearLayout(this);
        historyList.setOrientation(LinearLayout.VERTICAL);
        page.addView(historyList, matchWrap());
        return scroll;
    }

    private View buildSettingsPage() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout page = pageContainer();
        scroll.addView(page);

        page.addView(sectionTitle("设置"));
        page.addView(subheading("告警阈值（mA）"));

        EditText low = numericInput("低电流阈值", preferences.lowThresholdMa());
        EditText high = numericInput("高电流阈值", preferences.highThresholdMa());
        page.addView(low, matchWrap());
        LinearLayout.LayoutParams highParams = matchWrap();
        highParams.topMargin = dp(8);
        page.addView(high, highParams);

        Button save = actionButton("保存阈值", PRIMARY);
        LinearLayout.LayoutParams saveParams = matchWrap();
        saveParams.topMargin = dp(10);
        save.setOnClickListener(view -> {
            try {
                int lowValue = Integer.parseInt(low.getText().toString().trim());
                int highValue = Integer.parseInt(high.getText().toString().trim());
                if (preferences.saveThresholds(lowValue, highValue)) {
                    Toast.makeText(this, "阈值已保存", Toast.LENGTH_SHORT).show();
                    refreshDisplay();
                } else {
                    Toast.makeText(this, "高阈值必须大于低阈值", Toast.LENGTH_LONG).show();
                }
            } catch (NumberFormatException exception) {
                Toast.makeText(this, "请输入有效整数", Toast.LENGTH_LONG).show();
            }
        });
        page.addView(save, saveParams);

        page.addView(subheading("通信参数"));
        page.addView(infoCard(
                "BLE服务：FFE0\n收发特征：FFE1（Notify/Write）\n自动轮询：120秒\n扫描窗口：8秒\n" +
                        "表地址：00～15，由M0的4位编码开关决定"
        ));

        page.addView(subheading("数据协议"));
        TextView protocol = infoCard(
                "METER_TEST,ADDR=01,CURRENT_MA=1234,STATUS=NORMAL\\r\\n\n\n" +
                        "App依据电流值重新判断：\n" +
                        "低电流 < 0.2A；高电流 > 2A；其余正常。"
        );
        protocol.setTypeface(Typeface.MONOSPACE);
        page.addView(protocol);
        return scroll;
    }

    private void showPage(int index) {
        selectedPage = index;
        for (int i = 0; i < pages.length; i++) {
            pages[i].setVisibility(i == index ? View.VISIBLE : View.GONE);
            Button button = navigationButtons.get(i);
            button.setTextColor(i == index ? Color.WHITE : PRIMARY_DARK);
            button.setBackground(rounded(
                    i == index ? Color.argb(225, 10, 132, 255) : Color.TRANSPARENT,
                    15,
                    Color.TRANSPARENT
            ));
        }
        if (index == 2) {
            refreshDisplay();
        }
    }

    private void rebuildDeviceList() {
        if (deviceList == null) {
            return;
        }
        deviceList.removeAllViews();
        if (devices.isEmpty()) {
            deviceList.addView(bodyText("尚未扫描。点击“扫描设备”查找HC-42。"));
            return;
        }
        for (DeviceInfo info : devices.values()) {
            LinearLayout row = new LinearLayout(this);
            row.setOrientation(LinearLayout.HORIZONTAL);
            row.setGravity(Gravity.CENTER_VERTICAL);
            row.setPadding(dp(14), dp(12), dp(10), dp(12));
            row.setBackground(rounded(GLASS, 20, GLASS_STROKE));
            row.setElevation(dp(7));

            LinearLayout labels = new LinearLayout(this);
            labels.setOrientation(LinearLayout.VERTICAL);
            TextView name = text(info.name, 17, TEXT);
            name.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
            labels.addView(name);
            labels.addView(text(
                    info.mac + "    RSSI " + info.rssi + " dBm",
                    12,
                    Color.DKGRAY
            ));
            row.addView(labels, new LinearLayout.LayoutParams(0, matchWrap().height, 1f));

            Button connect = actionButton("连接", PRIMARY);
            connect.setOnClickListener(view -> ensurePermissions(() ->
                    startReaderAction(
                            MeterPollingService.ACTION_CONNECT_STREAM,
                            info.mac,
                            null
                    )));
            row.addView(connect, new LinearLayout.LayoutParams(dp(76), dp(44)));

            LinearLayout.LayoutParams rowParams = matchWrap();
            rowParams.bottomMargin = dp(8);
            deviceList.addView(row, rowParams);
        }
    }

    private void scheduleDeviceListRefresh() {
        if (deviceListRefreshScheduled) {
            return;
        }
        deviceListRefreshScheduled = true;
        uiHandler.postDelayed(deviceListRefreshTask, DEVICE_LIST_REFRESH_DELAY_MS);
    }

    private void rebuildDialogLog() {
        if (dialogLog == null) {
            return;
        }
        if (rawRecords.isEmpty()) {
            dialogLog.setText("等待连接和数据…");
            return;
        }
        boolean hex = dialogHex != null && dialogHex.isChecked();
        StringBuilder builder = new StringBuilder();
        for (RawRecord record : rawRecords) {
            builder.append('[')
                    .append(timeFormat.format(new Date(record.timestamp)))
                    .append("] ")
                    .append(record.direction)
                    .append("  ");
            if (hex) {
                for (byte value : record.payload) {
                    builder.append(String.format(Locale.ROOT, "%02X ", value & 0xFF));
                }
                builder.append('\n');
            } else {
                builder.append(new String(record.payload, StandardCharsets.US_ASCII));
                if (record.payload.length == 0
                        || record.payload[record.payload.length - 1] != '\n') {
                    builder.append('\n');
                }
            }
        }
        dialogLog.setText(builder.toString());
    }

    private void sendDialogData() {
        String input = dialogInput.getText().toString();
        byte[] payload;
        if (dialogHex.isChecked()) {
            payload = parseHex(input);
            if (payload == null) {
                Toast.makeText(this, "HEX格式错误，请输入成对十六进制数", Toast.LENGTH_LONG).show();
                return;
            }
        } else {
            payload = input.getBytes(StandardCharsets.UTF_8);
        }
        if (appendCrLf.isChecked()) {
            ByteArrayOutputStream output = new ByteArrayOutputStream();
            output.write(payload, 0, payload.length);
            output.write('\r');
            output.write('\n');
            payload = output.toByteArray();
        }
        if (payload.length == 0) {
            return;
        }
        startReaderAction(MeterPollingService.ACTION_SEND_DATA, null, payload);
        dialogInput.setText("");
    }

    private byte[] parseHex(String input) {
        String normalized = input.replaceAll("[^0-9A-Fa-f]", "");
        if (normalized.length() == 0 || (normalized.length() & 1) != 0) {
            return null;
        }
        byte[] result = new byte[normalized.length() / 2];
        try {
            for (int i = 0; i < result.length; i++) {
                result[i] = (byte) Integer.parseInt(
                        normalized.substring(i * 2, i * 2 + 2),
                        16
                );
            }
            return result;
        } catch (NumberFormatException exception) {
            return null;
        }
    }

    private void refreshDisplay() {
        if (latestList == null || historyList == null || trendView == null) {
            return;
        }
        List<MeterReading> latest = database.latestByAddress();
        List<MeterReading> history = database.history(30);
        List<MeterReading> trend = database.trend(120);

        latestList.removeAllViews();
        int normal = 0;
        int alarms = 0;
        int offline = 0;
        for (MeterReading reading : latest) {
            if (MeterReading.NORMAL.equals(reading.status)) {
                normal++;
            } else {
                alarms++;
                if (MeterReading.OFFLINE.equals(reading.status)) {
                    offline++;
                }
            }
            latestList.addView(readingCard(reading));
        }
        if (latest.isEmpty()) {
            latestList.addView(bodyText("暂无读数，请先连接电流表或启动自动读表。"));
        }
        displaySummary.setText(String.format(
                Locale.CHINA,
                "电流表 %d 台｜正常 %d｜告警 %d｜离线 %d",
                latest.size(),
                normal,
                alarms,
                offline
        ));

        historyList.removeAllViews();
        for (MeterReading reading : history) {
            TextView line = text(
                    String.format(
                            Locale.CHINA,
                            "%s  %02d号  %-8s  %s",
                            dateTimeFormat.format(new Date(reading.timestamp)),
                            reading.address,
                            reading.currentText(),
                            reading.statusText()
                    ),
                    13,
                    statusColor(reading.status)
            );
            line.setTypeface(Typeface.MONOSPACE);
            line.setPadding(dp(4), dp(6), dp(4), dp(6));
            historyList.addView(line);
        }
        if (history.isEmpty()) {
            historyList.addView(bodyText("暂无历史记录"));
        }
        trendView.setReadings(trend);
    }

    private View readingCard(MeterReading reading) {
        LinearLayout card = new LinearLayout(this);
        card.setOrientation(LinearLayout.HORIZONTAL);
        card.setGravity(Gravity.CENTER_VERTICAL);
        card.setPadding(dp(14), dp(12), dp(14), dp(12));
        card.setBackground(rounded(statusBackground(reading.status), 20, GLASS_STROKE));
        card.setElevation(dp(7));

        TextView address = text(
                String.format(Locale.CHINA, "%02d", reading.address),
                26,
                statusColor(reading.status)
        );
        address.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        address.setGravity(Gravity.CENTER);
        card.addView(address, new LinearLayout.LayoutParams(dp(56), dp(56)));

        LinearLayout values = new LinearLayout(this);
        values.setOrientation(LinearLayout.VERTICAL);
        TextView current = text(reading.currentText(), 22, TEXT);
        current.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        values.addView(current);
        values.addView(text(
                reading.statusText() + " · " + dateTimeFormat.format(new Date(reading.timestamp)),
                12,
                statusColor(reading.status)
        ));
        card.addView(values, new LinearLayout.LayoutParams(0, matchWrap().height, 1f));

        LinearLayout.LayoutParams params = matchWrap();
        params.bottomMargin = dp(8);
        card.setLayoutParams(params);
        return card;
    }

    private void ensurePermissions(Runnable action) {
        List<String> missing = new ArrayList<>();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            addIfMissing(missing, Manifest.permission.BLUETOOTH_SCAN);
            addIfMissing(missing, Manifest.permission.BLUETOOTH_CONNECT);
        } else {
            addIfMissing(missing, Manifest.permission.ACCESS_FINE_LOCATION);
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            addIfMissing(missing, Manifest.permission.POST_NOTIFICATIONS);
        }

        if (missing.isEmpty()) {
            action.run();
        } else {
            pendingPermissionAction = action;
            requestPermissions(missing.toArray(new String[0]), REQUEST_PERMISSIONS);
        }
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
        boolean bluetoothGranted = true;
        for (int i = 0; i < permissions.length; i++) {
            if (!Manifest.permission.POST_NOTIFICATIONS.equals(permissions[i])
                    && grantResults[i] != PackageManager.PERMISSION_GRANTED) {
                bluetoothGranted = false;
            }
        }
        if (bluetoothGranted && pendingPermissionAction != null) {
            Runnable action = pendingPermissionAction;
            pendingPermissionAction = null;
            action.run();
        } else {
            pendingPermissionAction = null;
            Toast.makeText(this, "需要附近设备权限才能搜索HC-42", Toast.LENGTH_LONG).show();
        }
    }

    private void startReaderAction(String action, String mac, byte[] payload) {
        Intent intent = new Intent(this, MeterPollingService.class);
        intent.setAction(action);
        if (mac != null) {
            intent.putExtra(MeterPollingService.EXTRA_MAC, mac);
        }
        if (payload != null) {
            intent.putExtra(MeterPollingService.EXTRA_PAYLOAD, payload);
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
                && !MeterPollingService.ACTION_STOP.equals(action)) {
            startForegroundService(intent);
        } else {
            startService(intent);
        }
    }

    private void registerEventReceiver() {
        IntentFilter filter = new IntentFilter();
        filter.addAction(MeterPollingService.ACTION_DEVICE);
        filter.addAction(MeterPollingService.ACTION_RAW);
        filter.addAction(MeterPollingService.ACTION_READING);
        filter.addAction(MeterPollingService.ACTION_STATE);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(eventReceiver, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(eventReceiver, filter);
        }
    }

    private void saveDialogOptions() {
        if (dialogHex != null && appendCrLf != null) {
            preferences.saveDialogOptions(dialogHex.isChecked(), appendCrLf.isChecked());
        }
    }

    private LinearLayout pageContainer() {
        LinearLayout page = new LinearLayout(this);
        page.setOrientation(LinearLayout.VERTICAL);
        page.setPadding(dp(16), dp(16), dp(16), dp(20));
        return page;
    }

    private LinearLayout horizontal() {
        LinearLayout layout = new LinearLayout(this);
        layout.setOrientation(LinearLayout.HORIZONTAL);
        layout.setGravity(Gravity.CENTER_VERTICAL);
        return layout;
    }

    private TextView sectionTitle(String value) {
        TextView view = text(value, 24, PRIMARY_DARK);
        view.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        view.setPadding(0, 0, 0, dp(8));
        return view;
    }

    private TextView subheading(String value) {
        TextView view = text(value, 17, PRIMARY_DARK);
        view.setTypeface(Typeface.DEFAULT, Typeface.BOLD);
        view.setPadding(0, dp(20), 0, dp(8));
        return view;
    }

    private TextView bodyText(String value) {
        TextView view = text(value, 14, Color.DKGRAY);
        view.setLineSpacing(0, 1.2f);
        view.setPadding(0, 0, 0, dp(12));
        return view;
    }

    private TextView infoCard(String value) {
        TextView view = text(value, 14, TEXT);
        view.setLineSpacing(0, 1.25f);
        view.setPadding(dp(14), dp(14), dp(14), dp(14));
        view.setBackground(rounded(GLASS, 20, GLASS_STROKE));
        view.setElevation(dp(7));
        return view;
    }

    private TextView text(String value, int sp, int color) {
        TextView view = new TextView(this);
        view.setText(value);
        view.setTextSize(sp);
        view.setTextColor(color);
        return view;
    }

    private Button actionButton(String value, int color) {
        Button button = new Button(this);
        button.setText(value);
        button.setTextColor(Color.WHITE);
        button.setTextSize(14);
        button.setAllCaps(false);
        button.setMinHeight(0);
        button.setMinimumHeight(0);
        button.setBackground(rounded(color, 15, Color.TRANSPARENT));
        button.setElevation(dp(4));
        return button;
    }

    private EditText numericInput(String hint, int value) {
        EditText input = new EditText(this);
        input.setHint(hint);
        input.setText(Integer.toString(value));
        input.setTextSize(16);
        input.setSingleLine(true);
        input.setInputType(InputType.TYPE_CLASS_NUMBER);
        input.setPadding(dp(12), dp(10), dp(12), dp(10));
        input.setBackground(rounded(GLASS, 16, GLASS_STROKE));
        input.setElevation(dp(4));
        return input;
    }

    private GradientDrawable rounded(int fill, int radiusDp, int stroke) {
        GradientDrawable drawable = new GradientDrawable();
        drawable.setColor(fill);
        drawable.setCornerRadius(dp(radiusDp));
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
                return Color.rgb(230, 115, 0);
            case MeterReading.OFFLINE:
                return Color.rgb(90, 98, 104);
            default:
                return Color.rgb(27, 120, 62);
        }
    }

    private int statusBackground(String status) {
        switch (status) {
            case MeterReading.HIGH:
                return Color.argb(230, 255, 232, 232);
            case MeterReading.LOW:
                return Color.argb(230, 255, 244, 218);
            case MeterReading.OFFLINE:
                return Color.argb(230, 232, 235, 237);
            default:
                return Color.argb(230, 230, 247, 236);
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

    private LinearLayout.LayoutParams weighted() {
        return new LinearLayout.LayoutParams(0, dp(48), 1f);
    }

    private LinearLayout.LayoutParams weightedWithLeftMargin() {
        LinearLayout.LayoutParams params = weighted();
        params.leftMargin = dp(8);
        return params;
    }

    private static final class DeviceInfo {
        final String name;
        final String mac;
        final int rssi;

        DeviceInfo(String name, String mac, int rssi) {
            this.name = name == null ? "未知设备" : name;
            this.mac = mac;
            this.rssi = rssi;
        }
    }

    private static final class RawRecord {
        final long timestamp;
        final String direction;
        final byte[] payload;

        RawRecord(long timestamp, String direction, byte[] payload) {
            this.timestamp = timestamp;
            this.direction = direction == null ? "RX" : direction;
            this.payload = Arrays.copyOf(payload, payload.length);
        }
    }
}
