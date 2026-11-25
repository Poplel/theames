// Theames bootstrap: injected into every Teams page. Sets up the page->Theames
// bridge, a floating launcher button, and the theme-manager panel. Idempotent.
// Talks to Theames via the CDP binding window.__theamesSend; replies arrive via
// window.__theamesOnResponse.
(function () {
  // Self-cleaning: if a previous bootstrap ran on this page, tear down its
  // listeners and UI before setting up again. This prevents stale handlers from
  // stacking (e.g. across soft reloads) and racing each other.
  if (window.__theamesCleanup) { try { window.__theamesCleanup(); } catch (e) {} }
  window.__theamesBootstrapped = true;

  var listeners = [];
  var menuWatchers = []; // [menu, observer] for each "..." flyout we keep our item in
  function on(target, type, fn, capture) {
    target.addEventListener(type, fn, capture);
    listeners.push([target, type, fn, capture]);
  }
  window.__theamesCleanup = function () {
    listeners.forEach(function (l) { try { l[0].removeEventListener(l[1], l[2], l[3]); } catch (e) {} });
    listeners = [];
    var p = document.getElementById('theames-panel');
    if (p) p.remove();
    window.__theamesFlyoutHooked = false;
    // Stop re-adding our "..." menu item and take it out (also what uninstalling relies on).
    menuWatchers.forEach(function (w) { try { w[1].disconnect(); } catch (e) {} w[0].__theamesObserved = false; });
    menuWatchers = [];
    [].forEach.call(document.querySelectorAll('[data-tid="theames-menu-item"]'), function (n) { n.remove(); });
    // Stop every running mod and the transparency sweep too. The next bootstrap
    // starts with an empty activeMods, so anything left running here would be
    // orphaned and stack with its re-enabled copy (duplicate overlays, fighting
    // observers = flicker).
    Object.keys(activeMods).forEach(disableMod);
    setTransparency(false);
  };

  var pending = {};
  var seq = 0;

  window.__theamesOnResponse = function (raw) {
    try {
      var msg = JSON.parse(raw);
      var p = pending[msg.id];
      if (!p) return;
      delete pending[msg.id];
      if (msg.ok) p.resolve(msg.data);
      else p.reject(new Error(msg.error || 'theames error'));
    } catch (e) {
      console.error('[theames] bad response', e);
    }
  };

  function req(action, payload) {
    return new Promise(function (resolve, reject) {
      if (typeof window.__theamesSend !== 'function') {
        reject(new Error('theames bridge unavailable'));
        return;
      }
      var id = ++seq;
      pending[id] = { resolve: resolve, reject: reject };
      window.__theamesSend(JSON.stringify({ id: id, action: action, payload: payload || null }));
    });
  }
  window.__theamesReq = req;

  // ---- Theme runtime (reversible) ----------------------------------------
  // Styles live in their own <style> elements (swap/remove freely). Mods are
  // arbitrary JS, so when a mod is enabled we record what it adds — DOM nodes,
  // observers, timers — and undo exactly that when it's disabled. No reloads.
  var activeMods = {}; // ref -> { nodes, observers, intervals, timeouts, ourObs }

  // Unchanged CSS is left alone: rewriting a <style> makes Teams restyle its whole page (thousands of
  // nodes), and the theme is re-applied on every page each time anything in the panel changes.
  function setStyle(id, css) {
    var s = document.getElementById(id);
    if (!css) { if (s) s.remove(); return; }
    if (s && s.textContent === css) return;
    if (!s) { s = document.createElement('style'); s.id = id; }
    s.textContent = css;
    (document.head || document.documentElement).appendChild(s);
  }

  function enableMod(mod) {
    if (activeMods[mod.name]) return;
    var tracked = { nodes: [], observers: [], intervals: [], timeouts: [], source: mod.source };
    var RealMO = window.MutationObserver;
    var RealSI = window.setInterval;
    var RealST = window.setTimeout;

    // Record nodes the mod inserts. Only the mod's SYNCHRONOUS additions count: JS is
    // single-threaded, so while its source evaluates nothing else (React!) can add
    // nodes. Watching longer would also record Teams' own nodes and delete them on disable.
    tracked.ourObs = new RealMO(function () {});
    tracked.ourObs.observe(document.documentElement, { childList: true, subtree: true });

    // Record observers/timers the mod creates so they can be stopped later.
    window.MutationObserver = function (cb) { var o = new RealMO(cb); tracked.observers.push(o); return o; };
    window.setInterval = function () { var id = RealSI.apply(window, arguments); tracked.intervals.push(id); return id; };
    window.setTimeout = function () { var id = RealST.apply(window, arguments); tracked.timeouts.push(id); return id; };

    // Expose the background for old-style mods that reference it as a global.
    window.imageBase64Background = window.__theame.background || '';
    try { (0, eval)(mod.source); } catch (e) { console.error('[theames] mod ' + mod.name + ' failed', e); }

    window.MutationObserver = RealMO;
    window.setInterval = RealSI;
    window.setTimeout = RealST;
    tracked.ourObs.takeRecords().forEach(function (m) {
      for (var i = 0; i < m.addedNodes.length; i++) {
        if (m.addedNodes[i].nodeType === 1) tracked.nodes.push(m.addedNodes[i]);
      }
    });
    tracked.ourObs.disconnect();
    activeMods[mod.name] = tracked;
  }

  function disableMod(name) {
    var t = activeMods[name];
    if (!t) return;
    try { t.ourObs.disconnect(); } catch (e) {}
    t.observers.forEach(function (o) { try { o.disconnect(); } catch (e) {} });
    t.intervals.forEach(function (id) { clearInterval(id); });
    t.timeouts.forEach(function (id) { clearTimeout(id); });
    t.nodes.forEach(function (n) { try { if (n.parentNode) n.parentNode.removeChild(n); } catch (e) {} });
    delete activeMods[name];
  }

  // ---- Core transparency ---------------------------------------------------
  // Teams paints opaque surfaces with literal colours, so a background on <body> is
  // invisible unless they're cleared. Backgrounds opt in via the manifest
  // (`transparent`, default true). CSS clears known containers; a sweep tags other
  // surfaces by their computed colour. Tags are an attribute, inert once the style
  // is removed, so turning it off is fully reversible.
  var CLEAR_ATTR = 'data-theames-clear';
  var SURFACES = {
    'rgb(31, 31, 31)': 1, 'rgb(20, 20, 20)': 1, 'rgb(26, 26, 26)': 1, 'rgba(26, 26, 26, 0.5)': 1,
    'rgb(41, 41, 41)': 1, 'rgb(24, 24, 24)': 1, 'rgb(36, 36, 36)': 1, 'rgb(15, 15, 15)': 1, 'rgb(25, 25, 25)': 1
  };
  // Never cleared: Theames' own UI (ids "theames-…", e.g. mod panels, which often borrow Teams' colours) and
  // Teams' pop-up surfaces (menus, popovers, dialogs, dropdown lists, tooltips) - they float over content, so
  // cleared they are unreadable. A theme can still restyle them (Aurora makes them frosted glass).
  // Pop-ups: only the surface itself keeps its colour (the grey inside - menu items etc. - is cleared as
  // usual, so a theme that frosts the surface gets clean glass, and one that doesn't still looks like Teams).
  var KEEP = '[data-tid="chat-pane-message"],[data-tid="ckeditor"],[id^="theames-"]';
  // (Fluent v9 .fui-* and the older v0 .ui-*; v0 pop-ups paint on .ui-popup__content__content.)
  var KEEP_SELF = '.fui-MenuPopover,.fui-PopoverSurface,.fui-DialogSurface,.fui-Listbox,.fui-Tooltip__content,' +
    '.ui-popup__content__content,.ui-dialog,.ui-tooltip__content,.ui-dropdown__items-list';
  var TRANSPARENCY_CSS =
    '[data-tid="slot-measurer"],[data-tid^="app-layout-area--"]{background:transparent!important;background-color:transparent!important}' +
    '[' + CLEAR_ATTR + ']{background:transparent!important;background-color:transparent!important}';

  function setTransparency(on) {
    var st = window.__theamesTransp;
    // Already on: keep the running sweep. Re-sweeping the whole page on every apply (each slider tick in the
    // panel) read the computed style of every element in Teams.
    if (on && st && document.getElementById('theames-transparency')) return;
    if (st) {
      try { st.obs.disconnect(); } catch (e) {}
      document.removeEventListener('transitionend', st.onEnd, true);
      window.__theamesTransp = null;
    }
    var style = document.getElementById('theames-transparency');
    if (!on) {
      if (style) style.remove();
      document.documentElement.removeAttribute('data-theames-transparent');
      [].forEach.call(document.querySelectorAll('[' + CLEAR_ATTR + ']'), function (n) { n.removeAttribute(CLEAR_ATTR); });
      return;
    }
    if (!style) { style = document.createElement('style'); style.id = 'theames-transparency'; }
    style.textContent = TRANSPARENCY_CSS;
    (document.head || document.documentElement).appendChild(style);
    // For theme CSS / mods that want to style differently over a see-through background.
    document.documentElement.setAttribute('data-theames-transparent', '');

    function tag(el) {
      if (el.hasAttribute(CLEAR_ATTR) || el.matches(KEEP_SELF)) return;
      if (SURFACES[getComputedStyle(el).backgroundColor]) el.setAttribute(CLEAR_ATTR, '');
    }
    // Walks the subtree but skips KEEP subtrees entirely (one closest() for the root, then pruning).
    function sweep(root) {
      if (root.nodeType !== 1 || root.closest(KEEP)) return;
      var walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
        acceptNode: function (n) { return n.matches(KEEP) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT; }
      });
      for (var n = root; n; n = walker.nextNode()) tag(n);
    }
    // The whole body, not just the app layout: pop-ups render in portals next to it.
    var layout = document.body;
    sweep(layout);
    var queued = [], raf = false;
    var obs = new MutationObserver(function (muts) {
      muts.forEach(function (m) {
        for (var i = 0; i < m.addedNodes.length; i++) if (m.addedNodes[i].nodeType === 1) queued.push(m.addedNodes[i]);
      });
      if (raf || !queued.length) return;
      raf = true;
      requestAnimationFrame(function () {
        raf = false;
        var q = queued; queued = [];
        q.forEach(function (n) { if (document.contains(n)) sweep(n); });
      });
    });
    obs.observe(layout, { childList: true, subtree: true });
    // Many Teams surfaces fade their background in (transition: all), so a fresh node can be swept before it
    // has reached its surface colour: check again when its background transition ends.
    var onEnd = function (e) {
      if (!/^background/.test(e.propertyName) || !e.target.closest || e.target.closest(KEEP)) return;
      tag(e.target);
    };
    document.addEventListener('transitionend', onEnd, true);
    window.__theamesTransp = { obs: obs, onEnd: onEnd };
  }

  function applyTheme(theme) {
    window.__theame = window.__theame || {};
    window.__theame.background = (theme && theme.background) || '';

    var bg = window.__theame.background;
    var bgColor = (theme && theme.background_color) || '';
    setStyle('theames-background', bgColor
      ? 'body{background:' + bgColor + '!important;background-attachment:fixed!important;}'
      : bg
        ? "body{background-image:linear-gradient(rgba(31,31,31,0.2),rgba(31,31,31,0.2)),url('" + bg + "')!important;background-size:cover!important;background-position:center!important;background-repeat:no-repeat!important;background-attachment:fixed!important;}"
        : '');
    setStyle('theames-chats', (theme && theme.chats) || '');
    setTransparency(!!(theme && (theme.background || theme.background_color) && theme.transparent));

    var want = {};
    (theme && theme.mods || []).forEach(function (m) { want[m.name] = m; });
    Object.keys(activeMods).forEach(function (name) { if (!want[name]) disableMod(name); });
    (theme && theme.mods || []).forEach(function (m) {
      // A variable change rewrites the mod's source: restart it so it picks the value up.
      if (activeMods[m.name] && activeMods[m.name].source !== m.source) disableMod(m.name);
      enableMod(m);
    });
  }

  window.__theamesApplyTheme = function () {
    return req('getTheme').then(applyTheme).catch(function (e) { console.error('[theames] applyTheme', e); });
  };

  // ---- UI -----------------------------------------------------------------
  var PANEL_ID = 'theames-panel';
  var MENU_SELECTOR = '[data-tid="more-options-menu-content"]';
  var TRIGGER_SELECTOR = '[data-tid="more-options-header"]';
  var ITEM_TID = 'theames-menu-item';

  function el(tag, props, children) {
    var e = document.createElement(tag);
    if (props) Object.keys(props).forEach(function (k) {
      if (k === 'style') e.style.cssText = props[k];
      else if (k === 'text') e.textContent = props[k];
      else if (k.slice(0, 2) === 'on') e.addEventListener(k.slice(2), props[k]);
      else e.setAttribute(k, props[k]);
    });
    (children || []).forEach(function (c) { e.appendChild(c); });
    return e;
  }

  var escHandler = null;
  function closePanel() {
    var p = document.getElementById(PANEL_ID);
    if (p) p.remove();
    if (escHandler) { document.removeEventListener('keydown', escHandler); escHandler = null; }
  }

  function openPanel() {
    closePanel();
    req('list')
      .then(function (data) { renderPanel(data); })
      .catch(function (e) { console.error('[theames] openPanel', e); });
  }
  window.__theamesOpenPanel = openPanel;
  window.__theamesClosePanel = closePanel;

  // Open the panel from the "..." item. The panel opens first; the flyout is
  // dismissed on the next tick (dismissing it synchronously here aborts the
  // rest of this handler).
  function activate() {
    openPanel();
    setTimeout(closeFlyout, 0);
  }

  // Colour pickers for a theme's declared variables. Changes apply live (throttled
  // while dragging) and are saved per theme, so the theme file is never edited.
  function toHex(v) {
    v = String(v || '').trim();
    if (/^#[0-9a-f]{3}$/i.test(v)) return '#' + v[1] + v[1] + v[2] + v[2] + v[3] + v[3];
    return /^#[0-9a-f]{6}$/i.test(v) ? v.toLowerCase() : '#000000';
  }
  function variableRows(t) {
    var wrap = el('div', { style: 'margin-top:8px;padding-top:8px;border-top:1px solid #3a3a3a' },
      [el('div', { text: 'Customize', style: 'font-weight:600;opacity:.8;margin-bottom:4px' })]);
    (t.variables || []).forEach(function (v) {
      var timer = null, last = null;
      var isRange = v.type === 'range', isToggle = v.type === 'toggle';
      var input = isToggle
        ? el('input', { type: 'checkbox', style: 'width:16px;height:16px;margin:0 11px;cursor:pointer' })
        : isRange
        ? el('input', { type: 'range', min: String(v.min != null ? v.min : 0), max: String(v.max != null ? v.max : 10), step: String(v.step != null ? v.step : 1),
            style: 'width:110px;cursor:pointer' })
        : el('input', { type: 'color', style: 'width:38px;height:26px;padding:0;border:1px solid #555;border-radius:6px;background:none;cursor:pointer' });
      function isOn(x) { return !/^(|0|false|off|no)$/i.test(String(x == null ? '' : x).trim()); }
      function setInput(x) { if (isToggle) input.checked = isOn(x); else input.value = isRange ? String(x) : toHex(x); }
      function cur() { return isToggle ? (input.checked ? '1' : '0') : input.value; }
      setInput(v.value);
      var readout = isRange ? el('span', { text: String(v.value), style: 'min-width:20px;text-align:right;opacity:.8' }) : null;
      function same(a, b) { return isToggle ? isOn(a) === isOn(b) : isRange ? String(a) === String(b) : toHex(a) === toHex(b); }
      var reset = el('button', { text: '\u21BA', title: 'Reset to default',
        style: 'background:none;border:0;color:#fff;cursor:pointer;font-size:15px;opacity:.7;display:' + (same(v.value, v.default) ? 'none' : 'inline') });
      function send(value) {
        last = value;
        req('setVariable', { file: t.file, name: v.name, value: value });
        reset.style.display = (value === null || same(value, v.default)) ? 'none' : 'inline';
      }
      input.addEventListener('input', function () {
        if (readout) readout.textContent = cur();
        if (timer) return;
        timer = setTimeout(function () { timer = null; send(cur()); }, 120);
      });
      input.addEventListener('change', function () {
        if (timer) { clearTimeout(timer); timer = null; }
        if (last !== cur()) send(cur());
      });
      reset.addEventListener('click', function () {
        setInput(v.default);
        if (readout) readout.textContent = cur();
        send(null);
      });
      wrap.appendChild(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:3px 0' },
        [input, el('span', { text: v.label || v.name, style: 'flex:1' })].concat(readout ? [readout] : []).concat([reset])));
    });
    return wrap;
  }

  function renderPanel(data) {
    var sel = data.selection || { mods: [], background: null, icon: null };
    var modSel = sel.mods || [];

    var sections = [];
    (data.themes || []).forEach(function (t) {
      var rows = [];
      (t.mods || []).forEach(function (m) {
        var ref = t.file + '::' + m;
        var cb = el('input', { type: 'checkbox', 'data-mod': ref });
        cb.checked = modSel.indexOf(ref) >= 0;
        rows.push(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
          [cb, el('span', { text: 'mod · ' + m })]));
      });
      (t.backgrounds || []).forEach(function (b) {
        var ref = t.file + '::' + b;
        var rb = el('input', { type: 'radio', name: 'theames-bg', 'data-bg': ref });
        rb.checked = sel.background === ref;
        rows.push(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
          [rb, el('span', { text: 'background · ' + b })]));
      });
      (t.chats || []).forEach(function (c) {
        var ref = t.file + '::' + c;
        var rc = el('input', { type: 'radio', name: 'theames-chat', 'data-chat': ref });
        rc.checked = sel.chats === ref;
        rows.push(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
          [rc, el('span', { text: 'chats · ' + c })]));
      });
      if (t.hasIcon) {
        var ir = el('input', { type: 'radio', name: 'theames-icon', 'data-icon': t.file });
        ir.checked = sel.icon === t.file;
        rows.push(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
          [ir, el('span', { text: 'icon · ' + t.name })]));
      }
      if ((t.variables || []).length) rows.push(variableRows(t));
      // this theme's own auto-update toggle
      var au = el('input', { type: 'checkbox', 'data-tid': 'theames-theme-auto', 'data-file': t.file, style: 'margin:0;cursor:pointer' });
      au.checked = t.autoUpdate !== false;
      au.addEventListener('change', function () { req('setThemeAutoUpdate', { file: t.file, on: au.checked }); });
      rows.push(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:6px 0 0;margin-top:6px;border-top:1px solid #333;cursor:pointer;font-size:12px;opacity:.8' },
        [au, el('span', { text: 'Update automatically' })]));
      sections.push(el('div', { style: 'margin:10px 0;padding:8px;border:1px solid #3a3a3a;border-radius:8px' },
        [el('div', { text: t.name + '  ' + (t.version || ''), style: 'font-weight:600;margin-bottom:4px' })].concat(rows)));
    });

    // "Default Teams" is a built-in theme card first in the list. Its rows are the
    // empty choice (data-bg / data-chat / data-icon = "") in the same radio groups, so
    // picking one clears that component back to stock Teams (the icon: Teams' own).
    var stockBg = el('input', { type: 'radio', name: 'theames-bg', 'data-bg': '' });
    stockBg.checked = !sel.background;
    var stockChat = el('input', { type: 'radio', name: 'theames-chat', 'data-chat': '' });
    stockChat.checked = !sel.chats;
    var stockIcon = el('input', { type: 'radio', name: 'theames-icon', 'data-icon': '' });
    stockIcon.checked = !sel.icon;
    var theamesIcon = el('input', { type: 'radio', name: 'theames-icon', 'data-icon': '@theames' });
    theamesIcon.checked = sel.icon === '@theames';
    sections.unshift(el('div', { style: 'margin:10px 0;padding:8px;border:1px solid #3a3a3a;border-radius:8px' }, [
      el('div', { text: 'Default Teams', style: 'font-weight:600;margin-bottom:4px' }),
      el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
        [stockBg, el('span', { text: 'background · Default Teams' })]),
      el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
        [stockChat, el('span', { text: 'chats · Default Teams' })]),
      el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
        [stockIcon, el('span', { text: 'icon · Default Teams' })])
    ].concat(data.theamesIcon ? [el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
        [theamesIcon, el('span', { text: 'icon · Theames' })])] : [])));

    if (!(data.themes || []).length) {
      sections.push(el('div', { text: 'No themes installed. Drop .theame files into the themes folder.', style: 'opacity:.7;padding:8px' }));
    }

    var apply = el('button', {
      text: 'Apply', style: btnStyle('#5b5fc7'),
      onclick: function () {
        var mods = [].slice.call(document.querySelectorAll('#' + PANEL_ID + ' input[data-mod]:checked')).map(function (c) { return c.getAttribute('data-mod'); });
        var bgEl = document.querySelector('#' + PANEL_ID + ' input[data-bg]:checked');
        var chatEl = document.querySelector('#' + PANEL_ID + ' input[data-chat]:checked');
        var icEl = document.querySelector('#' + PANEL_ID + ' input[data-icon]:checked');
        req('setSelection', {
          mods: mods,
          background: bgEl ? bgEl.getAttribute('data-bg') || null : null,
          chats: chatEl ? chatEl.getAttribute('data-chat') || null : null,
          icon: icEl ? icEl.getAttribute('data-icon') || null : null
        }).then(function () {
          apply.textContent = 'Applied ✓';
          setTimeout(function () { apply.textContent = 'Apply'; }, 1200);
        });
      }
    });
    var openFolder = el('button', { text: 'Open themes folder', style: btnStyle('#444'), onclick: function () { req('openFolder'); } });
    var close = el('button', { text: 'Close', style: btnStyle('#444'), onclick: closePanel });
    // Uninstall: a second click within a few seconds confirms.
    var armed = null;
    var uninstall = el('button', {
      text: 'Uninstall Theames', style: btnStyle('#7a2626'),
      onclick: function () {
        if (!armed) {
          uninstall.textContent = 'Click again to uninstall';
          armed = setTimeout(function () { armed = null; uninstall.textContent = 'Uninstall Theames'; }, 4000);
          return;
        }
        clearTimeout(armed);
        uninstall.textContent = 'Uninstalling…';
        uninstall.disabled = true;
        req('uninstall').catch(function () {});
      }
    });

    // Settings: automatic updates (Theames + themes), with a "Check now".
    var upd = el('div', { style: 'margin:10px 0;padding:8px;border:1px solid #3a3a3a;border-radius:8px' });
    var wasChecking = false, pollTimer = null;
    function renderUpdates(u) {
      while (upd.firstChild) upd.removeChild(upd.firstChild);
      var cb = el('input', { type: 'checkbox', 'data-tid': 'theames-auto-update', style: 'width:16px;height:16px;margin:0;cursor:pointer' });
      cb.checked = !!u.auto;
      cb.addEventListener('change', function () { req('setAutoUpdate', { on: cb.checked }).then(renderUpdates).then(poll); });
      var when = u.lastCheck && !u.checking ? ' \u00b7 checked ' + new Date(u.lastCheck * 1000).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : '';
      var check = el('button', { text: u.checking ? 'Checking\u2026' : 'Check now', 'data-tid': 'theames-check-updates', style: btnStyle('#444') + ';padding:4px 10px;flex:none',
        onclick: function () { req('checkUpdates').then(renderUpdates).then(poll); } });
      if (u.checking) check.disabled = true;
      upd.appendChild(el('div', { text: 'Settings', style: 'font-weight:600;margin-bottom:4px' }));
      upd.appendChild(el('label', { style: 'display:flex;gap:8px;align-items:center;padding:2px 0;cursor:pointer' },
        [cb, el('span', { text: 'Update Theames automatically' })]));
      upd.appendChild(el('div', { style: 'display:flex;gap:8px;align-items:center;margin-top:6px' }, [
        el('span', { 'data-tid': 'theames-update-status', text: 'Theames ' + u.version + (u.message ? ' \u00b7 ' + u.message : '') + when,
          style: 'flex:1;font-size:12px;' + (u.error ? 'color:#ff9a9a' : 'opacity:.75') }), check]));
      // a check that just installed themes: show the new versions
      if (wasChecking && !u.checking && /^Updated/.test(u.message || '')) setTimeout(openPanel, 600);
      wasChecking = !!u.checking;
      return u;
    }
    function poll(u) {
      if (!u || !u.checking || pollTimer) return;
      pollTimer = setTimeout(function () {
        pollTimer = null;
        if (!document.getElementById(PANEL_ID)) return;
        req('getUpdates').then(renderUpdates).then(poll).catch(function () {});
      }, 1000);
    }
    req('getUpdates').then(renderUpdates).then(poll).catch(function () { upd.style.display = 'none'; });

    var card = el('div', {
      style: 'width:380px;max-width:92vw;max-height:82vh;overflow:auto;' +
        'background:#1f1f1f;color:#fff;border:1px solid #3a3a3a;border-radius:14px;padding:18px;' +
        'font:13px -apple-system,Segoe UI,sans-serif;box-shadow:0 16px 50px rgba(0,0,0,.6)'
    }, [
      el('div', { text: 'Theames', style: 'font-size:17px;font-weight:700;margin-bottom:10px' })
    ].concat(sections).concat([
      el('div', { style: 'display:flex;gap:8px;margin-top:12px;flex-wrap:wrap' }, [apply, openFolder, close]),
      upd,
      el('div', { style: 'margin-top:14px;padding-top:10px;border-top:1px solid #3a3a3a' }, [uninstall])
    ]));

    var overlay = el('div', {
      id: PANEL_ID,
      style: 'position:fixed;inset:0;z-index:2147483647;display:flex;align-items:center;justify-content:center;' +
        'background:rgba(0,0,0,.45);backdrop-filter:blur(2px)',
      onclick: function (e) { if (e.target === overlay) closePanel(); }
    }, [card]);

    // Close on Escape while the modal is open (cleared in closePanel).
    overlay.tabIndex = -1;
    escHandler = function (ev) { if (ev.key === 'Escape') closePanel(); };
    document.addEventListener('keydown', escHandler);

    document.body.appendChild(overlay);
  }

  function btnStyle(bg) {
    return 'background:' + bg + ';color:#fff;border:0;border-radius:8px;padding:7px 12px;cursor:pointer;font:13px sans-serif';
  }

  // Dismiss the open "..." flyout via an outside pointer interaction (how
  // Fluent closes its menu). No Escape — that would also close our modal.
  // Dispatching these synthetically can abort the current call frame, so this
  // is always run on its own tick, after the panel is already opening.
  function closeFlyout() {
    try {
      ['pointerdown', 'mousedown', 'mouseup', 'click'].forEach(function (type) {
        document.body.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, view: window }));
      });
    } catch (e) {}
  }

  // Add a native-looking "Theames" item to the Settings-and-more flyout,
  // reusing the current items' Fluent classes so it matches whatever Teams
  // build is running.
  function injectMenuItem(menu) {
    if (!menu || menu.querySelector('[data-tid="' + ITEM_TID + '"]')) return;
    var sample = menu.querySelector('[role="menuitem"]');
    if (!sample) return;

    var item = document.createElement('div');
    item.setAttribute('role', 'menuitem');
    item.setAttribute('tabindex', '0');
    item.setAttribute('data-tid', ITEM_TID);
    item.className = sample.className;

    var si = sample.querySelector('.fui-MenuItem__icon');
    var sc = sample.querySelector('.fui-MenuItem__content');
    var icon = el('span', { style: 'display:inline-flex;align-items:center;justify-content:center' });
    icon.className = si ? si.className : '';
    icon.textContent = '🎨';
    var label = el('span', { text: 'Theames' });
    label.className = sc ? sc.className : '';
    item.appendChild(icon);
    item.appendChild(label);

    menu.appendChild(item);
  }

  // Inject our item and keep it present: Teams/React re-renders the menu and
  // drops nodes it doesn't own, so re-add ours whenever it disappears while the
  // menu is open.
  function injectAndWatch(menu) {
    injectMenuItem(menu);
    if (menu.__theamesObserved) return;
    menu.__theamesObserved = true;
    var obs = new MutationObserver(function () {
      if (!document.contains(menu)) { obs.disconnect(); return; }
      if (!menu.querySelector('[data-tid="' + ITEM_TID + '"]')) injectMenuItem(menu);
    });
    obs.observe(menu, { childList: true, subtree: true });
    menuWatchers.push([menu, obs]);
  }

  // When the "..." trigger is clicked, poll briefly for the flyout and inject.
  // Activation of our item is intercepted at the window-capture level, before
  // Teams' own handlers can swallow the event.
  function hookFlyout() {
    if (window.__theamesFlyoutHooked) return;
    window.__theamesFlyoutHooked = true;

    on(document, 'click', function (e) {
      var trigger = e.target.closest && e.target.closest(TRIGGER_SELECTOR);
      if (!trigger) return;
      var tries = 0;
      var iv = setInterval(function () {
        var menu = document.querySelector(MENU_SELECTOR);
        if (menu) { injectAndWatch(menu); clearInterval(iv); }
        if (++tries > 25) clearInterval(iv);
      }, 30);
    }, true);

    var activateHandler = function (e) {
      var hit = e.target && e.target.closest && e.target.closest('[data-tid="' + ITEM_TID + '"]');
      if (!hit) return;
      if (e.type === 'keydown' && e.key !== 'Enter' && e.key !== ' ') return;
      e.preventDefault();
      e.stopImmediatePropagation();
      activate();
    };
    on(window, 'click', activateHandler, true);
    on(window, 'keydown', activateHandler, true);
  }

  function boot() {
    if (!document.body) { setTimeout(boot, 300); return; }
    hookFlyout();
    window.__theamesApplyTheme();
  }

  boot();
})();
