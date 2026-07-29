package com.jun.nuedc.reader;

import android.content.ContentValues;
import android.content.Context;
import android.database.Cursor;
import android.database.sqlite.SQLiteDatabase;
import android.database.sqlite.SQLiteOpenHelper;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

public final class MeterDatabase extends SQLiteOpenHelper {
    private static final String DATABASE_NAME = "wireless_meter_reader.db";
    private static final int DATABASE_VERSION = 3;
    public static final int MAX_STORED_READINGS = 1000;

    public MeterDatabase(Context context) {
        super(context, DATABASE_NAME, null, DATABASE_VERSION);
    }

    @Override
    public void onCreate(SQLiteDatabase db) {
        db.execSQL(
                "CREATE TABLE meters (" +
                        "address INTEGER PRIMARY KEY," +
                        "mac TEXT NOT NULL UNIQUE," +
                        "name TEXT NOT NULL," +
                        "last_seen INTEGER NOT NULL)"
        );
        db.execSQL(
                "CREATE TABLE readings (" +
                        "id INTEGER PRIMARY KEY AUTOINCREMENT," +
                        "timestamp INTEGER NOT NULL," +
                        "address INTEGER NOT NULL," +
                        "current_ma INTEGER NOT NULL," +
                        "status TEXT NOT NULL," +
                        "mac TEXT NOT NULL," +
                        "device_name TEXT NOT NULL," +
                        "rssi INTEGER NOT NULL," +
                        "source TEXT NOT NULL)"
        );
        db.execSQL("CREATE INDEX idx_readings_time ON readings(timestamp DESC)");
        db.execSQL("CREATE INDEX idx_readings_address_time ON readings(address, timestamp DESC)");
    }

    @Override
    public void onUpgrade(SQLiteDatabase db, int oldVersion, int newVersion) {
        // Versions 1-3 share the same schema. Version 3 restores multi-record history.
    }

    public synchronized MeterReading insertReading(MeterReading reading) {
        SQLiteDatabase db = getWritableDatabase();
        ContentValues values = valuesFor(reading);
        long id = db.insertOrThrow("readings", null, values);
        registerMeter(db, reading);
        prune(db);
        return new MeterReading(
                id,
                reading.timestamp,
                reading.address,
                reading.currentMa,
                reading.status,
                reading.mac,
                reading.deviceName,
                reading.rssi,
                reading.source
        );
    }

    public synchronized List<MeterReading> markMissingMetersOffline(
            Set<String> seenMacs,
            long timestamp
    ) {
        Set<String> normalizedSeen = new HashSet<>();
        for (String mac : seenMacs) {
            normalizedSeen.add(mac.toUpperCase());
        }

        SQLiteDatabase db = getWritableDatabase();
        List<MeterReading> inserted = new ArrayList<>();
        try (Cursor cursor = db.query(
                "meters",
                new String[]{"address", "mac", "name"},
                null,
                null,
                null,
                null,
                "address ASC"
        )) {
            while (cursor.moveToNext()) {
                int address = cursor.getInt(0);
                String mac = cursor.getString(1);
                String name = cursor.getString(2);
                if (normalizedSeen.contains(mac.toUpperCase()) || isLatestOffline(db, address)) {
                    continue;
                }

                MeterReading offline = new MeterReading(
                        0,
                        timestamp,
                        address,
                        -1,
                        MeterReading.OFFLINE,
                        mac,
                        name,
                        -127,
                        "AUTO"
                );
                long id = db.insertOrThrow("readings", null, valuesFor(offline));
                inserted.add(new MeterReading(
                        id,
                        offline.timestamp,
                        offline.address,
                        offline.currentMa,
                        offline.status,
                        offline.mac,
                        offline.deviceName,
                        offline.rssi,
                        offline.source
                ));
            }
        }
        prune(db);
        return inserted;
    }

    public synchronized List<MeterReading> latestByAddress() {
        SQLiteDatabase db = getReadableDatabase();
        List<MeterReading> result = new ArrayList<>();
        String sql =
                "SELECT r.id,r.timestamp,r.address,r.current_ma,r.status,r.mac," +
                        "r.device_name,r.rssi,r.source " +
                        "FROM readings r " +
                        "INNER JOIN (" +
                        "SELECT address,MAX(id) latest_id FROM readings GROUP BY address" +
                        ") latest ON r.id=latest.latest_id ORDER BY r.address ASC";
        try (Cursor cursor = db.rawQuery(sql, null)) {
            while (cursor.moveToNext()) {
                result.add(fromCursor(cursor));
            }
        }
        return result;
    }

    public synchronized List<MeterReading> history(int limit) {
        return queryReadings(null, null, Math.max(1, limit));
    }

    public synchronized List<MeterReading> trendForAddress(int address, int limit) {
        List<MeterReading> descending = queryReadings(
                "address=? AND status<>?",
                new String[]{Integer.toString(address), MeterReading.OFFLINE},
                Math.max(1, limit)
        );
        List<MeterReading> ascending = new ArrayList<>(descending.size());
        for (int i = descending.size() - 1; i >= 0; i--) {
            ascending.add(descending.get(i));
        }
        return ascending;
    }

    public synchronized int storedCount() {
        try (Cursor cursor = getReadableDatabase().rawQuery(
                "SELECT COUNT(*) FROM readings",
                null
        )) {
            return cursor.moveToFirst() ? cursor.getInt(0) : 0;
        }
    }

    public synchronized void clearHistory() {
        SQLiteDatabase db = getWritableDatabase();
        db.beginTransaction();
        try {
            db.delete("readings", null, null);
            db.delete("meters", null, null);
            db.setTransactionSuccessful();
        } finally {
            db.endTransaction();
        }
    }

    private List<MeterReading> queryReadings(String selection, String[] args, int limit) {
        SQLiteDatabase db = getReadableDatabase();
        List<MeterReading> result = new ArrayList<>();
        try (Cursor cursor = db.query(
                "readings",
                null,
                selection,
                args,
                null,
                null,
                "timestamp DESC,id DESC",
                Integer.toString(limit)
        )) {
            while (cursor.moveToNext()) {
                result.add(fromCursor(cursor));
            }
        }
        return result;
    }

    private void registerMeter(SQLiteDatabase db, MeterReading reading) {
        ContentValues meter = new ContentValues();
        meter.put("address", reading.address);
        meter.put("mac", reading.mac);
        meter.put("name", reading.deviceName);
        meter.put("last_seen", reading.timestamp);
        db.insertWithOnConflict("meters", null, meter, SQLiteDatabase.CONFLICT_REPLACE);
    }

    private boolean isLatestOffline(SQLiteDatabase db, int address) {
        try (Cursor cursor = db.query(
                "readings",
                new String[]{"status"},
                "address=?",
                new String[]{Integer.toString(address)},
                null,
                null,
                "id DESC",
                "1"
        )) {
            return cursor.moveToFirst() && MeterReading.OFFLINE.equals(cursor.getString(0));
        }
    }

    private ContentValues valuesFor(MeterReading reading) {
        ContentValues values = new ContentValues();
        values.put("timestamp", reading.timestamp);
        values.put("address", reading.address);
        values.put("current_ma", reading.currentMa);
        values.put("status", reading.status);
        values.put("mac", reading.mac);
        values.put("device_name", reading.deviceName);
        values.put("rssi", reading.rssi);
        values.put("source", reading.source);
        return values;
    }

    private MeterReading fromCursor(Cursor cursor) {
        return new MeterReading(
                cursor.getLong(cursor.getColumnIndexOrThrow("id")),
                cursor.getLong(cursor.getColumnIndexOrThrow("timestamp")),
                cursor.getInt(cursor.getColumnIndexOrThrow("address")),
                cursor.getInt(cursor.getColumnIndexOrThrow("current_ma")),
                cursor.getString(cursor.getColumnIndexOrThrow("status")),
                cursor.getString(cursor.getColumnIndexOrThrow("mac")),
                cursor.getString(cursor.getColumnIndexOrThrow("device_name")),
                cursor.getInt(cursor.getColumnIndexOrThrow("rssi")),
                cursor.getString(cursor.getColumnIndexOrThrow("source"))
        );
    }

    private void prune(SQLiteDatabase db) {
        db.execSQL(
                "DELETE FROM readings WHERE id NOT IN (" +
                        "SELECT id FROM readings ORDER BY id DESC LIMIT " +
                        MAX_STORED_READINGS +
                        ")"
        );
    }
}
