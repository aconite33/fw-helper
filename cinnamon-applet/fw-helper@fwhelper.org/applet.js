/*
 * Framework Monitor - a Cinnamon panel readout for the Framework Laptop 13.
 *
 * The panel keeps rate-like values as sparklines and point values as text: a single
 * number cannot show whether 40% is a spike settling or a climb starting, while a graph
 * of a temperature trades precision for a shape nobody needs.
 *
 * The dropdown is the detailed view - ring gauges, a usage history chart, per-core
 * bars, a details breakdown, and the processes actually responsible.
 *
 * Reads /proc and sysfs directly rather than talking to fw-helperd (see read.js). Every
 * value is world-readable, so this needs no daemon, no D-Bus policy and no root, and it
 * keeps working when the daemon is stopped. If it ever needs something only the daemon
 * knows - a fan duty we are commanding, rather than an RPM firmware chose - that is the
 * point to add D-Bus, not before.
 */

const Applet = imports.ui.applet;
const GLib = imports.gi.GLib;
const Main = imports.ui.main;
const Mainloop = imports.mainloop;
const PopupMenu = imports.ui.popupMenu;
const Settings = imports.ui.settings;
const St = imports.gi.St;

// Cinnamon's own multi-file applets (calendar, grouped-window-list) use require() for
// siblings rather than the imports.* namespace.
const Draw = require("./draw");
const Read = require("./read");

const COLOR_USER = [0.20, 0.52, 0.93];
const COLOR_SYSTEM = [0.92, 0.34, 0.30];
const COLOR_MEM = [0.36, 0.72, 0.46];
const COLOR_TEMP = [0.95, 0.55, 0.25];
const COLOR_BATTERY = [0.42, 0.76, 0.42];

const PANEL_GAP = 4;
const PANEL_PAD_Y = 3;
const PANEL_BATTERY_WIDTH = 42;
const PANEL_BOLT_WIDTH = 10;
/* Beyond this the panel is a disk manager rather than a readout; the dropdown still
 * lists every mount. */
const PANEL_MAX_DISKS = 4;

const MENU_WIDTH = 300;
const CPU_PANEL_HEIGHT = 168;
const MEM_PANEL_HEIGHT = 92;
const BAR_PANEL_HEIGHT = 26;

/* Temperature has no natural 0..1 scale, so the gauge needs an explicit span. 30 C is
 * about idle on this board and 100 C is Tjmax, where the CPU throttles itself. */
const TEMP_MIN_C = 30;
const TEMP_MAX_C = 100;

/* Load average is unbounded; a full ring at one-per-core is the point where the machine
 * is saturated, which is the reading that matters. */
function loadFraction(load, coreCount) {
    return Math.min(1, parseFloat(load) / Math.max(1, coreCount));
}

/* Everything in a Cinnamon applet runs on the compositor's own main loop, so any slow
 * synchronous work here does not merely make the applet late - it stalls the desktop,
 * pointer included. Two readings are heavy enough to deserve their own, slower cadence
 * than the display tick:
 *
 *   - Walking every /proc/PID for the process list: hundreds of file reads.
 *   - Asking each mount for its free space: normally instant, but a filesystem that is
 *     unhealthy or backed by something slow can make the call block.
 *
 * Neither changes fast enough to be worth a two-second refresh, so both are throttled
 * independently of how often the panel redraws.
 */
const DISK_INTERVAL_S = 5;
const PROC_INTERVAL_S = 3;
/* A single update taking longer than this is a problem worth naming rather than
 * silently tolerating: at this length it is visible as a stutter. */
const SLOW_UPDATE_MS = 250;

function nowSeconds() {
    return GLib.get_monotonic_time() / 1000000;
}

function FrameworkMonitor(metadata, orientation, panelHeight, instanceId) {
    this._init(metadata, orientation, panelHeight, instanceId);
}

FrameworkMonitor.prototype = {
    __proto__: Applet.TextIconApplet.prototype,

    _init: function (metadata, orientation, panelHeight, instanceId) {
        Applet.TextIconApplet.prototype._init.call(this, orientation, panelHeight, instanceId);

        this.set_applet_label("…");
        this._applet_label.add_style_class_name("fw-helper-label");

        this._cpuHistory = [];
        this._memHistory = [];
        this._coreValues = [];
        this._mounts = [];
        this._batteryLevel = null;
        this._batteryCharging = false;
        this._prevCpu = null;
        this._prevCores = null;
        this._procMap = null;
        this._procList = [];
        this._state = {};
        this._lastDisk = 0;
        this._lastProc = 0;
        this._procJiffies = 0;
        this._warned = false;

        this._graphArea = new St.DrawingArea({ style_class: "fw-helper-graph" });
        this._graphArea.connect("repaint", (area) => this._drawPanel(area));
        this.actor.add_actor(this._graphArea);
        // TextIconApplet has already added the icon box and label, so without this the
        // graphs would land after the numbers.
        this.actor.set_child_at_index(this._graphArea, 0);

        this.settings = new Settings.AppletSettings(this, metadata.uuid, instanceId);
        for (let key of ["interval", "show-temp", "show-fan", "show-power",
                         "show-battery", "show-cpu", "show-mem", "show-disk",
                         "show-cores", "disk-bar-width", "graph-width", "compact",
                         "show-icon", "proc-count"]) {
            this.settings.bind(key, key.replace(/-/g, "_"),
                () => this._onSettingsChanged());
        }
        this._applyAppearance();

        this.menuManager = new PopupMenu.PopupMenuManager(this);
        this.menu = new Applet.AppletPopupMenu(this, orientation);
        this.menuManager.addMenu(this.menu);

        // The dropdown is taller than a laptop screen once every sensor, disk and
        // process has a row, and an over-tall popup does not scroll on its own - it
        // just runs off the top, taking the CPU section and the first disk with it.
        this._menuBox = new St.BoxLayout({ vertical: true });
        this._scroll = new St.ScrollView({
            x_fill: true, y_fill: true, y_align: St.Align.START,
            style_class: "vfade",
        });
        this._scroll.set_policy(St.PolicyType.NEVER, St.PolicyType.AUTOMATIC);
        this._scroll.set_clip_to_allocation(true);
        this._scroll.add_actor(this._menuBox);
        this.menu.addActor(this._scroll);

        // While a drag is in progress the menu must not treat the motion as a click
        // outside itself and close.
        let vscroll = this._scroll.get_vscroll_bar();
        vscroll.connect("scroll-start", () => { this.menu.passEvents = true; });
        vscroll.connect("scroll-stop", () => { this.menu.passEvents = false; });

        this._menuSection = new PopupMenu.PopupMenuSection();
        this._menuBox.add(this._menuSection.actor);
        this._rows = {};
        // Processes are the expensive reading, so they are only gathered while the menu
        // is actually showing them - and gathered at once on opening rather than after
        // the next tick, so the list is never a blank first impression.
        this.menu.connect("open-state-changed", (menu, open) => {
            if (!open) return;
            // Measured against the monitor rather than fixed in CSS: the same applet
            // runs on a 1504-tall laptop panel and on an external display.
            let monitor = Main.layoutManager.primaryMonitor;
            let cap = Math.max(240, Math.round(monitor.height * 0.75));
            this._scroll.style = "max-height: " + cap + "px;";
            this._update();
        });

        this._resolve();
        this._update();
        this._restartTimer();
    },

    _resolve: function () {
        this._ec = Read.hwmon("cros_ec");
        this._cpuHwmon = Read.hwmon("k10temp") || Read.hwmon("coretemp");
        this._bat = Read.supply("Battery");
        this._ac = Read.supply("Mains");
    },

    _slotWidth: function () {
        return Math.max(16, this.graph_width || 42);
    },

    _applyAppearance: function () {
        // The icon costs more panel width than any single reading, and the numbers are
        // the point of this applet.
        if (this.show_icon) {
            this.set_applet_icon_symbolic_name("temperature-symbolic");
        } else {
            this.hide_applet_icon();
        }

        let slot = this._slotWidth();
        let width = 0;
        if (this.show_cpu) width += slot + PANEL_GAP;
        if (this.show_cores) width += this._coreBarsWidth() + PANEL_GAP;
        if (this.show_mem) width += slot + PANEL_GAP;
        if (this.show_disk) {
            let n = Math.min(PANEL_MAX_DISKS, this._panelDisks().length);
            if (n > 0) width += n * (this._diskBarWidth() + PANEL_GAP);
        }
        if (this.show_battery && this._batteryLevel !== null) {
            width += PANEL_BATTERY_WIDTH + PANEL_GAP;
            if (this._batteryCharging) width += PANEL_BOLT_WIDTH;
        }

        this._graphArea.set_width(Math.max(0, width));
        this._graphArea.visible = width > 0;

        // History is one sample per pixel column, so a width change resizes it.
        let max = slot;
        for (let key of ["_cpuHistory", "_memHistory"]) {
            if (this[key].length > max) this[key] = this[key].slice(this[key].length - max);
        }
        this._graphArea.queue_repaint();
    },

    _coreBarsWidth: function () {
        let n = Math.max(1, this._coreValues.length);
        return Math.min(64, n * 4);
    },

    _diskBarWidth: function () {
        return Math.max(6, this.disk_bar_width || 14);
    },

    _panelDisks: function () {
        return this._mounts || [];
    },

    _onSettingsChanged: function () {
        this._applyAppearance();
        this._update();
        this._restartTimer();
    },

    _restartTimer: function () {
        if (this._timer) {
            Mainloop.source_remove(this._timer);
            this._timer = null;
        }
        let seconds = Math.max(1, this.interval || 2);
        this._timer = Mainloop.timeout_add_seconds(seconds, () => {
            this._tick();
            return true;
        });
    },

    /* A throw inside a Mainloop callback returns undefined, which removes the source -
     * so an error on one tick would silently stop the applet forever rather than
     * skipping a frame. Catching keeps the timer alive, and the warning fires once so a
     * recurring fault does not fill the journal. */
    _tick: function () {
        let started = nowSeconds();
        try {
            this._update();
        } catch (e) {
            if (!this._warned) {
                this._warned = true;
                global.logError("fw-helper applet update failed: " + e);
            }
            return;
        }
        let elapsedMs = (nowSeconds() - started) * 1000;
        if (elapsedMs > SLOW_UPDATE_MS && !this._warned) {
            this._warned = true;
            global.logWarning("fw-helper applet update took "
                + elapsedMs.toFixed(0) + " ms, which stalls the compositor");
        }
    },

    _fg: function (area) {
        try {
            let c = area.get_theme_node().get_foreground_color();
            return [c.red / 255, c.green / 255, c.blue / 255];
        } catch (e) {
            return [0.6, 0.6, 0.6];
        }
    },

    /* ---------- panel ---------- */

    _drawPanel: function (area) {
        let cr = area.get_context();
        try {
            let [w, h] = area.get_surface_size();
            if (w <= 0 || h <= 0) return;
            let fg = this._fg(area);
            let gh = Math.max(6, h - PANEL_PAD_Y * 2);
            let slot = this._slotWidth();
            let x = 0;

            if (this.show_cpu) {
                Draw.spark(cr, x, PANEL_PAD_Y, slot, gh, this._cpuHistory,
                    COLOR_USER, fg);
                x += slot + PANEL_GAP;
            }
            if (this.show_cores) {
                let cw = this._coreBarsWidth();
                Draw.cores(cr, x, PANEL_PAD_Y, cw, gh, this._coreValues,
                    COLOR_USER, fg);
                x += cw + PANEL_GAP;
            }
            if (this.show_mem) {
                Draw.spark(cr, x, PANEL_PAD_Y, slot, gh, this._memHistory,
                    COLOR_MEM, fg);
                x += slot + PANEL_GAP;
            }
            if (this.show_disk) {
                let bw = this._diskBarWidth();
                let disks = this._panelDisks().slice(0, PANEL_MAX_DISKS);
                for (let d of disks) {
                    Draw.usageBar(cr, x, PANEL_PAD_Y, bw, gh, d.percent / 100, fg, true);
                    x += bw + PANEL_GAP;
                }
            }
            if (this.show_battery && this._batteryLevel !== null) {
                if (this._batteryCharging) {
                    Draw.bolt(cr, x + PANEL_BOLT_WIDTH / 2, h / 2, gh * 0.7, fg);
                    x += PANEL_BOLT_WIDTH;
                }
                let bh = Math.min(gh, 19);
                Draw.battery(cr, x, (h - bh) / 2, PANEL_BATTERY_WIDTH, bh,
                    this._batteryLevel, this._batteryCharging, fg);
            }
        } finally {
            // GJS will not collect the Cairo context on its own.
            cr.$dispose();
        }
    },

    /* ---------- menu scaffolding ---------- */

    _section: function (key, title) {
        if (this._rows[key]) return;
        if (Object.keys(this._rows).length > 0) {
            this._menuSection.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        }
        let item = new PopupMenu.PopupBaseMenuItem({ reactive: false });
        item.actor.add_style_class_name("fw-helper-row");
        let label = new St.Label({ text: title, style_class: "fw-helper-section" });
        item.addActor(label, { expand: true });
        this._menuSection.addMenuItem(item);
        this._rows[key] = true;
    },

    _row: function (key, label, value, color) {
        if (!this._rows[key]) {
            let item = new PopupMenu.PopupBaseMenuItem({ reactive: false });
            item.actor.add_style_class_name("fw-helper-row");
            let box = new St.BoxLayout();
            let dot = null;
            if (color) {
                dot = new St.DrawingArea({ style_class: "fw-helper-swatch" });
                dot.set_width(10);
                dot.set_height(10);
                dot.connect("repaint", (a) => {
                    let cr = a.get_context();
                    try {
                        Draw.swatch(cr, 0, 1, 8, color);
                    } finally {
                        cr.$dispose();
                    }
                });
                box.add_actor(dot);
            }
            let left = new St.Label({ text: label, style_class: "fw-helper-key" });
            box.add_actor(left);
            let right = new St.Label({ text: "—", style_class: "fw-helper-value" });
            item.addActor(box, { expand: true });
            item.addActor(right, { align: St.Align.END });
            this._menuSection.addMenuItem(item);
            this._rows[key] = { item: item, left: left, right: right };
        }
        let row = this._rows[key];
        row.left.set_text(label);
        row.right.set_text(value);
        row.item.actor.visible = true;
    },

    _hideRow: function (key) {
        if (this._rows[key] && this._rows[key].item) {
            this._rows[key].item.actor.visible = false;
        }
    },

    /* A drawn block inside the menu. `paint(cr, w, h, fg)` does the work. */
    _canvas: function (key, height, paint) {
        if (!this._rows[key]) {
            let item = new PopupMenu.PopupBaseMenuItem({ reactive: false });
            item.actor.add_style_class_name("fw-helper-row");
            let area = new St.DrawingArea({ style_class: "fw-helper-canvas" });
            area.set_width(MENU_WIDTH);
            area.set_height(height);
            area.connect("repaint", (a) => {
                let cr = a.get_context();
                try {
                    let [w, h] = a.get_surface_size();
                    this._rows[key].paint(cr, w, h, this._fg(a));
                } finally {
                    cr.$dispose();
                }
            });
            item.addActor(area, { expand: true });
            this._menuSection.addMenuItem(item);
            this._rows[key] = { item: item, area: area, paint: paint };
        }
        this._rows[key].paint = paint;
        this._rows[key].area.queue_repaint();
    },

    /* ---------- update ---------- */

    _update: function () {
        let s = this._state;

        s.ecTemps = Read.sensors(this._ec);
        s.fanRpm = this._ec ? Read.int(this._ec + "/fan1_input") : null;
        // A failed read usually means the hwmon index moved under us. Re-resolve once
        // rather than showing dashes until the applet is reloaded.
        if (s.ecTemps.length === 0 && s.fanRpm === null) {
            this._resolve();
            s.ecTemps = Read.sensors(this._ec);
            s.fanRpm = this._ec ? Read.int(this._ec + "/fan1_input") : null;
        }

        s.cpuTemps = Read.sensors(this._cpuHwmon);
        s.battery = Read.battery(this._bat, this._ac);
        s.memory = Read.memory();
        // Throttled: free space is the part that can block, and a disk does not fill
        // fast enough to need a two-second refresh. A drive mounted or unmounted still
        // appears or vanishes within DISK_INTERVAL_S, with nothing to configure.
        let now = nowSeconds();
        if (now - this._lastDisk >= DISK_INTERVAL_S || this._mounts.length === 0) {
            this._lastDisk = now;
            this._mounts = Read.mounts();
        }
        s.mounts = this._mounts;
        s.load = Read.loadAvg();
        s.uptime = Read.uptime();

        this._batteryLevel = (s.battery.capacity === null)
            ? null : s.battery.capacity / 100;
        this._batteryCharging = (s.battery.status === "Charging");

        let times = Read.cpuTimes();
        s.cpu = null;
        if (times) {
            s.cpu = Read.cpuDelta(this._prevCpu, times.all);
            if (this._prevCores && times.cores.length === this._prevCores.length) {
                this._coreValues = times.cores.map((c, i) => {
                    let d = Read.cpuDelta(this._prevCores[i], c);
                    return d === null ? 0 : d.busy;
                });
            }
            this._prevCpu = times.all;
            this._prevCores = times.cores;
            s.coreCount = times.cores.length || 1;
        }

        if (s.cpu !== null) this._push("_cpuHistory", s.cpu.busy);
        if (s.memory !== null) this._push("_memHistory", s.memory.percent / 100);

        // Only walk /proc while someone is looking at the result, and then no more often
        // than PROC_INTERVAL_S - this is hundreds of synchronous reads on the
        // compositor's own loop.
        //
        // Jiffies are accumulated across skipped ticks so the shares stay correct: the
        // delta must be measured over the same span the process counters were, not over
        // the last display tick.
        if (s.cpu !== null) this._procJiffies += s.cpu.totalJiffies;
        if (this.menu.isOpen && this._procJiffies > 0
            && now - this._lastProc >= PROC_INTERVAL_S) {
            this._lastProc = now;
            let r = Read.processes(this._procMap, this._procJiffies,
                s.coreCount || 1, Math.max(1, this.proc_count || 5));
            this._procMap = r.map;
            this._procList = r.list;
            this._procJiffies = 0;
        }

        s.cpuTemp = this._pickCpuTemp(s.cpuTemps, s.ecTemps);

        this._graphArea.queue_repaint();
        this._applyAppearance();
        this._updatePanel(s);
        if (this.menu.isOpen) this._updateMenu(s);
    },

    _push: function (key, value) {
        let h = this[key];
        h.push(value);
        let max = this._slotWidth();
        while (h.length > max) h.shift();
    },

    /* Prefer the CPU die sensor (k10temp Tctl on AMD) over the EC's board sensors, then
     * anything the EC labels as cpu, then the hottest thing on the board. */
    _pickCpuTemp: function (cpuTemps, ecTemps) {
        if (cpuTemps.length > 0) return cpuTemps[0].celsius;
        let named = ecTemps.filter((t) => t.label.indexOf("cpu") !== -1);
        let pool = named.length > 0 ? named : ecTemps;
        if (pool.length === 0) return null;
        return Math.max.apply(null, pool.map((t) => t.celsius));
    },

    _updatePanel: function (s) {
        let compact = this.compact;
        let parts = [];

        if (this.show_temp && s.cpuTemp !== null) {
            parts.push(Math.round(s.cpuTemp) + (compact ? "°" : "°C"));
        }
        if (this.show_fan && s.fanRpm !== null) {
            // A stopped fan is worth saying plainly rather than showing "0": firmware
            // keeps it off entirely at idle here, so that is normal, not a fault.
            if (s.fanRpm === 0) parts.push("off");
            else if (compact) {
                parts.push(s.fanRpm >= 1000
                    ? (s.fanRpm / 1000).toFixed(1) + "k" : String(s.fanRpm));
            } else parts.push(s.fanRpm + " rpm");
        }
        if (this.show_power && s.battery.watts !== null) {
            parts.push(compact
                ? Math.round(s.battery.watts) + "W"
                : s.battery.watts.toFixed(1) + " W");
        }
        // The battery percentage is no longer text: it is drawn inside the battery,
        // where the number and the level are the same object rather than two.

        this.set_applet_label(parts.join("  "));

        let tip = [];
        if (s.cpuTemp !== null) tip.push("CPU " + s.cpuTemp.toFixed(1) + " °C");
        if (s.cpu !== null) tip.push("Load " + (s.cpu.busy * 100).toFixed(0) + "%");
        if (s.fanRpm !== null) tip.push("Fan " + s.fanRpm + " rpm");
        if (s.memory) {
            tip.push("RAM " + s.memory.usedGiB.toFixed(1) + " / "
                + s.memory.totalGiB.toFixed(1) + " GiB");
        }
        for (let d of (s.mounts || [])) {
            tip.push("Disk " + d.name + " " + d.percent.toFixed(0) + "% used, "
                + d.freeGiB.toFixed(0) + " GiB free");
        }
        if (s.battery.capacity !== null) {
            tip.push("Battery " + s.battery.capacity + "%"
                + (s.battery.status ? " (" + s.battery.status + ")" : ""));
        }
        this.set_applet_tooltip(tip.join("\n"));
    },

    /* ---------- menu ---------- */

    _updateMenu: function (s) {
        this._cpuSection(s);
        this._memSection(s);
        this._diskSection(s);
        this._sensorSection(s);
        this._batterySection(s);
        this._processSection(s);
    },

    _cpuSection: function (s) {
        this._section("sec:cpu", "CPU");

        let busy = s.cpu === null ? 0 : s.cpu.busy;
        let user = s.cpu === null ? 0 : s.cpu.user;
        let system = s.cpu === null ? 0 : s.cpu.system;
        let temp = s.cpuTemp;
        let load = s.load;
        let coreCount = s.coreCount || 1;
        let history = this._cpuHistory;
        let cores = this._coreValues;

        this._canvas("cpu:canvas", CPU_PANEL_HEIGHT, (cr, w, h, fg) => {
            let cy = 40;
            // Temperature and load flank the usage ring, which is the one that carries
            // two segments and therefore earns the extra size.
            if (temp !== null) {
                Draw.ring(cr, 52, cy, 24, 6, [{
                    value: (temp - TEMP_MIN_C) / (TEMP_MAX_C - TEMP_MIN_C),
                    color: COLOR_TEMP,
                }], Math.round(temp) + "°", null, fg);
                Draw.text(cr, 52, cy + 40, "temp", 10, fg, 0.5, "center");
            }
            Draw.ring(cr, w / 2, cy, 32, 8, [
                { value: system, color: COLOR_SYSTEM },
                { value: user, color: COLOR_USER },
            ], Math.round(busy * 100) + "%", null, fg);
            Draw.text(cr, w / 2, cy + 48, "usage", 10, fg, 0.5, "center");

            if (load) {
                Draw.ring(cr, w - 52, cy, 24, 6, [{
                    value: loadFraction(load.one, coreCount),
                    color: COLOR_USER,
                }], load.one, null, fg);
                Draw.text(cr, w - 52, cy + 40, "load", 10, fg, 0.5, "center");
            }

            Draw.text(cr, w / 2, 94, "Usage history", 10, fg, 0.5, "center");
            Draw.spark(cr, 10, 100, w - 20, 40, history, COLOR_USER, fg);
            if (cores.length > 0) {
                Draw.cores(cr, 10, 144, w - 20, 16, cores, COLOR_USER, fg);
            }
        });

        if (s.cpu !== null) {
            this._row("cpu:user", "User", (user * 100).toFixed(1) + "%", COLOR_USER);
            this._row("cpu:system", "System", (system * 100).toFixed(1) + "%",
                COLOR_SYSTEM);
            this._row("cpu:idle", "Idle", ((1 - busy) * 100).toFixed(1) + "%");
        }
        if (s.load) {
            this._row("cpu:load", "Load average",
                s.load.one + "   " + s.load.five + "   " + s.load.fifteen);
        }
        if (s.uptime) this._row("cpu:uptime", "Uptime", s.uptime);
    },

    _memSection: function (s) {
        if (!s.memory) return;
        this._section("sec:mem", "Memory");
        let m = s.memory;
        let history = this._memHistory;

        this._canvas("mem:canvas", MEM_PANEL_HEIGHT, (cr, w, h, fg) => {
            Draw.ring(cr, 46, 44, 28, 7,
                [{ value: m.percent / 100, color: COLOR_MEM }],
                Math.round(m.percent) + "%", null, fg);
            Draw.text(cr, w / 2 + 40, 20, "Usage history", 10, fg, 0.5, "center");
            Draw.spark(cr, 92, 28, w - 102, 48, history, COLOR_MEM, fg);
        });

        this._row("mem:used", "Used", m.usedGiB.toFixed(2) + " GiB", COLOR_MEM);
        this._row("mem:cached", "Cached", m.cachedGiB.toFixed(2) + " GiB");
        this._row("mem:free", "Available", m.availableGiB.toFixed(2) + " GiB");
        this._row("mem:total", "Total", m.totalGiB.toFixed(2) + " GiB");
        if (m.swapTotalGiB > 0) {
            this._row("mem:swap", "Swap",
                m.swapUsedGiB.toFixed(2) + " / " + m.swapTotalGiB.toFixed(2) + " GiB");
        }
    },

    _diskSection: function (s) {
        if (!s.mounts || s.mounts.length === 0) return;
        this._section("sec:disk", "Disks");

        // Rows are keyed by mount point, so an unmounted drive's row is hidden rather
        // than left showing a stale figure, and a newly mounted one builds its own.
        let live = {};
        for (let d of s.mounts) {
            live[d.point] = true;
            let percent = d.percent / 100;
            this._canvas("disk:bar:" + d.point, BAR_PANEL_HEIGHT, (cr, w, h, fg) => {
                Draw.usageBar(cr, 10, 7, w - 20, 12, percent, fg, false);
            });
            this._row("disk:row:" + d.point,
                d.name + "   " + d.fstype,
                d.usedGiB.toFixed(0) + " of " + d.totalGiB.toFixed(0) + " GiB   ("
                + d.percent.toFixed(0) + "%,  " + d.freeGiB.toFixed(0) + " free)");
        }
        for (let key of Object.keys(this._rows)) {
            let m = key.match(/^disk:(?:bar|row):(.*)$/);
            if (m && !live[m[1]]) this._hideRow(key);
        }
    },

    _sensorSection: function (s) {
        if (s.ecTemps.length === 0 && s.fanRpm === null) return;
        this._section("sec:sensors", "Sensors");
        for (let t of s.cpuTemps) {
            this._row("t:" + t.label, t.label, t.celsius.toFixed(1) + " °C");
        }
        for (let t of s.ecTemps) {
            let value = t.celsius.toFixed(1) + " °C";
            if (t.crit !== null) value += "   (crit " + t.crit.toFixed(0) + ")";
            this._row("t:ec:" + t.label, t.label, value);
        }
        if (s.fanRpm !== null) {
            this._row("fan", "fan", s.fanRpm === 0 ? "off" : s.fanRpm + " rpm");
        }
    },

    _batterySection: function (s) {
        let b = s.battery;
        if (b.capacity === null) return;
        this._section("sec:battery", "Battery");

        let level = b.capacity / 100;
        let charging = (b.status === "Charging");
        this._canvas("bat:canvas", BAR_PANEL_HEIGHT, (cr, w, h, fg) => {
            Draw.hbar(cr, 10, 8, w - 20, 10, level,
                charging ? COLOR_USER : COLOR_BATTERY, fg);
        });

        this._row("bat:level", "Level", b.capacity + "%   ·   " + (b.status || "—"),
            charging ? COLOR_USER : COLOR_BATTERY);
        // Charge held against charge the pack can hold: what a percentage cannot say,
        // which is how much running time is left in absolute terms.
        if (b.nowMah !== null && b.fullMah !== null) {
            this._row("bat:charge", "Charge",
                Math.round(b.nowMah) + " of " + Math.round(b.fullMah) + " mAh");
        }
        // And capacity against what it shipped with, which is the ageing story.
        if (b.healthPercent !== null) {
            this._row("bat:health", "Health",
                b.healthPercent.toFixed(1) + "%   ("
                + Math.round(b.designMah) + " mAh design)");
        }
        if (b.cycles !== null) this._row("bat:cycles", "Cycles", String(b.cycles));
        if (b.volts !== null) this._row("bat:volts", "Voltage", b.volts.toFixed(2) + " V");

        if (b.watts !== null) {
            let value = b.watts.toFixed(2) + " W";
            if (b.minutes !== null) {
                let hrs = Math.floor(b.minutes / 60);
                let mins = b.minutes % 60;
                value += "   ·   " + hrs + "h " + (mins < 10 ? "0" : "") + mins + "m left";
            }
            this._row("bat:draw", "System draw", value);
        } else if (b.onAc) {
            // Say why there is no number rather than showing a dash: nothing reports
            // whole-machine draw on mains.
            this._row("bat:draw", "System draw", "on AC — not reported");
        } else {
            this._hideRow("bat:draw");
        }
    },

    _processSection: function (s) {
        if (this._procList.length === 0) return;
        this._section("sec:proc", "Top processes");
        let limit = Math.max(1, this.proc_count || 5);
        for (let i = 0; i < limit; i++) {
            let key = "proc:" + i;
            let p = this._procList[i];
            if (!p) {
                this._hideRow(key);
                continue;
            }
            let name = p.name.length > 22 ? p.name.substring(0, 21) + "…" : p.name;
            this._row(key, name,
                p.cpu.toFixed(1) + "%   " + p.rssGiB.toFixed(2) + " GiB");
        }
    },

    on_applet_clicked: function () {
        this.menu.toggle();
    },

    on_panel_height_changed: function () {
        this._graphArea.queue_repaint();
    },

    on_applet_removed_from_panel: function () {
        if (this._timer) {
            Mainloop.source_remove(this._timer);
            this._timer = null;
        }
        this.settings.finalize();
    },
};

function main(metadata, orientation, panelHeight, instanceId) {
    return new FrameworkMonitor(metadata, orientation, panelHeight, instanceId);
}
