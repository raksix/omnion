/*
 * Omnion analytics — the tracker the tracking snippet loads (docs/requests/REQ-007).
 *
 * Deliberately small and dependency-free, and deliberately *cookieless*: it reads nothing from
 * storage, writes nothing to storage and sets no cookie. The visitor identifier is computed by
 * the collector from the request itself (a daily-salted hash), so this script has nothing to
 * remember between pages.
 *
 * What it sends, in one batched beacon per page:
 *   · the pageview (path, title, referrer, viewport, language) and the campaign parameters the
 *     address bar carries;
 *   · custom events queued through `window.omnionAnalytics.track(name, properties)`;
 *   · downloads (links to a file) and outbound clicks (links to another host), as events.
 *
 * `Do Not Track` and `Global Privacy Control` stop the script before it makes a request at all;
 * the collector enforces the same rules again on the server, because a client-side promise is
 * not a promise.
 */
(function () {
  "use strict";

  var script = document.currentScript;
  var siteKey = script ? script.getAttribute("data-site") : null;
  if (!siteKey) {
    return;
  }

  // The signals first: nothing leaves the browser when a visitor asked not to be counted.
  var dnt =
    navigator.doNotTrack === "1" ||
    navigator.doNotTrack === "yes" ||
    window.doNotTrack === "1" ||
    navigator.msDoNotTrack === "1";
  var gpc = navigator.globalPrivacyControl === true;
  if (dnt || gpc) {
    window.omnionAnalytics = { track: function () {}, enabled: false };
    return;
  }

  var ENDPOINT = "/api/v1/public/analytics/collect?site=" + encodeURIComponent(siteKey);
  var FILE_PATTERN = /\.(pdf|zip|gz|tar|csv|xlsx?|docx?|pptx?|txt|rtf|mp3|mp4|webm|mov|png|jpe?g|gif|svg|webp)$/i;

  var startedAt = Date.now();
  var maxScroll = 0;
  var queue = [];
  var pending = null;

  function post(body) {
    var text = JSON.stringify(body);
    if (navigator.sendBeacon) {
      navigator.sendBeacon(ENDPOINT, new Blob([text], { type: "text/plain" }));
      return;
    }
    var request = new XMLHttpRequest();
    request.open("POST", ENDPOINT, true);
    request.setRequestHeader("Content-Type", "text/plain");
    request.send(text);
  }

  function flushEvents() {
    if (pending !== null) {
      clearTimeout(pending);
      pending = null;
    }
    if (!queue.length) {
      return;
    }
    var events = queue;
    queue = [];
    post({ events: events });
  }

  function enqueue(name, properties, value) {
    if (!name || typeof name !== "string") {
      return;
    }
    var event = { name: name, properties: properties || {} };
    if (typeof value === "number" && isFinite(value)) {
      event.value = value;
    }
    queue.push(event);
    // One batched request per second at most: a burst of clicks is one beacon, not ten.
    if (pending === null) {
      pending = setTimeout(function () {
        pending = null;
        flushEvents();
      }, 1000);
    }
  }

  function scrollDepth() {
    var height = Math.max(
      document.body ? document.body.scrollHeight : 0,
      document.documentElement ? document.documentElement.scrollHeight : 0
    );
    if (!height) {
      return 0;
    }
    var seen = window.scrollY + window.innerHeight;
    return Math.max(0, Math.min(100, Math.round((seen / height) * 100)));
  }

  function campaign() {
    var params = new URLSearchParams(window.location.search);
    var names = ["source", "medium", "campaign", "term", "content"];
    var utm = {};
    var any = false;
    for (var index = 0; index < names.length; index += 1) {
      var value = params.get("utm_" + names[index]);
      if (value) {
        utm[names[index]] = value.slice(0, 200);
        any = true;
      }
    }
    return any ? utm : null;
  }

  function onScroll() {
    var depth = scrollDepth();
    if (depth > maxScroll) {
      maxScroll = depth;
    }
  }

  function onClick(event) {
    var node = event.target;
    while (node && node.tagName !== "A") {
      node = node.parentNode;
    }
    if (!node || !node.href) {
      return;
    }

    var href = node.href;
    var isFile = FILE_PATTERN.test(node.pathname || href);
    var isOutbound = false;
    try {
      isOutbound = new URL(href, window.location.href).hostname !== window.location.hostname;
    } catch (error) {
      isOutbound = false;
    }

    if (isFile) {
      enqueue("download", { file: href.slice(0, 500) });
    } else if (isOutbound) {
      enqueue("outbound", { url: href.slice(0, 500) });
    }
  }

  function sendPageview() {
    var beacon = {
      pageview: {
        path: window.location.pathname + window.location.search,
        title: document.title ? document.title.slice(0, 300) : null,
        referrer: document.referrer ? document.referrer.slice(0, 1000) : null,
        screen: { width: window.innerWidth, height: window.innerHeight },
        language: navigator.language || null
      },
      events: queue
    };
    var utm = campaign();
    if (utm) {
      beacon.utm = utm;
    }
    queue = [];
    post(beacon);
  }

  function sendEngagement() {
    var duration = Date.now() - startedAt;
    // A visit shorter than two seconds says nothing a pageview does not already say.
    if (duration < 2000) {
      return;
    }
    enqueue("page_engagement", {
      path: window.location.pathname,
      duration_ms: duration,
      scroll_depth: maxScroll
    });
    flushEvents();
  }

  window.addEventListener("scroll", onScroll, { passive: true });
  document.addEventListener("click", onClick, true);
  window.addEventListener("pagehide", sendEngagement);
  document.addEventListener("visibilitychange", function () {
    if (document.visibilityState === "hidden") {
      sendEngagement();
    }
  });

  if (document.readyState === "complete") {
    sendPageview();
  } else {
    window.addEventListener("load", sendPageview);
  }

  window.omnionAnalytics = {
    enabled: true,
    track: enqueue
  };
})();
