// Behavior test for the embedded web dashboard's render functions.
//
// Usage: node dashboard_dom_harness.js <path-to-extracted-dashboard-script.js>
//
// Loads the dashboard's inline <script> into a sandbox with a minimal DOM
// shim, renders hostile track/file/device names, and checks that they end up
// as plain text and data values (never parsed as markup or code), and that
// clicking rendered items sends the exact original values to the API.
// Exits non-zero with a message on the first failed check.

'use strict';

const fs = require('fs');
const vm = require('vm');
const assert = require('assert');

const scriptPath = process.argv[2];
if (!scriptPath) {
  console.error('usage: node dashboard_dom_harness.js <dashboard-script.js>');
  process.exit(2);
}
const source = fs.readFileSync(scriptPath, 'utf8');

// ---------------------------------------------------------------------------
// Hostile values
// ---------------------------------------------------------------------------
const H = {
  quote: "it's a track",
  dquote: 'say "hello"',
  entity: "x&#39;);globalThis.__pwned=1;//",
  scriptTag: '</script><script>globalThis.__pwned=1</script>',
  backslash: 'C:\\music\\ends with backslash\\',
  img: '<img src=x onerror="globalThis.__pwned=1">',
};
const MARKERS = ['__pwned', '<img', '<script', '&#39;', "it's", 'say "hello"', 'backslash'];

// ---------------------------------------------------------------------------
// Minimal DOM shim
// ---------------------------------------------------------------------------
const innerHtmlWrites = [];
const createdTags = [];

function camel(name) {
  return name.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
}

class FakeText {
  constructor(text) {
    this.nodeType = 3;
    this.parentNode = null;
    this.data = String(text);
  }
  get textContent() { return this.data; }
}

class FakeElement {
  constructor(tag, id) {
    this.nodeType = 1;
    this.tagName = String(tag).toUpperCase();
    this.id = id || '';
    this.childNodes = [];
    this.parentNode = null;
    this.attributes = {};
    this.dataset = {};
    this.style = {};
    this.listeners = {};
    this.className = '';
    this.title = '';
    this.value = '';
    this.disabled = false;
    this.src = '';
    const self = this;
    this.classList = {
      add(...c) { const s = new Set(self.className.split(/\s+/).filter(Boolean)); c.forEach(x => s.add(x)); self.className = [...s].join(' '); },
      remove(...c) { self.className = self.className.split(/\s+/).filter(x => x && !c.includes(x)).join(' '); },
      contains(c) { return self.className.split(/\s+/).includes(c); },
      toggle(c, force) { const has = this.contains(c); const want = force === undefined ? !has : !!force; if (want) this.add(c); else this.remove(c); return want; },
    };
  }
  get children() { return this.childNodes.filter(n => n.nodeType === 1); }
  appendChild(child) {
    if (child.parentNode) {
      const sib = child.parentNode.childNodes;
      sib.splice(sib.indexOf(child), 1);
    }
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
  append(...nodes) {
    for (const n of nodes) this.appendChild(typeof n === 'string' ? new FakeText(n) : n);
  }
  replaceChildren(...nodes) {
    this._clear();
    this.append(...nodes);
  }
  _clear() {
    for (const c of this.childNodes) c.parentNode = null;
    this.childNodes = [];
  }
  get textContent() { return this.childNodes.map(c => c.textContent).join(''); }
  set textContent(v) {
    this._clear();
    const s = v === null || v === undefined ? '' : String(v);
    if (s) this.appendChild(new FakeText(s));
  }
  get innerText() { return this.textContent; }
  set innerText(v) { this.textContent = v; }
  set innerHTML(v) {
    innerHtmlWrites.push(String(v));
    this._clear();
    this._html = String(v);
  }
  get innerHTML() { return this._html || ''; }
  setAttribute(name, value) {
    name = String(name).toLowerCase();
    value = String(value);
    this.attributes[name] = value;
    if (name.startsWith('data-')) this.dataset[camel(name.slice(5))] = value;
    else if (name === 'class') this.className = value;
    else if (name === 'title') this.title = value;
  }
  getAttribute(name) {
    name = String(name).toLowerCase();
    if (name.startsWith('data-')) {
      const v = this.dataset[camel(name.slice(5))];
      return v === undefined ? null : v;
    }
    if (name === 'class') return this.className;
    return Object.prototype.hasOwnProperty.call(this.attributes, name) ? this.attributes[name] : null;
  }
  addEventListener(type, fn) {
    (this.listeners[type] = this.listeners[type] || []).push(fn);
  }
  removeEventListener() {}
  getBoundingClientRect() { return { left: 0, width: 100 }; }
}

const byId = new Map();
const document = {
  getElementById(id) {
    if (!byId.has(id)) byId.set(id, new FakeElement('div', id));
    return byId.get(id);
  },
  createElement(tag) {
    createdTags.push(String(tag).toLowerCase());
    return new FakeElement(tag);
  },
  createElementNS(_ns, tag) {
    createdTags.push(String(tag).toLowerCase());
    return new FakeElement(tag);
  },
  createTextNode(text) { return new FakeText(text); },
  createDocumentFragment() { return new FakeElement('#fragment'); },
  querySelectorAll() { return []; },
  querySelector() { return null; },
  activeElement: null,
};

const fetchCalls = [];
// Set to answer fetches (the status poll checks); otherwise they never resolve.
let fetchResponder = null;
function fakeFetch(url, opts) {
  fetchCalls.push({ url: String(url), opts: opts || {} });
  if (fetchResponder) return fetchResponder(String(url));
  return new Promise(() => {}); // never resolves; keeps renders deterministic
}

const sandbox = {
  document,
  window: { addEventListener() {}, removeEventListener() {} },
  fetch: fakeFetch,
  setTimeout() { return 0; },
  clearTimeout() {},
  setInterval() { return 0; },
  clearInterval() {},
  confirm() { return true; },
  prompt() { return promptAnswer; },
  console,
};
let promptAnswer = null;
vm.createContext(sandbox);
vm.runInContext(source, sandbox, { filename: 'dashboard.js' });

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function walk(node, out = []) {
  if (node.nodeType === 1) {
    out.push(node);
    for (const c of node.childNodes) walk(c, out);
  }
  return out;
}
function byClass(root, cls) {
  return walk(root).filter(n => n.classList.contains(cls));
}
function click(container, target) {
  const handlers = container.listeners.click || [];
  assert.ok(handlers.length > 0, `#${container.id} has no delegated click listener`);
  let stopped = false;
  const ev = {
    type: 'click',
    target,
    currentTarget: container,
    preventDefault() {},
    stopPropagation() { stopped = true; },
  };
  // Emulate bubbling through any listeners attached directly on elements.
  for (let n = target; n && !stopped; n = n.parentNode) {
    for (const fn of (n.listeners && n.listeners.click) || []) {
      ev.currentTarget = n;
      fn.call(n, ev);
    }
    if (n === container) break;
  }
}
function lastAction() {
  const call = fetchCalls[fetchCalls.length - 1];
  assert.ok(call, 'expected a fetch call');
  assert.strictEqual(call.url, '/api/action');
  return JSON.parse(call.opts.body);
}
function assertNoMarkupFromData() {
  for (const html of innerHtmlWrites) {
    for (const m of MARKERS) {
      assert.ok(!html.includes(m), `innerHTML was assigned data-derived markup containing ${JSON.stringify(m)}: ${html.slice(0, 200)}`);
    }
  }
  for (const el of walk(document.getElementById('queueList'))
    .concat(walk(document.getElementById('upNextList')))
    .concat(walk(document.getElementById('browserList')))
    .concat(walk(document.getElementById('bookmarksRow')))
    .concat(walk(document.getElementById('devicesList')))
    .concat(walk(document.getElementById('playlistsList')))) {
    for (const name of Object.keys(el.attributes)) {
      assert.ok(!name.startsWith('on'), `element has inline handler attribute ${name}`);
    }
  }
  assert.ok(!createdTags.includes('img'), 'rendering created an <img> element');
  assert.ok(!createdTags.includes('script'), 'rendering created a <script> element');
  assert.strictEqual(sandbox.__pwned, undefined, 'hostile payload executed');
}

const all = Object.values(H);

// ---------------------------------------------------------------------------
// 1. Queue + devices via renderStatus
// ---------------------------------------------------------------------------
const status = {
  is_playing: true,
  is_paused: false,
  title: H.scriptTag,
  artist: H.img,
  album: H.entity,
  format: H.dquote,
  duration_seconds: 200,
  position_seconds: 10,
  volume: 50,
  output_device: H.img,
  filepath: H.backslash,
  queue_index: 0,
  queue_total: all.length,
  next_track_title: all[1],
  up_next: all.slice(1, 4).map((v, i) => ({ index: i + 1, title: v, artist: v, duration_seconds: 60 })),
  up_next_remaining: all.length - 1,
  up_next_end: 'loops',
  queue_mode: 'loop',
  is_scanning: false,
  is_autopilot: true,
  active_mix_profile: H.img,
  key: H.scriptTag,
  queue: all.map((v, i) => ({
    index: i,
    title: v,
    artist: v,
    album: v,
    duration_seconds: 60,
    filepath: '/music/' + v,
    is_current: i === 0,
  })),
  devices: all.map((v, i) => ({ id: i, name: v, host_port: v, is_active: i === 1 })),
};
sandbox.renderStatus(status);

const queueList = document.getElementById('queueList');
assertNoMarkupFromData();
const titles = byClass(queueList, 'queue-item-title').map(n => n.textContent);
assert.deepStrictEqual(titles, all, 'queue titles not rendered verbatim as text');
const artists = byClass(queueList, 'queue-item-artist').map(n => n.textContent);
assert.deepStrictEqual(artists, all, 'queue artists not rendered verbatim as text');
assert.strictEqual(document.getElementById('title').textContent, H.scriptTag);
assert.strictEqual(document.getElementById('artist').textContent, H.img);
assert.strictEqual(document.getElementById('album').textContent, H.entity);
const upNextList = document.getElementById('upNextList');
assert.deepStrictEqual(byClass(upNextList, 'up-next-title').map(n => n.textContent), all.slice(1, 4),
  'Up Next titles not rendered verbatim as text');
assert.deepStrictEqual(byClass(upNextList, 'up-next-artist').map(n => n.textContent), all.slice(1, 4));
assert.strictEqual(document.getElementById('outputBadge').textContent, H.img);

const items = byClass(queueList, 'queue-item');
assert.strictEqual(items.length, all.length);
assert.ok(items[0].classList.contains('current'), 'current queue item not marked');
assert.ok(!items[1].classList.contains('current'));
assert.strictEqual(byClass(items[0], 'queue-num')[0].textContent, '▶');
assert.strictEqual(byClass(items[1], 'queue-num')[0].textContent, '2.');

for (let i = 0; i < all.length; i++) {
  const item = items[i];
  const main = byClass(item, 'queue-item-main')[0];
  click(queueList, byClass(main, 'queue-item-title')[0]);
  assert.deepStrictEqual(lastAction(), { action: 'play_index', value: null, index: i });

  const buttons = walk(item).filter(n => n.tagName === 'BUTTON');
  const seed = buttons.find(b => b.title === 'Seed Radio station from this track');
  const remove = buttons.find(b => b.title === 'Remove track');
  assert.ok(seed && remove, 'queue item buttons missing');

  const before = fetchCalls.length;
  click(queueList, seed);
  assert.strictEqual(fetchCalls.length, before + 1, 'seed click should send exactly one action');
  assert.deepStrictEqual(lastAction(), { action: 'seed_radio', value: null, path: '/music/' + all[i] });

  click(queueList, remove);
  assert.strictEqual(fetchCalls.length, before + 2, 'remove click should send exactly one action');
  assert.deepStrictEqual(lastAction(), { action: 'remove_index', value: null, index: i });

  // Clicking the duration label does nothing (matches original behavior).
  click(queueList, byClass(item, 'queue-item-dur')[0]);
  assert.strictEqual(fetchCalls.length, before + 2, 'duration click should not send an action');
}

const devicesList = document.getElementById('devicesList');
const deviceNames = byClass(devicesList, 'device-name').map(n =>
  n.childNodes.filter(c => c.nodeType === 3).map(c => c.data).join('').trim());
assert.deepStrictEqual(deviceNames, all, 'device names not rendered verbatim as text');
assert.deepStrictEqual(byClass(devicesList, 'device-host').map(n => n.textContent), all);
const deviceItems = byClass(devicesList, 'device-item');
assert.ok(deviceItems[1].classList.contains('active'));
assert.strictEqual(byClass(deviceItems[1], 'active-tag')[0].textContent, 'ACTIVE');
assert.strictEqual(byClass(deviceItems[0], 'active-tag').length, 0);
for (let i = 0; i < deviceItems.length; i++) {
  click(devicesList, byClass(deviceItems[i], 'device-host')[0]);
  assert.deepStrictEqual(lastAction(), { action: 'select_device', value: null, id: i });
}

// ---------------------------------------------------------------------------
// 2. File browser + bookmarks via renderBrowser
// ---------------------------------------------------------------------------
const browserData = {
  current_path: H.backslash,
  parent_path: '/music',
  bookmarks: all.map(v => '/bookmarks/' + v),
  entries: all.map(v => ({ name: 'd' + v, path: '/music/d' + v, is_dir: true }))
    .concat(all.map(v => ({ name: 'f' + v, path: '/music/f' + v, is_dir: false }))),
  home_dir: null,
  terminal_dir: null,
};
sandbox.renderBrowser(browserData, '');
assertNoMarkupFromData();
assert.strictEqual(document.getElementById('browserCurrentPath').textContent, H.backslash);

const browserList = document.getElementById('browserList');
const dirs = byClass(browserList, 'dir');
const files = byClass(browserList, 'file');
assert.strictEqual(dirs.length, all.length);
assert.strictEqual(files.length, all.length);
assert.deepStrictEqual(dirs.map(d => byClass(d, 'browser-name')[0].textContent), all.map(v => 'd' + v + '/'));
assert.deepStrictEqual(files.map(f => byClass(f, 'browser-name')[0].textContent), all.map(v => 'f' + v));

for (let i = 0; i < all.length; i++) {
  click(browserList, byClass(dirs[i], 'browser-name')[0]);
  let call = fetchCalls[fetchCalls.length - 1];
  assert.strictEqual(call.url, '/api/browser?path=' + encodeURIComponent('/music/d' + all[i]));

  const before = fetchCalls.length;
  click(browserList, byClass(files[i], 'browser-name')[0]);
  assert.deepStrictEqual(lastAction(), { action: 'add_file', value: null, path: '/music/f' + all[i] });

  const buttons = walk(files[i]).filter(n => n.tagName === 'BUTTON');
  const seed = buttons.find(b => b.title === 'Seed Radio station from this track');
  const add = buttons.find(b => b.title === 'Add to queue');
  assert.ok(seed && add, 'file item buttons missing');
  click(browserList, seed);
  assert.strictEqual(fetchCalls.length, before + 2, 'seed click should send exactly one action');
  assert.deepStrictEqual(lastAction(), { action: 'seed_radio', value: null, path: '/music/f' + all[i] });
  click(browserList, add);
  assert.strictEqual(fetchCalls.length, before + 3, 'add click should send exactly one action');
  assert.deepStrictEqual(lastAction(), { action: 'add_file', value: null, path: '/music/f' + all[i] });
}

const bookmarksRow = document.getElementById('bookmarksRow');
const chips = byClass(bookmarksRow, 'chip-btn');
assert.strictEqual(chips.length, all.length);
for (let i = 0; i < chips.length; i++) {
  const expectedPath = '/bookmarks/' + all[i];
  const parts = expectedPath.split(/[/\\]/);
  const name = parts[parts.length - 1] || parts[parts.length - 2] || expectedPath;
  assert.strictEqual(chips[i].textContent, '★ ' + name);
  click(bookmarksRow, chips[i]);
  const call = fetchCalls[fetchCalls.length - 1];
  assert.strictEqual(call.url, '/api/browser?path=' + encodeURIComponent(expectedPath));
}

// Filtering still works and only shows matching entries.
sandbox.renderBrowser(browserData, 'd' + H.img.toLowerCase());
assert.strictEqual(byClass(browserList, 'browser-item').length, 1);
assertNoMarkupFromData();

// ---------------------------------------------------------------------------
// 3. Playlists via renderPlaylists (issue #50)
// ---------------------------------------------------------------------------
const playlistsList = document.getElementById('playlistsList');
const playlistsSnapshot = {
  state: 'ready',
  message: null,
  revision: 9,
  last_playlist: H.img,
  playlists: all.map(v => ({
    name: v,
    track_count: 2,
    total_duration_seconds: 65,
    entries: [
      { index: 0, title: v, artist: v, duration_seconds: 65, path: '/music/' + v, outside: false, missing: false },
      { index: 1, title: v, artist: '', duration_seconds: -1, path: '', outside: true, missing: true },
    ],
  })),
};
sandbox.renderPlaylists(playlistsSnapshot);
assertNoMarkupFromData();
assert.deepStrictEqual(byClass(playlistsList, 'playlist-name').map(n => n.textContent), all,
  'playlist names not rendered verbatim as text');
assert.strictEqual(byClass(playlistsList, 'playlist-meta')[0].textContent, '2 tracks · 1:05');
assert.strictEqual(byClass(playlistsList, 'playlist-track').length, 0, 'playlists start closed');

for (let i = 0; i < all.length; i++) {
  const card = () => byClass(playlistsList, 'playlist-card')[i];
  const buttons = walk(card()).filter(n => n.tagName === 'BUTTON');
  const play = buttons.find(b => b.title === 'Play playlist (replaces the queue)');
  const append = buttons.find(b => b.title === 'Append playlist to the queue');
  assert.ok(play && append, 'playlist buttons missing');
  click(playlistsList, play);
  assert.deepStrictEqual(lastAction(), { action: 'playlist_play', value: null, name: all[i], index: 0, revision: 9 });
  click(playlistsList, append);
  assert.deepStrictEqual(lastAction(), { action: 'playlist_append', value: null, name: all[i] });

  // Open it: its tracks show, the missing one marked, the outside one labelled.
  const before = fetchCalls.length;
  click(playlistsList, byClass(card(), 'playlist-name')[0]);
  assert.strictEqual(fetchCalls.length, before, 'opening a playlist sends nothing');
  assert.ok(card().classList.contains('open'));
  const rows = byClass(card(), 'playlist-track');
  assert.strictEqual(rows.length, 2);
  assert.strictEqual(byClass(rows[0], 'queue-item-title')[0].textContent, all[i] + ' - ' + all[i]);
  assert.strictEqual(byClass(rows[0], 'queue-item-dur')[0].textContent, '1:05');
  assert.strictEqual(byClass(rows[1], 'queue-item-title')[0].textContent, '✗ missing: ' + all[i]);
  assert.strictEqual(byClass(rows[1], 'queue-item-dur')[0].textContent, '----');
  assert.ok(rows[1].classList.contains('missing') && rows[1].classList.contains('outside'));
  assertNoMarkupFromData();

  click(playlistsList, byClass(rows[1], 'queue-item-title')[0]);
  assert.deepStrictEqual(lastAction(), { action: 'playlist_play', value: null, name: all[i], index: 1, revision: 9 });

  // Track edits name the playlist, the index and the snapshot's revision.
  const trackButton = (title) => walk(rows[1]).find(n => n.tagName === 'BUTTON' && n.title === title);
  click(playlistsList, trackButton('Move up'));
  assert.deepStrictEqual(lastAction(),
    { action: 'playlist_move', value: null, name: all[i], index: 1, direction: 'up', revision: 9 });
  click(playlistsList, trackButton('Move down'));
  assert.deepStrictEqual(lastAction(),
    { action: 'playlist_move', value: null, name: all[i], index: 1, direction: 'down', revision: 9 });
  click(playlistsList, trackButton('Remove from playlist'));
  assert.deepStrictEqual(lastAction(), { action: 'playlist_remove', value: null, name: all[i], index: 1, revision: 9 });

  // Rename sends the typed name as is (the app validates it); Delete asks first.
  const headButton = (title) => walk(card()).find(n => n.tagName === 'BUTTON' && n.title === title);
  promptAnswer = H.img;
  click(playlistsList, headButton('Rename playlist'));
  assert.deepStrictEqual(lastAction(), { action: 'playlist_rename', value: null, name: all[i], new_name: H.img });
  promptAnswer = null;
  const beforeRename = fetchCalls.length;
  click(playlistsList, headButton('Rename playlist'));
  assert.strictEqual(fetchCalls.length, beforeRename, 'a cancelled rename sends nothing');
  click(playlistsList, headButton('Delete playlist'));
  assert.deepStrictEqual(lastAction(), { action: 'playlist_delete', value: null, name: all[i] });
  assertNoMarkupFromData();

  // Close it again.
  click(playlistsList, byClass(card(), 'playlist-name')[0]);
  assert.strictEqual(byClass(card(), 'playlist-track').length, 0);
}

// New playlist: the typed name goes to the app.
function fire(id) {
  for (const fn of document.getElementById(id).listeners.click || []) fn({ preventDefault() {} });
}
promptAnswer = H.dquote;
fire('btnPlaylistCreate');
assert.deepStrictEqual(lastAction(), { action: 'playlist_create', value: null, name: H.dquote });

// Add to playlist from a browser file: the picker lists the playlists as text, the last one
// added to marked, and a click adds the file's exact path.
sandbox.renderBrowser(browserData, '');
const pickerList = document.getElementById('playlistPickerList');
for (let i = 0; i < all.length; i++) {
  const fileRow = byClass(browserList, 'file')[i];
  const toPlaylist = walk(fileRow).find(n => n.tagName === 'BUTTON' && n.title === 'Add to playlist');
  assert.ok(toPlaylist, 'browser file row has no Add to playlist button');
  click(browserList, toPlaylist);
  assert.ok(document.getElementById('playlistPicker').classList.contains('show'), 'picker did not open');
  assert.strictEqual(document.getElementById('playlistPickerLabel').textContent, 'f' + all[i]);
  const pickRows = byClass(pickerList, 'picker-row');
  assert.strictEqual(pickRows.length, all.length + 1);
  assert.deepStrictEqual(pickRows.slice(0, all.length).map(r => r.textContent), all.map(v => v + ' (2 tracks)'));
  assert.strictEqual(pickRows[all.length].textContent, '+ New playlist…');
  assert.deepStrictEqual(pickRows.filter(r => r.classList.contains('last')).map(r => r.dataset.name), [H.img]);
  for (const node of walk(pickerList)) {
    for (const name of Object.keys(node.attributes)) {
      assert.ok(!name.startsWith('on'), `picker row has inline handler attribute ${name}`);
    }
  }
  click(pickerList, pickRows[(i + 1) % all.length]);
  assert.deepStrictEqual(lastAction(),
    { action: 'playlist_add', value: null, name: all[(i + 1) % all.length], path: '/music/f' + all[i] });
  assert.ok(!document.getElementById('playlistPicker').classList.contains('show'), 'picker did not close');
}
// "+ New playlist…" creates first (the add follows once the app accepts the name).
click(browserList, walk(byClass(browserList, 'file')[0]).find(n => n.title === 'Add to playlist'));
promptAnswer = '  ' + H.quote + ' ';
click(pickerList, byClass(pickerList, 'picker-row').find(r => r.dataset.action === 'new'));
assert.deepStrictEqual(lastAction(), { action: 'playlist_create', value: null, name: H.quote });
assertNoMarkupFromData();

// Not loaded: the app's message is shown as text.
sandbox.renderPlaylists({ state: 'failed', message: H.scriptTag, revision: 0, last_playlist: null, playlists: [] });
assert.strictEqual(playlistsList.textContent, H.scriptTag);
assertNoMarkupFromData();

// ---------------------------------------------------------------------------
// 4. Up Next (issue #104): the tracks /api/status lists and its end note, in tab [1]'s words
// ---------------------------------------------------------------------------
function upNextRows(over) {
  sandbox.renderStatus(Object.assign({}, status, over));
  return upNextList.childNodes.map(n => n.classList.contains('up-next-note')
    ? `note ${n.dataset.end}: ${n.textContent}`
    : `${n.dataset.index}: ${byClass(n, 'up-next-title')[0].textContent}`);
}
const upNextItem = (index) => ({ index, title: 'T' + index, artist: 'A', duration_seconds: 60 });

// Repeat-one: nothing else is next, the current track repeats.
assert.deepStrictEqual(upNextRows({ up_next: [], up_next_remaining: 0, up_next_end: 'repeat_one' }),
  ['note repeat_one: Repeat one · this track repeats']);
assert.strictEqual(document.getElementById('upNextRow').style.display, 'flex');
assert.strictEqual(document.getElementById('upNextCount').textContent, '');
// Continuous stream on the last track: it reshuffles, so no next track is named.
assert.deepStrictEqual(upNextRows({ up_next: [], up_next_remaining: 0, up_next_end: 'reshuffles' }),
  ['note reshuffles: End of queue · reshuffles and starts over']);
// Continuous stream earlier on: the tracks to the end of the queue, then the reshuffle.
assert.deepStrictEqual(upNextRows({ up_next: [upNextItem(3)], up_next_remaining: 1, up_next_end: 'reshuffles' }),
  ['3: T3', 'note reshuffles: End of queue · reshuffles and starts over']);
assert.strictEqual(document.getElementById('upNextCount').textContent, '1 remaining');
// Stop mode: on the last track, and with every track listed.
assert.deepStrictEqual(upNextRows({ up_next: [], up_next_remaining: 0, up_next_end: 'stops' }),
  ['note stops: End of queue · playback stops after this track']);
assert.deepStrictEqual(upNextRows({ up_next: [upNextItem(2), upNextItem(3)], up_next_remaining: 2, up_next_end: 'stops' }),
  ['2: T2', '3: T3', 'note stops: End of queue · playback stops']);
// More tracks than listed: the end isn't reached yet.
assert.deepStrictEqual(upNextRows({
  up_next: [upNextItem(1), upNextItem(2), upNextItem(3)], up_next_remaining: 5, up_next_end: 'stops',
}), ['1: T1', '2: T2', '3: T3']);
// Loop mode runs past the end to the first track; a single track repeats.
assert.deepStrictEqual(upNextRows({
  up_next: [upNextItem(3), upNextItem(0), upNextItem(1)], up_next_remaining: 3, up_next_end: 'loops',
}), ['3: T3', 'note wraps: End of queue · loops to 1', '0: T0', '1: T1']);
assert.deepStrictEqual(upNextRows({ up_next: [], up_next_remaining: 0, up_next_end: 'loops' }),
  ['note loops: End of queue · repeats this track']);
// Nothing loaded: no Up Next.
upNextRows({ up_next: [], up_next_remaining: 0, up_next_end: null });
assert.strictEqual(document.getElementById('upNextRow').style.display, 'none');
// An unknown end kind gets no note.
assert.deepStrictEqual(upNextRows({ up_next: [], up_next_remaining: 0, up_next_end: '__proto__' }), []);
assertNoMarkupFromData();

// Status poll: remote access turned off reads as that, not as OFFLINE; a lost connection is OFFLINE.
(async () => {
  const label = document.getElementById('statusLabel');
  fetchResponder = () => Promise.resolve({
    ok: false,
    status: 403,
    json: async () => ({ ok: false, error: 'remote access is off' }),
  });
  await sandbox.pollStatus();
  assert.strictEqual(label.textContent, 'REMOTE ACCESS OFF');
  fetchResponder = () => Promise.reject(new Error('connection refused'));
  await sandbox.pollStatus();
  assert.strictEqual(label.textContent, 'OFFLINE');
  fetchResponder = null;
  console.log('dashboard DOM harness: all checks passed');
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
