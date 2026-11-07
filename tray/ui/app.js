/* tunna-tray popup — talks to the local manager API */

(() => {
  'use strict';

  const API = 'http://127.0.0.1:8080';
  const POLL_MS = 10000;

  const $ = (id) => document.getElementById(id);
  const ui = {
    list: $('list'),
    count: $('count'),
    domain: $('base-domain'),
    status: $('status'),
    statusText: $('status-text'),
    toast: $('toast'),
    form: $('add-form'),
    port: $('port'),
    host: $('host'),
    expose: $('expose'),
  };

  /* Tauri webview has no CORS for localhost, plain fetch only for browser dev. */
  const http = (url, opts) => (window.__TAURI__?.http?.fetch
    ? window.__TAURI__.http.fetch(url, opts)
    : fetch(url, opts));

  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  const escapeHtml = (value) => String(value).replace(/[&<>"']/g, (ch) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]
  ));

  const ICONS = {
    copy: '<svg class="ic-copy" viewBox="0 0 16 16" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5V3.6c0-.6-.5-1.1-1.1-1.1H3.6c-.6 0-1.1.5-1.1 1.1v5.8c0 .6.5 1.1 1.1 1.1h1.9"/></svg>',
    check: '<svg class="ic-check" viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-7"/></svg>',
    trash: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 4.5h10M6.5 4.5V3.2c0-.4.3-.7.7-.7h1.6c.4 0 .7.3.7.7v1.3M4.4 4.5l.5 7.6c0 .8.6 1.4 1.4 1.4h3.4c.8 0 1.4-.6 1.4-1.4l.5-7.6"/><path d="M6.7 7v4M9.3 7v4"/></svg>',
    dome: '<svg class="dome" viewBox="0 0 64 64" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M10 48V32a22 22 0 0 1 44 0v16"/><path d="M5 54h54" opacity=".5"/><path d="M22 48v-8M32 48V34M42 48v-5" opacity=".7"/><circle cx="32" cy="21" r="2.2" stroke="none" fill="currentColor"/></svg>',
  };

  let baseDomain = 'example.com';
  let routes = [];
  let lastKey = '';
  let loaded = false;
  let busy = false;
  let toastTimer = 0;

  /* ---------- API ---------- */

  async function api(path, options) {
    const res = await http(API + path, options);
    const text = await res.text();
    let data = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch (_) { /* non-JSON error body */ }

    const ok = typeof res.status === 'number'
      ? res.status >= 200 && res.status < 300
      : res.ok !== false;
    if (!ok) throw new Error(data?.error || `manager responded ${res.status ?? '?'}`);
    return data;
  }

  const jsonBody = (method, payload) => ({
    method,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });

  /* ---------- status / toast ---------- */

  function setStatus(state) {
    if (ui.status.dataset.state === state) return;
    ui.status.dataset.state = state;
    ui.statusText.textContent = state === 'online' ? 'live' : state === 'offline' ? 'offline' : 'checking';
  }

  function showToast(message) {
    clearTimeout(toastTimer);
    ui.toast.textContent = message;
    ui.toast.hidden = false;
    ui.toast.classList.remove('is-out');
    toastTimer = setTimeout(() => {
      ui.toast.classList.add('is-out');
      toastTimer = setTimeout(() => { ui.toast.hidden = true; }, 140);
    }, 4000);
  }

  /* ---------- render ---------- */

  function placeholderFor(port) {
    const p = (port || '').trim() || '3000';
    return `auto → p${p}.${baseDomain}`;
  }

  function syncPlaceholder() {
    ui.host.placeholder = placeholderFor(ui.port.value);
  }

  function routeRow(route) {
    const host = escapeHtml(route.host);
    const enabled = !!route.enabled;
    return `<li class="route${enabled ? '' : ' is-off'}" data-host="${host}">
      <div class="route-main">
        <span class="route-host" title="${host}">${host}</span>
        <span class="port-chip">:${escapeHtml(route.port)}</span>
      </div>
      <div class="route-actions">
        <button class="icon-btn copy" type="button" data-action="copy" data-host="${host}"
                aria-label="Copy https URL for ${host}" title="Copy https://${host}">
          ${ICONS.copy}${ICONS.check}
        </button>
        <label class="switch" title="${enabled ? 'Disable' : 'Enable'} ${host}">
          <input type="checkbox" role="switch" data-action="toggle" data-host="${host}"
                 aria-label="Tunnel ${host}"${enabled ? ' checked' : ''}>
          <span class="track"></span><span class="knob"></span>
        </label>
        <button class="icon-btn delete" type="button" data-action="delete" data-host="${host}"
                aria-label="Delete route ${host}" title="Delete route">
          ${ICONS.trash}
        </button>
      </div>
    </li>`;
  }

  function renderRoutes() {
    const active = document.activeElement;
    const focus = active && ui.list.contains(active) && active.dataset.action
      ? { action: active.dataset.action, host: active.dataset.host }
      : null;

    ui.list.setAttribute('aria-busy', 'false');

    if (!routes.length) {
      ui.list.classList.add('is-empty');
      ui.list.innerHTML = `<li class="empty">
        ${ICONS.dome}
        <h3>No routes tunneled</h3>
        <p>Nothing under the dome yet. Add a local port below.</p>
      </li>`;
      ui.count.textContent = '';
      return;
    }

    ui.list.classList.remove('is-empty');
    ui.list.innerHTML = routes.map(routeRow).join('');
    ui.count.textContent = `${routes.length} route${routes.length === 1 ? '' : 's'}`;

    if (focus && focus.host) {
      const match = ui.list.querySelector(
        `[data-action="${focus.action}"][data-host="${CSS.escape(focus.host)}"]`
      );
      match?.focus();
    }
  }

  function renderSkeleton() {
    ui.list.classList.remove('is-empty');
    ui.list.setAttribute('aria-busy', 'true');
    ui.list.innerHTML = [128, 96, 112].map((width) => `<li class="skeleton-row" aria-hidden="true">
      <span class="sk sk-host" style="width:${width}px"></span>
      <span class="sk sk-port"></span>
      <span class="sk sk-ctl"></span>
      <span class="sk sk-ctl"></span>
    </li>`).join('');
    ui.count.textContent = '';
  }

  function renderOffline() {
    ui.list.classList.add('is-empty');
    ui.list.setAttribute('aria-busy', 'false');
    ui.list.innerHTML = `<li class="empty offline">
      ${ICONS.dome}
      <h3>Manager unreachable</h3>
      <p>tunna isn't answering on 127.0.0.1:8080. Start it and try again.</p>
      <button class="ghost-btn" type="button" data-action="retry">Retry</button>
    </li>`;
    ui.count.textContent = '';
  }

  function applyData(data) {
    if (!data || !Array.isArray(data.routes)) throw new Error('Unexpected response from manager');

    baseDomain = data.base_domain || baseDomain;
    ui.domain.textContent = baseDomain;
    ui.domain.title = baseDomain;
    syncPlaceholder();

    routes = data.routes;
    loaded = true;

    const key = JSON.stringify([baseDomain, routes]);
    if (key !== lastKey) {
      lastKey = key;
      renderRoutes();
    } else if (ui.list.getAttribute('aria-busy') === 'true') {
      renderRoutes();
    }
  }

  /* ---------- load / poll ---------- */

  async function load({ skeleton = false } = {}) {
    if (skeleton) renderSkeleton();
    const started = performance.now();

    try {
      const data = await api('/api/routes');
      if (skeleton) await sleep(Math.max(0, 300 - (performance.now() - started)));
      setStatus('online');
      applyData(data);
      return true;
    } catch (err) {
      if (skeleton) await sleep(Math.max(0, 300 - (performance.now() - started)));
      const wasOnline = ui.status.dataset.state === 'online';
      setStatus('offline');
      if (!loaded) renderOffline();
      if (!skeleton && wasOnline) showToast(err?.message || 'Cannot reach manager');
      return false;
    }
  }

  /* ---------- actions ---------- */

  async function copyUrl(host, button) {
    const url = `https://${host}`;
    try {
      await navigator.clipboard.writeText(url);
    } catch (_) {
      try {
        fallbackCopy(url);
      } catch (err) {
        showToast('Could not copy the URL');
        return;
      }
    }

    button.classList.add('copied');
    button.title = 'Copied';
    clearTimeout(button._copiedTimer);
    button._copiedTimer = setTimeout(() => {
      button.classList.remove('copied');
      button.title = `Copy ${url}`;
    }, 1200);
  }

  function fallbackCopy(text) {
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.cssText = 'position:fixed;opacity:0;pointer-events:none';
    document.body.appendChild(area);
    area.select();
    try {
      if (!document.execCommand('copy')) throw new Error('copy failed');
    } finally {
      area.remove();
    }
  }

  async function toggleRoute(input) {
    const host = input.dataset.host;
    const enabled = input.checked;
    input.disabled = true;
    try {
      await api(`/api/routes/${encodeURIComponent(host)}`, jsonBody('PATCH', { enabled }));
      await load();
    } catch (err) {
      input.checked = !enabled;
      input.disabled = false;
      showToast(err?.message || 'Could not update route');
    }
  }

  async function deleteRoute(button) {
    const host = button.dataset.host;
    button.disabled = true;
    try {
      await api(`/api/routes/${encodeURIComponent(host)}`, { method: 'DELETE' });
    } catch (err) {
      button.disabled = false;
      showToast(err?.message || 'Could not delete route');
    }
    await load();
  }

  /* ---------- events ---------- */

  ui.list.addEventListener('click', (event) => {
    const button = event.target.closest('button[data-action]');
    if (!button || button.disabled) return;

    if (button.dataset.action === 'copy') {
      copyUrl(button.dataset.host, button);
    } else if (button.dataset.action === 'delete') {
      deleteRoute(button);
    } else if (button.dataset.action === 'retry') {
      load({ skeleton: true });
    }
  });

  ui.list.addEventListener('change', (event) => {
    const input = event.target.closest('input[data-action="toggle"]');
    if (input) toggleRoute(input);
  });

  ui.form.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (busy) return;

    const raw = ui.port.value.trim();
    const port = Number(raw);
    if (!/^\d+$/.test(raw) || port < 1 || port > 65535) {
      showToast('Port must be between 1 and 65535');
      ui.port.focus();
      return;
    }

    const host = ui.host.value.trim();
    busy = true;
    ui.expose.disabled = true;
    ui.expose.setAttribute('aria-busy', 'true');

    try {
      await api('/api/routes', jsonBody('POST', host ? { port, host } : { port }));
      ui.port.value = '';
      ui.host.value = '';
      syncPlaceholder();
      ui.port.focus();
      await load();
    } catch (err) {
      showToast(err?.message || 'Could not add route');
    } finally {
      busy = false;
      ui.expose.disabled = false;
      ui.expose.removeAttribute('aria-busy');
    }
  });

  ui.port.addEventListener('input', () => {
    const digits = ui.port.value.replace(/\D/g, '').slice(0, 5);
    if (ui.port.value !== digits) ui.port.value = digits;
    syncPlaceholder();
  });

  setInterval(() => {
    if (document.visibilityState === 'visible' && !busy) load();
  }, POLL_MS);

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') load();
  });

  syncPlaceholder();
  load({ skeleton: true });
})();
