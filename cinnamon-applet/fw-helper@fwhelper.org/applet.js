/*
 * Framework Monitor - a Cinnamon panel readout for the Framework Laptop 13.
 *
 * Reads sysfs directly rather than talking to fw-helperd. Every value shown here is
 * world-readable on this board - the cros_ec temperatures, fan1_input, and the battery's
 * current/voltage - so the applet needs no daemon, no D-Bus policy and no root, and it
 * keeps working when the daemon is stopped. If it ever needs to show something only the
 * daemon knows (a duty we are commanding, rather than an RPM firmware chose), that is
 * the point to add D-Bus, not before.
 *
 * Two hardware facts drive the shape of this file, both learned the hard way elsewhere
 * in this project:
 *
 *   - hwmon indices are NOT stable across boots. Every node is resolved by reading its
 *     `name` file, never by index, and re-resolved whenever a read fails.
 *   - Battery power is only meaningful while discharging. On mains, `current_now`
 *     describes what is going INTO the battery, which is not what the machine is using.
 *     Shown as "on AC" rather than as a number that means something else.
 */

const Applet = imports.ui.applet;
const GLib = imports.gi.GLib;
const Mainloop = imports.mainloop;
const PopupMenu = imports.ui.popupMenu;
const Settings = imports.ui.settings;
const St = imports.gi.St;

const HWMON = "/sys/class/hwmon";
const POWER_SUPPLY = "/sys/class/power_supply";

/* ---------- sysfs helpers ------------------------------------------------ */

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

/* ---------- applet ------------------------------------------------------- */

function FrameworkMonitor(metadata, orientation, panelHeight, instanceId) {
    this._init(metadata, orientation, panelHeight, instanceId);
}

FrameworkMonitor.prototype = {
    __proto__: Applet.TextIconApplet.prototype,

    _init: function (metadata, orientation, panelHeight, instanceId) {
        Applet.TextIconApplet.prototype._init.call(this, orientation, panelHeight, instanceId);

        this.set_applet_icon_symbolic_name("temperature-symbolic");
        this.set_applet_label("…");

        this.settings = new Settings.AppletSettings(this, metadata.uuid, instanceId);
        for (let key of ["interval", "show-temp", "show-fan", "show-power"]) {
            this.settings.bind(key, key.replace(/-/g, "_"), () => this._restartTimer());
        }

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
        this._cpu = findHwmon("k10temp") || findHwmon("coretemp");
        this._bat = findSupply("Battery");
        this._ac = findSupply("Mains");
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

        let cpuTemps = readSensors(this._cpu);
        let battery = readBattery(this._bat, this._ac);

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

        this._updatePanel(cpuTemp, fanRpm, battery);
        this._updateMenu(cpuTemps, ecTemps, fanRpm, battery);
    },

    _updatePanel: function (cpuTemp, fanRpm, battery) {
        let parts = [];
        if (this.show_temp && cpuTemp !== null) {
            parts.push(Math.round(cpuTemp) + "°C");
        }
        if (this.show_fan && fanRpm !== null) {
            // A stopped fan is worth saying plainly. On this board firmware keeps it off
            // entirely at idle, so 0 rpm is the normal state, not a fault.
            parts.push(fanRpm === 0 ? "fan off" : fanRpm + " rpm");
        }
        if (this.show_power && battery.watts !== null) {
            parts.push(battery.watts.toFixed(1) + " W");
        }
        this.set_applet_label(parts.length > 0 ? parts.join("  ") : "no sensors");

        let tip = [];
        if (cpuTemp !== null) tip.push("CPU " + cpuTemp.toFixed(1) + " °C");
        if (fanRpm !== null) tip.push("Fan " + fanRpm + " rpm");
        if (battery.capacity !== null) tip.push("Battery " + battery.capacity + "%");
        if (battery.watts !== null) {
            tip.push("Draw " + battery.watts.toFixed(2) + " W");
        } else if (battery.onAc) {
            tip.push("On AC");
        }
        this.set_applet_tooltip(tip.join("\n"));
    },

    /* Rows are created once and updated in place. Rebuilding the menu every tick would
     * close it under the user's cursor while they are reading it. */
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
        row.item.actor.visible = true;
    },

    _updateMenu: function (cpuTemps, ecTemps, fanRpm, battery) {
        if (!this._headerDone) {
            this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
            this._headerDone = true;
        }

        for (let t of cpuTemps) {
            this._setRow("cpu:" + t.label, t.label, t.celsius.toFixed(1) + " °C");
        }
        for (let t of ecTemps) {
            let value = t.celsius.toFixed(1) + " °C";
            if (t.crit !== null) value += "   (crit " + t.crit.toFixed(0) + ")";
            this._setRow("ec:" + t.label, t.label, value);
        }

        if (fanRpm !== null) {
            this._setRow("fan", "fan", fanRpm === 0 ? "off" : fanRpm + " rpm");
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
