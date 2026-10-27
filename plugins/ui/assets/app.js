(function () {
  'use strict';

  var SESSION = document.querySelector('meta[name="st-session"]').content;
  var CATEGORIES = [
    ['todo', 'To do'],
    ['in-progress', 'In progress'],
    ['done', 'Done'],
    ['cancelled', 'Cancelled']
  ];

  var state = {
    tickets: [],
    statuses: {},
    warnings: {},
    searchIds: null,
    selected: null,
    detail: null,
    cursor: null,
    stream: null,
    lastSeq: 0,
    offline: null,
    health: null,
    dragId: null
  };

  function $(id) { return document.getElementById(id); }

  function el(tag, attrs) {
    var node = document.createElement(tag);
    var key;
    for (key in attrs || {}) {
      if (key === 'text') node.textContent = attrs[key];
      else if (key === 'class') node.className = attrs[key];
      else node.setAttribute(key, attrs[key]);
    }
    for (var i = 2; i < arguments.length; i++) {
      if (arguments[i]) node.appendChild(arguments[i]);
    }
    return node;
  }

  function sleep(ms) { return new Promise(function (r) { setTimeout(r, ms); }); }

  function ApiError(status, message) {
    this.status = status;
    this.message = message;
  }

  function api(method, path, body) {
    var options = { method: method, headers: { 'X-SimpleTickets-Session': SESSION } };
    if (body !== undefined) {
      options.headers['Content-Type'] = 'application/json';
      options.body = JSON.stringify(body);
    }
    return fetch('/api/v1' + path, options).then(function (res) {
      return res.json().catch(function () { return null; }).then(function (data) {
        if (!res.ok) throw new ApiError(res.status, (data && data.error) || 'HTTP ' + res.status);
        return data;
      });
    }, function () {
      throw new ApiError(0, 'The board server is not reachable');
    });
  }

  var toastTimer = null;
  function toast(message) {
    var node = $('toast');
    node.textContent = message;
    node.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { node.classList.remove('show'); }, 4000);
  }

  function today() {
    var d = new Date();
    var pad = function (n) { return (n < 10 ? '0' : '') + n; };
    return d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate());
  }

  function categoryOf(ticket) {
    return ticket.effective_category || ticket.category || 'todo';
  }

  function statusesIn(category) {
    return state.statuses[category] || [];
  }

  function allStatuses() {
    var out = [];
    CATEGORIES.forEach(function (c) { out = out.concat(statusesIn(c[0])); });
    return out;
  }

  function laneStatus(ticket) {
    var list = statusesIn(categoryOf(ticket));
    if (list.indexOf(ticket.status) >= 0) return ticket.status;
    return list[0] || ticket.status;
  }

  function compareTickets(a, b) {
    var da = a.due || '9999-99-99';
    var db = b.due || '9999-99-99';
    if (da !== db) return da < db ? -1 : 1;
    var na = parseInt(String(a.id).replace(/\D+/g, ''), 10) || 0;
    var nb = parseInt(String(b.id).replace(/\D+/g, ''), 10) || 0;
    return na - nb || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
  }

  function filters() {
    return {
      assignee: $('f-assignee').value,
      tag: $('f-tag').value,
      priority: $('f-priority').value,
      blocked: $('f-blocked').value,
      text: $('f-text').value.trim()
    };
  }

  function visibleTickets() {
    var f = filters();
    return state.tickets.filter(function (t) {
      if (f.assignee && (t.assignee || '') !== f.assignee) return false;
      if (f.tag && (t.tags || []).indexOf(f.tag) < 0) return false;
      if (f.priority && (t.priority || '') !== f.priority) return false;
      if (f.blocked === 'yes' && !t.blocked) return false;
      if (f.blocked === 'no' && t.blocked) return false;
      if (f.text && state.searchIds && !state.searchIds[t.id]) return false;
      return true;
    });
  }

  function fillSelect(select, values, anyLabel) {
    var current = select.value;
    while (select.firstChild) select.removeChild(select.firstChild);
    select.appendChild(el('option', { value: '', text: anyLabel }));
    values.forEach(function (v) { select.appendChild(el('option', { value: v, text: v })); });
    if (values.indexOf(current) >= 0) select.value = current;
  }

  function uniqueSorted(items) {
    var seen = {};
    items.forEach(function (v) { if (v) seen[v] = true; });
    return Object.keys(seen).sort();
  }

  function refreshFilterOptions() {
    fillSelect($('f-assignee'), uniqueSorted(state.tickets.map(function (t) { return t.assignee; })), 'Any assignee');
    var tags = [];
    state.tickets.forEach(function (t) { tags = tags.concat(t.tags || []); });
    fillSelect($('f-tag'), uniqueSorted(tags), 'Any tag');
    fillSelect($('f-priority'), uniqueSorted(state.tickets.map(function (t) { return t.priority; })), 'Any priority');
    var list = $('priority-list');
    while (list.firstChild) list.removeChild(list.firstChild);
    uniqueSorted(['low', 'medium', 'high'].concat(state.tickets.map(function (t) { return t.priority; })))
      .forEach(function (p) { list.appendChild(el('option', { value: p })); });
  }

  function chip(text, cls) {
    return el('span', { class: 'chip' + (cls ? ' ' + cls : ''), text: text });
  }

  function cardNode(ticket) {
    var warnings = state.warnings[ticket.id] || [];
    var chips = el('div', { class: 'chips' });
    if (ticket.priority) chips.appendChild(chip(ticket.priority));
    if (ticket.due) {
      var open = categoryOf(ticket) !== 'done' && categoryOf(ticket) !== 'cancelled';
      chips.appendChild(chip('due ' + ticket.due, open && ticket.due < today() ? 'overdue' : ''));
    }
    if (ticket.assignee) chips.appendChild(chip('@' + ticket.assignee));
    (ticket.tags || []).forEach(function (t) { chips.appendChild(chip('#' + t)); });
    if (ticket.blocked) chips.appendChild(chip('blocked', 'blocked'));
    if (ticket.parent) chips.appendChild(chip('sub of ' + ticket.parent));
    if ((ticket.children || []).length) chips.appendChild(chip(ticket.children.length + ' sub-tickets'));
    if (ticket.frozen) chips.appendChild(chip('frozen'));
    if (warnings.length) chips.appendChild(chip(warnings.length + (warnings.length === 1 ? ' warning' : ' warnings'), 'warn'));
    var card = el('article', {
      class: 'card',
      tabindex: '0',
      role: 'button',
      draggable: 'true',
      'data-id': ticket.id,
      'aria-label': ticket.id + ' ' + ticket.title + ', ' + ticket.status
    },
      el('span', { class: 'id', text: ticket.id }),
      el('span', { class: 'title', text: ticket.title }),
      chips);
    card.addEventListener('click', function () { openDetail(ticket.id); });
    card.addEventListener('keydown', function (e) { cardKey(e, ticket); });
    card.addEventListener('dragstart', function (e) {
      state.dragId = ticket.id;
      e.dataTransfer.setData('text/plain', ticket.id);
      e.dataTransfer.effectAllowed = 'move';
      card.classList.add('dragging');
    });
    card.addEventListener('dragend', function () {
      state.dragId = null;
      card.classList.remove('dragging');
    });
    return card;
  }

  function cardKey(e, ticket) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      openDetail(ticket.id);
    } else if (e.shiftKey && (e.key === 'ArrowRight' || e.key === 'ArrowLeft')) {
      e.preventDefault();
      var index = CATEGORIES.map(function (c) { return c[0]; }).indexOf(categoryOf(ticket));
      var next = index + (e.key === 'ArrowRight' ? 1 : -1);
      while (next >= 0 && next < CATEGORIES.length && !statusesIn(CATEGORIES[next][0]).length) {
        next += e.key === 'ArrowRight' ? 1 : -1;
      }
      if (next >= 0 && next < CATEGORIES.length) moveTo(ticket.id, statusesIn(CATEGORIES[next][0])[0]);
    }
  }

  function laneNode(category, status, tickets, showTitle) {
    var lane = el('div', { class: 'lane', 'data-status': status || '', role: 'group', 'aria-label': status || category });
    if (showTitle) lane.appendChild(el('h3', { text: status }));
    tickets.forEach(function (t) { lane.appendChild(cardNode(t)); });
    if (!status) return lane;
    lane.addEventListener('dragover', function (e) {
      if (!state.dragId) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
      lane.classList.add('over');
    });
    lane.addEventListener('dragleave', function () { lane.classList.remove('over'); });
    lane.addEventListener('drop', function (e) {
      e.preventDefault();
      lane.classList.remove('over');
      var id = e.dataTransfer.getData('text/plain') || state.dragId;
      if (id) moveTo(id, status);
    });
    return lane;
  }

  function render() {
    var active = document.activeElement;
    var focusId = active && active.classList && active.classList.contains('card') ? active.getAttribute('data-id') : null;
    var board = $('board');
    var scrollLeft = board.scrollLeft;
    while (board.firstChild) board.removeChild(board.firstChild);
    var visible = visibleTickets();
    CATEGORIES.forEach(function (c) {
      var category = c[0];
      var inCategory = visible.filter(function (t) { return categoryOf(t) === category; }).sort(compareTickets);
      var names = statusesIn(category);
      var column = el('section', { class: 'column', 'aria-label': c[1] },
        el('h2', {}, el('span', { text: c[1] }), el('span', { class: 'count', text: String(inCategory.length) })));
      if (names.length <= 1) {
        column.appendChild(laneNode(category, names[0], inCategory, false));
      } else {
        names.forEach(function (name) {
          var inLane = inCategory.filter(function (t) { return laneStatus(t) === name; });
          column.appendChild(laneNode(category, name, inLane, true));
        });
      }
      board.appendChild(column);
    });
    board.scrollLeft = scrollLeft;
    if (focusId) {
      var again = board.querySelector('.card[data-id="' + focusId.replace(/"/g, '') + '"]');
      if (again) again.focus();
    }
  }

  function applyTicket(ticket) {
    var found = false;
    state.tickets = state.tickets.map(function (t) {
      if (t.id !== ticket.id) return t;
      found = true;
      return ticket;
    });
    if (!found) state.tickets.push(ticket);
    if (state.selected === ticket.id) state.detail = ticket;
  }

  function moveTo(id, status) {
    var ticket = state.tickets.filter(function (t) { return t.id === id; })[0];
    if (!ticket || !status || ticket.status === status) return;
    api('PATCH', '/tickets/' + encodeURIComponent(id), { status: status }).then(function (updated) {
      applyTicket(updated);
      render();
      renderDetail();
      toast(id + ' moved to ' + status);
      scheduleRefresh();
    }, function (e) {
      toast('Could not move ' + id + ': ' + e.message);
      render();
    });
  }

  function loadConfig() {
    return api('GET', '/config').then(function (data) {
      state.statuses = (data && data.config && data.config.statuses) || {};
      var select = document.querySelector('#create-form [name="status"]');
      while (select.firstChild) select.removeChild(select.firstChild);
      select.appendChild(el('option', { value: '', text: 'Default' }));
      allStatuses().forEach(function (s) { select.appendChild(el('option', { value: s, text: s })); });
    });
  }

  function loadWarnings() {
    return api('GET', '/doctor').then(function (data) {
      var map = {};
      ((data && data.warnings) || []).forEach(function (w) {
        (map[w.id] = map[w.id] || []).push(w);
      });
      state.warnings = map;
      render();
      renderDetail();
    });
  }

  function runSearch() {
    var text = $('f-text').value.trim();
    if (!text) {
      state.searchIds = null;
      return Promise.resolve();
    }
    return api('GET', '/search?q=' + encodeURIComponent(text)).then(function (rows) {
      var ids = {};
      (rows || []).forEach(function (r) { ids[r.id] = true; });
      if ($('f-text').value.trim() === text) state.searchIds = ids;
    }, function (e) {
      state.searchIds = {};
      toast('Search failed: ' + e.message);
    });
  }

  function loadTickets() {
    return api('GET', '/tickets').then(function (list) {
      state.tickets = list || [];
      refreshFilterOptions();
      return runSearch();
    }).then(function () {
      render();
      if (state.selected) return loadDetail(state.selected, true);
    });
  }

  function reload() {
    return Promise.all([loadConfig(), loadTickets(), loadWarnings()]);
  }

  var refreshTimer = null;
  function scheduleRefresh() {
    clearTimeout(refreshTimer);
    refreshTimer = setTimeout(function () { loadTickets().catch(noop); }, 150);
  }
  var warnTimer = null;
  function scheduleWarnings() {
    clearTimeout(warnTimer);
    warnTimer = setTimeout(function () { loadWarnings().catch(noop); }, 300);
  }
  function noop() {}

  function handleEvents(events) {
    var tickets = false;
    var warnings = false;
    var config = false;
    var health = false;
    (events || []).forEach(function (event) {
      if (event.seq <= state.lastSeq) return;
      state.lastSeq = event.seq;
      var type = String(event.type);
      if (type.indexOf('ticket.') === 0 || type === 'rule.applied') tickets = true;
      if (type === 'warning.raised' || type === 'scan' || type === 'tombstone.forgotten') warnings = true;
      if (type === 'config.changed' || type === 'migration.finished') config = true;
      if (type.indexOf('root.') === 0 || type === 'update.available') health = true;
    });
    if (config) loadConfig().then(function () { render(); }).catch(noop);
    if (tickets) scheduleRefresh();
    if (warnings || tickets) scheduleWarnings();
    if (health) loadHealth();
  }

  function resync(page) {
    state.cursor = page.next;
    state.stream = page.stream;
    state.lastSeq = page.next;
    return reload();
  }

  function pollLoop() {
    var delay = 1000;
    function step() {
      var request;
      if (state.cursor === null) {
        request = api('GET', '/events').then(resync);
      } else {
        var query = '?since=' + state.cursor + '&wait=25&stream=' + encodeURIComponent(state.stream);
        request = api('GET', '/events' + query).then(function (page) {
          if (page.gap) return resync(page);
          state.cursor = page.next;
          handleEvents(page.events);
        });
      }
      request.then(function () {
        delay = 1000;
        setOffline(null);
        step();
      }, function (e) {
        setOffline(e.status === 401 ? 'The daemon refused the board token.' : 'Cannot reach the daemon; showing the last known state and retrying.');
        sleep(delay).then(function () {
          delay = Math.min(delay * 2, 10000);
          step();
        });
      });
    }
    step();
  }

  function setOffline(message) {
    state.offline = message;
    renderBanner();
  }

  function loadHealth() {
    return api('GET', '/health').then(function (health) {
      state.health = health;
      renderBanner();
    }, function () {
      state.health = null;
    });
  }

  function renderBanner() {
    var banner = $('banner');
    var parts = [];
    var bad = false;
    if (state.offline) {
      parts.push(state.offline);
      bad = true;
    } else if (state.health) {
      if (state.health.status === 'degraded') {
        (state.health.offline_roots || []).forEach(function (r) {
          parts.push('Folder offline, its tickets are frozen: ' + r.root + ' (' + r.message + ')');
        });
        if (!parts.length) parts.push('The daemon reports a degraded state.');
      }
      if (state.health.update && state.health.update.latest) {
        parts.push('Update available: ' + state.health.update.latest);
      }
    }
    banner.hidden = parts.length === 0;
    banner.className = 'banner' + (bad ? ' bad' : '');
    banner.textContent = parts.join(' | ');
  }

  function loadDetail(id, quiet) {
    return api('GET', '/tickets/' + encodeURIComponent(id)).then(function (ticket) {
      if (state.selected !== id) return;
      state.detail = ticket;
      renderDetail();
    }, function (e) {
      if (e.status === 404) closeDetail();
      else if (!quiet) toast('Could not load ' + id + ': ' + e.message);
    });
  }

  var lastOpener = null;
  function openDetail(id) {
    if (state.selected !== id) lastOpener = document.activeElement;
    state.selected = id;
    state.detail = state.tickets.filter(function (t) { return t.id === id; })[0] || null;
    renderDetail();
    loadDetail(id, false);
    var panel = $('detail');
    var heading = panel.querySelector('h2');
    if (heading) heading.focus();
  }

  function closeDetail() {
    state.selected = null;
    state.detail = null;
    renderDetail();
    if (lastOpener && document.body.contains(lastOpener)) lastOpener.focus();
  }

  function linkButtons(label, ids) {
    if (!ids || !ids.length) return null;
    var box = el('div', { class: 'links' });
    ids.forEach(function (id) {
      var b = el('button', { type: 'button', text: id });
      b.addEventListener('click', function () { openDetail(id); });
      box.appendChild(b);
    });
    return [el('dt', { text: label }), el('dd', {}, box)];
  }

  function field(label, name, value, type) {
    var input = el('input', { name: name, type: type || 'text' });
    input.value = value || '';
    return el('label', { text: label }, input);
  }

  function renderDetail() {
    var panel = $('detail');
    var t = state.detail;
    while (panel.firstChild) panel.removeChild(panel.firstChild);
    panel.hidden = !state.selected;
    if (!t) return;
    var close = el('button', { type: 'button', text: 'Close' });
    close.addEventListener('click', closeDetail);
    panel.appendChild(el('div', { class: 'bar' }, el('span', { class: 'id', text: t.id }), close));
    panel.appendChild(el('h2', { text: t.title, tabindex: '-1' }));
    var warnings = state.warnings[t.id] || [];
    if (warnings.length) {
      var list = el('ul');
      warnings.forEach(function (w) { list.appendChild(el('li', { text: w.message })); });
      panel.appendChild(el('div', { class: 'warnlist', role: 'note' }, el('strong', { text: 'Rule warnings' }), list));
    }
    var dl = el('dl');
    var rows = [
      ['Status', t.status + (t.effective_category ? ' (' + t.effective_category + ')' : '')],
      ['Priority', t.priority],
      ['Due', t.due],
      ['Assignee', t.assignee],
      ['Tags', (t.tags || []).join(', ')],
      ['Blocked', t.blocked ? 'yes' : 'no'],
      ['Parent', null],
      ['Path', t.path]
    ];
    rows.forEach(function (r) {
      if (r[1] === null || r[1] === undefined || r[1] === '') return;
      dl.appendChild(el('dt', { text: r[0] }));
      dl.appendChild(el('dd', { text: r[1] }));
    });
    [
      linkButtons('Parent', t.parent ? [t.parent] : []),
      linkButtons('Sub-tickets', t.children),
      linkButtons('Blocked by', t.blocked_by),
      linkButtons('Open blockers', t.open_blockers),
      linkButtons('Blocks', t.blocks),
      linkButtons('Related', t.related),
      linkButtons('Related by', t.related_by)
    ].forEach(function (pair) {
      if (pair) { dl.appendChild(pair[0]); dl.appendChild(pair[1]); }
    });
    panel.appendChild(dl);
    if (t.body !== null && t.body !== undefined) {
      panel.appendChild(el('h3', { text: 'Body' }));
      panel.appendChild(el('pre', { text: t.body || '(empty)' }));
    }
    panel.appendChild(editForm(t));
  }

  function editForm(t) {
    var statusSelect = el('select', { name: 'status' });
    var names = allStatuses();
    if (names.indexOf(t.status) < 0) names.unshift(t.status);
    names.forEach(function (s) { statusSelect.appendChild(el('option', { value: s, text: s })); });
    statusSelect.value = t.status;
    var form = el('form', { 'aria-label': 'Edit ' + t.id },
      el('label', { text: 'Status' }, statusSelect),
      field('Title', 'title', t.title),
      field('Priority', 'priority', t.priority),
      field('Assignee', 'assignee', t.assignee),
      field('Due', 'due', t.due, 'date'),
      field('Tags', 'tags', (t.tags || []).join(', ')),
      el('button', { type: 'submit', class: 'primary', text: 'Save changes' }));
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var data = new FormData(form);
      var patch = {};
      var text = function (k) { return String(data.get(k) || '').trim(); };
      if (text('status') !== t.status) patch.status = text('status');
      if (text('title') && text('title') !== t.title) patch.title = text('title');
      ['priority', 'assignee', 'due'].forEach(function (k) {
        var v = text(k);
        if (v !== (t[k] || '')) patch[k] = v === '' ? null : v;
      });
      var tags = parseTags(text('tags'));
      if (tags.join(',') !== (t.tags || []).join(',')) patch.tags = tags;
      if (!Object.keys(patch).length) return toast('Nothing changed');
      api('PATCH', '/tickets/' + encodeURIComponent(t.id), patch).then(function (updated) {
        applyTicket(updated);
        render();
        renderDetail();
        toast('Saved ' + t.id);
        scheduleRefresh();
      }, function (err) { toast('Could not save: ' + err.message); });
    });
    return form;
  }

  function parseTags(text) {
    return text.split(',').map(function (s) { return s.trim(); }).filter(Boolean);
  }

  function setupCreate() {
    var dialog = $('create');
    var form = $('create-form');
    $('new-ticket').addEventListener('click', function () {
      form.reset();
      $('create-error').textContent = '';
      dialog.showModal();
      form.elements.title.focus();
    });
    $('create-cancel').addEventListener('click', function () { dialog.close(); });
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var data = new FormData(form);
      var text = function (k) { return String(data.get(k) || '').trim(); };
      var body = { title: text('title') };
      ['status', 'priority', 'assignee', 'due', 'parent'].forEach(function (k) {
        if (text(k)) body[k] = text(k);
      });
      var tags = parseTags(text('tags'));
      if (tags.length) body.tags = tags;
      if (String(data.get('body') || '').trim()) body.body = String(data.get('body'));
      api('POST', '/tickets', body).then(function (created) {
        dialog.close();
        applyTicket(created);
        refreshFilterOptions();
        render();
        toast('Created ' + created.id);
        scheduleRefresh();
      }, function (err) { $('create-error').textContent = err.message; });
    });
  }

  function setupFilters() {
    var timer = null;
    $('filters').addEventListener('submit', function (e) { e.preventDefault(); });
    ['f-assignee', 'f-tag', 'f-priority', 'f-blocked'].forEach(function (id) {
      $(id).addEventListener('change', render);
    });
    $('f-text').addEventListener('input', function () {
      clearTimeout(timer);
      timer = setTimeout(function () { runSearch().then(render); }, 250);
    });
    $('f-clear').addEventListener('click', function () {
      $('filters').reset();
      state.searchIds = null;
      render();
    });
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape' && state.selected && !$('create').open) closeDetail();
    });
  }

  function start() {
    setupFilters();
    setupCreate();
    loadHealth();
    setInterval(loadHealth, 10000);
    pollLoop();
  }

  start();
})();
