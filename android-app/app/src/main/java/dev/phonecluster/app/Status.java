package dev.phonecluster.app;

/** Estado que el servicio publica y la pantalla muestra. */
public final class Status {
    public enum Phase {
        STOPPED,
        SEARCHING,
        WAITING_APPROVAL,
        REJECTED,
        DOWNLOADING,
        STARTING,
        RUNNING,
        ERROR,
    }

    public static final class Snapshot {
        public final Phase phase;
        public final String message;
        /** 0..1 o negativo si no aplica. */
        public final double progress;
        public final String detail;

        Snapshot(Phase phase, String message, double progress, String detail) {
            this.phase = phase;
            this.message = message;
            this.progress = progress;
            this.detail = detail;
        }
    }

    private static Snapshot current =
            new Snapshot(Phase.STOPPED, "Pulsa «Unirme» para que este teléfono trabaje para tu Mac.", -1, "");

    private Status() {}

    public static synchronized Snapshot get() {
        return current;
    }

    public static synchronized void set(Phase phase, String message) {
        current = new Snapshot(phase, message, -1, current.detail);
    }

    public static synchronized void progress(Phase phase, String message, double progress) {
        current = new Snapshot(phase, message, progress, current.detail);
    }

    public static synchronized void detail(String detail) {
        current = new Snapshot(current.phase, current.message, current.progress, detail);
    }
}
