// Plugin browse, cards, modal, search, installed 
import { esc, uid } from './util.js';

// Deps injected by initPlugins() 
let _api, _appendMessage, _switchPage, _toast;

export function initPlugins({ api, appendMessage, switchPage, toast }) {
  _api = api;
  _appendMessage = appendMessage;
  _switchPage = switchPage;
  _toast = toast || (() => {});
  _attachListeners();
}

// Language color map 
export const LANG_COLORS = {
  TypeScript:'#3178c6', JavaScript:'#f7df1e', Python:'#3572A5', Rust:'#dea584',
  Go:'#00ADD8', Java:'#b07219', Ruby:'#701516', 'C#':'#178600', 'C++':'#f34b7d',
  Kotlin:'#A97BFF', Swift:'#ffac45', Shell:'#89e051', Nix:'#7e7eff', Lua:'#000080',
  Dockerfile:'#384d54', HTML:'#e34c26', CSS:'#563d7c',
};

// Category detection 
const CATEGORY_RULES = {
  'Browser/Web':    ['playwright','puppeteer','browser','web','scrape','crawl','selenium','screenshot','http','fetch'],
  'Database':       ['postgres','mysql','sqlite','mongodb','redis','neon','supabase','database','sql','db','turso','prisma','drizzle'],
  'Search':         ['search','exa','brave','bing','google','semantic','perplexity','tavily','serp'],
  'Files/Storage':  ['filesystem','file','s3','drive','dropbox','storage','gdrive','onedrive','blob'],
  'Code/Git':       ['github','gitlab','git','code','editor','vscode','bitbucket','jira','linear','ci'],
  'Cloud/Infra':    ['aws','gcp','azure','cloudflare','docker','kubernetes','terraform','ansible','pulumi','k8s'],
  'AI/Vectors':     ['openai','anthropic','huggingface','embeddings','vector','qdrant','pinecone','weaviate','chroma','llm','ollama'],
  'Communication':  ['slack','email','gmail','discord','telegram','sms','twilio','teams','chat','sendgrid','mailgun'],
  'Productivity':   ['calendar','notion','jira','linear','trello','asana','todo','task','sheets','airtable','clickup'],
  'Finance/Crypto': ['crypto','bitcoin','finance','stock','trading','stripe','payment','coinbase','defi'],
};

export function detectCategory(plugin) {
  const haystack = [
    plugin.name,
    plugin.description,
    ...(plugin.topics || []),
  ].join(' ').toLowerCase();

  for (const [cat, keywords] of Object.entries(CATEGORY_RULES)) {
    if (keywords.some(k => haystack.includes(k))) return cat;
  }
  return 'Other';
}

// Browse state 
export const browseState = { type: 'all', sort: 'stars', category: 'all', page: 1, loading: false, hasMore: false, count: 0, allItems: [] };

// Modal state 
let pluginModal, modalClose, modalRepoBtn, modalInstallBtn, modalPlugin = null;

// Install-poll state 
const _installingPlugins = new Set();
let _pollTimer = null;

// Track installed plugin IDs and repo URLs so cards can reflect state
export const _installedIds    = new Set();
export const _installedRepos  = new Set();

function _startInstallPoll(pluginId) {
  _installingPlugins.add(pluginId);
  if (_pollTimer) return;
  _pollTimer = setInterval(() => {
    if (_installingPlugins.size === 0) { clearInterval(_pollTimer); _pollTimer = null; return; }
    loadPluginsInstalled();
  }, 3000);
}

function _attachListeners() {
  // Plugin tab switching
  document.querySelectorAll('.ptab').forEach(btn => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('.ptab').forEach(b => b.classList.toggle('active', b === btn));
      document.querySelectorAll('.plugin-panel').forEach(p => p.classList.toggle('active', p.id === `plugins-${btn.dataset.ptab}`));
    });
  });

  // Browse filter pills
  document.querySelectorAll('.fpill').forEach(btn => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('.fpill').forEach(b => b.classList.toggle('active', b === btn));
      browseState.type = btn.dataset.type;
      loadBrowse(true);
    });
  });

  document.getElementById('browse-sort').addEventListener('change', e => {
    browseState.sort = e.target.value;
    loadBrowse(true);
  });

  document.getElementById('browse-category').addEventListener('change', e => {
    browseState.category = e.target.value;
    renderBrowseFiltered();
  });

  document.getElementById('btn-load-more').addEventListener('click', () => loadBrowse(false));

  // Plugin search
  document.getElementById('btn-plugin-search').addEventListener('click', searchPlugins);
  document.getElementById('plugin-query').addEventListener('keydown', e => { if (e.key === 'Enter') searchPlugins(); });

  // Modal refs + listeners
  pluginModal     = document.getElementById('plugin-modal');
  modalClose      = document.getElementById('modal-close');
  modalRepoBtn    = document.getElementById('modal-repo-btn');
  modalInstallBtn = document.getElementById('modal-install-btn');

  modalClose.addEventListener('click', closePluginModal);
  pluginModal.addEventListener('click', e => { if (e.target === pluginModal) closePluginModal(); });
  modalRepoBtn.addEventListener('click', () => modalPlugin && _api.openExternal(modalPlugin.repo_url));
  modalInstallBtn.addEventListener('click', () => {
    if (modalPlugin) handlePluginAction2('install', modalPlugin.id, modalPlugin);
    closePluginModal();
  });
  document.addEventListener('keydown', e => { if (e.key === 'Escape') closePluginModal(); });
}

// Browse 
export function renderBrowseFiltered() {
  const ul = document.getElementById('browse-list');
  ul.innerHTML = '';
  const cat = browseState.category;
  const visible = cat === 'all'
    ? browseState.allItems
    : browseState.allItems.filter(p => detectCategory(p) === cat);

  if (!visible.length) {
    ul.innerHTML = `<li class="grid-empty">No <strong>${cat === 'all' ? '' : cat + ' '}plugins</strong> in this batch - try loading more</li>`;
  } else {
    for (const p of visible) ul.appendChild(makePluginCard(p));
  }
  setBrowseStatus(`${visible.length} of ${browseState.allItems.length} plugins shown`);
}

export async function loadBrowse(reset = false) {
  if (browseState.loading) return;
  if (reset) {
    browseState.page = 1;
    browseState.count = 0;
    browseState.hasMore = false;
    browseState.allItems = [];
    document.getElementById('browse-list').innerHTML = '';
  }

  browseState.loading = true;
  setBrowseStatus('Loading…');
  document.getElementById('btn-load-more').style.display = 'none';

  try {
    const { items, hasMore } = await _api.browsePlugins({
      type: browseState.type,
      sort: browseState.sort,
      page: browseState.page,
    });

    browseState.allItems.push(...items);
    browseState.count = browseState.allItems.length;
    browseState.hasMore = hasMore;
    browseState.page++;

    renderBrowseFiltered();
    document.getElementById('btn-load-more').style.display = hasMore ? '' : 'none';
  } catch (err) {
    setBrowseStatus(`Error: ${esc(String(err))}`);
  } finally {
    browseState.loading = false;
  }
}

export function setBrowseStatus(msg) {
  document.getElementById('browse-status').textContent = msg;
}

// Plugin detail modal 
export function openPluginModal(plugin) {
  modalPlugin = plugin;

  document.getElementById('modal-name').textContent      = plugin.name;
  document.getElementById('modal-full-name').textContent = plugin.full_name || plugin.id;
  document.getElementById('modal-desc').textContent      = plugin.description || 'No description provided.';

  const avatar = document.getElementById('modal-avatar');
  if (plugin.owner_avatar) { avatar.src = plugin.owner_avatar; avatar.style.display = ''; }
  else { avatar.style.display = 'none'; }

  document.getElementById('modal-stars').querySelector('span').textContent = (plugin.stars  || 0).toLocaleString();
  document.getElementById('modal-forks').querySelector('span').textContent = (plugin.forks  || 0).toLocaleString();

  const langEl = document.getElementById('modal-lang');
  langEl.textContent   = plugin.language || '';
  langEl.style.display = plugin.language ? '' : 'none';

  const upd = plugin.updated_at ? new Date(plugin.updated_at).toLocaleDateString(undefined, { year:'numeric', month:'short', day:'numeric' }) : '';
  document.getElementById('modal-updated').textContent = upd ? `Updated ${upd}` : '';

  const topicsEl = document.getElementById('modal-topics');
  topicsEl.innerHTML = '';
  for (const t of (plugin.topics || []).slice(0, 12)) {
    const span = document.createElement('span');
    span.className = 'topic-chip';
    span.textContent = t;
    topicsEl.appendChild(span);
  }

  const catEl = document.getElementById('modal-category');
  const cat   = detectCategory(plugin);
  catEl.innerHTML = `<span class="badge ${plugin.plugin_type === 'mcp' ? 'badge-mcp' : 'badge-skill'}">${plugin.plugin_type === 'mcp' ? 'MCP' : 'Skill'}</span>
    <span class="badge badge-cat">${esc(cat)}</span>`;

  pluginModal.classList.remove('hidden');
  document.body.classList.add('modal-open');
}

export function closePluginModal() {
  pluginModal.classList.add('hidden');
  document.body.classList.remove('modal-open');
  modalPlugin = null;
}

// Search 
export async function searchPlugins() {
  const q = document.getElementById('plugin-query').value.trim();
  if (!q) return;

  // Switch to search tab
  document.querySelectorAll('.ptab').forEach(b => b.classList.toggle('active', b.dataset.ptab === 'search'));
  document.querySelectorAll('.plugin-panel').forEach(p => p.classList.toggle('active', p.id === 'plugins-search'));

  const ul = document.getElementById('search-list');
  ul.innerHTML = '<li style="color:var(--text-3);padding:12px 14px">Searching GitHub…</li>';

  try {
    const results = await _api.searchGitHub(q);
    renderPluginList('search-list', results, true);
  } catch (err) {
    ul.innerHTML = `<li style="color:var(--text-3);padding:12px 14px">Search failed: ${esc(String(err))}</li>`;
  }
}

// Installed plugins 
export function loadPluginsInstalled() { _api.send({ type: 'list_plugins', id: uid() }); }

export function handlePluginList(msg) {
  const plugins = msg.plugins || [];
  const installed = plugins.filter(p => p.status !== 'available');
  const results   = plugins.filter(p => p.status === 'available');

  // Rebuild installed-ID + repo sets so cards can reflect state
  _installedIds.clear();
  _installedRepos.clear();
  for (const p of installed) {
    _installedIds.add(p.id);
    if (p.repo_url) _installedRepos.add(p.repo_url);
  }
  // Also mark plugins that are currently installing
  for (const id of _installingPlugins) _installedIds.add(id);

  if (results.length) renderPluginList('search-list', results, true);
  if (installed.length || !results.length) renderPluginList('installed-list', installed, false);

  // Refresh browse/search cards to show updated install state
  _refreshCardInstallButtons();

  // Check if any tracked installs have completed
  if (_installingPlugins.size > 0) {
    for (const id of [..._installingPlugins]) {
      const p = plugins.find(pl => pl.id === id);
      if (!p) continue;
      if (p.status === 'installed') {
        _installingPlugins.delete(id);
        _toast(`${p.name} installed successfully`, 'success');
      } else if (p.status === 'error') {
        _installingPlugins.delete(id);
        _toast(`${p.name} install failed${p.error_msg ? ': ' + p.error_msg : ''}`, 'error');
      }
    }
    if (_installingPlugins.size === 0 && _pollTimer) {
      clearInterval(_pollTimer);
      _pollTimer = null;
    }
  }
}

// Update install buttons on already-rendered browse/search cards without full re-render
function _refreshCardInstallButtons() {
  document.querySelectorAll('.ia-btn.install[data-plugin-id]').forEach(btn => {
    const id = btn.dataset.pluginId;
    const isInstalled = _installedIds.has(id);
    const isInstalling = _installingPlugins.has(id);
    if (isInstalling) {
      btn.textContent = 'Installing…'; btn.disabled = true;
    } else if (isInstalled) {
      btn.textContent = 'Installed'; btn.disabled = true; btn.classList.add('installed');
    } else {
      btn.textContent = 'Install'; btn.disabled = false; btn.classList.remove('installed');
    }
  });
}

// Plugin cards 
export function makePluginCard(p) {
  const li    = document.createElement('li');
  const isMcp = p.plugin_type === 'mcp';
  const cat   = detectCategory(p);
  li.className = `plugin-card ${isMcp ? 'mcp-card' : 'skill-card'}`;

 // Header: avatar + name/owner + type badge 
  const header = document.createElement('div');
  header.className = 'pc-header';

  const avatarWrap = document.createElement('div');
  avatarWrap.className = 'pc-avatar-wrap';

  if (p.owner_avatar) {
    const img = document.createElement('img');
    img.className = 'pc-avatar';
    img.src = p.owner_avatar;
    img.alt = '';
    img.loading = 'lazy';
    img.addEventListener('error', () => { img.replaceWith(makeAvatarFallback(p.name)); });
    avatarWrap.appendChild(img);
  } else {
    avatarWrap.appendChild(makeAvatarFallback(p.name));
  }

  const meta = document.createElement('div');
  meta.className = 'pc-meta';

  const nameBtn = document.createElement('button');
  nameBtn.className = 'pc-name-btn';
  nameBtn.textContent = p.name;
  nameBtn.title = 'View details';
  nameBtn.addEventListener('click', () => openPluginModal(p));

  const owner = document.createElement('span');
  owner.className = 'pc-owner';
  owner.textContent = p.full_name || p.id || '';

  meta.appendChild(nameBtn);
  meta.appendChild(owner);

  const typeBadge = document.createElement('span');
  typeBadge.className = `badge ${isMcp ? 'badge-mcp' : 'badge-skill'}`;
  typeBadge.textContent = isMcp ? 'MCP' : 'Skill';

  header.appendChild(avatarWrap);
  header.appendChild(meta);
  header.appendChild(typeBadge);

 // Description 
  const desc = document.createElement('p');
  desc.className = 'pc-desc';
  desc.textContent = p.description || 'No description available.';

 // Footer: cat + lang + stars | install 
  const footer = document.createElement('div');
  footer.className = 'pc-footer';

  const footerLeft = document.createElement('div');
  footerLeft.className = 'pc-footer-left';

  const catBadge = document.createElement('span');
  catBadge.className = 'badge badge-cat';
  catBadge.textContent = cat;
  footerLeft.appendChild(catBadge);

  if (p.language || p.stars) {
    const sep = document.createElement('span');
    sep.className = 'pc-sep';
    sep.textContent = '·';
    footerLeft.appendChild(sep);
  }

  if (p.language) {
    const color = LANG_COLORS[p.language] || '#94a3b8';
    const dot = document.createElement('span');
    dot.className = 'lang-dot';
    dot.style.background = color;
    footerLeft.appendChild(dot);
    const langLabel = document.createElement('span');
    langLabel.className = 'pc-lang';
    langLabel.textContent = p.language;
    footerLeft.appendChild(langLabel);
  }

  if (p.stars) {
    const stars = document.createElement('span');
    stars.className = 'pc-stars';
    stars.textContent = `★ ${p.stars >= 1000 ? (p.stars / 1000).toFixed(1) + 'k' : p.stars.toLocaleString()}`;
    footerLeft.appendChild(stars);
  }

  const isInstalled  = _installedIds.has(p.id) || _installedRepos.has(p.repo_url);
  const isInstalling = _installingPlugins.has(p.id);
  const installBtn = document.createElement('button');
  installBtn.className = 'ia-btn install';
  installBtn.dataset.pluginId = p.id;
  if (isInstalling) {
    installBtn.textContent = 'Installing…'; installBtn.disabled = true;
  } else if (isInstalled) {
    installBtn.textContent = 'Installed'; installBtn.disabled = true; installBtn.classList.add('installed');
  } else {
    installBtn.textContent = 'Install';
    installBtn.addEventListener('click', e => { e.stopPropagation(); handlePluginAction2('install', p.id, p); });
  }

  footer.appendChild(footerLeft);
  footer.appendChild(installBtn);

  li.appendChild(header);
  li.appendChild(desc);
  li.appendChild(footer);
  return li;
}

export function makeAvatarFallback(name) {
  const div = document.createElement('div');
  div.className = 'pc-avatar-fallback';
  div.textContent = (name || '?')[0].toUpperCase();
  return div;
}

// Row-style card for installed list
export function makeInstalledRow(p) {
  const li    = document.createElement('li');
  const isMcp = p.plugin_type === 'mcp';
  const enabled = p.enabled || p.status === 'installed';

  const name = document.createElement('span');
  name.className = 'iname';
  name.textContent = p.name;

  const typeBadge = document.createElement('span');
  typeBadge.className = `badge ${isMcp ? 'badge-mcp' : 'badge-skill'}`;
  typeBadge.textContent = isMcp ? 'MCP' : 'Skill';

  const desc = document.createElement('span');
  desc.className = 'idesc';
  desc.textContent = p.description || '';

  const actions = document.createElement('div');
  actions.className = 'iactions';
  const mcpAction = isMcp
    ? (p.running
        ? `<button class="ia-btn stop" data-id="${esc(p.id)}" data-action="stop-mcp">■ Stop</button>`
        : `<button class="ia-btn" data-id="${esc(p.id)}" data-action="start-mcp">▶ Start</button>`)
    : '';
  const runningDot = p.running ? `<span class="running-dot" title="Running"></span>` : '';
  actions.innerHTML = `
    ${runningDot}
    <button class="ia-btn" data-id="${esc(p.id)}" data-action="${enabled ? 'disable' : 'enable'}">${enabled ? 'Disable' : 'Enable'}</button>
    ${mcpAction}
    <button class="ia-btn danger" data-id="${esc(p.id)}" data-action="uninstall">Remove</button>`;
  actions.querySelectorAll('[data-action]').forEach(btn =>
    btn.addEventListener('click', e => { e.stopPropagation(); handlePluginAction2(btn.dataset.action, btn.dataset.id); })
  );

  li.appendChild(name);
  li.appendChild(typeBadge);
  li.appendChild(desc);
  li.appendChild(actions);
  return li;
}

export function renderPluginList(listId, plugins, isSearch) {
  const ul = document.getElementById(listId);
  ul.innerHTML = '';
  if (!plugins.length) {
    const msg = document.createElement('li');
    if (isSearch) { msg.className = 'grid-empty'; msg.textContent = 'No results found'; }
    else { msg.style.cssText = 'color:var(--text-3);padding:12px 14px'; msg.textContent = 'No plugins installed'; }
    ul.appendChild(msg);
    return;
  }
  for (const p of plugins) {
    ul.appendChild(isSearch ? makePluginCard(p) : makeInstalledRow(p));
  }
}

export function handlePluginAction2(action, pluginId, pluginData = null) {
  switch (action) {
    case 'install': {
      const msg = { type: 'install_plugin', id: uid(), plugin_id: pluginId };
      if (pluginData) {
        if (pluginData.name)          msg.name        = pluginData.name;
        if (pluginData.description)   msg.description = pluginData.description;
        msg.plugin_type               = pluginData.plugin_type || 'mcp';
        if (pluginData.repo_url)      msg.repo_url    = pluginData.repo_url;
        // Browse-list plugins use id = "owner/repo" (full_name) but don't set
        // owner/repo as separate fields - derive them from full_name or id.
        const fullName = pluginData.full_name || pluginData.id || '';
        const [derivedOwner, derivedRepo] = fullName.split('/');
        msg.owner = pluginData.owner || derivedOwner || '';
        msg.repo  = pluginData.repo  || derivedRepo  || '';
        if (pluginData.stars != null) msg.stars = pluginData.stars;
      }
      _api.send(msg);
      // Immediately mark the card button as installing
      const btn = document.querySelector(`.ia-btn.install[data-plugin-id="${CSS.escape(pluginId)}"]`);
      if (btn) { btn.textContent = 'Installing…'; btn.disabled = true; }
      break;
    }
    case 'enable':    _api.send({ type: 'set_plugin_enabled', id: uid(), plugin_id: pluginId, enabled: true  }); break;
    case 'disable':   _api.send({ type: 'set_plugin_enabled', id: uid(), plugin_id: pluginId, enabled: false }); break;
    case 'uninstall': _api.send({ type: 'uninstall_plugin',   id: uid(), plugin_id: pluginId }); break;
    case 'start-mcp': _api.send({ type: 'start_mcp', id: uid(), plugin_id: pluginId }); break;
    case 'stop-mcp':  _api.send({ type: 'stop_mcp',  id: uid(), plugin_id: pluginId }); break;
  }
}

export function handlePluginAction(msg) {
  if (msg.success) {
    loadPluginsInstalled();
    _toast(msg.message || `Plugin ${msg.action} started`, 'info');
    if (msg.action === 'install') _startInstallPoll(msg.plugin_id);
    // Spawn is async - refresh again after 1.5s so running state reflects reality
    if (msg.action === 'start_mcp' || msg.action === 'stop_mcp') {
      setTimeout(loadPluginsInstalled, 1500);
    }
  } else {
    _toast(`Plugin error: ${msg.message}`, 'error');
  }
}
