package dev.phonecluster.app;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.SharedPreferences;
import android.content.pm.ServiceInfo;
import android.net.wifi.WifiManager;
import android.os.BatteryManager;
import android.os.Build;
import android.os.IBinder;
import android.os.PowerManager;
import android.provider.Settings;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;
import java.util.UUID;

/**
 * Servicio en primer plano: encuentra el Mac, pide el alta, descarga el
 * modelo y mantiene vivo {@code pc-agent}, que a su vez lanza llama-server.
 */
public class ClusterService extends Service {
    static final String ACTION_START = "dev.phonecluster.START";
    static final String ACTION_STOP = "dev.phonecluster.STOP";
    static final String PREFS = "cluster";

    private static final String CHANNEL = "cluster";
    private static final int NOTIFICATION_ID = 1;
    private static final String AGENT = "libpcagent.so";
    private static final String LLAMA = "libllamaserver";
    private static final String RPC = "librpcserver";

    private volatile boolean stopping;
    private Thread worker;
    private Process agent;
    private PowerManager.WakeLock wakeLock;
    private WifiManager.WifiLock wifiLock;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            prefs(this).edit().putBoolean("enabled", false).apply();
            shutdown();
            return START_NOT_STICKY;
        }
        try {
            startInForeground(lastNotified.isEmpty() ? "Conectando con tu Mac…" : lastNotified);
        } catch (RuntimeException e) {
            // Reinicio de START_STICKY sin permiso para primer plano (Android 12+).
            Status.set(Status.Phase.STOPPED, "Abre la app para que el teléfono vuelva a trabajar.");
            stopSelf();
            return START_NOT_STICKY;
        }
        prefs(this).edit().putBoolean("enabled", true).apply();
        if (worker == null || !worker.isAlive()) {
            stopping = false;
            acquireLocks();
            worker = new Thread(this::run, "cluster-worker");
            worker.start();
        }
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        stopping = true;
        stopAgent();
        releaseLocks();
        super.onDestroy();
    }

    static SharedPreferences prefs(Context ctx) {
        return ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    // ---------------------------------------------------------------- flujo

    private void run() {
        long backoff = 2000;
        while (!stopping) {
            try {
                String base = findCoordinator();
                if (base == null) continue;
                JSONObject cfg = joinUntilApproved(base);
                if (cfg == null) continue;
                File model = ensureModel(base, cfg);
                writeAgentConfig(base, cfg, model);
                runAgent(cfg, base);
                backoff = 2000;
            } catch (Exception e) {
                if (stopping) break;
                say(Status.Phase.ERROR, "Algo falló: " + e.getMessage() + ". Reintentando…");
                notifyText("Reintentando: " + e.getMessage());
                sleep(backoff);
                backoff = Math.min(backoff * 2, 30000);
            }
        }
    }

    /** URL guardada si responde, si no la que escribió el usuario, si no Bonjour. */
    private String findCoordinator() {
        SharedPreferences p = prefs(this);
        String manual = p.getString("manual_address", null);
        String saved = p.getString("coordinator", null);
        for (String candidate : new String[] {manual, saved}) {
            if (candidate != null && Net.reachable(candidate)) {
                return candidate;
            }
        }
        say(Status.Phase.SEARCHING, "Buscando tu Mac en la Wi-Fi…");
        notifyText("Buscando tu Mac en la Wi-Fi…");
        String found = Net.discover(this, 8000);
        if (found != null && Net.reachable(found)) {
            p.edit().putString("coordinator", found).apply();
            return found;
        }
        say(Status.Phase.SEARCHING,
                "No encuentro tu Mac. Comprueba que la app Phone Cluster está abierta en el Mac y que "
                        + "los dos estáis en la misma Wi-Fi. También puedes escribir su dirección abajo.");
        sleep(4000);
        return null;
    }

    /** Pide el alta hasta que el Mac la acepte. Devuelve la configuración o null si cambió algo. */
    private JSONObject joinUntilApproved(String base) throws Exception {
        SharedPreferences p = prefs(this);
        String deviceId = p.getString("device_id", null);
        if (deviceId == null) {
            deviceId = UUID.randomUUID().toString();
            p.edit().putString("device_id", deviceId).apply();
        }
        JSONObject req = new JSONObject();
        req.put("device_id", deviceId);
        req.put("model", Build.MODEL);
        req.put("label", deviceLabel());
        boolean announced = false;
        while (!stopping) {
            // Con el token de una aprobación anterior, el Mac lo acepta sin preguntar.
            // Solo se envía al mismo Mac que lo dio: un enlace de otra web no lo obtiene.
            String token = p.getString("token", null);
            boolean sameMac = base.equals(p.getString("token_base", null));
            if (token != null && sameMac) req.put("token", token); else req.remove("token");
            JSONObject res = Net.postJson(base + "/api/v1/join", req);
            String status = res.optString("status");
            if ("approved".equals(status)) {
                JSONObject cfg = res.getJSONObject("config");
                p.edit().putString("token", cfg.getString("token")).putString("token_base", base).apply();
                Status.detail("Mac: " + base + "\nNombre en el cluster: " + cfg.optString("name"));
                return cfg;
            }
            if ("rejected".equals(status)) {
                say(Status.Phase.REJECTED, "El Mac rechazó este teléfono.");
                notifyText("El Mac rechazó este teléfono");
                sleep(10000);
                continue;
            }
            if (!announced) {
                Status.detail("Mac: " + base);
                announced = true;
            }
            say(Status.Phase.WAITING_APPROVAL,
                    "Casi listo: en el Mac, pulsa «Aceptar» junto a " + deviceLabel() + ".");
            notifyText("Esperando que lo aceptes en el Mac");
            sleep(3000);
        }
        return null;
    }

    private File ensureModel(String base, JSONObject cfg) throws Exception {
        String file = cfg.optString("model_file", "");
        long size = cfg.optLong("model_size", -1);
        while (file.isEmpty() || "null".equals(file)) {
            say(Status.Phase.DOWNLOADING, "El Mac aún está preparando el modelo…");
            sleep(5000);
            JSONObject again = joinUntilApproved(base);
            if (again == null) throw new IOException("detenido");
            file = again.optString("model_file", "");
            size = again.optLong("model_size", -1);
            cfg = again;
        }
        // El nombre llega por la red: solo un nombre de fichero .gguf, sin rutas.
        if (!file.matches("[A-Za-z0-9._-]+\\.gguf") || file.startsWith(".")) {
            throw new IOException("nombre de modelo no válido: " + file);
        }
        File dir = new File(getFilesDir(), "models");
        dir.mkdirs();
        File model = new File(dir, file);
        if (model.isFile() && (size <= 0 || model.length() == size)) {
            return model;
        }
        // Un modelo distinto ocupa sitio: se borra.
        File[] old = dir.listFiles();
        if (old != null) {
            for (File f : old) {
                if (!f.getName().equals(file) && !f.getName().equals(file + ".part")) f.delete();
            }
        }
        long mb = size > 0 ? size / (1024 * 1024) : 0;
        sayProgress(Status.Phase.DOWNLOADING, "Descargando el modelo desde tu Mac (" + mb + " MB)…", 0);
        notifyText("Descargando el modelo…");
        Net.download(base + cfg.optString("model_url", "/api/v1/files/model"), cfg.getString("token"),
                model, size,
                (done, total) -> sayProgress(Status.Phase.DOWNLOADING,
                        "Descargando el modelo desde tu Mac (" + mb + " MB)…",
                        total > 0 ? (double) done / total : -1),
                () -> stopping);
        return model;
    }

    private void writeAgentConfig(String base, JSONObject cfg, File model) throws Exception {
        String lib = getApplicationInfo().nativeLibraryDir;
        int port = cfg.optInt("llama_port", 8080);
        List<String> command = new ArrayList<>();
        command.add(lib + "/" + variant(LLAMA));
        command.add("-m");
        command.add(model.getAbsolutePath());
        String alias = cfg.optString("model_alias", "");
        if (!alias.isEmpty() && !"null".equals(alias)) {
            command.add("--alias");
            command.add(alias);
        }
        command.add("--host");
        command.add("0.0.0.0");
        command.add("--port");
        command.add(String.valueOf(port));
        JSONArray args = cfg.optJSONArray("llama_args");
        List<String> extra = new ArrayList<>();
        if (args != null) {
            for (int i = 0; i < args.length(); i++) extra.add(args.getString(i));
        }
        // En segundo plano Android deja la app en menos núcleos: con un hilo por
        // núcleo y espera activa, llama.cpp casi se detiene. Pocos hilos que
        // duermen al esperar (ver DECISIONS.md, D-51).
        if (!extra.contains("-t") && !extra.contains("--threads")) {
            extra.add("-t");
            extra.add(threads());
        }
        if (!extra.contains("--poll")) {
            extra.add("--poll");
            extra.add("0");
        }
        command.addAll(extra);
        StringBuilder toml = new StringBuilder();
        toml.append("name = ").append(q(cfg.getString("name"))).append('\n');
        toml.append("coordinator_url = ").append(q(base)).append('\n');
        toml.append("token = ").append(q(cfg.getString("token"))).append('\n');
        toml.append("llama_url = ").append(q("http://127.0.0.1:" + port)).append('\n');
        toml.append("heartbeat_secs = 5\n");
        toml.append("battery_file = ").append(q(batteryFile().getAbsolutePath())).append('\n');
        toml.append("llama_command = [");
        for (int i = 0; i < command.size(); i++) {
            if (i > 0) toml.append(", ");
            toml.append(q(command.get(i)));
        }
        toml.append("]\n");
        // Modo dividido: el agente añade -H <IP de la Wi-Fi> y -p (D-56).
        toml.append("rpc_command = [").append(q(lib + "/" + variant(RPC))).append(", \"-t\", ")
                .append(q(threads())).append(", \"-c\"]\n");
        writeAtomically(new File(getFilesDir(), "config.toml"), toml.toString());
    }

    /**
     * {@code <base>.so} compilado para ARMv8.2 con dotprod, o {@code <base>_v8.so}
     * genérico si el procesador no tiene esas instrucciones: con el
     * optimizado moriría por instrucción ilegal (D-57).
     */
    private static String variant(String base) {
        return base + (hasDotprod() ? ".so" : "_v8.so");
    }

    private static boolean hasDotprod() {
        try {
            String cpuinfo = new String(Files.readAllBytes(new File("/proc/cpuinfo").toPath()),
                    StandardCharsets.UTF_8);
            for (String line : cpuinfo.split("\n")) {
                if (line.startsWith("Features") && !line.contains("asimddp")) return false;
            }
            return cpuinfo.contains("asimddp");
        } catch (IOException | SecurityException e) {
            return false; // sin saberlo, la variante que funciona en todos
        }
    }

    /** Hilos para llama.cpp: pocos, ver D-51. */
    private static String threads() {
        return String.valueOf(Math.max(1, Math.min(4, Runtime.getRuntime().availableProcessors())));
    }

    /** Lanza pc-agent y lo mantiene vivo hasta que se detenga el servicio. */
    private void runAgent(JSONObject cfg, String base) throws Exception {
        String lib = getApplicationInfo().nativeLibraryDir;
        File files = getFilesDir();
        File log = new File(files, "agent.log");
        long backoff = 1000;
        long lastBattery = 0;
        long lastNodeCheck = 0;
        long failingSince = 0;
        String localIp = localIpFor(base);
        while (!stopping) {
            killStrays(lib);
            if (log.length() > 5L * 1024 * 1024) log.delete();
            ProcessBuilder pb = new ProcessBuilder(lib + "/" + AGENT, "--config",
                    new File(files, "config.toml").getAbsolutePath());
            pb.environment().put("HOME", files.getAbsolutePath());
            pb.environment().put("RUST_LOG", "info");
            pb.environment().put("TMPDIR", getCacheDir().getAbsolutePath());
            pb.directory(files);
            pb.redirectErrorStream(true);
            pb.redirectOutput(ProcessBuilder.Redirect.appendTo(log));
            writeBattery();
            say(Status.Phase.STARTING, "Arrancando el modelo en este teléfono…");
            notifyText("Arrancando el modelo…");
            long started = System.currentTimeMillis();
            synchronized (this) {
                if (stopping) break;
                agent = pb.start();
            }
            while (!stopping && agent.isAlive()) {
                long now = System.currentTimeMillis();
                if (now - lastBattery > 30000) {
                    writeBattery();
                    lastBattery = now;
                }
                if (now - lastNodeCheck > 5000) {
                    lastNodeCheck = now;
                    if (reportNodeState(cfg, base)) {
                        failingSince = 0;
                    } else if (failingSince == 0) {
                        failingSince = now;
                    }
                    // El Mac cambió de IP o dejó de conocernos: volver a buscarlo.
                    if (failingSince != 0 && now - failingSince > 60000) {
                        say(Status.Phase.SEARCHING, "Perdí el contacto con tu Mac. Buscándolo de nuevo…");
                        stopAgent();
                        return;
                    }
                    // La IP del teléfono cambió: el agente debe anunciar la nueva.
                    String ip = localIpFor(base);
                    if (ip != null && localIp != null && !ip.equals(localIp)) {
                        stopAgent();
                        return;
                    }
                }
                sleep(1000);
            }
            if (stopping) break;
            Process dead;
            synchronized (this) {
                dead = agent;
            }
            if (dead == null) break;
            int code = dead.exitValue();
            if (System.currentTimeMillis() - started > 60000) backoff = 1000;
            say(Status.Phase.ERROR,
                    "El agente se detuvo (código " + code + "). Lo vuelvo a arrancar…");
            sleep(backoff);
            backoff = Math.min(backoff * 2, 30000);
        }
    }

    /**
     * Muestra lo que el coordinador sabe de este teléfono (Ready, Busy, Hot…).
     * Devuelve false si el Mac no responde o ya no nos conoce.
     */
    private boolean reportNodeState(JSONObject cfg, String base) {
        try {
            JSONObject res = new JSONObject(Net.getText(base + "/api/v1/nodes", cfg.getString("token"), 3000));
            JSONArray nodes = res.getJSONArray("nodes");
            String name = cfg.getString("name");
            for (int i = 0; i < nodes.length(); i++) {
                JSONObject n = nodes.getJSONObject(i);
                if (!name.equals(n.optString("name"))) continue;
                String state = n.optString("status");
                String text;
                switch (state) {
                    case "Ready": text = "Funcionando: este teléfono ya trabaja para tu Mac."; break;
                    case "Busy": text = "Funcionando: atendiendo peticiones."; break;
                    case "Down": text = "Cargando el modelo en memoria…"; break;
                    case "Hot": text = "En pausa: el teléfono está caliente. Volverá solo al enfriarse."; break;
                    case "LowBattery": text = "En pausa: batería baja. Enchúfalo para que siga trabajando."; break;
                    case "Draining": text = "En pausa desde el Mac (Drain)."; break;
                    case "Worker": text = "Modo dividido: este teléfono guarda y calcula una parte de las capas del modelo."; break;
                    default: text = "Estado en el Mac: " + state;
                }
                say("Ready".equals(state) || "Busy".equals(state) || "Worker".equals(state)
                        ? Status.Phase.RUNNING : Status.Phase.STARTING, text);
                notifyText(text);
                return !"Offline".equals(state);
            }
            return false;
        } catch (Exception e) {
            // El Mac puede estar reiniciándose; el agente reintenta solo.
            return false;
        }
    }

    /** IP local con la que este teléfono llega al Mac (UDP connect no envía nada). */
    private static String localIpFor(String base) {
        try (java.net.DatagramSocket s = new java.net.DatagramSocket()) {
            java.net.URL u = new java.net.URL(base);
            s.connect(java.net.InetAddress.getByName(u.getHost()), u.getPort() > 0 ? u.getPort() : 80);
            return s.getLocalAddress().getHostAddress();
        } catch (Exception e) {
            return null;
        }
    }

    private void say(Status.Phase phase, String message) {
        if (!stopping) Status.set(phase, message);
    }

    private void sayProgress(Status.Phase phase, String message, double progress) {
        if (!stopping) Status.progress(phase, message, progress);
    }

    // ---------------------------------------------------------------- utilidades

    private File batteryFile() {
        return new File(getFilesDir(), "battery.json");
    }

    /** Batería en el formato de termux-battery-status, que es lo que lee pc-agent. */
    private void writeBattery() {
        Intent b = registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
        if (b == null) return;
        try {
            int level = b.getIntExtra(BatteryManager.EXTRA_LEVEL, -1);
            int scale = b.getIntExtra(BatteryManager.EXTRA_SCALE, 100);
            int plugged = b.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0);
            int status = b.getIntExtra(BatteryManager.EXTRA_STATUS, -1);
            int temp = b.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, Integer.MIN_VALUE);
            JSONObject json = new JSONObject();
            json.put("percentage", level >= 0 && scale > 0 ? level * 100 / scale : JSONObject.NULL);
            json.put("plugged", plugged == 0 ? "UNPLUGGED"
                    : plugged == BatteryManager.BATTERY_PLUGGED_AC ? "PLUGGED_AC"
                    : plugged == BatteryManager.BATTERY_PLUGGED_USB ? "PLUGGED_USB" : "PLUGGED_WIRELESS");
            json.put("status", status == BatteryManager.BATTERY_STATUS_CHARGING ? "CHARGING"
                    : status == BatteryManager.BATTERY_STATUS_FULL ? "FULL"
                    : status == BatteryManager.BATTERY_STATUS_NOT_CHARGING ? "NOT_CHARGING" : "DISCHARGING");
            if (temp != Integer.MIN_VALUE) json.put("temperature", temp / 10.0);
            writeAtomically(batteryFile(), json.toString());
        } catch (Exception ignored) {
            // sin batería este ciclo; el agente lo marca como null
        }
    }

    /** Mata llama-server/pc-agent que hayan quedado de una ejecución anterior. */
    private static void killStrays(String lib) {
        File[] procs = new File("/proc").listFiles();
        if (procs == null) return;
        int self = android.os.Process.myPid();
        for (File p : procs) {
            int pid;
            try {
                pid = Integer.parseInt(p.getName());
            } catch (NumberFormatException e) {
                continue;
            }
            if (pid == self) continue;
            try {
                String cmd = new String(Files.readAllBytes(new File(p, "cmdline").toPath()), StandardCharsets.UTF_8);
                if (cmd.startsWith(lib + "/" + AGENT) || cmd.startsWith(lib + "/" + LLAMA)
                        || cmd.startsWith(lib + "/" + RPC)) {
                    android.os.Process.killProcess(pid);
                }
            } catch (IOException | SecurityException ignored) {
                // procesos de otros usuarios
            }
        }
    }

    private synchronized void stopAgent() {
        if (agent == null) {
            killStrays(getApplicationInfo().nativeLibraryDir);
            return;
        }
        agent.destroy(); // SIGTERM: pc-agent apaga su llama-server
        try {
            if (!agent.waitFor(3, java.util.concurrent.TimeUnit.SECONDS)) agent.destroyForcibly();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        agent = null;
        killStrays(getApplicationInfo().nativeLibraryDir);
    }

    private void shutdown() {
        stopping = true;
        if (worker != null) worker.interrupt();
        new Thread(() -> {
            try {
                if (worker != null) worker.join(5000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            stopAgent();
            releaseLocks();
            Status.set(Status.Phase.STOPPED, "Detenido. Pulsa «Unirme» para volver a trabajar para tu Mac.");
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
        }, "cluster-stop").start();
    }

    private String deviceLabel() {
        String name = Settings.Global.getString(getContentResolver(), "device_name");
        if (name == null || name.trim().isEmpty()) name = Build.MANUFACTURER + " " + Build.MODEL;
        return name.trim();
    }

    private static String q(String s) {
        return "\"" + s.replace("\\", "\\\\").replace("\"", "\\\"") + "\"";
    }

    private static void writeAtomically(File file, String text) throws IOException {
        File tmp = new File(file.getPath() + ".tmp");
        try (OutputStream out = new FileOutputStream(tmp)) {
            out.write(text.getBytes(StandardCharsets.UTF_8));
        }
        if (!tmp.renameTo(file)) throw new IOException("no se pudo escribir " + file.getName());
    }

    private void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    private void acquireLocks() {
        PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "phonecluster:agent");
        wakeLock.setReferenceCounted(false);
        wakeLock.acquire();
        WifiManager wifi = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        int mode = Build.VERSION.SDK_INT >= 29
                ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
        wifiLock = wifi.createWifiLock(mode, "phonecluster:wifi");
        wifiLock.setReferenceCounted(false);
        wifiLock.acquire();
    }

    private void releaseLocks() {
        if (wakeLock != null && wakeLock.isHeld()) wakeLock.release();
        if (wifiLock != null && wifiLock.isHeld()) wifiLock.release();
    }

    // ---------------------------------------------------------------- notificación

    private Notification buildNotification(String text) {
        NotificationManager nm = getSystemService(NotificationManager.class);
        if (nm.getNotificationChannel(CHANNEL) == null) {
            nm.createNotificationChannel(new NotificationChannel(CHANNEL, "Phone Cluster",
                    NotificationManager.IMPORTANCE_LOW));
        }
        PendingIntent open = PendingIntent.getActivity(this, 0,
                new Intent(this, MainActivity.class), PendingIntent.FLAG_IMMUTABLE);
        PendingIntent stop = PendingIntent.getService(this, 1,
                new Intent(this, ClusterService.class).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE);
        return new Notification.Builder(this, CHANNEL)
                .setSmallIcon(android.R.drawable.stat_sys_upload_done)
                .setContentTitle("Phone Cluster")
                .setContentText(text)
                .setContentIntent(open)
                .setOngoing(true)
                .addAction(new Notification.Action.Builder(null, "Detener", stop).build())
                .build();
    }

    private void startInForeground(String text) {
        Notification n = buildNotification(text);
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(NOTIFICATION_ID, n);
        }
    }

    private String lastNotified = "";

    private void notifyText(String text) {
        if (stopping || text.equals(lastNotified)) return;
        lastNotified = text;
        getSystemService(NotificationManager.class).notify(NOTIFICATION_ID, buildNotification(text));
    }
}
