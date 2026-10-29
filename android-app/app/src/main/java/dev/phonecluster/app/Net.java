package dev.phonecluster.app;

import android.content.Context;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiManager;

import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/** HTTP en la LAN y descubrimiento del Mac por Bonjour (NSD). */
final class Net {
    static final String SERVICE_TYPE = "_phonecluster._tcp.";

    private Net() {}

    interface Progress {
        void update(long done, long total);
    }

    static JSONObject postJson(String url, JSONObject body) throws IOException {
        HttpURLConnection c = open(url, 5000, 10000);
        try {
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setRequestProperty("Content-Type", "application/json");
            try (OutputStream out = c.getOutputStream()) {
                out.write(body.toString().getBytes(StandardCharsets.UTF_8));
            }
            int code = c.getResponseCode();
            String text = readAll(code >= 400 ? c.getErrorStream() : c.getInputStream());
            if (code >= 400) {
                throw new IOException("HTTP " + code + ": " + text);
            }
            return new JSONObject(text);
        } catch (org.json.JSONException e) {
            throw new IOException("respuesta no válida del Mac", e);
        } finally {
            c.disconnect();
        }
    }

    static String getText(String url, String token, int timeoutMs) throws IOException {
        HttpURLConnection c = open(url, timeoutMs, timeoutMs);
        try {
            if (token != null) {
                c.setRequestProperty("Authorization", "Bearer " + token);
            }
            int code = c.getResponseCode();
            String text = readAll(code >= 400 ? c.getErrorStream() : c.getInputStream());
            if (code >= 400) {
                throw new IOException("HTTP " + code);
            }
            return text;
        } finally {
            c.disconnect();
        }
    }

    static boolean reachable(String base) {
        try {
            getText(base + "/healthz", null, 2500);
            return true;
        } catch (IOException e) {
            return false;
        }
    }

    /**
     * Descarga a {@code dest} reanudando desde {@code dest.part} si existe.
     * Devuelve cuando el fichero está completo.
     */
    static void download(String url, String token, File dest, long expected, Progress progress,
                         java.util.function.BooleanSupplier cancelled) throws IOException {
        File part = new File(dest.getPath() + ".part");
        long have = part.exists() ? part.length() : 0;
        if (expected > 0 && have > expected) {
            part.delete();
            have = 0;
        }
        if (expected > 0 && have == expected) {
            // Se completó pero no llegó a renombrarse: pedir "bytes=N-" daría 416.
            if (!part.renameTo(dest)) throw new IOException("no se pudo guardar el modelo");
            return;
        }
        HttpURLConnection c = open(url, 5000, 30000);
        try {
            c.setRequestProperty("Authorization", "Bearer " + token);
            if (have > 0) {
                c.setRequestProperty("Range", "bytes=" + have + "-");
            }
            int code = c.getResponseCode();
            if (code == 200) {
                have = 0; // el servidor ignoró el Range: empezar de cero
            } else if (code == 416) {
                part.delete(); // el trozo no encaja con el modelo actual: la próxima vez desde cero
                throw new IOException("descarga a medias no válida; se reinicia");
            } else if (code != 206) {
                throw new IOException("el Mac respondió HTTP " + code + " al pedir el modelo");
            }
            long total = expected > 0 ? expected : have + Math.max(0, c.getContentLengthLong());
            try (InputStream in = c.getInputStream();
                 OutputStream out = new FileOutputStream(part, have > 0)) {
                byte[] buf = new byte[1 << 16];
                long done = have;
                long lastReport = 0;
                int n;
                while ((n = in.read(buf)) > 0) {
                    if (cancelled.getAsBoolean()) {
                        throw new IOException("descarga cancelada");
                    }
                    out.write(buf, 0, n);
                    done += n;
                    if (done - lastReport > (1 << 20)) {
                        progress.update(done, total);
                        lastReport = done;
                    }
                }
            }
        } finally {
            c.disconnect();
        }
        if (expected > 0 && part.length() != expected) {
            throw new IOException("descarga incompleta (" + part.length() + " de " + expected + " bytes)");
        }
        if (!part.renameTo(dest)) {
            throw new IOException("no se pudo guardar el modelo");
        }
    }

    /**
     * Busca el coordinador en la Wi-Fi. Devuelve {@code http://ip:puerto} o null.
     */
    @SuppressWarnings("deprecation")
    static String discover(Context ctx, long timeoutMs) {
        NsdManager nsd = (NsdManager) ctx.getSystemService(Context.NSD_SERVICE);
        WifiManager wifi = (WifiManager) ctx.getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        WifiManager.MulticastLock lock = wifi.createMulticastLock("phonecluster-nsd");
        lock.setReferenceCounted(false);
        lock.acquire();
        AtomicReference<String> found = new AtomicReference<>();
        CountDownLatch done = new CountDownLatch(1);
        // Resolver de uno en uno: NsdManager falla con FAILURE_ALREADY_ACTIVE si no.
        Object resolving = new Object();
        NsdManager.DiscoveryListener listener = new NsdManager.DiscoveryListener() {
            @Override public void onStartDiscoveryFailed(String t, int e) { done.countDown(); }
            @Override public void onStopDiscoveryFailed(String t, int e) {}
            @Override public void onDiscoveryStarted(String t) {}
            @Override public void onDiscoveryStopped(String t) {}
            @Override public void onServiceLost(NsdServiceInfo s) {}

            @Override public void onServiceFound(NsdServiceInfo service) {
                synchronized (resolving) {
                    if (found.get() != null) return;
                    nsd.resolveService(service, new NsdManager.ResolveListener() {
                        @Override public void onResolveFailed(NsdServiceInfo s, int e) {}

                        @Override public void onServiceResolved(NsdServiceInfo s) {
                            InetAddress host = s.getHost();
                            if (host == null) return;
                            String ip = host instanceof Inet4Address
                                    ? host.getHostAddress()
                                    : "[" + host.getHostAddress() + "]";
                            found.compareAndSet(null, "http://" + ip + ":" + s.getPort());
                            done.countDown();
                        }
                    });
                }
            }
        };
        try {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener);
            done.await(timeoutMs, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        } finally {
            try {
                nsd.stopServiceDiscovery(listener);
            } catch (IllegalArgumentException ignored) {
                // nunca llegó a arrancar
            }
            lock.release();
        }
        return found.get();
    }

    /** Acepta "192.168.1.50", "192.168.1.50:7800" o una URL completa. */
    static String normalizeAddress(String text) {
        String t = text.trim();
        if (t.isEmpty()) return null;
        if (!t.startsWith("http://") && !t.startsWith("https://")) {
            t = "http://" + t;
        }
        while (t.endsWith("/")) t = t.substring(0, t.length() - 1);
        String hostPart = t.substring(t.indexOf("://") + 3);
        if (!hostPart.contains(":")) {
            t = t + ":7800";
        }
        return t;
    }

    private static HttpURLConnection open(String url, int connectMs, int readMs) throws IOException {
        HttpURLConnection c = (HttpURLConnection) new URL(url).openConnection();
        c.setConnectTimeout(connectMs);
        c.setReadTimeout(readMs);
        c.setUseCaches(false);
        return c;
    }

    private static String readAll(InputStream in) throws IOException {
        if (in == null) return "";
        try (InputStream s = in) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            int n;
            while ((n = s.read(buf)) > 0) out.write(buf, 0, n);
            return out.toString(StandardCharsets.UTF_8.name());
        }
    }
}
