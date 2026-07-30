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
import android.os.SystemClock;
import android.util.Log;

import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.TreeSet;
import java.util.UUID;

/**
 * 链路自治的帧流服务:发现覆盖范围内的所有 HC-42 就全部接入,断了退避重连,
 * 每一帧都广播给界面。前端的任何操作都不影响通信 —— 手动/自动只决定存不存库:
 * 手动存档在界面侧完成;自动模式在这里按周期把各表「此刻的最新帧」快照落库
 * (1.5 秒内有帧存值,否则存离线),语义与界面原型的 autoRound 一致。
 *
 * 并发模型:回调驱动 + 每台表一个链路状态机。已建立的连接并行收数,
 * 但同一时刻只发起一条新链(多数机型的协议栈按序建链,并发发起容易 133)。
 */
public final class MeterPollingService extends Service {
    public static final String ACTION_CONNECT_BASIC =
            "com.jun.nuedc.reader.action.CONNECT_BASIC";       // 兼容旧名:启动链路管理
    public static final String ACTION_START_AUTO =
            "com.jun.nuedc.reader.action.START_AUTO";
    public static final String ACTION_STOP_AUTO =
            "com.jun.nuedc.reader.action.STOP_AUTO";
    public static final String ACTION_UPDATE_INTERVAL =
            "com.jun.nuedc.reader.action.UPDATE_INTERVAL";
    public static final String ACTION_READING =
            "com.jun.nuedc.reader.event.READING";
    public static final String ACTION_STATE =
            "com.jun.nuedc.reader.event.STATE";
    /** 实时帧:帧一直在流,只进界面不落库。 */
    public static final String ACTION_FRAME =
            "com.jun.nuedc.reader.event.FRAME";

    public static final String EXTRA_ADDRESS = "address";
    public static final String EXTRA_CURRENT_MA = "current_ma";
    public static final String EXTRA_STATUS = "status";
    public static final String EXTRA_TIMESTAMP = "timestamp";
    public static final String EXTRA_STATE = "state";
    public static final String EXTRA_DETAIL = "detail";
    public static final String EXTRA_AUTO = "auto";
    public static final String EXTRA_MAC = "mac";
    public static final String EXTRA_NAME = "name";
    public static final String EXTRA_RSSI = "rssi";

    private static final String TAG = "MeterPollingService";
    private static final String NOTIFICATION_CHANNEL = "meter_reader_service";
    private static final int NOTIFICATION_ID = 100;

    private static final long SCAN_ON_MS = 8_000L;             // 扫 8 秒歇 12 秒,避开系统节流
    private static final long SCAN_OFF_MS = 12_000L;
    private static final long PUMP_MS = 1_000L;
    private static final long FRESH_MS = 1_500L;               // 与界面的静默判定一致
    private static final long RETRY_MS = 1_500L;
    private static final long RETRY_SLOW_MS = 10_000L;         // 连败三次后放慢,别拖累别的链
    private static final int RETRY_SLOW_AFTER = 3;
    /* 表数超过手机控制器的并发上限时轮流连:满员后把驻留最久的一台轮出去,
       让排队的接入。多数手机支持 ~7 条并发 GATT,取保守值。 */
    private static final int MAX_ACTIVE_LINKS = 7;
    private static final long ROTATE_DWELL_MS = 4_000L;        // 每台至少驻留这么久且拿到帧才轮出
    private static final long PARKED_FRESH_MS = 120_000L;      // 轮歇中的表:上一帧在此窗口内即算在场
                                                               // (16 台满轮转最坏 ~60-80 s,留余量)

    private static final UUID SERVICE_UUID =
            UUID.fromString("0000ffe0-0000-1000-8000-00805f9b34fb");
    private static final UUID CHARACTERISTIC_UUID =
            UUID.fromString("0000ffe1-0000-1000-8000-00805f9b34fb");
    private static final UUID CLIENT_CONFIG_UUID =
            UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");

    private static final int LINK_IDLE = 0;
    private static final int LINK_CONNECTING = 1;
    private static final int LINK_SUBSCRIBED = 2;

    /** 一台 HC-42 一条链:独立解析缓冲、独立重试节奏。 */
    private static final class MeterLink {
        BluetoothDevice device;
        String name;
        final String mac;
        int rssi;
        BluetoothGatt gatt;
        final MeterFrameParser parser = new MeterFrameParser();
        int state = LINK_IDLE;
        long lastDataAt;                                       // elapsedRealtime,任意通知都算
        int address = -1;                                      // 帧里报出来的编码开关地址
        int lastMa = -1;
        long lastFrameAt;                                      // elapsedRealtime
        long subscribedAt;                                     // elapsedRealtime,本次会话订阅时刻
        boolean parked;                                        // 健康,只是被轮出去让位
        int failures;
        long nextAttemptAt;

        MeterLink(BluetoothDevice device, String name, String mac, int rssi) {
            this.device = device;
            this.name = name;
            this.mac = mac;
            this.rssi = rssi;
        }
    }

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Map<String, MeterLink> links = new LinkedHashMap<>();

    private BluetoothAdapter adapter;
    private BluetoothLeScanner scanner;
    private MeterDatabase database;
    private ReaderPreferences preferences;
    private NotificationManager notificationManager;

    private boolean running;
    private boolean scanning;
    private boolean autoMode;
    private MeterLink connectingLink;
    private long nextCycleAtElapsedMs;
    private int lastSubscribedCount = -1;
    private String lastNotificationText = "";

    private final Runnable scanOn = this::startScanWindow;
    private final Runnable scanOff = this::stopScanWindow;
    private final Runnable pump = this::pumpLinks;
    private final Runnable connectTimeout = () -> {
        if (connectingLink != null) {
            linkDown(connectingLink, "连接超时");
        }
    };
    private final Runnable autoRoundRunnable = this::autoRound;
    private final Runnable countdownTick = this::updateCountdownNotification;

    private final ScanCallback scanCallback = new ScanCallback() {
        @Override
        public void onScanResult(int callbackType, ScanResult result) {
            handler.post(() -> handleScanResult(result));
        }

        @Override
        public void onBatchScanResults(List<ScanResult> results) {
            handler.post(() -> {
                for (ScanResult result : results) {
                    handleScanResult(result);
                }
            });
        }

        @Override
        public void onScanFailed(int errorCode) {
            handler.post(() -> broadcastState("ERROR", "蓝牙扫描失败：" + errorCode));
        }
    };

    private final BluetoothGattCallback gattCallback = new BluetoothGattCallback() {
        @Override
        public void onConnectionStateChange(BluetoothGatt gatt, int status, int newState) {
            handler.post(() -> handleConnectionState(gatt, status, newState));
        }

        @Override
        public void onServicesDiscovered(BluetoothGatt gatt, int status) {
            handler.post(() -> handleServicesDiscovered(gatt, status));
        }

        @Override
        public void onDescriptorWrite(BluetoothGatt gatt, BluetoothGattDescriptor descriptor,
                                      int status) {
            handler.post(() -> handleDescriptorWrite(gatt, status));
        }

        @Override
        @SuppressWarnings("deprecation")
        public void onCharacteristicChanged(
                BluetoothGatt gatt,
                BluetoothGattCharacteristic characteristic
        ) {
            byte[] value = characteristic.getValue();
            handler.post(() -> handleData(gatt, characteristic, value));
        }

        @Override
        public void onCharacteristicChanged(
                BluetoothGatt gatt,
                BluetoothGattCharacteristic characteristic,
                byte[] value
        ) {
            byte[] copy = value == null ? null : value.clone();
            handler.post(() -> handleData(gatt, characteristic, copy));
        }
    };

    @Override
    public void onCreate() {
        super.onCreate();
        database = new MeterDatabase(this);
        preferences = new ReaderPreferences(this);
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
            startLinks();
        } else if (ACTION_START_AUTO.equals(action)) {
            startLinks();
            autoMode = true;
            autoRound();                                       // 一键启动:立刻先存一轮
        } else if (ACTION_STOP_AUTO.equals(action)) {
            autoMode = false;
            handler.removeCallbacks(autoRoundRunnable);
            handler.removeCallbacks(countdownTick);
            nextCycleAtElapsedMs = 0L;
            startLinks();                                      // 链路照常在线,只是不再定时落库
            broadcastState("CONNECTED", "自动存档已停止，帧流保持在线");
            updateNotification(streamingText());
        } else if (ACTION_UPDATE_INTERVAL.equals(action)) {
            if (autoMode) {
                broadcastState(
                        "AUTO_WAIT",
                        String.format(
                                Locale.CHINA,
                                "采集间隔已设为%s，重新计时",
                                preferences.pollingIntervalText()
                        )
                );
                scheduleRound(preferences.pollingIntervalMs());
            }
        }
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public void onDestroy() {
        running = false;
        handler.removeCallbacksAndMessages(null);
        stopScanQuietly();
        for (MeterLink link : links.values()) {
            closeGatt(link);
        }
        links.clear();
        database.close();
        super.onDestroy();
    }

    /* ══════════ 链路管理:与采集模式无关,常驻 ══════════ */
    private void startLinks() {
        if (running) {
            return;
        }
        running = true;
        broadcastState("SCANNING", "正在搜索覆盖范围内的电流表");
        updateNotification("正在搜索电流表");
        handler.removeCallbacks(scanOn);
        handler.removeCallbacks(scanOff);
        handler.removeCallbacks(pump);
        handler.post(scanOn);
        handler.postDelayed(pump, PUMP_MS);
    }

    private void startScanWindow() {
        if (!running || adapter == null) {
            return;
        }
        scanner = adapter.getBluetoothLeScanner();
        if (scanner == null) {
            broadcastState("ERROR", "无法取得BLE扫描器");
            handler.postDelayed(scanOn, SCAN_OFF_MS);
            return;
        }
        ScanSettings settings = new ScanSettings.Builder()
                .setScanMode(ScanSettings.SCAN_MODE_BALANCED)
                .build();
        try {
            scanner.startScan(null, settings, scanCallback);
            scanning = true;
        } catch (SecurityException | IllegalStateException exception) {
            Log.w(TAG, "Unable to start BLE scan", exception);
        }
        handler.postDelayed(scanOff, SCAN_ON_MS);
    }

    private void stopScanWindow() {
        stopScanQuietly();
        if (running) {
            handler.postDelayed(scanOn, SCAN_OFF_MS);
        }
    }

    private void stopScanQuietly() {
        if (!scanning) {
            return;
        }
        scanning = false;
        try {
            if (scanner != null && hasBlePermissions()) {
                scanner.stopScan(scanCallback);
            }
        } catch (SecurityException | IllegalStateException exception) {
            Log.w(TAG, "Unable to stop scan", exception);
        }
    }

    private void handleScanResult(ScanResult result) {
        if (!running || result == null || result.getDevice() == null) {
            return;
        }
        BluetoothDevice device = result.getDevice();
        String name = safeDeviceName(device);
        if ("未知设备".equals(name)
                && result.getScanRecord() != null
                && result.getScanRecord().getDeviceName() != null) {
            name = result.getScanRecord().getDeviceName();
        }
        if (!isMeterCandidate(name)) {
            return;
        }
        String mac = device.getAddress();
        MeterLink link = links.get(mac);
        if (link == null) {
            link = new MeterLink(device, name, mac, result.getRssi());
            links.put(mac, link);
            broadcastState("CONNECTING", "发现 " + name + "，接入帧流");
        } else {
            link.device = device;
            link.name = name;
            link.rssi = result.getRssi();
        }
    }

    /** 每秒一拍:看门狗 + 建链队列(同一时刻只发起一条)+ 满员轮歇。 */
    private void pumpLinks() {
        if (!running) {
            return;
        }
        long now = SystemClock.elapsedRealtime();
        for (MeterLink link : links.values()) {
            if (link.state == LINK_SUBSCRIBED
                    && now - link.lastDataAt > ReaderPreferences.FRAME_TIMEOUT_MS) {
                linkDown(link, "数据超时");
            }
        }
        if (connectingLink == null) {
            // 排队最久的先上(nextAttemptAt 最小的到期链路)
            MeterLink waiting = null;
            for (MeterLink link : links.values()) {
                if (link.state == LINK_IDLE && link.nextAttemptAt <= now
                        && (waiting == null || link.nextAttemptAt < waiting.nextAttemptAt)) {
                    waiting = link;
                }
            }
            if (waiting != null) {
                if (subscribedCount() < MAX_ACTIVE_LINKS) {
                    connect(waiting);
                } else {
                    parkOldest(now);                           // 满员:轮出驻留最久的,下一拍连它
                }
            }
        }
        int count = subscribedCount();
        if (count != lastSubscribedCount) {
            lastSubscribedCount = count;
            broadcastState(count > 0 ? "CONNECTED" : "SCANNING", streamingText());
            if (!autoMode) {
                updateNotification(streamingText());
            }
        }
        handler.postDelayed(pump, PUMP_MS);
    }

    private void connect(MeterLink link) {
        link.state = LINK_CONNECTING;
        link.parser.reset();
        connectingLink = link;
        try {
            link.gatt = link.device.connectGatt(
                    this,
                    false,
                    gattCallback,
                    BluetoothDevice.TRANSPORT_LE
            );
        } catch (SecurityException exception) {
            link.gatt = null;
        }
        if (link.gatt == null) {
            linkDown(link, "无法创建GATT连接");
            return;
        }
        handler.removeCallbacks(connectTimeout);
        handler.postDelayed(connectTimeout, ReaderPreferences.CONNECTION_TIMEOUT_MS);
    }

    private MeterLink findLink(BluetoothGatt gatt) {
        for (MeterLink link : links.values()) {
            if (link.gatt == gatt) {
                return link;
            }
        }
        return null;
    }

    private void handleConnectionState(BluetoothGatt gatt, int status, int newState) {
        MeterLink link = findLink(gatt);
        if (link == null) {
            safeClose(gatt);
            return;
        }
        if (newState == BluetoothProfile.STATE_CONNECTED
                && status == BluetoothGatt.GATT_SUCCESS) {
            try {
                if (!gatt.discoverServices()) {
                    linkDown(link, "无法发现GATT服务");
                }
            } catch (SecurityException exception) {
                linkDown(link, "服务发现权限错误");
            }
        } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
            linkDown(link, "连接断开");
        }
    }

    private void handleServicesDiscovered(BluetoothGatt gatt, int status) {
        MeterLink link = findLink(gatt);
        if (link == null) {
            return;
        }
        if (status != BluetoothGatt.GATT_SUCCESS) {
            linkDown(link, "服务发现失败");
            return;
        }
        BluetoothGattService service = gatt.getService(SERVICE_UUID);
        BluetoothGattCharacteristic characteristic =
                service == null ? null : service.getCharacteristic(CHARACTERISTIC_UUID);
        if (characteristic == null) {
            linkDown(link, "未找到HC-42的FFE0/FFE1服务");
            return;
        }
        try {
            if (!gatt.setCharacteristicNotification(characteristic, true)) {
                linkDown(link, "无法启用FFE1通知");
                return;
            }
            BluetoothGattDescriptor descriptor =
                    characteristic.getDescriptor(CLIENT_CONFIG_UUID);
            if (descriptor == null) {
                linkDown(link, "FFE1缺少通知描述符");
                return;
            }
            descriptor.setValue(BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE);
            if (!gatt.writeDescriptor(descriptor)) {
                linkDown(link, "启用FFE1通知失败");
            }
        } catch (SecurityException exception) {
            linkDown(link, "启用通知权限错误");
        }
    }

    private void handleDescriptorWrite(BluetoothGatt gatt, int status) {
        MeterLink link = findLink(gatt);
        if (link == null) {
            return;
        }
        if (status != BluetoothGatt.GATT_SUCCESS) {
            linkDown(link, "通知描述符写入失败");
            return;
        }
        link.state = LINK_SUBSCRIBED;
        link.failures = 0;
        link.parked = false;
        link.subscribedAt = SystemClock.elapsedRealtime();
        link.lastDataAt = link.subscribedAt;
        if (connectingLink == link) {
            connectingLink = null;                             // 释放建链槽,轮到下一台
            handler.removeCallbacks(connectTimeout);
        }
    }

    /** 满员轮歇:把驻留最久、且这次会话已经拿到过帧的链轮出去,排到队尾。 */
    private void parkOldest(long now) {
        MeterLink oldest = null;
        for (MeterLink link : links.values()) {
            if (link.state != LINK_SUBSCRIBED
                    || now - link.subscribedAt < ROTATE_DWELL_MS
                    || link.lastFrameAt < link.subscribedAt) {
                continue;                                      // 还没坐热或还没出数,先不动
            }
            if (oldest == null || link.subscribedAt < oldest.subscribedAt) {
                oldest = link;
            }
        }
        if (oldest == null) {
            return;
        }
        closeGatt(oldest);
        oldest.state = LINK_IDLE;
        oldest.parked = true;
        oldest.failures = 0;
        oldest.nextAttemptAt = now;                            // 立即到期,但排在等更久的后面
    }

    private void handleData(BluetoothGatt gatt, BluetoothGattCharacteristic characteristic,
                            byte[] value) {
        MeterLink link = findLink(gatt);
        if (link == null || !CHARACTERISTIC_UUID.equals(characteristic.getUuid())) {
            return;
        }
        link.lastDataAt = SystemClock.elapsedRealtime();
        List<MeterFrameParser.ParsedFrame> frames = link.parser.feed(value);
        if (frames.isEmpty()) {
            return;
        }
        MeterFrameParser.ParsedFrame frame = frames.get(frames.size() - 1);
        link.address = frame.address;
        link.lastMa = frame.currentMa;
        link.lastFrameAt = link.lastDataAt;
        broadcastFrame(frame, link);
    }

    private void linkDown(MeterLink link, String reason) {
        Log.w(TAG, link.mac + " " + reason);
        if (connectingLink == link) {
            connectingLink = null;
            handler.removeCallbacks(connectTimeout);
        }
        closeGatt(link);
        link.state = LINK_IDLE;
        link.parked = false;                                   // 真失败,不算轮歇
        link.failures++;
        link.nextAttemptAt = SystemClock.elapsedRealtime()
                + (link.failures >= RETRY_SLOW_AFTER ? RETRY_SLOW_MS : RETRY_MS);
    }

    private void closeGatt(MeterLink link) {
        BluetoothGatt gatt = link.gatt;
        link.gatt = null;
        if (gatt == null) {
            return;
        }
        try {
            if (hasBlePermissions()) {
                gatt.disconnect();
            }
        } catch (SecurityException ignored) {
            // 下面仍然会 close
        }
        safeClose(gatt);
    }

    private void safeClose(BluetoothGatt gatt) {
        if (gatt != null) {
            try {
                gatt.close();
            } catch (RuntimeException ignored) {
                // 个别厂商栈在已关闭时会抛
            }
        }
    }

    /* ══════════ 自动存档:快照各表此刻的最新帧,与原型 autoRound 一致 ══════════ */
    private void autoRound() {
        if (!autoMode) {
            return;
        }
        long nowUp = SystemClock.elapsedRealtime();
        long wall = System.currentTimeMillis();

        // 地址 → 新鲜帧的链路(同地址取最近报数的那条链)。
        // 正在流的 1.5 s 内算新鲜;被轮歇让位的健康链放宽到一个轮转周期,
        // 否则表比连接上限多时会把轮歇中的表误记成离线。
        Map<Integer, MeterLink> fresh = new HashMap<>();
        for (MeterLink link : links.values()) {
            if (link.address < 0 || link.lastFrameAt <= 0) {
                continue;
            }
            long age = nowUp - link.lastFrameAt;
            boolean usable = age <= FRESH_MS || (link.parked && age <= PARKED_FRESH_MS);
            if (!usable) {
                continue;
            }
            MeterLink prev = fresh.get(link.address);
            if (prev == null || link.lastFrameAt > prev.lastFrameAt) {
                fresh.put(link.address, link);
            }
        }
        // 已登记 ∪ 在报数:每轮一表一条,有帧存值,没帧存离线
        Map<Integer, String[]> registered = database.registeredMeters();
        TreeSet<Integer> addrs = new TreeSet<>(registered.keySet());
        addrs.addAll(fresh.keySet());

        int stored = 0;
        for (int addr : addrs) {
            MeterLink link = fresh.get(addr);
            MeterReading reading;
            if (link != null) {
                reading = database.insertReading(new MeterReading(
                        0, wall, addr, link.lastMa,
                        MeterReading.classify(
                                link.lastMa,
                                ReaderPreferences.DEFAULT_LOW_THRESHOLD_MA,
                                ReaderPreferences.DEFAULT_HIGH_THRESHOLD_MA
                        ),
                        link.mac, link.name, link.rssi, "AUTO"
                ));
            } else {
                String[] meta = registered.get(addr);
                reading = database.insertReading(new MeterReading(
                        0, wall, addr, -1, MeterReading.OFFLINE,
                        meta[0], meta[1], -127, "AUTO"
                ));
            }
            broadcastReading(reading);
            stored++;
        }
        broadcastState(
                "AUTO_WAIT",
                String.format(
                        Locale.CHINA,
                        "本轮存档%d台，%s后再存一轮",
                        stored,
                        preferences.pollingIntervalText()
                )
        );
        scheduleRound(preferences.pollingIntervalMs());
    }

    private void scheduleRound(long delayMs) {
        long safeDelayMs = Math.max(1_000L, delayMs);
        handler.removeCallbacks(autoRoundRunnable);
        handler.removeCallbacks(countdownTick);
        nextCycleAtElapsedMs = SystemClock.elapsedRealtime() + safeDelayMs;
        updateCountdownNotification();
        handler.postDelayed(autoRoundRunnable, safeDelayMs);
    }

    private void updateCountdownNotification() {
        if (!autoMode || nextCycleAtElapsedMs <= 0L) {
            return;
        }
        long remainingMs = nextCycleAtElapsedMs - SystemClock.elapsedRealtime();
        if (remainingMs <= 0L) {
            updateNotification("即将存档下一轮");
            return;
        }
        long remainingSeconds = Math.max(1L, (remainingMs + 999L) / 1_000L);
        updateNotification(String.format(
                Locale.CHINA,
                "%s · %d秒后自动存档",
                streamingText(),
                remainingSeconds
        ));
        long untilNextSecond = remainingMs - (remainingSeconds - 1L) * 1_000L;
        handler.postDelayed(
                countdownTick,
                Math.max(50L, Math.min(1_000L, untilNextSecond))
        );
    }

    /* ══════════ 杂项 ══════════ */
    private int subscribedCount() {
        int count = 0;
        for (MeterLink link : links.values()) {
            if (link.state == LINK_SUBSCRIBED) {
                count++;
            }
        }
        return count;
    }

    private String streamingText() {
        int count = subscribedCount();
        return count > 0
                ? String.format(Locale.CHINA, "%d台在报数", count)
                : "正在搜索电流表";
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

    private void broadcastFrame(MeterFrameParser.ParsedFrame frame, MeterLink link) {
        Intent intent = eventIntent(ACTION_FRAME);
        intent.putExtra(EXTRA_ADDRESS, frame.address);
        intent.putExtra(EXTRA_CURRENT_MA, frame.currentMa);
        intent.putExtra(EXTRA_TIMESTAMP, System.currentTimeMillis());
        intent.putExtra(EXTRA_MAC, link.mac);
        intent.putExtra(EXTRA_NAME, link.name);
        intent.putExtra(EXTRA_RSSI, link.rssi);
        sendBroadcast(intent);
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
        if (text.equals(lastNotificationText)) {
            return;
        }
        lastNotificationText = text;
        notificationManager.notify(NOTIFICATION_ID, buildNotification(text));
    }
}
