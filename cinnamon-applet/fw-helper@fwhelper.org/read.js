/*
 * Every reading the applet takes, and nothing else.
 *
 * All of it comes from /proc and sysfs, world-readable, so the applet needs no daemon,
 * no D-Bus policy and no root. Kept apart from the drawing and the widget code so the
 * hardware quirks live in one place.
 */

const Gio = imports.gi.Gio;
const GLib = imports.gi.GLib;

const HWMON = "/sys/class/hwmon";
const POWER_SUPPLY = "/sys/class/power_supply";
const KIB_PER_GIB = 1048576;
const BYTES_PER_GIB = 1073741824;

function file(path) {
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

function int(path) {
    let raw = file(path);
    if (raw === null) return null;
    let n = parseInt(raw, 10);
    return isNaN(n) ? null : n;
}

function dir(path) {
    let out = [];
    try {
        let d = GLib.Dir.open(path, 0);
        let name;
        while ((name = d.read_name()) !== null) out.push(name);
        d.close();
    } catch (e) {
        // Absent directory is not an error, just a machine without this hardware.
    }
    return out;
}

/* Resolve an hwmon node by its name file. Never by index - the same node came up as
 * hwmon11 on one boot and hwmon9 on the next on the reference machine. */
function hwmon(wanted) {
    for (let entry of dir(HWMON)) {
        if (!entry.startsWith("hwmon")) continue;
        if (file(HWMON + "/" + entry + "/name") === wanted) {
            return HWMON + "/" + entry;
        }
    }
    return null;
}

/* Resolve a power supply by `type`, not name: this board calls mains ACAD, others call
 * it AC or ADP1, and the battery is BAT0 as often as BAT1. */
function supply(wantedType) {
    for (let entry of dir(POWER_SUPPLY)) {
        if (file(POWER_SUPPLY + "/" + entry + "/type") === wantedType) {
            return POWER_SUPPLY + "/" + entry;
        }
    }
    return null;
}

function sensors(node) {
    let out = [];
    if (!node) return out;
    for (let i = 1; i <= 8; i++) {
        let milli = int(node + "/temp" + i + "_input");
        if (milli === null) continue;
        let crit = int(node + "/temp" + i + "_crit");
        out.push({
            label: file(node + "/temp" + i + "_label") || ("temp" + i),
            celsius: milli / 1000,
            crit: crit === null ? null : crit / 1000,
        });
    }
    return out;
}

function battery(batNode, acNode) {
    let out = {
        status: null, capacity: null, watts: null, minutes: null, onAc: null,
        nowMah: null, fullMah: null, designMah: null, healthPercent: null,
        cycles: null, volts: null, technology: null, model: null,
    };
    if (acNode) {
        let online = int(acNode + "/online");
        if (online !== null) out.onAc = (online === 1);
    }
    if (!batNode) return out;

    out.status = file(batNode + "/status");
    out.capacity = int(batNode + "/capacity");
    out.cycles = int(batNode + "/cycle_count");
    out.technology = file(batNode + "/technology");
    out.model = file(batNode + "/model_name");

    let uv = int(batNode + "/voltage_now");
    if (uv !== null) out.volts = uv / 1e6;

    // Charge family (uAh). An energy-family battery reports uWh and has no charge_* at
    // all, so these simply stay null there.
    let now = int(batNode + "/charge_now");
    let full = int(batNode + "/charge_full");
    let design = int(batNode + "/charge_full_design");
    if (now !== null) out.nowMah = now / 1000;
    if (full !== null) out.fullMah = full / 1000;
    if (design !== null) out.designMah = design / 1000;
    // Health is what the pack can still hold against what it shipped able to hold.
    if (full !== null && design) out.healthPercent = (full / design) * 100;

    // Draw is only meaningful while discharging: on mains, current_now describes what
    // is going INTO the battery, which is not what the machine is using.
    if (out.status !== "Discharging") return out;

    // Energy family first (power_now in uW); already watts, no multiplication needed.
    // This board has no power_now, so reading only that would silently show nothing.
    let uw = int(batNode + "/power_now");
    if (uw !== null && uw > 0) {
        out.watts = uw / 1e6;
        let uwh = int(batNode + "/energy_now");
        if (uwh !== null) out.minutes = Math.round(uwh * 60 / uw);
        return out;
    }

    let ua = int(batNode + "/current_now");
    if (ua !== null && uv !== null && ua > 0) {
        out.watts = (ua / 1e6) * (uv / 1e6);
        if (now !== null) out.minutes = Math.round(now * 60 / ua);
    }
    return out;
}

function parseCpuLine(fields) {
    if (fields.length < 5 || fields.some(isNaN)) return null;
    return {
        user: fields[0] + fields[1],                                    // user + nice
        system: fields[2] + (fields[5] || 0) + (fields[6] || 0),        // sys+irq+softirq
        idle: fields[3] + fields[4],                                    // idle + iowait
        total: fields.reduce((a, b) => a + b, 0),
    };
}

/* Cumulative jiffies since boot, aggregate and per core. Load is a RATE, so a single
 * sample says nothing - it must be differenced against the previous one. */
function cpuTimes() {
    let text = file("/proc/stat");
    if (text === null) return null;
    let out = { all: null, cores: [] };
    for (let line of text.split("\n")) {
        if (!line.startsWith("cpu")) break; // cpu lines come first
        let m = line.match(/^cpu(\d*)\s+(.*)$/);
        if (!m) continue;
        let parsed = parseCpuLine(m[2].trim().split(/\s+/).map(Number));
        if (parsed === null) continue;
        if (m[1] === "") out.all = parsed;
        else out.cores.push(parsed);
    }
    return out.all === null ? null : out;
}

/* Busy fractions between two samples. Null when there is nothing to difference. */
function cpuDelta(prev, now) {
    if (!prev || !now) return null;
    let total = now.total - prev.total;
    if (total <= 0) return null;
    return {
        user: (now.user - prev.user) / total,
        system: (now.system - prev.system) / total,
        busy: 1 - (now.idle - prev.idle) / total,
        totalJiffies: total,
    };
}

function memory() {
    let text = file("/proc/meminfo");
    if (text === null) return null;
    let kv = {};
    for (let line of text.split("\n")) {
        let m = line.match(/^(\w+):\s+(\d+)/);
        if (m) kv[m[1]] = parseInt(m[2], 10); // kB
    }
    if (!kv.MemTotal) return null;

    // MemAvailable, not MemFree. Free excludes reclaimable page cache, so on a machine
    // with any uptime it reports nearly everything as used - true of the kernel's
    // bookkeeping and useless as a description of pressure.
    let available = (kv.MemAvailable !== undefined) ? kv.MemAvailable : kv.MemFree;
    let used = kv.MemTotal - available;
    let swapTotal = kv.SwapTotal || 0;

    return {
        totalGiB: kv.MemTotal / KIB_PER_GIB,
        usedGiB: used / KIB_PER_GIB,
        availableGiB: available / KIB_PER_GIB,
        cachedGiB: ((kv.Cached || 0) + (kv.Buffers || 0)) / KIB_PER_GIB,
        percent: (used / kv.MemTotal) * 100,
        swapTotalGiB: swapTotal / KIB_PER_GIB,
        swapUsedGiB: (swapTotal - (kv.SwapFree || 0)) / KIB_PER_GIB,
    };
}

function disk(path) {
    try {
        let info = Gio.File.new_for_path(path)
            .query_filesystem_info("filesystem::size,filesystem::free", null);
        let size = info.get_attribute_uint64("filesystem::size");
        let free = info.get_attribute_uint64("filesystem::free");
        if (!size) return null;
        return {
            totalGiB: size / BYTES_PER_GIB,
            usedGiB: (size - free) / BYTES_PER_GIB,
            freeGiB: free / BYTES_PER_GIB,
            // Against total, so this reads a point or two below `df`, which computes
            // against the space actually available to a non-root user.
            percent: ((size - free) / size) * 100,
        };
    } catch (e) {
        return null;
    }
}

function loadAvg() {
    let text = file("/proc/loadavg");
    if (text === null) return null;
    let f = text.split(/\s+/);
    return (f.length >= 3) ? { one: f[0], five: f[1], fifteen: f[2] } : null;
}

function uptime() {
    let text = file("/proc/uptime");
    if (text === null) return null;
    let seconds = parseFloat(text.split(/\s+/)[0]);
    if (isNaN(seconds)) return null;
    let d = Math.floor(seconds / 86400);
    let h = Math.floor((seconds % 86400) / 3600);
    let m = Math.floor((seconds % 3600) / 60);
    return d > 0 ? (d + "d " + h + "h") : (h > 0 ? (h + "h " + m + "m") : (m + "m"));
}

/* Top processes by CPU share.
 *
 * Per-process CPU is a rate too, so this needs the previous sample and the same total
 * jiffies the aggregate delta was computed over. Scaled by core count so a process
 * pinning one core reads 100%, matching what top and htop show, rather than 1/12th of
 * the machine.
 *
 * Walking every /proc/PID is the expensive part of this applet, so the caller only asks
 * while the menu is open.
 */
function processes(prevMap, totalJiffies, coreCount, limit) {
    let now = {};
    let out = [];
    for (let name of dir("/proc")) {
        if (!/^\d+$/.test(name)) continue;
        let stat = file("/proc/" + name + "/stat");
        if (stat === null) continue;

        // comm is parenthesised and may itself contain spaces or parens, so the fields
        // can only be split after the LAST closing paren.
        let open = stat.indexOf("(");
        let close = stat.lastIndexOf(")");
        if (open < 0 || close < 0 || close < open) continue;
        let comm = stat.substring(open + 1, close);
        let rest = stat.substring(close + 2).split(/\s+/);
        // rest[0] is field 3 (state), so field N is rest[N - 3].
        let utime = parseInt(rest[11], 10);
        let stime = parseInt(rest[12], 10);
        if (isNaN(utime) || isNaN(stime)) continue;

        let jiffies = utime + stime;
        now[name] = jiffies;

        let prev = prevMap ? prevMap[name] : undefined;
        if (prev === undefined || totalJiffies <= 0) continue;
        let share = ((jiffies - prev) / totalJiffies) * 100 * coreCount;
        if (share < 0.1) continue;

        let rssPages = 0;
        let statm = file("/proc/" + name + "/statm");
        if (statm !== null) {
            let p = parseInt(statm.split(/\s+/)[1], 10);
            if (!isNaN(p)) rssPages = p;
        }
        out.push({
            name: comm,
            cpu: share,
            rssGiB: (rssPages * 4096) / BYTES_PER_GIB,
        });
    }
    out.sort((a, b) => b.cpu - a.cpu);
    return { list: out.slice(0, limit), map: now };
}

module.exports = {
    file, int, dir, hwmon, supply, sensors, battery,
    cpuTimes, cpuDelta, memory, disk, loadAvg, uptime, processes,
};
