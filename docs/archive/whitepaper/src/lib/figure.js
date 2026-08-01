/* Tessera white paper — shared figure runtime.
 *
 * This file is the readable reference copy. The build script globs only
 * `src/*.html`, so this file is NOT concatenated into the published page:
 * its contents are inlined verbatim inside the <script> block at the end of
 * `src/00-style.html`. The two must be kept identical.
 *
 * Everything lands on one global, `TF`. No modules, no imports, no network.
 */
(function (global) {
  "use strict";

  /* ---- theme ---------------------------------------------------------- */

  var mq = global.matchMedia
    ? global.matchMedia("(prefers-color-scheme: dark)")
    : null;

  /* Resolve the theme the page is actually painted in. An explicit
   * data-theme stamp on the root element wins over the OS preference in
   * both directions; this mirrors the CSS cascade in 00-style.html. */
  function theme() {
    var stamped = document.documentElement.getAttribute("data-theme");
    if (stamped === "dark" || stamped === "light") return stamped;
    return mq && mq.matches ? "dark" : "light";
  }

  /* Call fn("light"|"dark") whenever the effective theme changes.
   * Figures that read computed token values (e.g. to paint a canvas) must
   * subscribe; figures styled purely by CSS custom properties need not.
   * Returns an unsubscribe function. */
  function onThemeChange(fn) {
    var last = theme();
    function fire() {
      var now = theme();
      if (now === last) return;
      last = now;
      fn(now);
    }
    if (mq) {
      if (mq.addEventListener) mq.addEventListener("change", fire);
      else if (mq.addListener) mq.addListener(fire);
    }
    var obs = new MutationObserver(fire);
    obs.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"]
    });
    return function unsubscribe() {
      if (mq) {
        if (mq.removeEventListener) mq.removeEventListener("change", fire);
        else if (mq.removeListener) mq.removeListener(fire);
      }
      obs.disconnect();
    };
  }

  /* ---- SVG ------------------------------------------------------------ */

  var NS = "http://www.w3.org/2000/svg";

  /* Namespaced element factory.
   *   svg("rect", {x: 0, y: 0, width: 10, height: 4, fill: "var(--cat-1)"})
   * Two attribute names are treated specially:
   *   text     -> textContent
   *   children -> array of nodes to append
   * Attributes whose value is null or undefined are skipped, so callers can
   * pass optional attributes without branching. */
  function svg(tag, attrs) {
    var el = document.createElementNS(NS, tag);
    if (attrs) {
      for (var k in attrs) {
        if (!Object.prototype.hasOwnProperty.call(attrs, k)) continue;
        var v = attrs[k];
        if (v === null || v === undefined) continue;
        if (k === "text") {
          el.textContent = String(v);
        } else if (k === "children") {
          for (var i = 0; i < v.length; i++) el.appendChild(v[i]);
        } else {
          el.setAttribute(k, String(v));
        }
      }
    }
    return el;
  }

  /* ---- numbers -------------------------------------------------------- */

  var GROUP = /\B(?=(\d{3})+(?!\d))/g;

  /* Group a number with thousands separators, at a fixed number of decimals.
   *   fmt(1048576)      -> "1,048,576"
   *   fmt(2.4157, 2)    -> "2.42"
   * Non-finite input renders as an em dash rather than "NaN". */
  function fmt(n, decimals) {
    if (n === null || n === undefined || !isFinite(n)) return "—";
    var d = decimals === undefined ? 0 : decimals;
    var neg = n < 0;
    var s = Math.abs(n).toFixed(d);
    var parts = s.split(".");
    parts[0] = parts[0].replace(GROUP, ",");
    return (neg ? "-" : "") + parts.join(".");
  }

  /* Format a duration given in microseconds, choosing µs / ms / s so the
   * mantissa stays readable. Measurements in probes/results.md are quoted in
   * microseconds, so that is the input unit.
   *   fmt.dur(430)      -> "430 µs"
   *   fmt.dur(21500)    -> "21.5 ms"
   *   fmt.dur(4200000)  -> "4.20 s"
   * The gap before the unit is a non-breaking space, so a value never wraps
   * away from its unit. Same in fmt.bytes. */
  fmt.dur = function (us) {
    if (us === null || us === undefined || !isFinite(us)) return "—";
    var a = Math.abs(us);
    if (a < 1000) return fmt(us, a < 10 ? 1 : 0) + " µs";
    if (a < 1e6) return fmt(us / 1000, a < 1e5 ? 1 : 0) + " ms";
    return fmt(us / 1e6, 2) + " s";
  };

  /* Format a byte count with binary prefixes. */
  fmt.bytes = function (b) {
    if (b === null || b === undefined || !isFinite(b)) return "—";
    var units = ["B", "KiB", "MiB", "GiB", "TiB"];
    var a = Math.abs(b), i = 0;
    while (a >= 1024 && i < units.length - 1) { a /= 1024; i++; }
    return fmt(b < 0 ? -a : a, i === 0 ? 0 : (a < 10 ? 2 : 1)) + " " + units[i];
  };

  /* ---- misc ----------------------------------------------------------- */

  /* Read a CSS custom property off an element (default :root). Figures that
   * paint to <canvas> cannot use var(); they read tokens through this and
   * re-read them from an onThemeChange callback. */
  function token(name, el) {
    return getComputedStyle(el || document.documentElement)
      .getPropertyValue(name)
      .trim();
  }

  /* True when the viewer has asked for reduced motion. Animated figures must
   * check this and present their end state immediately instead. */
  function reducedMotion() {
    return !!(global.matchMedia &&
      global.matchMedia("(prefers-reduced-motion: reduce)").matches);
  }

  global.TF = {
    theme: theme,
    onThemeChange: onThemeChange,
    svg: svg,
    fmt: fmt,
    token: token,
    reducedMotion: reducedMotion,
    NS: NS
  };
})(window);
