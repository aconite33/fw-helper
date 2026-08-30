/*
 * Framework Monitor - a Cinnamon panel readout for the Framework Laptop 13.
 *
 * Rate-like values (CPU load, memory) are drawn as sparklines, because a single number
 * cannot show whether 40% is a spike settling or a climb starting. Point values that
 * only make sense exactly (CPU temperature, battery percentage, watts) stay as text -
 * a graph of those trades precision for a shape nobody needs. Disk gets a bar rather
 * than a sparkline: it barely moves, so its history is a flat line carrying no
 * information, while "how full" is the whole question.
 *
 * Reads /proc and sysfs directly rather than talking to fw-helperd. Every value shown
 * here is world-readable, so the applet needs no daemon, no D-Bus policy and no root,
 * and it keeps working when the daemon is stopped. If it ever needs to show something
 * only the daemon knows (a duty we are commanding, rather than an RPM firmware chose),
 * that is the point to add D-Bus, not before.
 *
 * Hardware facts that drive the shape of this file, learned the hard way elsewhere in
 * this project:
 *
 *   - hwmon indices are NOT stable across boots. Every node is resolved by reading its
 *     `name` file, never by index, and re-resolved whenever a read fails.
 *   - Battery power is only meaningful while discharging. On mains, `current_now`
 *     describes what is going INTO the battery, which is not what the machine is using.
 */

const Applet = imports.ui.applet;
const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;
const Mainloop = imports.mainloop;
const PopupMenu = imports.ui.popupMenu;
const Settings = imports.ui.settings;
const St = imports.gi.St;

const HWMON = "/sys/class/hwmon";
const POWER_SUPPLY = "/sys/class/power_supply";
const KIB_PER_GIB = 1048576;
const BYTES_PER_GIB = 1073741824;

/* Distinct hues rather than shades of the theme foreground: the point of three graphs
 * side by side is telling them apart. Chosen mid-saturation so they hold up on both a
 * light and a dark panel, which a theme-derived colour cannot do for three series. */
const COLOR_CPU = [0.36, 0.68, 0.96];
const COLOR_MEM = [0.47, 0.80, 0.50];
const COLOR_DISK = [0.96, 0.71, 0.36];

const GRAPH_GAP = 4;
const DISK_BAR_WIDTH = 9;
const GRAPH_PAD_Y = 3;

/* ---------- sysfs / proc helpers ----------------------------------------- */

function readFile(path) {
    try {
        let [ok, contents] = GLib.file_get_contents(path);
        if (!ok || contents === null) return null;
        // GJS hands back a Uint8Array on current versions and a string on older ones.
        let text = (contents instanceof Uint8Array)
            ? new TextDecoder().decode(contents)
            : String(contents);
        return text.trim();
    } catch (e) {
        return null;
    }
}

function readInt(path) {
    let raw = readFile(path);
    if (raw === null) return null;
    let n = parseInt(raw, 10);
    return isNaN(n) ? null : n;
}

function listDir(path) {
    let out = [];
    try {
        let dir = GLib.Dir.open(path, 0);
        let name;
        while ((name = dir.read_name()) !== null) out.push(name);
        dir.close();
    } catch (e) {
        // Directory absent: not an error, just a machine without this hardware.
    }
    return out;
}

/* Resolve an hwmon node by its name file. Never by index - the same node came up as
 * hwmon11 on one boot and hwmon9 on the next on the reference machine. */
function findHwmon(wanted) {
    for (let entry of listDir(HWMON)) {
        if (!entry.startsWith("hwmon")) continue;
        if (readFile(HWMON + "/" + entry + "/name") === wanted) {
            return HWMON + "/" + entry;
        }
    }
    return null;
}

/* Resolve a power supply by its `type`, not its name: this board calls the mains supply
 * ACAD, others call it AC or ADP1, and the battery is BAT0 as often as BAT1. */
function findSupply(wantedType) {
    for (let entry of listDir(POWER_SUPPLY)) {
        if (readFile(POWER_SUPPLY + "/" + entry + "/type") === wantedType) {
            return POWER_SUPPLY + "/" + entry;
        }
    }
    return null;
}

/* ---------- drawing ------------------------------------------------------ */

function clamp01(v) {
    return Math.max(0, Math.min(1, v));
}

/* Filled area chart. History runs oldest-to-newest left-to-right, values 0..1. */
function drawSpark(cr, x, y, w, h, history, color) {
    cr.setSourceRGBA(color[0], color[1], color[2], 0.13);
    cr.rectangle(x, y, w, h);
    cr.fill();

    if (history.length < 2) return;

    let step = w / (history.length - 1);
    let pointY = (i) => y + h - clamp01(history[i]) * h;

    cr.moveTo(x, y + h);
    for (let i = 0; i < history.length; i++) {
        cr.lineTo(x + i * step, pointY(i));
    }
    cr.lineTo(x + (history.length - 1) * step, y + h);
    cr.closePath();
    cr.setSourceRGBA(color[0], color[1], color[2], 0.33);
    cr.fill();

    cr.moveTo(x, pointY(0));
    for (let i = 1; i < history.length; i++) {
        cr.lineTo(x + i * step, pointY(i));
    }
    cr.setSourceRGBA(color[0], color[1], color[2], 0.95);
    cr.setLineWidth(1.5);
    cr.stroke();
}

/* Vertical fill bar, for a value whose history carries nothing. */
function drawBar(cr, x, y, w, h, value, color) {
    cr.setSourceRGBA(color[0], color[1], color[2], 0.13);
    cr.rectangle(x, y, w, h);
    cr.fill();

    if (value === null) return;
    let filled = clamp01(value) * h;
    cr.setSourceRGBA(color[0], color[1], color[2], 0.8);
    cr.rectangle(x, y + h - filled, w, filled);
    cr.fill();
}

/* ---------- readings ----------------------------------------------------- */

function readSensors(dir) {
    let out = [];
    if (!dir) return out;
    for (let i = 1; i <= 8; i++) {
        let milli = readInt(dir + "/temp" + i + "_input");
        if (milli === null) continue;
        let crit = readInt(dir + "/temp" + i + "_crit");
        out.push({
            label: readFile(dir + "/temp" + i + "_label") || ("temp" + i),
            celsius: milli / 1000,
            crit: crit === null ? null : crit / 1000,
        });
    }
    return out;
}

function readBattery(batDir, acDir) {
    let out = {
        status: null, capacity: null, watts: null, minutes: null, onAc: null,
        nowMah: null, fullMah: null, designMah: null, healthPercent: null,
        cycles: null, volts: null, technology: null, model: null,
    };
    if (acDir) {
        let online = readInt(acDir + "/online");
        if (online !== null) out.onAc = (online === 1);
    }
    if (!batDir) return out;

    out.status = readFile(batDir + "/status");
    out.capacity = readInt(batDir + "/capacity");
    out.cycles = readInt(batDir + "/cycle_count");
    out.technology = readFile(batDir + "/technology");
    out.model = readFile(batDir + "/model_name");

    let uv = readInt(batDir + "/voltage_now");
    if (uv !== null) out.volts = uv / 1e6;

    // Charge family (uAh). An energy-family battery reports uWh instead and has no
    // charge_* at all, so these simply stay null there.
    let now = readInt(batDir + "/charge_now");
    let full = readInt(batDir + "/charge_full");
    let design = readInt(batDir + "/charge_full_design");
    if (now !== null) out.nowMah = now / 1000;
    if (full !== null) out.fullMah = full / 1000;
    if (design !== null) out.designMah = design / 1000;
    // Health is what the pack can still hold against what it shipped able to hold.
    if (full !== null && design) out.healthPercent = (full / design) * 100;

    // Draw is only meaningful while discharging - see the header note.
    if (out.status !== "Discharging") return out;

    // Energy family first (power_now in uW); it is already watts and needs no
    // multiplication. This board has no power_now and uses the charge family instead,
    // so reading only the energy one would silently show nothing.
    let uw = readInt(batDir + "/power_now");
    if (uw !== null && uw > 0) {
        out.watts = uw / 1e6;
        let uwh = readInt(batDir + "/energy_now");
        if (uwh !== null) out.minutes = Math.round(uwh * 60 / uw);
        return out;
    }

    let ua = readInt(batDir + "/current_now");
    if (ua !== null && uv !== null && ua > 0) {
        out.watts = (ua / 1e6) * (uv / 1e6);
        if (now !== null) out.minutes = Math.round(now * 60 / ua);
    }
    return out;
}

/* Cumulative CPU jiffies since boot. Load is a RATE, so a single sample says nothing -
 * it has to be differenced against the previous one. */
function readCpuTimes() {
    let text = readFile("/proc/stat");
    if (text === null) return null;
    let line = text.split("\n")[0];
    if (!line.startsWith("cpu ")) return null;
    let fields = line.trim().split(/\s+/).slice(1).map(Number);
    if (fields.length < 5 || fields.some(isNaN)) return null;
    // idle + iowait: the machine is not doing work in either.
    return {
        idle: fields[3] + fields[4],
        total: fields.reduce((a, b) => a + b, 0),
    };
}

function readMemory() {
    let text = readFile("/proc/meminfo");
    if (text === null) return null;
    let kv = {};
    for (let line of text.split("\n")) {
        let m = line.match(/^(\w+):\s+(\d+)/);
        if (m) kv[m[1]] = parseInt(m[2], 10); // kB
    }
    if (!kv.MemTotal) return null;

    // MemAvailable, not MemFree. Free excludes reclaimable page cache, so on any
    // machine that has been up a while it reports nearly everything as "used" - which
    // is true of the kernel's bookkeeping and useless as a description of pressure.
    let available = (kv.MemAvailable !== undefined) ? kv.MemAvailable : kv.MemFree;
    let used = kv.MemTotal - available;

    let swapTotal = kv.SwapTotal || 0;
    return {
        totalGiB: kv.MemTotal / KIB_PER_GIB,
        usedGiB: used / KIB_PER_GIB,
        percent: (used / kv.MemTotal) * 100,
        swapTotalGiB: swapTotal / KIB_PER_GIB,
        swapUsedGiB: (swapTotal - (kv.SwapFree || 0)) / KIB_PER_GIB,
    };
}

function readDisk(path) {
    try {
        let info = Gio.File.new_for_path(path)
            .query_filesystem_info("filesystem::size,filesystem::free", null);
        let size = info.get_attribute_uint64("filesystem::size");
        let free = info.get_attribute_uint64("filesystem::free");
        if (!size) return null;
        let used = size - free;
        return {
            totalGiB: size / BYTES_PER_GIB,
            usedGiB: used / BYTES_PER_GIB,
            freeGiB: free / BYTES_PER_GIB,
            // Against total, so this can read a percent or two below `df`, which
            // computes against the space actually available to a non-root user.
            percent: (used / size) * 100,
        };
    } catch (e) {
        return null;
    }
}

function readLoadAvg() {
    let text = readFile("/proc/loadavg");
    if (text === null) return null;
    let f = text.split(/\s+/);
    return (f.length >= 3) ? f.slice(0, 3).join("  ") : null;
}

/* ---------- applet ------------------------------------------------------- */

function FrameworkMonitor(metadata, orientation, panelHeight, instanceId) {
    this._init(metadata, orientation, panelHeight, instanceId);
}

FrameworkMonitor.prototype = {
    __proto__: Applet.TextIconApplet.prototype,

    _init: function (metadata, orientation, panelHeight, instanceId) {
        Applet.TextIconApplet.prototype._init.call(this, orientation, panelHeight, instanceId);

        this.set_applet_label("…");
        // Trims the default applet padding and font size; see stylesheet.css.
        this._applet_label.add_style_class_name("fw-helper-label");

        this._cpuHistory = [];
        this._memHistory = [];
        this._diskValue = null;
        this._prevCpu = null;

        this._graphArea = new St.DrawingArea({ style_class: "fw-helper-graph" });
        this._graphArea.connect("repaint", (area) => this._drawGraphs(area));
        this.actor.add_actor(this._graphArea);
        // TextIconApplet has already added the icon box and label, so the graphs would
        // otherwise land after the numbers.
        this.actor.set_child_at_index(this._graphArea, 0);

        this.settings = new Settings.AppletSettings(this, metadata.uuid, instanceId);
        for (let key of ["interval", "show-temp", "show-fan", "show-power",
                         "show-battery", "show-cpu", "show-mem", "show-disk",
                         "disk-path", "graph-width", "compact", "show-icon"]) {
            this.settings.bind(key, key.replace(/-/g, "_"),
                () => this._onSettingsChanged());
        }
        this._applyAppearance();

        this.menuManager = new PopupMenu.PopupMenuManager(this);
        this.menu = new Applet.AppletPopupMenu(this, orientation);
        this.menuManager.addMenu(this.menu);
        this._menuRows = {};

        this._resolve();
        this._update();
        this._restartTimer();
    },

    /* Locate every node once, and again whenever a read fails. */
    _resolve: function () {
        this._ec = findHwmon("cros_ec");
        this._cpuHwmon = findHwmon("k10temp") || findHwmon("coretemp");
        this._bat = findSupply("Battery");
        this._ac = findSupply("Mains");
    },

    _slotWidth: function () {
        return Math.max(16, this.graph_width || 42);
    },

    _applyAppearance: function () {
        // The icon costs more panel width than any single reading, so it is optional
        // and off by default - the numbers are the point of this applet.
        if (this.show_icon) {
            this.set_applet_icon_symbolic_name("temperature-symbolic");
        } else {
            this.hide_applet_icon();
        }

        let slot = this._slotWidth();
        let width = 0;
        if (this.show_cpu) width += slot + GRAPH_GAP;
        if (this.show_mem) width += slot + GRAPH_GAP;
        if (this.show_disk) width += DISK_BAR_WIDTH + GRAPH_GAP;

        this._graphArea.set_width(Math.max(0, width));
        this._graphArea.visible = width > 0;

        // History is one sample per pixel column, so a width change resizes it.
        this._trimHistory();
        this._graphArea.queue_repaint();
    },

    _trimHistory: function () {
        let max = this._slotWidth();
        for (let key of ["_cpuHistory", "_memHistory"]) {
            let h = this[key];
            if (h.length > max) this[key] = h.slice(h.length - max);
        }
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
            this._update();
            return true; // keep repeating
        });
    },

    _drawGraphs: function (area) {
        let cr = area.get_context();
        try {
            let [w, h] = area.get_surface_size();
            if (w <= 0 || h <= 0) return;

            let gh = Math.max(6, h - GRAPH_PAD_Y * 2);
            let slot = this._slotWidth();
            let x = 0;

            if (this.show_cpu) {
                drawSpark(cr, x, GRAPH_PAD_Y, slot, gh, this._cpuHistory, COLOR_CPU);
                x += slot + GRAPH_GAP;
            }
            if (this.show_mem) {
                drawSpark(cr, x, GRAPH_PAD_Y, slot, gh, this._memHistory, COLOR_MEM);
                x += slot + GRAPH_GAP;
            }
            if (this.show_disk) {
                drawBar(cr, x, GRAPH_PAD_Y, DISK_BAR_WIDTH, gh, this._diskValue,
                    COLOR_DISK);
            }
        } finally {
            // GJS will not collect the Cairo context on its own.
            cr.$dispose();
        }
    },

    /* CPU busy percentage since the previous tick. Null on the first call, which has
     * nothing to difference against. */
    _cpuPercent: function () {
        let now = readCpuTimes();
        if (now === null) return null;
        let prev = this._prevCpu;
        this._prevCpu = now;
        if (prev === null) return null;

        let deltaTotal = now.total - prev.total;
        let deltaIdle = now.idle - prev.idle;
        if (deltaTotal <= 0) return null;
        return Math.max(0, Math.min(100, (1 - deltaIdle / deltaTotal) * 100));
    },

    _push: function (key, value) {
        let h = this[key];
        h.push(value);
        let max = this._slotWidth();
        if (h.length > max) h.shift();
    },

    _update: function () {
        let ecTemps = readSensors(this._ec);
        let fanRpm = this._ec ? readInt(this._ec + "/fan1_input") : null;

        // A failed read usually means the hwmon index moved under us. Re-resolve once
        // and retry rather than showing dashes until the applet is reloaded.
        if (ecTemps.length === 0 && fanRpm === null) {
            this._resolve();
            ecTemps = readSensors(this._ec);
            fanRpm = this._ec ? readInt(this._ec + "/fan1_input") : null;
        }

        let cpuTemps = readSensors(this._cpuHwmon);
        let battery = readBattery(this._bat, this._ac);
        let cpu = this._cpuPercent();
        let memory = readMemory();
        let disk = readDisk(this.disk_path || "/");

        if (cpu !== null) this._push("_cpuHistory", cpu / 100);
        if (memory !== null) this._push("_memHistory", memory.percent / 100);
        this._diskValue = disk === null ? null : disk.percent / 100;
        this._graphArea.queue_repaint();

        // Prefer the CPU die sensor (k10temp Tctl on AMD) over the EC's board sensors,
        // then anything the EC labels as cpu, then the hottest thing on the board.
        let cpuTemp = null;
        if (cpuTemps.length > 0) {
            cpuTemp = cpuTemps[0].celsius;
        } else {
            let named = ecTemps.filter((t) => t.label.indexOf("cpu") !== -1);
            let pool = named.length > 0 ? named : ecTemps;
            if (pool.length > 0) {
                cpuTemp = Math.max.apply(null, pool.map((t) => t.celsius));
            }
        }

        this._updatePanel(cpuTemp, fanRpm, battery, cpu, memory, disk);
        this._updateMenu(cpuTemps, ecTemps, fanRpm, battery, cpu, memory, disk);
    },

    _updatePanel: function (cpuTemp, fanRpm, battery, cpu, memory, disk) {
        let compact = this.compact;
        let parts = [];

        // Text is reserved for values that only mean anything exactly. Load and memory
        // are in the graphs; putting them here too would just be noise.
        if (this.show_temp && cpuTemp !== null) {
            parts.push(Math.round(cpuTemp) + (compact ? "°" : "°C"));
        }
        if (this.show_fan && fanRpm !== null) {
            // A stopped fan is worth saying plainly rather than showing "0". On this
            // board firmware keeps it off entirely at idle, so that is the normal
            // state, not a fault.
            if (fanRpm === 0) {
                parts.push("off");
            } else if (compact) {
                parts.push(fanRpm >= 1000
                    ? (fanRpm / 1000).toFixed(1) + "k"
                    : String(fanRpm));
            } else {
                parts.push(fanRpm + " rpm");
            }
        }
        if (this.show_power && battery.watts !== null) {
            parts.push(compact
                ? Math.round(battery.watts) + "W"
                : battery.watts.toFixed(1) + " W");
        }
        if (this.show_battery && battery.capacity !== null) {
            // A leading + marks charging, so a rising percentage is not mistaken for a
            // draining one at a glance.
            let mark = (battery.status === "Charging") ? "+" : "";
            parts.push(mark + battery.capacity + "%");
        }

        let separator = compact ? " · " : "  ";
        this.set_applet_label(parts.length > 0 ? parts.join(separator) : "");

        let tip = [];
        if (cpuTemp !== null) tip.push("CPU " + cpuTemp.toFixed(1) + " °C");
        if (fanRpm !== null) tip.push("Fan " + fanRpm + " rpm");
        if (cpu !== null) tip.push("Load " + cpu.toFixed(0) + "%");
        if (memory !== null) {
            tip.push("RAM " + memory.usedGiB.toFixed(1) + " / "
                + memory.totalGiB.toFixed(1) + " GiB");
        }
        if (disk !== null) tip.push("Disk " + disk.freeGiB.toFixed(0) + " GiB free");
        if (battery.capacity !== null) {
            tip.push("Battery " + battery.capacity + "%"
                + (battery.status ? " (" + battery.status + ")" : ""));
        }
        if (battery.watts !== null) {
            tip.push("Draw " + battery.watts.toFixed(2) + " W");
        } else if (battery.onAc) {
            tip.push("On AC");
        }
        this.set_applet_tooltip(tip.join("\n"));
    },

    /* Rows are created once and updated in place. Rebuilding the menu every tick would
     * close it under the user's cursor while they are reading it. Creation order is
     * call order, so _updateMenu must call these in the order they should appear. */
    _menuRow: function (key, label) {
        if (this._menuRows[key]) return this._menuRows[key];

        let item = new PopupMenu.PopupBaseMenuItem({ reactive: false });
        let left = new St.Label({ text: label });
        let right = new St.Label({ text: "—" });
        item.addActor(left, { expand: true });
        item.addActor(right, { align: St.Align.END });
        this.menu.addMenuItem(item);

        this._menuRows[key] = { item: item, left: left, right: right };
        return this._menuRows[key];
    },

    _setRow: function (key, label, value) {
        let row = this._menuRow(key, label);
        row.left.set_text(label);
        row.right.set_text(value);
    },

    _separatorOnce: function (key) {
        if (this._menuRows[key]) return;
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._menuRows[key] = true;
    },

    _updateMenu: function (cpuTemps, ecTemps, fanRpm, battery, cpu, memory, disk) {
        for (let t of cpuTemps) {
            this._setRow("cputemp:" + t.label, t.label, t.celsius.toFixed(1) + " °C");
        }
        for (let t of ecTemps) {
            let value = t.celsius.toFixed(1) + " °C";
            if (t.crit !== null) value += "   (crit " + t.crit.toFixed(0) + ")";
            this._setRow("ec:" + t.label, t.label, value);
        }
        if (fanRpm !== null) {
            this._setRow("fan", "fan", fanRpm === 0 ? "off" : fanRpm + " rpm");
        }

        if (cpu !== null || memory !== null || disk !== null) {
            this._separatorOnce("sep:system");
        }
        if (cpu !== null) {
            let value = cpu.toFixed(0) + "%";
            let load = readLoadAvg();
            if (load !== null) value += "   (load " + load + ")";
            this._setRow("cpu", "cpu load", value);
        }
        if (memory !== null) {
            this._setRow("mem", "memory",
                memory.usedGiB.toFixed(1) + " / " + memory.totalGiB.toFixed(1)
                + " GiB   (" + memory.percent.toFixed(0) + "%)");
            if (memory.swapTotalGiB > 0) {
                this._setRow("swap", "swap",
                    memory.swapUsedGiB.toFixed(1) + " / "
                    + memory.swapTotalGiB.toFixed(1) + " GiB");
            }
        }
        if (disk !== null) {
            this._setRow("disk", "disk " + (this.disk_path || "/"),
                disk.usedGiB.toFixed(0) + " / " + disk.totalGiB.toFixed(0)
                + " GiB   (" + disk.freeGiB.toFixed(0) + " GiB free)");
        }

        if (battery.capacity !== null || battery.watts !== null) {
            this._separatorOnce("sep:battery");
        }
        if (battery.capacity !== null) {
            let value = battery.capacity + "%";
            if (battery.status) value += "  ·  " + battery.status;
            this._setRow("battery", "battery", value);
        }
        // Charge held against charge the pack can hold - the pair that says how much
        // running time is left in absolute terms, which a percentage cannot.
        if (battery.nowMah !== null && battery.fullMah !== null) {
            this._setRow("charge", "charge",
                Math.round(battery.nowMah) + " / " + Math.round(battery.fullMah)
                + " mAh");
        }
        // And capacity against what it shipped with, which is the ageing story.
        if (battery.healthPercent !== null) {
            this._setRow("health", "health",
                battery.healthPercent.toFixed(1) + "% of "
                + Math.round(battery.designMah) + " mAh design");
        }
        if (battery.cycles !== null) {
            this._setRow("cycles", "charge cycles", String(battery.cycles));
        }
        if (battery.volts !== null) {
            this._setRow("volts", "voltage", battery.volts.toFixed(2) + " V");
        }
        if (battery.watts !== null) {
            let value = battery.watts.toFixed(2) + " W";
            if (battery.minutes !== null) {
                let h = Math.floor(battery.minutes / 60);
                let m = battery.minutes % 60;
                value += "  ·  " + h + "h " + (m < 10 ? "0" : "") + m + "m left";
            }
            this._setRow("draw", "system draw", value);
        } else if (battery.onAc) {
            // Say why there is no number rather than showing a dash. Nothing reports
            // whole-machine draw on mains.
            this._setRow("draw", "system draw", "on AC — not reported");
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
