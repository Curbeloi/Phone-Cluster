package dev.phonecluster.app;

import android.Manifest;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.graphics.Typeface;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.PowerManager;
import android.provider.Settings;
import android.text.InputType;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ProgressBar;
import android.widget.ScrollView;
import android.widget.TextView;

/** Una sola pantalla: estado, botón Unirme/Detener y dirección manual del Mac. */
public class MainActivity extends Activity {
    private final Handler ui = new Handler(Looper.getMainLooper());
    private TextView message;
    private TextView detail;
    private ProgressBar bar;
    private Button action;
    private EditText address;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        requestWindowFeature(Window.FEATURE_NO_TITLE);
        View root = buildLayout();
        // Android 15+ dibuja de borde a borde: dejar sitio a las barras del sistema.
        root.setOnApplyWindowInsetsListener((v, insets) -> {
            android.graphics.Insets bars = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.ime());
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom);
            return insets;
        });
        setContentView(root);
        SharedPreferences p = ClusterService.prefs(this);
        address.setText(p.getString("manual_address", ""));
        // Solo la primera vez: al rotar o volver desde Recientes el enlace ya se usó.
        boolean fromHistory = (getIntent().getFlags() & Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY) != 0;
        if (savedInstanceState == null && !fromHistory) handleIntent(getIntent());
        // Si ya estaba unido, seguir trabajando al abrir la app.
        if (p.getBoolean("enabled", false)) start();
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handleIntent(intent);
    }

    @Override
    protected void onResume() {
        super.onResume();
        ui.post(refresh);
    }

    @Override
    protected void onPause() {
        ui.removeCallbacks(refresh);
        super.onPause();
    }

    private final Runnable refresh = new Runnable() {
        @Override
        public void run() {
            Status.Snapshot s = Status.get();
            message.setText(s.message);
            detail.setText(s.detail);
            if (s.progress >= 0) {
                bar.setVisibility(View.VISIBLE);
                bar.setIndeterminate(false);
                bar.setProgress((int) Math.round(s.progress * 1000));
            } else if (s.phase == Status.Phase.SEARCHING || s.phase == Status.Phase.STARTING
                    || s.phase == Status.Phase.WAITING_APPROVAL) {
                bar.setVisibility(View.VISIBLE);
                bar.setIndeterminate(true);
            } else {
                bar.setVisibility(View.INVISIBLE);
            }
            boolean running = s.phase != Status.Phase.STOPPED;
            action.setText(running ? "Detener" : "Unirme");
            ui.postDelayed(this, 500);
        }
    };

    private void onAction() {
        if (Status.get().phase != Status.Phase.STOPPED) {
            startService(new Intent(this, ClusterService.class).setAction(ClusterService.ACTION_STOP));
            return;
        }
        saveAddress();
        askPermissions();
        start();
    }

    private void start() {
        Intent i = new Intent(this, ClusterService.class).setAction(ClusterService.ACTION_START);
        try {
            startForegroundService(i);
            if (Status.get().phase == Status.Phase.STOPPED) {
                Status.set(Status.Phase.SEARCHING, "Buscando tu Mac en la Wi-Fi…");
            }
        } catch (RuntimeException e) {
            // ForegroundServiceStartNotAllowedException si Android no lo permite ahora mismo.
            Status.set(Status.Phase.ERROR, "Android no dejó arrancar el servicio: " + e.getMessage());
        }
    }

    private void saveAddress() {
        String text = Net.normalizeAddress(address.getText().toString());
        SharedPreferences.Editor e = ClusterService.prefs(this).edit();
        if (text == null) e.remove("manual_address"); else e.putString("manual_address", text);
        e.apply();
    }

    /** Notificaciones (Android 13+) y que el sistema no lo duerma por ahorro de batería. */
    private void askPermissions() {
        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
        PowerManager pm = getSystemService(PowerManager.class);
        if (!pm.isIgnoringBatteryOptimizations(getPackageName())) {
            try {
                startActivity(new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                        Uri.parse("package:" + getPackageName())));
            } catch (RuntimeException ignored) {
                // algunos fabricantes no tienen esta pantalla
            }
        }
    }

    /**
     * Enlace {@code phonecluster://connect?host=…&port=…} de la página del QR:
     * guarda la dirección del Mac y se une sin buscar. Para probar sin tocar la
     * pantalla, en un build depurable, también vale
     * {@code am start -n dev.phonecluster.app/.MainActivity --es coordinator http://10.0.2.2:7800 --ez join true}.
     */
    private void handleIntent(Intent intent) {
        if (intent == null) return;
        Uri data = intent.getData();
        if (data != null && "phonecluster".equals(data.getScheme())) {
            String host = data.getQueryParameter("host");
            String port = data.getQueryParameter("port");
            if (isLanAddress(host) && isPort(port)) {
                String wanted = Net.normalizeAddress(host + ":" + port);
                SharedPreferences p = ClusterService.prefs(this);
                String current = p.getString("manual_address", p.getString("coordinator", null));
                if (current == null || current.equals(wanted)) {
                    connectTo(host + ":" + port);
                } else {
                    // Un enlace no cambia de Mac en silencio: podría venir de cualquier web.
                    new AlertDialog.Builder(this)
                            .setTitle("¿Cambiar de Mac?")
                            .setMessage("Este teléfono está unido a " + current
                                    + ". ¿Quieres unirlo al Mac de " + wanted + "?")
                            .setPositiveButton("Cambiar", (d, w) -> {
                                address.setText(host + ":" + port);
                                saveAddress();
                                if (Status.get().phase == Status.Phase.STOPPED) {
                                    connectTo(host + ":" + port);
                                    return;
                                }
                                // Parar el servicio y unirse al nuevo cuando haya parado.
                                startService(new Intent(this, ClusterService.class)
                                        .setAction(ClusterService.ACTION_STOP));
                                ui.postDelayed(() -> connectTo(host + ":" + port), 2000);
                            })
                            .setNegativeButton("Cancelar", null)
                            .show();
                }
            }
            setIntent(new Intent(this, MainActivity.class)); // consumido
            return;
        }
        // Extras de prueba (am start): solo en builds depurables. Si no,
        // cualquier app del teléfono podría unirlo a otro coordinador.
        boolean debuggable = (getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0;
        if (!debuggable) return;
        String coordinator = intent.getStringExtra("coordinator");
        if (coordinator != null) {
            address.setText(coordinator);
            saveAddress();
        }
        if (intent.getBooleanExtra("join", false) && Status.get().phase == Status.Phase.STOPPED) {
            start();
        }
    }

    private void connectTo(String hostPort) {
        address.setText(hostPort);
        saveAddress();
        if (Status.get().phase == Status.Phase.STOPPED) {
            askPermissions();
            start();
        }
    }

    /** Solo IPv4 privadas: el enlace llega desde una página web. */
    static boolean isLanAddress(String host) {
        if (host == null) return false;
        String[] p = host.split("\\.");
        if (p.length != 4) return false;
        int[] o = new int[4];
        try {
            for (int i = 0; i < 4; i++) {
                o[i] = Integer.parseInt(p[i]);
                if (o[i] < 0 || o[i] > 255) return false;
            }
        } catch (NumberFormatException e) {
            return false;
        }
        return o[0] == 10 || (o[0] == 172 && o[1] >= 16 && o[1] <= 31) || (o[0] == 192 && o[1] == 168);
    }

    static boolean isPort(String port) {
        try {
            int n = Integer.parseInt(port);
            return n > 0 && n < 65536;
        } catch (NumberFormatException e) {
            return false;
        }
    }

    // ---------------------------------------------------------------- vista

    private View buildLayout() {
        int pad = dp(20);
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(pad, dp(32), pad, pad);

        TextView title = new TextView(this);
        title.setText("Phone Cluster");
        title.setTextSize(TypedValue.COMPLEX_UNIT_SP, 28);
        title.setTypeface(Typeface.DEFAULT_BOLD);
        col.addView(title);

        TextView subtitle = new TextView(this);
        subtitle.setText("Este teléfono presta su procesador a tu Mac para ejecutar un modelo de IA.");
        subtitle.setTextSize(TypedValue.COMPLEX_UNIT_SP, 15);
        subtitle.setPadding(0, dp(6), 0, dp(24));
        subtitle.setAlpha(0.75f);
        col.addView(subtitle);

        message = new TextView(this);
        message.setTextSize(TypedValue.COMPLEX_UNIT_SP, 20);
        col.addView(message);

        bar = new ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal);
        bar.setMax(1000);
        bar.setPadding(0, dp(12), 0, dp(4));
        col.addView(bar, new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT,
                LinearLayout.LayoutParams.WRAP_CONTENT));

        detail = new TextView(this);
        detail.setTextSize(TypedValue.COMPLEX_UNIT_SP, 14);
        detail.setAlpha(0.7f);
        detail.setPadding(0, dp(4), 0, dp(20));
        col.addView(detail);

        action = new Button(this);
        action.setText("Unirme");
        action.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
        action.setOnClickListener(v -> onAction());
        col.addView(action, new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, dp(56)));

        TextView help = new TextView(this);
        help.setText("¿No encuentra tu Mac? Escribe la dirección que aparece en la app del Mac "
                + "(por ejemplo 192.168.1.50) y pulsa «Unirme».");
        help.setTextSize(TypedValue.COMPLEX_UNIT_SP, 13);
        help.setAlpha(0.7f);
        help.setPadding(0, dp(32), 0, dp(6));
        col.addView(help);

        address = new EditText(this);
        address.setHint("Dirección del Mac (opcional)");
        address.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        address.setSingleLine(true);
        address.setGravity(Gravity.START);
        col.addView(address);

        ScrollView scroll = new ScrollView(this);
        scroll.addView(col);
        return scroll;
    }

    private int dp(int v) {
        return Math.round(TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v,
                getResources().getDisplayMetrics()));
    }
}
