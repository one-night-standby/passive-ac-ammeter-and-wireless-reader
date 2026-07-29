package com.jun.nuedc.reader;

import android.Manifest;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothGatt;
import android.bluetooth.BluetoothGattCallback;
import android.bluetooth.BluetoothGattCharacteristic;
import android.bluetooth.BluetoothGattDescriptor;
import android.bluetooth.BluetoothGattService;
import android.bluetooth.BluetoothManager;
import android.bluetooth.BluetoothProfile;
import android.bluetooth.le.BluetoothLeScanner;
import android.bluetooth.le.ScanCallback;
import android.bluetooth.le.ScanResult;
import android.bluetooth.le.ScanSettings;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.util.Log;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Queue;
import java.util.Set;
import java.util.UUID;

public final class MeterPollingService extends Service {
    public static final String ACTION_CONNECT_BASIC =
            "com.jun.nuedc.reader.action.CONNECT_BASIC";
    public static final String ACTION_MANUAL_READ =
            "com.jun.nuedc.reader.action.MANUAL_READ";
    public static final String ACTION_START_AUTO =
            "com.jun.nuedc.reader.action.START_AUTO";
    public static final String ACTION_STOP_AUTO =
            "com.jun.nuedc.reader.action.STOP_AUTO";
    public static final String ACTION_READING =
            "com.jun.nuedc.reader.event.READING";
    public static final String ACTION_STATE =
            "com.jun.nuedc.reader.event.STATE";

    public static final String EXTRA_ADDRESS = "address";
    public static final String EXTRA_CURRENT_MA = "current_ma";
    public static final String EXTRA_STATUS = "status";
    public static final String EXTRA_TIMESTAMP = "timestamp";
    public static final String EXTRA_STATE = "state";
    public static final String EXTRA_DETAIL = "detail";
    public static final String EXTRA_AUTO = "auto";

    private static final String TAG = "MeterPollingService";
    private static final String NOTIFICATION_CHANNEL = "meter_reader_service";
    private static final int NOTIFICATION_ID = 100;
    private static final long BASIC_RECONNECT_DELAY_MS = 1_500L;

    private static final UUID SERVICE_UUID =
            UUID.fromString("0000ffe0-0000-1000-8000-00805f9b34fb");
    private static final UUID CHARACTERISTIC_UUID =
            UUID.fromString("0000ffe1-0000-1000-8000-00805f9b34fb");
    private static final UUID CLIENT_CONFIG_UUID =
            UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Map<String, ScanTarget> scannedTargets = new HashMap<>();
    private final Queue<ScanTarget> readQueue = new ArrayDeque<>();
    private final Set<String> successfulMacs = new HashSet<>();
    private final MeterFrameParser parser = new MeterFrameParser();

    private BluetoothAdapter adapter;
    private BluetoothLeScanner scanner;
    private BluetoothGatt currentGatt;
    private BluetoothGattCharacteristic meterCharacteristic;
    private ScanTarget currentTarget;
    private MeterDatabase database;
    private NotificationManager notificationManager;

    private boolean autoMode;
    private boolean basicMode;
    private boolean operationActive;
    private boolean scanning;
    private boolean manualReadRequested;
    private String currentSource = "BASIC";
    private long cycleStartedAt;

    private final Runnable scanTimeout = this::finishScan;
    private final Runnable connectionTimeout = () -> failCurrent("连接超时");
    private final Runnable frameTimeout = this::handleFrameTimeout;
    private final Runnable nextCycle = this::beginAutoCycle;
    private final Runnable reconnectBasic = () -> {
        if (basicMode && !autoMode && !operationActive) {
            beginScan("BASIC");
        }
    };

    private final ScanCallback scanCallback = new ScanCallback() {
        @Override
        public void onScanResult(int callbackType, ScanResult result) {
            handleScanResult(result);
        }

        @Override
        public void onBatchScanResults(List<ScanResult> results) {
            for (ScanResult result : results) {
                handleScanResult(result);
            }
        }

        @Override
        public void onScanFailed(int errorCode) {
            handler.post(() -> {
                broadcastState("ERROR", "蓝牙扫描失败：" + errorCode);
                finishScan();
            });
        }
    };

    private final BluetoothGattCallback gattCallback = new BluetoothGattCallback() {
        @Override
        public void onConnectionStateChange(BluetoothGatt gatt, int status, int newState) {
            handler.post(() -> handleConnectionState(gatt, status, newState));
        }

        @Override
        public void onServicesDiscovered(BluetoothGatt gatt, int status) {
            handler.post(() -> enableMeterNotifications(gatt, status));
        }

        @Override
        @SuppressWarnings("deprecation")
        public void onCharacteristicChanged(
                BluetoothGatt gatt,
                BluetoothGattCharacteristic characteristic
        ) {
            byte[] value = characteristic.getValue();
            handler.post(() -> handleCharacteristic(gatt, characteristic, value));
        }

        @Override
        public void onCharacteristicChanged(
                BluetoothGatt gatt,
                BluetoothGattCharacteristic characteristic,
                byte[] value
        ) {
            byte[] copy = value == null ? null : value.clone();
            handler.post(() -> handleCharacteristic(gatt, characteristic, copy));
        }
    };

    @Override
    public void onCreate() {
        super.onCreate();
        database = new MeterDatabase(this);
        notificationManager = getSystemService(NotificationManager.class);
        BluetoothManager manager = getSystemService(BluetoothManager.class);
        adapter = manager == null ? null : manager.getAdapter();
        createNotificationChannel();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent == null || intent.getAction() == null) {
            return START_NOT_STICKY;
        }

        startForeground(NOTIFICATION_ID, buildNotification("正在准备蓝牙…"));
        if (!hasBlePermissions()) {
            broadcastState("ERROR", "缺少附近设备权限");
            return START_NOT_STICKY;
        }
        if (adapter == null || !adapter.isEnabled()) {
            broadcastState("ERROR", "请先打开手机蓝牙");
            return START_NOT_STICKY;
        }

        String action = intent.getAction();
        if (ACTION_CONNECT_BASIC.equals(action)) {
            ensureBasicConnection();
        } else if (ACTION_MANUAL_READ.equals(action)) {
            requestManualRead();
        } else if (ACTION_START_AUTO.equals(action)) {
            startAutoMode();
        } else if (ACTION_STOP_AUTO.equals(action)) {
            switchToBasicMode();
        }
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onDestroy() {
        cancelOperation();
        database.close();
        super.onDestroy();
    }

    private void ensureBasicConnection() {
        if (autoMode) {
            broadcastState("AUTO_WAIT", "自动模式运行中，每2分钟采集一次");
            return;
        }
        if (basicMode && currentGatt != null && meterCharacteristic != null) {
            broadcastState("CONNECTED", "基本模式已连接 " + displayName(currentTarget));
            return;
        }
        basicMode = true;
        manualReadRequested = false;
        cancelOperation();
        beginScan("BASIC");
    }

    private void requestManualRead() {
        if (basicMode && !autoMode && currentGatt != null && meterCharacteristic != null) {
            manualReadRequested = true;
            parser.reset();
            handler.removeCallbacks(frameTimeout);
            handler.postDelayed(frameTimeout, ReaderPreferences.FRAME_TIMEOUT_MS);
            broadcastState("WAITING", "基本模式：等待下一帧电流数据");
            updateNotification("基本模式：正在读取");
            return;
        }

        autoMode = false;
        basicMode = true;
        manualReadRequested = true;
        cancelOperation();
        beginScan("BASIC");
    }

    private void startAutoMode() {
        autoMode = true;
        basicMode = false;
        manualReadRequested = false;
        cancelOperation();
        beginAutoCycle();
    }

    private void switchToBasicMode() {
        autoMode = false;
        basicMode = true;
        manualReadRequested = false;
        cancelOperation();
        beginScan("BASIC");
    }

    private void beginAutoCycle() {
        if (!autoMode) {
            return;
        }
        cycleStartedAt = System.currentTimeMillis();
        beginScan("AUTO");
    }

    private void beginScan(String source) {
        if (operationActive) {
            return;
        }
        currentSource = source;
        operationActive = true;
        scanning = true;
        scannedTargets.clear();
        readQueue.clear();
        successfulMacs.clear();
        parser.reset();

        scanner = adapter.getBluetoothLeScanner();
        if (scanner == null) {
            broadcastState("ERROR", "无法取得BLE扫描器");
            handleNoTargets();
            return;
        }

        ScanSettings settings = new ScanSettings.Builder()
                .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
                .build();
        try {
            scanner.startScan(null, settings, scanCallback);
            String detail = "AUTO".equals(source)
                    ? "自动模式：正在搜索覆盖范围内的电流表"
                    : "基本模式：正在自动连接HC-42";
            broadcastState("SCANNING", detail);
            updateNotification(detail);
            handler.postDelayed(scanTimeout, ReaderPreferences.SCAN_DURATION_MS);
        } catch (SecurityException | IllegalStateException exception) {
            Log.e(TAG, "Unable to start BLE scan", exception);
            broadcastState("ERROR", "无法开始扫描：" + exception.getMessage());
            handleNoTargets();
        }
    }

    private void handleScanResult(ScanResult result) {
        if (!scanning || result == null || result.getDevice() == null) {
            return;
        }
        BluetoothDevice device = result.getDevice();
        String name = safeDeviceName(device);
        if ("未知设备".equals(name)
                && result.getScanRecord() != null
                && result.getScanRecord().getDeviceName() != null) {
            name = result.getScanRecord().getDeviceName();
        }
        scannedTargets.put(
                device.getAddress(),
                new ScanTarget(device, name, device.getAddress(), result.getRssi())
        );
        if ("BASIC".equals(currentSource) && isMeterCandidate(name)) {
            // Basic mode is one-to-one: connect as soon as the first HC-42 is found.
            finishScan();
        }
    }

    private void finishScan() {
        if (!scanning) {
            return;
        }
        scanning = false;
        handler.removeCallbacks(scanTimeout);
        try {
            if (scanner != null) {
                scanner.stopScan(scanCallback);
            }
        } catch (SecurityException exception) {
            Log.w(TAG, "Unable to stop scan", exception);
        }

        List<ScanTarget> targets = new ArrayList<>();
        for (ScanTarget target : scannedTargets.values()) {
            if (isMeterCandidate(target.name)) {
                targets.add(target);
            }
        }
        targets.sort(Comparator.comparingInt((ScanTarget target) -> target.rssi).reversed());
        if ("BASIC".equals(currentSource) && !targets.isEmpty()) {
            readQueue.add(targets.get(0));
        } else {
            readQueue.addAll(targets);
        }

        if (readQueue.isEmpty()) {
            handleNoTargets();
            return;
        }

        broadcastState(
                "CONNECTING",
                "AUTO".equals(currentSource)
                        ? String.format(Locale.CHINA, "发现%d台电流表，开始自动采集", readQueue.size())
                        : "发现HC-42，正在自动连接"
        );
        connectNext();
    }

    private void handleNoTargets() {
        operationActive = false;
        scanning = false;
        if (basicMode && !autoMode) {
            broadcastState("RECONNECTING", "未发现HC-42，稍后自动重试");
            updateNotification("未发现HC-42，正在重试");
            handler.removeCallbacks(reconnectBasic);
            handler.postDelayed(reconnectBasic, BASIC_RECONNECT_DELAY_MS);
        } else {
            finishAutoCycle();
        }
    }

    private void connectNext() {
        handler.removeCallbacks(connectionTimeout);
        handler.removeCallbacks(frameTimeout);
        closeCurrentGatt();
        parser.reset();
        currentTarget = readQueue.poll();
        if (currentTarget == null) {
            finishAutoCycle();
            return;
        }

        broadcastState("CONNECTING", "正在连接 " + displayName(currentTarget));
        updateNotification("正在连接 " + displayName(currentTarget));
        try {
            currentGatt = currentTarget.device.connectGatt(
                    this,
                    false,
                    gattCallback,
                    BluetoothDevice.TRANSPORT_LE
            );
            if (currentGatt == null) {
                failCurrent("无法创建GATT连接");
                return;
            }
            handler.postDelayed(connectionTimeout, ReaderPreferences.CONNECTION_TIMEOUT_MS);
        } catch (SecurityException exception) {
            failCurrent("连接权限错误");
        }
    }

    private void handleConnectionState(BluetoothGatt gatt, int status, int newState) {
        if (gatt != currentGatt) {
            safeClose(gatt);
            return;
        }
        if (newState == BluetoothProfile.STATE_CONNECTED
                && status == BluetoothGatt.GATT_SUCCESS) {
            handler.removeCallbacks(connectionTimeout);
            try {
                gatt.requestConnectionPriority(BluetoothGatt.CONNECTION_PRIORITY_HIGH);
                if (!gatt.discoverServices()) {
                    failCurrent("无法发现GATT服务");
                } else {
                    handler.postDelayed(
                            connectionTimeout,
                            ReaderPreferences.CONNECTION_TIMEOUT_MS
                    );
                }
            } catch (SecurityException exception) {
                failCurrent("服务发现权限错误");
            }
        } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
            handler.removeCallbacks(connectionTimeout);
            handler.removeCallbacks(frameTimeout);
            failCurrent("设备已断开");
        }
    }

    @SuppressWarnings("deprecation")
    private void enableMeterNotifications(BluetoothGatt gatt, int status) {
        if (gatt != currentGatt) {
            return;
        }
        handler.removeCallbacks(connectionTimeout);
        if (status != BluetoothGatt.GATT_SUCCESS) {
            failCurrent("服务发现失败");
            return;
        }

        BluetoothGattService service = gatt.getService(SERVICE_UUID);
        BluetoothGattCharacteristic characteristic =
                service == null ? null : service.getCharacteristic(CHARACTERISTIC_UUID);
        if (characteristic == null) {
            failCurrent("未找到HC-42的FFE0/FFE1服务");
            return;
        }

        try {
            meterCharacteristic = characteristic;
            if (!gatt.setCharacteristicNotification(characteristic, true)) {
                failCurrent("无法启用FFE1通知");
                return;
            }
            BluetoothGattDescriptor descriptor = characteristic.getDescriptor(CLIENT_CONFIG_UUID);
            if (descriptor == null) {
                failCurrent("FFE1缺少通知描述符");
                return;
            }
            descriptor.setValue(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE);
            if (!gatt.writeDescriptor(descriptor)) {
                failCurrent("启用FFE1通知失败");
                return;
            }

            if (basicMode && !autoMode) {
                operationActive = true;
                broadcastState("CONNECTED", "基本模式已连接 " + displayName(currentTarget));
                updateNotification("基本模式已连接，等待手动读取");
                if (manualReadRequested) {
                    handler.postDelayed(frameTimeout, ReaderPreferences.FRAME_TIMEOUT_MS);
                    broadcastState("WAITING", "基本模式：等待下一帧电流数据");
                }
            } else {
                broadcastState("WAITING", "自动模式：等待电流数据");
                handler.postDelayed(frameTimeout, ReaderPreferences.FRAME_TIMEOUT_MS);
            }
        } catch (SecurityException exception) {
            failCurrent("启用通知权限错误");
        }
    }

    private void handleCharacteristic(
            BluetoothGatt gatt,
            BluetoothGattCharacteristic characteristic,
            byte[] value
    ) {
        if (gatt != currentGatt || !CHARACTERISTIC_UUID.equals(characteristic.getUuid())) {
            return;
        }
        List<MeterFrameParser.ParsedFrame> frames = parser.feed(value);
        if (frames.isEmpty()) {
            return;
        }
        if (basicMode && !autoMode && !manualReadRequested) {
            return;
        }

        MeterFrameParser.ParsedFrame frame = frames.get(frames.size() - 1);
        handler.removeCallbacks(frameTimeout);
        ScanTarget target = currentTarget;
        if (target == null) {
            return;
        }

        String status = MeterReading.classify(
                frame.currentMa,
                ReaderPreferences.DEFAULT_LOW_THRESHOLD_MA,
                ReaderPreferences.DEFAULT_HIGH_THRESHOLD_MA
        );
        MeterReading reading = database.insertReading(new MeterReading(
                0,
                System.currentTimeMillis(),
                frame.address,
                frame.currentMa,
                status,
                target.mac,
                target.name,
                target.rssi,
                currentSource
        ));
        successfulMacs.add(target.mac);
        broadcastReading(reading);

        String result = String.format(
                Locale.CHINA,
                "%02d号表：%s",
                reading.address,
                reading.currentText()
        );
        if (basicMode && !autoMode) {
            manualReadRequested = false;
            broadcastState("CONNECTED", "基本模式读取完成：" + result);
            updateNotification("基本模式已连接，等待手动读取");
        } else {
            broadcastState("READ_OK", "自动采集：" + result);
            handler.postDelayed(this::connectNext, 250L);
        }
    }

    private void handleFrameTimeout() {
        if (basicMode && !autoMode && currentGatt != null) {
            manualReadRequested = false;
            broadcastState("ERROR", "等待电流数据超时，请重新手动读取");
            updateNotification("基本模式已连接，等待手动读取");
        } else {
            failCurrent("等待数据超时");
        }
    }

    private void failCurrent(String reason) {
        Log.w(TAG, reason);
        handler.removeCallbacks(connectionTimeout);
        handler.removeCallbacks(frameTimeout);
        closeCurrentGatt();
        currentTarget = null;

        if (basicMode && !autoMode) {
            operationActive = false;
            broadcastState("RECONNECTING", reason + "，正在自动重连");
            updateNotification("基本模式：正在自动重连");
            handler.removeCallbacks(reconnectBasic);
            handler.postDelayed(reconnectBasic, BASIC_RECONNECT_DELAY_MS);
        } else {
            handler.postDelayed(this::connectNext, 250L);
        }
    }

    private void finishAutoCycle() {
        if (!autoMode) {
            return;
        }
        List<MeterReading> offline =
                database.markMissingMetersOffline(successfulMacs, System.currentTimeMillis());
        for (MeterReading reading : offline) {
            broadcastReading(reading);
        }

        int successCount = successfulMacs.size();
        operationActive = false;
        currentTarget = null;
        closeCurrentGatt();
        broadcastState(
                "AUTO_WAIT",
                String.format(Locale.CHINA, "本轮完成：成功%d台，2分钟后再次采集", successCount)
        );

        long nextAt = Math.max(
                System.currentTimeMillis() + 1_000L,
                cycleStartedAt + ReaderPreferences.POLLING_INTERVAL_MS
        );
        long delay = nextAt - System.currentTimeMillis();
        updateNotification(
                String.format(Locale.CHINA, "%d秒后开始下一轮", Math.max(1, delay / 1_000L))
        );
        handler.removeCallbacks(nextCycle);
        handler.postDelayed(nextCycle, delay);
    }

    private void cancelOperation() {
        handler.removeCallbacks(nextCycle);
        handler.removeCallbacks(reconnectBasic);
        handler.removeCallbacks(scanTimeout);
        handler.removeCallbacks(connectionTimeout);
        handler.removeCallbacks(frameTimeout);
        scanning = false;
        operationActive = false;
        if (scanner != null && hasBlePermissions()) {
            try {
                scanner.stopScan(scanCallback);
            } catch (SecurityException ignored) {
                // Permission may be revoked while the service is running.
            }
        }
        readQueue.clear();
        closeCurrentGatt();
    }

    private void closeCurrentGatt() {
        BluetoothGatt gatt = currentGatt;
        currentGatt = null;
        meterCharacteristic = null;
        if (gatt == null) {
            return;
        }
        try {
            if (hasBlePermissions()) {
                gatt.disconnect();
            }
        } catch (SecurityException ignored) {
            // Close is still attempted below.
        }
        safeClose(gatt);
    }

    private void safeClose(BluetoothGatt gatt) {
        if (gatt != null) {
            try {
                gatt.close();
            } catch (RuntimeException ignored) {
                // Some vendor Bluetooth stacks throw when already closed.
            }
        }
    }

    private boolean isMeterCandidate(String name) {
        String normalized = name == null ? "" : name.toUpperCase(Locale.ROOT);
        return normalized.contains("HC-42")
                || normalized.contains("HC42")
                || normalized.contains("METER")
                || normalized.contains("AMMETER")
                || normalized.startsWith("BYJX_");
    }

    private String safeDeviceName(BluetoothDevice device) {
        try {
            String name = device.getName();
            return name == null || name.trim().isEmpty() ? "未知设备" : name;
        } catch (SecurityException exception) {
            return "未知设备";
        }
    }

    private String displayName(ScanTarget target) {
        if (target == null || target.name == null || target.name.isEmpty()) {
            return "HC-42";
        }
        return target.name;
    }

    private boolean hasBlePermissions() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) {
            return checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION)
                    == PackageManager.PERMISSION_GRANTED;
        }
        return checkSelfPermission(Manifest.permission.BLUETOOTH_SCAN)
                == PackageManager.PERMISSION_GRANTED
                && checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT)
                == PackageManager.PERMISSION_GRANTED;
    }

    private void broadcastReading(MeterReading reading) {
        Intent intent = eventIntent(ACTION_READING);
        intent.putExtra(EXTRA_ADDRESS, reading.address);
        intent.putExtra(EXTRA_CURRENT_MA, reading.currentMa);
        intent.putExtra(EXTRA_STATUS, reading.status);
        intent.putExtra(EXTRA_TIMESTAMP, reading.timestamp);
        sendBroadcast(intent);
    }

    private void broadcastState(String state, String detail) {
        Intent intent = eventIntent(ACTION_STATE);
        intent.putExtra(EXTRA_STATE, state);
        intent.putExtra(EXTRA_DETAIL, detail);
        intent.putExtra(EXTRA_AUTO, autoMode);
        sendBroadcast(intent);
    }

    private Intent eventIntent(String action) {
        Intent intent = new Intent(action);
        intent.setPackage(getPackageName());
        return intent;
    }

    private void createNotificationChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return;
        }
        NotificationChannel channel = new NotificationChannel(
                NOTIFICATION_CHANNEL,
                getString(R.string.notification_channel),
                NotificationManager.IMPORTANCE_LOW
        );
        notificationManager.createNotificationChannel(channel);
    }

    private Notification buildNotification(String text) {
        PendingIntent pendingIntent = PendingIntent.getActivity(
                this,
                0,
                new Intent(this, MainActivity.class),
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE
        );
        return new Notification.Builder(this, NOTIFICATION_CHANNEL)
                .setSmallIcon(R.drawable.ic_meter)
                .setContentTitle(getString(R.string.notification_title))
                .setContentText(text)
                .setContentIntent(pendingIntent)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .build();
    }

    private void updateNotification(String text) {
        notificationManager.notify(NOTIFICATION_ID, buildNotification(text));
    }

    private static final class ScanTarget {
        final BluetoothDevice device;
        final String name;
        final String mac;
        final int rssi;

        ScanTarget(BluetoothDevice device, String name, String mac, int rssi) {
            this.device = device;
            this.name = name;
            this.mac = mac;
            this.rssi = rssi;
        }
    }
}
