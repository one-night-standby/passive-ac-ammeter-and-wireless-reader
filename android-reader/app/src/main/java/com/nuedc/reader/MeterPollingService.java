package com.nuedc.reader;

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
import android.bluetooth.BluetoothStatusCodes;
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

import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.TreeSet;
import java.util.UUID;

/**
 * 请求-应答式读表服务。
 *
 * <p>电流表平时不出声:题目 2(2) 两个模式都写死「不操作电流表」,所以读数只能由
 * 读表器要。一次读表 = 往 FFE1 写一行 {@code MEAS,ADDR=n},等它把帧发回来。
 * 「没有帧」因此是常态而不是故障,连接健康与否只看 GATT 状态,不看数据流。
 *
 * <p>离线的判据跟着变成「问了不答」。这不是凑合:电流表靠负载电流取电,
 * 4.3 说的「负载断开」就是表掉电,掉电的表答不上话,两者物理上是同一件事。
 * 离线只广播给界面指示,<b>不落库</b>——4.4 要的是读数记录,不是缺席记录。
 *
 * <p>并发模型:回调驱动 + 每台表一条链路状态机,同一时刻只发起一条新链
 * (多数机型的协议栈按序建链,并发发起容易 133),每条链上同一时刻只有一个
 * 未完成的请求。
 */
public final class MeterPollingService extends Service {
    public static final String ACTION_CONNECT_BASIC =
            "com.nuedc.reader.action.CONNECT_BASIC";       // 兼容旧名:启动链路管理
    public static final String ACTION_START_AUTO =
            "com.nuedc.reader.action.START_AUTO";
    public static final String ACTION_STOP_AUTO =
            "com.nuedc.reader.action.STOP_AUTO";
    public static final String ACTION_UPDATE_INTERVAL =
            "com.nuedc.reader.action.UPDATE_INTERVAL";
    /** 基本模式的一对一手动读取,带 EXTRA_ADDRESS。 */
    public static final String ACTION_READ_NOW =
            "com.nuedc.reader.action.READ_NOW";
    public static final String ACTION_READING =
            "com.nuedc.reader.event.READING";
    public static final String ACTION_STATE =
            "com.nuedc.reader.event.STATE";
    /** 一次应答带回来的帧:只进界面,落不落库由采集模式决定。 */
    public static final String ACTION_FRAME =
            "com.nuedc.reader.event.FRAME";
    /**
     * 电流表发来的一整行原文,给标定端用。
     *
     * <p>读表器自己不听这条:它要的是解析过的读数帧。标定端要的恰恰相反——原始
     * RMS、{@code SRC} 和推送应答都在这一层,再往上就被解析器过滤掉了。
     */
    public static final String ACTION_LINE =
            "com.nuedc.reader.action.LINE";
    /** 往电流表写一行任意命令,给标定端推表用。带 EXTRA_LINE。 */
    public static final String ACTION_SEND_LINE =
            "com.nuedc.reader.action.SEND_LINE";
    /** 问了没人答:界面据此做离线指示。刻意不落库,见 onReplyMissing。 */
    public static final String ACTION_OFFLINE =
            "com.nuedc.reader.event.OFFLINE";

    public static final String EXTRA_ADDRESS = "address";
    public static final String EXTRA_CURRENT_MA = "current_ma";
    public static final String EXTRA_STATUS = "status";
    public static final String EXTRA_TIMESTAMP = "timestamp";
    public static final String EXTRA_STATE = "state";
    public static final String EXTRA_DETAIL = "detail";
    public static final String EXTRA_AUTO = "auto";
    /** 距下一轮还有多少毫秒。自动模式外为 -1,界面据此对齐倒计时环。 */
    public static final String EXTRA_NEXT_CYCLE_MS = "next_cycle_ms";
    public static final String EXTRA_LINE = "line";
    public static final String EXTRA_MAC = "mac";
    public static final String EXTRA_NAME = "name";
    public static final String EXTRA_RSSI = "rssi";

    private static final String TAG = "MeterPollingService";
    private static final String NOTIFICATION_CHANNEL = "meter_reader_service";
    private static final int NOTIFICATION_ID = 100;

    private static final long SCAN_ON_MS = 8_000L;             // 扫 8 秒歇 12 秒,避开系统节流
    private static final long SCAN_OFF_MS = 12_000L;
    private static final long PUMP_MS = 1_000L;
    private static final long ROUND_RETRY_MS = 500L;           // 轮次撞上手动读取时的退让
    /** 连续几拍心跳没来就算不在场。电流表 2 秒一拍,给三拍的余量。 */
    private static final long ALIVE_WINDOW_MS = 7_000L;
    private static final long RETRY_MS = 1_500L;
    private static final long RETRY_SLOW_MS = 10_000L;         // 连败三次后放慢,别拖累别的链
    private static final int RETRY_SLOW_AFTER = 3;
    /* 同时保持的连接数。一台电流表靠 4 位编码开关轮流扮演 16 个地址,现场最多
       两台实表,所以不需要原来那套「满员轮歇」的并发调度。 */
    private static final int MAX_ACTIVE_LINKS = 2;

    private static final UUID SERVICE_UUID =
            UUID.fromString("0000ffe0-0000-1000-8000-00805f9b34fb");
    private static final UUID CHARACTERISTIC_UUID =
            UUID.fromString("0000ffe1-0000-1000-8000-00805f9b34fb");
    private static final UUID CLIENT_CONFIG_UUID =
            UUID.fromString("00002902-0000-1000-8000-00805f9b34fb");

    /** `askingAddr` 的空值:这条链没有未完成的请求。 */
    private static final int NOT_ASKING = Integer.MIN_VALUE;

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
        BluetoothGattCharacteristic pipe;                      // FFE1,收帧也发命令
        int address = -1;                                      // 帧里报出来的编码开关地址
        int lastMa = -1;
        /** 这条链上正在等应答的地址;-1 表示广播问「你是几号」,MIN_VALUE 表示没在等。 */
        int askingAddr = NOT_ASKING;
        /** 这个请求是自动轮次发的还是手动发的。挂在链路上而不是做成全局状态:
            轮次进行中来一次手动读取,全局状态会被冲掉,那一轮就再也推不动。 */
        boolean askingForRound;
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
    private final Map<String, Runnable> replyTimeouts = new HashMap<>();
    /** 地址 → 最近一次心跳的 elapsedRealtime。谁在心跳,谁就在场。 */
    private final Map<Integer, Long> aliveAt = new HashMap<>();
    private final ArrayDeque<Integer> roundQueue = new ArrayDeque<>();
    private int roundStored;

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
    private final Runnable roundRetry = this::pumpRound;
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
            roundQueue.clear();
            handler.removeCallbacks(autoRoundRunnable);
            handler.removeCallbacks(countdownTick);
            nextCycleAtElapsedMs = 0L;
            startLinks();                                      // 链路照常在线,只是不再定时落库
            broadcastState("CONNECTED", "自动采集已停止，连接保持");
            updateNotification(linkText());
        } else if (ACTION_READ_NOW.equals(action)) {
            startLinks();
            readNow(intent.getIntExtra(EXTRA_ADDRESS, -1));
        } else if (ACTION_SEND_LINE.equals(action)) {
            startLinks();
            sendLine(intent.getStringExtra(EXTRA_LINE));
        } else if (ACTION_UPDATE_INTERVAL.equals(action)) {
            if (autoMode) {
                scheduleRound(preferences.pollingIntervalMs());
                broadcastState(
                        "AUTO_WAIT",
                        String.format(
                                Locale.CHINA,
                                "采集间隔已设为%s，重新计时",
                                preferences.pollingIntervalText()
                        )
                );
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

    /**
     * 每秒一拍:建链队列,同一时刻只发起一条。
     *
     * <p>这里<b>没有</b>数据看门狗。旧版本 6 秒收不到帧就断链重连,那建立在
     * 「电流表持续报数」上;现在空闲不发帧是常态,那套看门狗会把每条健康的链
     * 每 6 秒判死一次,永远连不稳。链路死活只由 GATT 状态回调决定。
     */
    private void pumpLinks() {
        if (!running) {
            return;
        }
        long now = SystemClock.elapsedRealtime();
        if (connectingLink == null) {
            // 排队最久的先上(nextAttemptAt 最小的到期链路)
            MeterLink waiting = null;
            for (MeterLink link : links.values()) {
                if (link.state == LINK_IDLE && link.nextAttemptAt <= now
                        && (waiting == null || link.nextAttemptAt < waiting.nextAttemptAt)) {
                    waiting = link;
                }
            }
            if (waiting != null && subscribedCount() < MAX_ACTIVE_LINKS) {
                connect(waiting);
            }
        }
        sweepAlive(now);
        int count = subscribedCount();
        if (count != lastSubscribedCount) {
            lastSubscribedCount = count;
            broadcastState(count > 0 ? "CONNECTED" : "SCANNING", linkText());
            if (!autoMode) {
                updateNotification(linkText());
            }
        }
        handler.postDelayed(pump, PUMP_MS);
    }

    /**
     * 心跳停了就报离线。
     *
     * <p>这是 4.3 的判据落地的地方:电流表靠负载电流取电,负载一断它跟着掉电,
     * 心跳随之消失。不落库——离线不是读数。
     */
    private void sweepAlive(long now) {
        java.util.Iterator<Map.Entry<Integer, Long>> it = aliveAt.entrySet().iterator();
        while (it.hasNext()) {
            Map.Entry<Integer, Long> entry = it.next();
            if (now - entry.getValue() > ALIVE_WINDOW_MS) {
                int addr = entry.getKey();
                it.remove();
                Intent intent = eventIntent(ACTION_OFFLINE);
                intent.putExtra(EXTRA_ADDRESS, addr);
                intent.putExtra(EXTRA_TIMESTAMP, System.currentTimeMillis());
                sendBroadcast(intent);
            }
        }
    }

    private void broadcastPresence(int addr, MeterLink link) {
        Intent intent = eventIntent(ACTION_FRAME);
        intent.putExtra(EXTRA_ADDRESS, addr);
        intent.putExtra(EXTRA_CURRENT_MA, -1);                 // 心跳不带读数
        intent.putExtra(EXTRA_TIMESTAMP, System.currentTimeMillis());
        intent.putExtra(EXTRA_MAC, link.mac);
        intent.putExtra(EXTRA_NAME, link.name);
        intent.putExtra(EXTRA_RSSI, link.rssi);
        sendBroadcast(intent);
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
        link.pipe = characteristic;
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
        if (connectingLink == link) {
            connectingLink = null;                             // 释放建链槽,轮到下一台
            handler.removeCallbacks(connectTimeout);
        }
        // 不用问「你是几号」:电流表 2 秒一拍地播自己的地址,等一拍就知道了,
        // 而且开关中途被拨动时这条路也照样跟得上。
    }

    /* ══════════ 请求-应答 ══════════ */

    /**
     * 往 FFE1 写一行命令。{@code addr < 0} 发广播 {@code MEAS}(用于发现),
     * 否则发 {@code MEAS,ADDR=n}——轮询已知地址必须带 ADDR,否则回来的帧属于
     * 哪个请求就说不清了:同一台设备在不同时刻是不同的表。
     */
    private boolean ask(MeterLink link, int addr, boolean forRound) {
        if (link.state != LINK_SUBSCRIBED || link.pipe == null) {
            return false;
        }
        String line = addr < 0
                ? "MEAS\n"
                : String.format(Locale.ROOT, "MEAS,ADDR=%d\n", addr);
        byte[] payload = line.getBytes(StandardCharsets.US_ASCII);
        if (!writeRaw(link, payload)) {
            Log.w(TAG, link.mac + " 写 MEAS 失败");
            return false;
        }
        link.askingAddr = addr;
        link.askingForRound = forRound;
        link.parser.reset();                                   // 半行残渣不该算进这次应答
        handler.postDelayed(replyTimeoutFor(link), ReaderPreferences.REPLY_TIMEOUT_MS);
        return true;
    }

    /** 往 FFE1 写一串字节。写成功只表示这一包交给了协议栈,不表示表收到了。 */
    private boolean writeRaw(MeterLink link, byte[] payload) {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                return link.gatt.writeCharacteristic(
                        link.pipe,
                        payload,
                        BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
                ) == BluetoothStatusCodes.SUCCESS;
            }
            link.pipe.setWriteType(BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE);
            link.pipe.setValue(payload);
            return link.gatt.writeCharacteristic(link.pipe);
        } catch (SecurityException exception) {
            return false;
        }
    }

    /**
     * 标定端要发的那一行命令。
     *
     * <p>不碰 {@code askingAddr},也不排超时:{@code CALPT} 之类不是一次读数请求,
     * 应答走的是 {@code CALACK},由标定端自己按行等。把它算成读数请求的话,一次
     * 推表会把每一条 MEAS 的应答窗口都占掉。
     */
    private void sendLine(String line) {
        if (line == null || line.isEmpty()) {
            return;
        }
        // 任何一条订阅上的链都行,不要求它是闲的:CALPT 之类不占应答窗口,而
        // 「闲」的条件是没有未完成的 MEAS——推表时正好可能有一次读数还没超时,
        // 挑剔到那一步只会让第一条命令被无声丢掉。
        MeterLink link = subscribedLink();
        if (link == null) {
            broadcastState("ERROR", "没有已连接的电流表");
            return;
        }
        String payload = line.endsWith("\n") ? line : line + "\n";
        if (!writeRaw(link, payload.getBytes(StandardCharsets.US_ASCII))) {
            broadcastState("ERROR", "命令发送失败：" + line);
        }
    }

    /** 每条链一个超时 runnable,便于单独撤销。 */
    private Runnable replyTimeoutFor(MeterLink link) {
        Runnable existing = replyTimeouts.get(link.mac);
        if (existing != null) {
            handler.removeCallbacks(existing);
        }
        Runnable timeout = () -> {
            if (link.askingAddr == NOT_ASKING) {
                return;
            }
            int addr = link.askingAddr;
            boolean forRound = link.askingForRound;
            link.askingAddr = NOT_ASKING;
            onReplyMissing(link, addr, forRound);
        };
        replyTimeouts.put(link.mac, timeout);
        return timeout;
    }

    private void clearReplyTimeout(MeterLink link) {
        Runnable timeout = replyTimeouts.get(link.mac);
        if (timeout != null) {
            handler.removeCallbacks(timeout);
        }
        link.askingAddr = NOT_ASKING;
        link.askingForRound = false;
    }

    private void handleData(BluetoothGatt gatt, BluetoothGattCharacteristic characteristic,
                            byte[] value) {
        MeterLink link = findLink(gatt);
        if (link == null || !CHARACTERISTIC_UUID.equals(characteristic.getUuid())) {
            return;
        }
        for (String rawLine : link.parser.feedLines(value)) {
            // 原样先播一次。标定端要的 CALACK/CALSTAT 这个解析器不认,而认得的
            // 那些帧它也想看原文;广播在解析之前,两者就都不会漏。
            broadcastLine(rawLine, link);
            MeterFrameParser.ParsedFrame frame = link.parser.parseLine(rawLine);
            if (frame == null) {
                continue;
            }
            link.address = frame.address;
            // 任何一帧都是在场的证据,不只是心跳。一次读数前后电流表要静默
            // 一段(测量 260 ms 加显示 1 s,下一拍最迟 3.3 s 才来),期间它照
            // 样在答读数——只认心跳的话,正在正常工作的表会被扫成离线。
            aliveAt.put(frame.address, SystemClock.elapsedRealtime());
            // 电流表上电时会不请自来地发一帧。那不是任何请求的应答,不能拿它
            // 推进轮次,也不能按上一次请求的身份落库——否则一次重启就会凭空
            // 多存一条、并且把轮次队列多弹一个地址出去。
            boolean solicited = link.askingAddr != NOT_ASKING
                    && (link.askingAddr < 0 || link.askingAddr == frame.address);
            boolean forRound = solicited && link.askingForRound;
            if (frame.alive) {
                // 心跳不是应答:不清超时、不推进轮次、不落库。它只回答
                // 「此刻谁在场」——而这正是拨码开关改了之后唯一会变的东西。
                // 清超时要放在这道判断之后:心跳每 2 秒一拍,而一次应答最迟要
                // 1.5 秒,所以等应答的窗口里几乎总会插进一拍心跳。先清的话,
                // 这一拍就把请求作废了,随后真正的读数帧变成「没人问过」,
                // 自动轮次既不落库也不往下推。
                // 每一拍都广播在场,不只在「新出现」时广播。只在跳变时发的话,
                // 界面的离线标记一旦被别的原因置上(比如一次读取超时),后面
                // 再多的心跳也清不掉它,那台表会一直显示离线。
                broadcastPresence(frame.address, link);
                continue;
            }
            if (solicited) {
                clearReplyTimeout(link);
            }
            if (frame.hasReading()) {
                link.lastMa = frame.currentMa;
                broadcastFrame(frame, link);
                onReplyArrived(frame.address, frame.currentMa, link, forRound);
            }
            // METER_CAL 到这里就没事了。它带的 FLAG 是给标定台看的,不是读数的
            // 状态标记:电流表测到什么就发什么,读数永远走 METER_TEST,读表器
            // 显示的就是电流表面板上那个数。
        }
    }

    private void linkDown(MeterLink link, String reason) {
        Log.w(TAG, link.mac + " " + reason);
        if (connectingLink == link) {
            connectingLink = null;
            handler.removeCallbacks(connectTimeout);
        }
        closeGatt(link);
        link.state = LINK_IDLE;
        link.pipe = null;
        // 链断了,这条链上没送到的请求也就没了应答。当作没人答处理,
        // 界面才不会一直挂在「读取中」。
        if (link.askingAddr != NOT_ASKING) {
            int pending = link.askingAddr;
            boolean forRound = link.askingForRound;
            clearReplyTimeout(link);
            onReplyMissing(link, pending, forRound);
        }
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

    /* ══════════ 采集轮次:逐台问,答了才存 ══════════ */

    /**
     * 一轮自动采集。对<b>注册表里的每一个地址</b>发一次 MEAS,逐台串行——
     * 一条链上同一时刻只能有一个未完成的请求,而电流表在测量期间收到的命令会
     * 直接丢弃,所以抢着发只会被吞掉。
     *
     * <p>2 分钟的周期对 16 个地址 × 最坏 2 秒超时绰绰有余。
     */
    private void autoRound() {
        if (!autoMode) {
            return;
        }
        // 只问此刻在心跳的地址。不用盲扫 0-15:不在场的地址扫过去只是 16 次
        // 超时,而心跳已经把「谁在场」直接告诉我们了。
        long now = SystemClock.elapsedRealtime();
        TreeSet<Integer> targets = new TreeSet<>();
        for (Map.Entry<Integer, Long> entry : aliveAt.entrySet()) {
            if (now - entry.getValue() <= ALIVE_WINDOW_MS) {
                targets.add(entry.getKey());
            }
        }
        roundQueue.clear();
        roundQueue.addAll(targets);
        roundStored = 0;
        pumpRound();
    }

    /** 队列里的下一个地址。空了就收尾并排下一轮。 */
    private void pumpRound() {
        Integer next = roundQueue.poll();
        if (next == null) {
            finishRound();
            return;
        }
        MeterLink link = idleSubscribedLink();
        if (link == null) {
            if (subscribedCount() > 0) {
                // 链在,只是正忙(多半是手动读取插了进来)。等一下再问这一个,
                // 别为了一次插队把整轮丢掉。
                roundQueue.addFirst(next);
                handler.postDelayed(roundRetry, ROUND_RETRY_MS);
                return;
            }
            // 一条链都没有:这一轮问不了,不必逐个记离线——离线本来就不落库。
            roundQueue.clear();
            finishRound();
            return;
        }
        if (!ask(link, next, true)) {
            onReplyMissing(link, next, true);
        }
    }

    private void finishRound() {
        if (!autoMode) {
            return;
        }
        // 先排下一轮再广播:界面的倒计时环跟着广播里的剩余时间走,顺序反了
        // 它拿到的就是上一轮那个早已走完的时刻。一台没测到的空轮同样要重新计时。
        scheduleRound(preferences.pollingIntervalMs());
        broadcastState("AUTO_WAIT", String.format(
                Locale.CHINA, "本轮存档%d台，%s后再采一轮",
                roundStored, preferences.pollingIntervalText()));
    }

    /** 有应答:落库并广播。只有这条路径写数据库。 */
    private void onReplyArrived(int addr, int currentMa, MeterLink link, boolean forRound) {
        // 手动读取不在这里落库:界面的读条走完时由 onCaptureRecord 存,
        // 存的来源标成 MANUAL。这里只管自动轮次。
        if (forRound && autoMode) {
            MeterReading reading = database.insertReading(new MeterReading(
                    0, System.currentTimeMillis(), addr, currentMa,
                    MeterReading.classify(
                            currentMa,
                            preferences.lowThresholdMa(),
                            preferences.highThresholdMa()
                    ),
                    link.mac, link.name, link.rssi, "AUTO"
            ));
            roundStored++;
            broadcastReading(reading);
            pumpRound();
        }
    }

    /**
     * 没等到应答。广播离线让界面指示,<b>不写数据库</b>。
     *
     * <p>题目 4.3 要的是离线「指示」,4.4 要的是读数「记录」——缺席不是读数。
     * 何况一台表轮流扮演 16 个地址,注册表里多数地址在任一时刻都不在场,
     * 逐条记下去只会把 30 条记录的空间填满假离线。
     */
    private void onReplyMissing(MeterLink link, int addr, boolean forRound) {
        // 在场与否由心跳说了算,不由一次读取的成败说了算。一台正在心跳的表
        // 只是这次没答上来(测量中、命令被丢、链路忙),它并没有不在——把它
        // 标成离线会让界面和记录都说谎。
        if (addr >= 0 && !isAlive(addr)) {
            Intent intent = eventIntent(ACTION_OFFLINE);
            intent.putExtra(EXTRA_ADDRESS, addr);
            intent.putExtra(EXTRA_TIMESTAMP, System.currentTimeMillis());
            sendBroadcast(intent);
        }
        if (forRound && autoMode) {
            pumpRound();
        }
    }

    /** 这个地址此刻还在心跳吗。 */
    private boolean isAlive(int addr) {
        Long last = aliveAt.get(addr);
        return last != null
                && SystemClock.elapsedRealtime() - last <= ALIVE_WINDOW_MS;
    }

    /** 已订阅且此刻没有未完成请求的链。 */
    private MeterLink subscribedLink() {
        for (MeterLink link : links.values()) {
            if (link.state == LINK_SUBSCRIBED) {
                return link;
            }
        }
        return null;
    }

    private MeterLink idleSubscribedLink() {
        for (MeterLink link : links.values()) {
            if (link.state == LINK_SUBSCRIBED && link.askingAddr == NOT_ASKING) {
                return link;
            }
        }
        return null;
    }

    /** 基本模式:一对一手动读一台。不排轮次,直接问。 */
    private void readNow(int addr) {
        MeterLink link = idleSubscribedLink();
        if (link == null) {
            broadcastState("ERROR", subscribedCount() > 0
                    ? "电流表正在应答上一次请求，请稍候"
                    : "没有已连接的电流表");
            onReplyMissing(null, addr, false);
            return;
        }
        if (!ask(link, addr, false)) {
            onReplyMissing(link, addr, false);
        }
    }

    private void scheduleRound(long delayMs) {
        long safeDelayMs = Math.max(ROUND_RETRY_MS, delayMs);
        handler.removeCallbacks(autoRoundRunnable);
        handler.removeCallbacks(countdownTick);
        nextCycleAtElapsedMs = SystemClock.elapsedRealtime() + safeDelayMs;
        updateCountdownNotification();
        handler.postDelayed(autoRoundRunnable, safeDelayMs);
    }

    /** 距下一轮的毫秒数;不在自动模式(或还没排上)时为 -1。 */
    private long remainingCycleMs() {
        if (!autoMode || nextCycleAtElapsedMs <= 0L) {
            return -1L;
        }
        return Math.max(0L, nextCycleAtElapsedMs - SystemClock.elapsedRealtime());
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
        // 间隔可以长到 300 分钟。那时逐秒重画通知就是一万八千次没人看的刷新,
        // 而「18000秒后」也不是人能一眼读出来的数。超过一分钟按分钟走,进了
        // 最后一分钟再逐秒。
        String left;
        long delayMs;
        if (remainingSeconds > 60L) {
            long minutes = (remainingSeconds + 59L) / 60L;
            left = minutes + "分钟";
            delayMs = remainingMs - (minutes - 1L) * 60_000L;
        } else {
            left = remainingSeconds + "秒";
            delayMs = remainingMs - (remainingSeconds - 1L) * 1_000L;
        }
        updateNotification(String.format(
                Locale.CHINA,
                "%s · %s后自动存档",
                linkText(),
                left
        ));
        handler.postDelayed(countdownTick, Math.max(50L, Math.min(60_000L, delayMs)));
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

    /** 连上几台,而不是几台在报数——没有表会主动报数。 */
    private String linkText() {
        int count = subscribedCount();
        return count > 0
                ? String.format(Locale.CHINA, "已连接%d台电流表", count)
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

    /**
     * 电流表说的每一行,原文。只有标定端听这条;读表器界面用的是解析过的帧。
     *
     * <p>带上 MAC 是因为标定是对着一台表做的,而链路可能不止一条:标定端要能
     * 认出这一行是不是它正在标的那台发的。
     */
    private void broadcastLine(String line, MeterLink link) {
        Intent intent = eventIntent(ACTION_LINE);
        intent.putExtra(EXTRA_LINE, line);
        intent.putExtra(EXTRA_MAC, link.mac);
        intent.putExtra(EXTRA_ADDRESS, link.address);
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
        intent.putExtra(EXTRA_NEXT_CYCLE_MS, remainingCycleMs());
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
