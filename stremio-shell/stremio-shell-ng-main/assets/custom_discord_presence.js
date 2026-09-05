(function () {
  'use strict';

  if (window.__stremioCustomDiscordPresence) return;
  window.__stremioCustomDiscordPresence = true;

  /**
   * Discord Rich Presence bridge (MyStremio).
   *
   * Ported to match Loukious/stremio-shell-ng behaviour: the shell now owns
   * presence rendering (poster art, episode names, buttons, timestamps), so
   * this script's only job is to report *where the user is* and mirror the
   * Settings toggles. No more DOM scraping of titles or clock labels, and no
   * more 3s payload churn -- we push on navigation and on setting changes.
   */

  const ENABLED_KEY = 'stremio-custom-discord-rp-enabled';
  const SHOW_PAUSED_KEY = 'stremio-custom-discord-rp-show-paused';
  const SHOW_MENU_KEY = 'stremio-custom-discord-rp-show-menu';

  // Safety-net resync. The shell refreshes the activity on its own cadence
  // (RPCconfig.ini -> [Activity] refresh_interval), so this only has to cover
  // route changes that slipped past the listeners below.
  const HEARTBEAT_MS = 15000;

  let lastPayload = '';
  let heartbeatTimer = null;
  let lastRoute = '';
  let cleared = true;

  function readBool(key, fallback) {
    try {
      const value = localStorage.getItem(key);
      if (value == null) return fallback;
      return value === 'true';
    } catch {
      return fallback;
    }
  }

  function isEnabled() {
    return readBool(ENABLED_KEY, false);
  }

  /**
   * Current app hash route, query string preserved off.
   * The shell matches on `/player/` and `/detail/` substrings.
   * @returns {string}
   */
  function getRoute() {
    const raw = (location.hash || '#/').replace(/^#/, '') || '/';
    return raw.split('?')[0] || '/';
  }

  function isPlayerRoute() {
    return /^\/player/.test(getRoute());
  }

  /**
   * Best-effort playback state, used only as a fallback before mpv has
   * reported `pause` natively.
   * @returns {boolean}
   */
  function isPausedFallback() {
    const controlBar =
      document.querySelector('[class*="control-bar"]') ||
      document.querySelector('[class*="player-container"]');
    if (!controlBar) return false;

    const playBtn = controlBar.querySelector(
      '[class*="button-container"][title*="Play"], [class*="button-container"][aria-label*="Play"]'
    );
    if (playBtn) return true;

    const pauseBtn = controlBar.querySelector(
      '[class*="button-container"][title*="Pause"], [class*="button-container"][aria-label*="Pause"]'
    );
    return !pauseBtn;
  }

  /**
   * Convert a `h:mm:ss` / `mm:ss` seek-bar label to seconds.
   * @param {string} raw
   * @returns {number}
   */
  function clockToSeconds(raw) {
    const parts = String(raw || '')
      .trim()
      .split(':')
      .map((part) => Number.parseInt(part, 10));
    if (parts.some((n) => Number.isNaN(n))) return 0;
    if (parts.length === 3) return parts[0] * 3600 + parts[1] * 60 + parts[2];
    if (parts.length === 2) return parts[0] * 60 + parts[1];
    if (parts.length === 1) return parts[0];
    return 0;
  }

  function readSeekLabels() {
    const labels = Array.from(
      document.querySelectorAll('[class*="seek-bar-container"] [class*="label"]')
    )
      .map((label) => (label.textContent || '').trim())
      .filter((text) => /^\d/.test(text));
    return {
      current: clockToSeconds(labels[0] || ''),
      duration: clockToSeconds(labels[labels.length - 1] || ''),
    };
  }

  function buildPayload() {
    const payload = {
      route: getRoute(),
      showPaused: readBool(SHOW_PAUSED_KEY, true),
      showMenu: readBool(SHOW_MENU_KEY, true),
    };

    if (isPlayerRoute()) {
      const { current, duration } = readSeekLabels();
      payload.paused = isPausedFallback();
      if (current > 0) payload.currentTimeSeconds = current;
      if (duration > 0) payload.durationSeconds = duration;
    }

    return payload;
  }

  async function sendPresence(payload) {
    if (!window.StremioCustomAPI?.invoke) return;
    // Drop the volatile clock fields when deciding whether anything changed,
    // otherwise we would spam the shell once per second.
    const identity = JSON.stringify({
      route: payload.route,
      showPaused: payload.showPaused,
      showMenu: payload.showMenu,
      paused: payload.paused,
    });
    if (identity === lastPayload && !cleared) return;
    lastPayload = identity;
    cleared = false;
    try {
      await window.StremioCustomAPI.invoke('update-discord-presence', payload);
    } catch (error) {
      console.warn('[StremioCustom] Discord presence update failed:', error);
      lastPayload = '';
    }
  }

  async function clearPresence() {
    lastPayload = '';
    if (cleared) return;
    cleared = true;
    if (!window.StremioCustomAPI?.invoke) return;
    try {
      await window.StremioCustomAPI.invoke('clear-discord-presence', {});
    } catch (_) {}
  }

  async function tick() {
    if (!isEnabled()) {
      await clearPresence();
      return;
    }
    await sendPresence(buildPayload());
  }

  function startPolling() {
    if (heartbeatTimer) return;
    heartbeatTimer = window.setInterval(tick, HEARTBEAT_MS);
    tick();
  }

  function stopPolling() {
    if (!heartbeatTimer) return;
    window.clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }

  function onRouteChange() {
    const route = getRoute();
    if (route === lastRoute) return;
    lastRoute = route;
    lastPayload = '';
    tick();
  }

  document.addEventListener('stremio-custom-route-change', onRouteChange);
  document.addEventListener('stremio-custom-playback-stopped', () => {
    lastPayload = '';
    tick();
  });

  // Reflect play/pause promptly; the shell also gets this natively from mpv.
  document.addEventListener('click', () => {
    if (isEnabled() && isPlayerRoute()) window.setTimeout(tick, 150);
  }, true);
  document.addEventListener('keyup', (event) => {
    if (event.key === ' ' && isEnabled() && isPlayerRoute()) window.setTimeout(tick, 150);
  }, true);

  window.addEventListener('storage', (event) => {
    if (
      event.key === ENABLED_KEY ||
      event.key === SHOW_PAUSED_KEY ||
      event.key === SHOW_MENU_KEY
    ) {
      lastPayload = '';
      if (isEnabled()) startPolling();
      tick();
    }
  });

  document.addEventListener('stremio-custom-bootstrap-ready', () => {
    lastRoute = getRoute();
    if (isEnabled()) startPolling();
  });

  window.StremioCustomDiscordPresence = {
    isEnabled,
    startPolling,
    stopPolling,
    tick,
    clearPresence,
    KEYS: {
      ENABLED: ENABLED_KEY,
      SHOW_PAUSED: SHOW_PAUSED_KEY,
      SHOW_MENU: SHOW_MENU_KEY,
    },
  };

  if (document.readyState !== 'loading') {
    if (isEnabled()) startPolling();
  } else {
    window.addEventListener('DOMContentLoaded', () => {
      if (isEnabled()) startPolling();
    });
  }

  console.info('[StremioCustom] Discord presence bridge ready (native renderer).');
})();
