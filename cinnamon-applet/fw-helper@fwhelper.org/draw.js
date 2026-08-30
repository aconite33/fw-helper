/*
 * Cairo drawing primitives for the panel readout and the dropdown.
 *
 * Kept separate from applet.js so the drawing can be reasoned about on its own: every
 * function here takes a context and a rectangle and touches no applet state.
 *
 * Colours arrive as [r, g, b] in 0..1. Distinct hues rather than shades of the theme
 * foreground, because the point of several series side by side is telling them apart -
 * which a single theme-derived colour cannot do. The theme foreground is still used for
 * every neutral: track backgrounds, text, and empty states.
 */

const Cairo = imports.cairo;

const CAIRO_ROUND = 1; // Cairo.LineCap.ROUND

function clamp01(v) {
    return Math.max(0, Math.min(1, v));
}

/* HSV so a usage ramp can travel around the colour wheel. A straight RGB interpolation
 * from green to purple passes through a muddy grey at the midpoint, which is exactly
 * where a half-full disk would sit. */
function hsv(h, s, v) {
    h = ((h % 360) + 360) % 360;
    let c = v * s;
    let x = c * (1 - Math.abs(((h / 60) % 2) - 1));
    let m = v - c;
    let rgb;
    if (h < 60) rgb = [c, x, 0];
    else if (h < 120) rgb = [x, c, 0];
    else if (h < 180) rgb = [0, c, x];
    else if (h < 240) rgb = [0, x, c];
    else if (h < 300) rgb = [x, 0, c];
    else rgb = [c, 0, x];
    return [rgb[0] + m, rgb[1] + m, rgb[2] + m];
}

/* Green when empty, purple when full, travelling through teal and blue. */
function usageColor(t) {
    return hsv(120 + clamp01(t) * 165, 0.62, 0.82);
}

function roundRect(cr, x, y, w, h, r) {
    r = Math.min(r, w / 2, h / 2);
    cr.newSubPath();
    cr.arc(x + w - r, y + r, r, -Math.PI / 2, 0);
    cr.arc(x + w - r, y + h - r, r, 0, Math.PI / 2);
    cr.arc(x + r, y + h - r, r, Math.PI / 2, Math.PI);
    cr.arc(x + r, y + r, r, Math.PI, 1.5 * Math.PI);
    cr.closePath();
}

/* GJS has spelled these both ways across versions, and getting it wrong silently
 * produces NaN coordinates rather than an error. */
function extent(ext, camel, snake) {
    let v = ext[camel];
    return (v === undefined) ? ext[snake] : v;
}

function text(cr, x, y, str, size, color, alpha, align) {
    cr.selectFontFace("Sans", 0, 0);
    cr.setFontSize(size);
    let ext = cr.textExtents(str);
    let w = extent(ext, "width", "width");
    let xb = extent(ext, "xBearing", "x_bearing");
    let dx = 0;
    if (align === "center") dx = -w / 2 - xb;
    else if (align === "right") dx = -w - xb;
    cr.setSourceRGBA(color[0], color[1], color[2], alpha);
    cr.moveTo(x + dx, y);
    cr.showText(str);
    cr.newPath();
}

function centeredText(cr, cx, cy, str, size, color, alpha) {
    cr.selectFontFace("Sans", 0, 0);
    cr.setFontSize(size);
    let ext = cr.textExtents(str);
    let w = extent(ext, "width", "width");
    let h = extent(ext, "height", "height");
    let xb = extent(ext, "xBearing", "x_bearing");
    let yb = extent(ext, "yBearing", "y_bearing");
    cr.setSourceRGBA(color[0], color[1], color[2], alpha);
    cr.moveTo(cx - w / 2 - xb, cy - h / 2 - yb);
    cr.showText(str);
    cr.newPath();
}

/* Text with a contrasting halo, for digits that sit on top of a coloured fill.
 *
 * The number inside a battery straddles the fill edge: part of it is over the fill and
 * part over the empty track, so no single colour is legible against both. Outlining it
 * in the inverse of the foreground works on either - a dark halo under light text on a
 * dark panel, and the reverse on a light one.
 */
/* The halo is drawn by stamping the text around itself rather than by stroking a text
 * path. `Cairo.Context.textPath` does not exist in this GJS - verified, not assumed -
 * and calling it throws mid-repaint, which silently loses the glyphs and everything
 * drawn after them. Eight offset copies cost nothing at this size. */
const HALO_OFFSETS = [
    [-1, -1], [0, -1], [1, -1],
    [-1, 0], [1, 0],
    [-1, 1], [0, 1], [1, 1],
];

function haloText(cr, cx, cy, str, size, fg) {
    cr.selectFontFace("Sans", 0, 1); // normal slant, bold weight
    cr.setFontSize(size);
    let ext = cr.textExtents(str);
    let w = extent(ext, "width", "width");
    let h = extent(ext, "height", "height");
    let xb = extent(ext, "xBearing", "x_bearing");
    let yb = extent(ext, "yBearing", "y_bearing");
    let bx = cx - w / 2 - xb;
    let by = cy - h / 2 - yb;

    cr.setSourceRGBA(1 - fg[0], 1 - fg[1], 1 - fg[2], 0.85);
    for (let o of HALO_OFFSETS) {
        cr.moveTo(bx + o[0], by + o[1]);
        cr.showText(str);
    }
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 1.0);
    cr.moveTo(bx, by);
    cr.showText(str);
    cr.newPath();
}

/* Text width, measured off-screen.
 *
 * Lets the caller reserve space for a string before drawing it, without involving the
 * widget layout at all - which is the point: a St.Label sizes itself to its own text,
 * so a panel built from labels changes width whenever a reading does.
 */
function measure(str, size) {
    let surface = new Cairo.ImageSurface(Cairo.Format.ARGB32, 1, 1);
    let cr = new Cairo.Context(surface);
    try {
        cr.selectFontFace("Sans", 0, 0);
        cr.setFontSize(size);
        let ext = cr.textExtents(str);
        return extent(ext, "xAdvance", "x_advance");
    } finally {
        cr.$dispose();
    }
}

/* Text on a vertical centre line rather than a baseline, so it lines up with the graphs
 * beside it whatever the glyphs are. */
function label(cr, x, cy, str, size, color, alpha, align) {
    cr.selectFontFace("Sans", 0, 0);
    cr.setFontSize(size);
    let ext = cr.textExtents(str);
    let w = extent(ext, "width", "width");
    let h = extent(ext, "height", "height");
    let xb = extent(ext, "xBearing", "x_bearing");
    let yb = extent(ext, "yBearing", "y_bearing");
    let dx = 0;
    if (align === "center") dx = -w / 2 - xb;
    else if (align === "right") dx = -w - xb;
    cr.setSourceRGBA(color[0], color[1], color[2], alpha);
    cr.moveTo(x + dx, cy - h / 2 - yb);
    cr.showText(str);
    cr.newPath();
}

/* Filled area chart. History runs oldest-to-newest, left to right, values 0..1. */
function spark(cr, x, y, w, h, history, color, fg) {
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.10);
    cr.rectangle(x, y, w, h);
    cr.fill();

    if (!history || history.length < 2) return;

    let step = w / (history.length - 1);
    let pointY = (i) => y + h - clamp01(history[i]) * h;

    cr.moveTo(x, y + h);
    for (let i = 0; i < history.length; i++) cr.lineTo(x + i * step, pointY(i));
    cr.lineTo(x + (history.length - 1) * step, y + h);
    cr.closePath();
    cr.setSourceRGBA(color[0], color[1], color[2], 0.30);
    cr.fill();

    cr.moveTo(x, pointY(0));
    for (let i = 1; i < history.length; i++) cr.lineTo(x + i * step, pointY(i));
    cr.setSourceRGBA(color[0], color[1], color[2], 0.95);
    cr.setLineWidth(1.5);
    cr.stroke();
}

/* Vertical fill bar, for a value whose history carries nothing. */
function vbar(cr, x, y, w, h, value, color, fg) {
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.13);
    cr.rectangle(x, y, w, h);
    cr.fill();
    if (value === null || value === undefined) return;
    let filled = clamp01(value) * h;
    cr.setSourceRGBA(color[0], color[1], color[2], 0.85);
    cr.rectangle(x, y + h - filled, w, filled);
    cr.fill();
}

function hbar(cr, x, y, w, h, value, color, fg) {
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.13);
    cr.rectangle(x, y, w, h);
    cr.fill();
    if (value === null || value === undefined) return;
    cr.setSourceRGBA(color[0], color[1], color[2], 0.85);
    cr.rectangle(x, y, clamp01(value) * w, h);
    cr.fill();
}

/* Arc gauge. `segments` are drawn consecutively from twelve o'clock, so a pair such as
 * system-then-user reads as one ring split by cause rather than two overlaid rings. */
function ring(cr, cx, cy, radius, thickness, segments, label, sublabel, fg) {
    cr.setLineWidth(thickness);
    cr.setLineCap(CAIRO_ROUND);

    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.14);
    cr.arc(cx, cy, radius, 0, 2 * Math.PI);
    cr.stroke();

    let start = -Math.PI / 2;
    for (let s of segments) {
        let sweep = clamp01(s.value) * 2 * Math.PI;
        if (sweep <= 0.001) continue;
        cr.setSourceRGBA(s.color[0], s.color[1], s.color[2], 0.95);
        cr.arc(cx, cy, radius, start, start + sweep);
        cr.stroke();
        start += sweep;
    }

    if (label) {
        let y = sublabel ? cy - radius * 0.10 : cy;
        centeredText(cr, cx, y, label, radius * 0.62, fg, 0.95);
    }
    if (sublabel) {
        centeredText(cr, cx, cy + radius * 0.42, sublabel, radius * 0.36, fg, 0.55);
    }
}

/* One column per core. Stats' most recognisable element, and the clearest way to show
 * that four cores are pinned while eight idle - which a single average hides. */
function cores(cr, x, y, w, h, values, color, fg) {
    let n = values.length;
    if (!n) return;
    let gap = n > 16 ? 1 : 2;
    let bw = Math.max(1.5, (w - gap * (n - 1)) / n);
    for (let i = 0; i < n; i++) {
        vbar(cr, x + i * (bw + gap), y, bw, h, values[i], color, fg);
    }
}

/* A legend swatch, for the Details rows. */
function swatch(cr, x, y, size, color) {
    cr.setSourceRGBA(color[0], color[1], color[2], 0.95);
    cr.rectangle(x, y, size, size);
    cr.fill();
}

/* A usage bar whose colour carries the reading as well as its length.
 *
 * The gradient spans the whole track, not the filled part, so a given fill level always
 * ends on the same colour - the hue means "this full", independently of the bar's size.
 * `vertical` fills upward from the bottom.
 */
function usageBar(cr, x, y, w, h, value, fg, vertical) {
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.13);
    cr.rectangle(x, y, w, h);
    cr.fill();
    if (value === null || value === undefined) return;

    let g = vertical
        ? new Cairo.LinearGradient(x, y + h, x, y)
        : new Cairo.LinearGradient(x, y, x + w, y);
    // Sampled rather than two stops: the interpolation has to happen in HSV, and a
    // Cairo gradient can only interpolate the RGB it is given.
    for (let i = 0; i <= 12; i++) {
        let t = i / 12;
        let c = usageColor(t);
        g.addColorStopRGBA(t, c[0], c[1], c[2], 0.92);
    }

    cr.save();
    let filled = clamp01(value);
    if (vertical) cr.rectangle(x, y + h - filled * h, w, filled * h);
    else cr.rectangle(x, y, filled * w, h);
    cr.clip();
    cr.setSource(g);
    cr.rectangle(x, y, w, h);
    cr.fill();
    cr.restore();
    cr.newPath();
}

/* A battery, drawn as one: outline, terminal nub, proportional fill, and the
 * percentage inside it.
 *
 * The fill is kept semi-transparent so the number stays legible over it on both a light
 * and a dark panel - the alternative is picking a text colour per fill level, which
 * fails at exactly the boundary where the digits straddle the fill edge.
 */
function battery(cr, x, y, w, h, level, charging, fg) {
    // Proportions and weight taken from the desktop's own battery icon: a thick, solid
    // outline and a clearly visible terminal, so it reads as a battery at panel size
    // rather than as a rounded rectangle.
    let nubW = Math.max(3, Math.round(w * 0.075));
    let bodyW = w - nubW - 1;
    let stroke = Math.max(1.5, h * 0.10);

    cr.setLineWidth(stroke);
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.85);
    roundRect(cr, x + stroke / 2, y + stroke / 2,
        bodyW - stroke, h - stroke, h * 0.22);
    cr.stroke();

    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.85);
    roundRect(cr, x + bodyW - stroke / 2, y + h * 0.28, nubW, h * 0.44, 1.5);
    cr.fill();

    let inset = stroke + 1;
    let innerW = bodyW - stroke - inset;
    let filled = innerW * clamp01(level);
    if (filled > 0) {
        let color = batteryColor(level, charging);
        // Kept below full opacity so the digits over it stay readable; the halo on the
        // text does the rest of that work.
        cr.setSourceRGBA(color[0], color[1], color[2], 0.75);
        roundRect(cr, x + inset, y + inset, Math.max(1.5, filled), h - inset * 2, 1.5);
        cr.fill();
    }

    // "100" is half again as wide as "58" and overflows the body at the same size, so
    // three digits get a smaller face rather than a clipped one.
    let str = String(Math.round(clamp01(level) * 100));
    haloText(cr, x + bodyW / 2, y + h / 2, str,
        h * (str.length >= 3 ? 0.50 : 0.66), fg);
}

/* Green while there is plenty, warning colours as it runs out. Charging keeps green:
 * the bolt says what is happening, so the colour does not need to. */
function batteryColor(level, charging) {
    if (charging) return [0.30, 0.78, 0.40];
    if (level <= 0.10) return [0.90, 0.32, 0.30];
    if (level <= 0.20) return [0.95, 0.65, 0.25];
    return [0.30, 0.78, 0.40];
}

/* Charging bolt, drawn beside the battery rather than inside it - two digits and a bolt
 * do not both fit legibly at panel size. */
function bolt(cr, cx, cy, h, fg) {
    let w = h * 0.42;
    cr.moveTo(cx + w * 0.15, cy - h / 2);
    cr.lineTo(cx - w * 0.5, cy + h * 0.12);
    cr.lineTo(cx - w * 0.05, cy + h * 0.12);
    cr.lineTo(cx - w * 0.15, cy + h / 2);
    cr.lineTo(cx + w * 0.5, cy - h * 0.12);
    cr.lineTo(cx + w * 0.05, cy - h * 0.12);
    cr.closePath();
    cr.setSourceRGBA(fg[0], fg[1], fg[2], 0.85);
    cr.fill();
}

module.exports = {
    clamp01, hsv, usageColor, roundRect,
    text, centeredText, haloText, measure, label, spark, vbar, hbar, usageBar, ring, cores, swatch,
    battery, batteryColor, bolt,
};
