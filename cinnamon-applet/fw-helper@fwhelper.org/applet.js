/*
 * Framework Monitor - a Cinnamon panel readout for the Framework Laptop 13.
 *
 * Shows CPU temperature, fan speed, battery draw, CPU load, memory and disk usage.
 *
 * Reads /proc and sysfs directly rather than talking to fw-helperd. Every value shown
 * here is world-readable - the cros_ec temperatures, fan1_input, the battery's
 * current/voltage, /proc/stat and /proc/meminfo - so the applet needs no daemon, no
 * D-Bus policy and no root, and it keeps working when the daemon is stopped. If it ever
 * needs to show something only the daemon knows (a duty we are commanding, rather than
 * an RPM firmware chose), that is the point to add D-Bus, not before.
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
    };
    if (acDir) {
        let online = readInt(acDir + "/online");
        if (online !== null) out.onAc = (online === 1);
    }
    if (!batDir) return out;

    out.status = readFile(batDir + "/status");
    out.capacity = readInt(batDir + "/capacity");

    // Only meaningful while discharging - see the header note.
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

    // Charge family: watts is V x I, and time remaining is charge over current.
    let ua = readInt(batDir + "/current_now");
    let uv = readInt(batDir + "/voltage_now");
    if (ua !== null && uv !== null && ua > 0) {
        out.watts = (ua / 1e6) * (uv / 1e6);
        let uah = readInt(batDir + "/charge_now");
        if (uah !== null) out.minutes = Math.round(uah * 60 / ua);
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

        this.settings = new Settings.AppletSettings(this, metadata.uuid, instanceId);
        for (let key of ["interval", "show-temp", "show-fan", "show-power",
                         "show-cpu", "show-mem", "show-disk", "disk-path",
                         "compact", "show-icon"]) {
            this.settings.bind(key, key.replace(/-/g, "_"),
                () => this._onSettingsChanged());
        }
        this._applyAppearance();

        this.menuManager = new PopupMenu.PopupMenuManager(this);
        this.menu = new Applet.AppletPopupMenu(this, orientation);
        this.menuManager.addMenu(this.menu);
        this._menuRows = {};

        this._prevCpu = null;
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

    /* The icon costs more panel width than any single reading, so it is optional and
     * off by default - the numbers are the point of this applet. */
    _applyAppearance: function () {
        if (this.show_icon) {
            this.set_applet_icon_symbolic_name("temperature-symbolic");
        } else {
            this.hide_applet_icon();
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
        let busy = (1 - deltaIdle / deltaTotal) * 100;
        return Math.max(0, Math.min(100, busy));
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
                // Four digits plus a unit is the widest field here; thousands notation
                // keeps it to three characters without losing anything readable.
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
        // CPU, memory and disk are all percentages, so bare numbers would be three
        // indistinguishable figures in a row. Each keeps a one-letter tag even in
        // compact mode - it costs one character and is the difference between a
        // readout and a puzzle.
        if (this.show_cpu && cpu !== null) {
            parts.push((compact ? "C" : "CPU ") + Math.round(cpu) + "%");
        }
        if (this.show_mem && memory !== null) {
            parts.push((compact ? "M" : "RAM ") + Math.round(memory.percent) + "%");
        }
        if (this.show_disk && disk !== null) {
            parts.push((compact ? "D" : "Disk ") + Math.round(disk.percent) + "%");
        }

        // A middle dot groups the fields more tightly than whitespace can, and stays
        // legible where a double space just reads as a gap.
        let separator = compact ? " · " : "  ";
        this.set_applet_label(parts.length > 0 ? parts.join(separator) : "no sensors");

        let tip = [];
        if (cpuTemp !== null) tip.push("CPU " + cpuTemp.toFixed(1) + " °C");
        if (fanRpm !== null) tip.push("Fan " + fanRpm + " rpm");
        if (cpu !== null) tip.push("Load " + cpu.toFixed(0) + "%");
        if (memory !== null) {
            tip.push("RAM " + memory.usedGiB.toFixed(1) + " / "
                + memory.totalGiB.toFixed(1) + " GiB");
        }
        if (disk !== null) {
            tip.push("Disk " + disk.freeGiB.toFixed(0) + " GiB free");
        }
        if (battery.capacity !== null) tip.push("Battery " + battery.capacity + "%");
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
