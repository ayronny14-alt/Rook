// ── Rook App ───────────────────────────────────────────────────────────────
import { uid, esc, escHtml, buildToolCard, buildThinkingCard, renderMarkdown, safeMarkdown } from './modules/util.js';
import { initPlugins, loadPluginsInstalled, loadBrowse, handlePluginList, handlePluginAction } from './modules/plugins.js';

const api = window.rook;


// ════════════════════════════════════════
// STATE
// ════════════════════════════════════════
let currentConvId = null;
let conversations  = [];
let streaming      = false;
let _convHasSentMessage = false; // tracks whether a message was sent in current conv (for memory panel empty state)
let streamBubble   = null;
let streamContent  = '';
let streamThinking = '';
let pendingApproval = null;
let currentMode    = localStorage.getItem('rook-mode') || 'Chat';
let _lastSettingsHash = '';
// In-flight chat id — needed so the Stop button can target the exact request.
// We track it here (rather than per-bubble) so a future "Cancel last" shortcut
// can hit the right id without DOM scraping.
let inFlightChatId = null;
// Conv that the active/last streaming response belongs to. Used to prevent
// chunks from a background stream corrupting whatever conv the user switched to.
let streamingConvId = null;
// Watchdog timer fired when the user clicks Stop. If the backend doesn't ack
// with `cancelled` within CANCEL_WATCHDOG_MS, we surface a clear warning so
// the user knows the cancel didn't take and can recover.
let cancelWatchdogTimer = null;
const CANCEL_WATCHDOG_MS = 5000;
const UPDATE_DISMISS_KEY  = 'rook-dismissed-update-v2'; // {version, until}
const UPDATE_DISMISS_MS   = 24 * 60 * 60 * 1000; // 24 hours

// Artifact tabs: [{id, name, content, type:'html'|'text'}]
let artifactTabs   = [];
let activeTabId    = null;
let currentAppVersion = null;
let currentUpdateState = null;

// ════════════════════════════════════════
// DOM
// ════════════════════════════════════════
const startupOverlay  = document.getElementById('startup-overlay');
// Status text node lives inside #startup-status now (after editorial redesign)
const startupStatus   = document.getElementById('startup-status-text') || document.getElementById('startup-status');
const startupLog      = document.getElementById('startup-log');
const appEl           = document.getElementById('app');

const messagesEl      = document.getElementById('messages');
const chatInput       = document.getElementById('chat-input');
const btnSend         = document.getElementById('btn-send');
const btnStop         = document.getElementById('btn-stop');
const mockBadge       = document.getElementById('mock-badge');
const memoryPanel     = document.getElementById('memory-panel');
const memoryPanelList = document.getElementById('memory-panel-list');
const memoryPanelEmpty= document.getElementById('memory-panel-empty');
const memoryPanelCount= document.getElementById('memory-panel-count');
const btnMemoryToggle = document.getElementById('btn-memory-toggle');
const btnMemoryClose  = document.getElementById('btn-memory-close');
const gnnChip         = document.getElementById('gnn-chip');
const btnNewChat      = document.getElementById('btn-new-chat');
const chatTitle       = document.getElementById('chat-title');
const convList        = document.getElementById('conversation-list');
const connDot         = document.getElementById('conn-dot');
const approvalStrip   = document.getElementById('approval-strip');
const approvalText    = document.getElementById('approval-text');
const btnApprove      = document.getElementById('btn-approve');
const btnReject       = document.getElementById('btn-reject');
const modelSelect     = document.getElementById('model-select');
const artifactPane    = document.getElementById('artifact-pane');
const artifactTabs_el = document.getElementById('artifact-tabs');
const artifactBody    = document.getElementById('artifact-body');
const btnCloseArtifact= document.getElementById('btn-close-artifact');
const inputHint       = document.getElementById('input-hint');
const btnModeToggle   = document.getElementById('btn-mode-toggle');
const activeModeLabel = document.getElementById('active-mode-label');
const modeMenu        = document.getElementById('mode-menu');

// Phase 7/9/14 additions
const ctxBarFill        = document.getElementById('ctx-bar-fill');
const emptyState        = document.getElementById('empty-state');
const reconnectBanner   = document.getElementById('reconnect-banner');
const btnReconnectNow   = document.getElementById('btn-reconnect-now');
const updateBanner      = document.getElementById('update-banner');
const btnUpdateNow      = document.getElementById('btn-update-now');
const btnUpdateDismiss  = document.getElementById('btn-update-dismiss');
const updateVersionLabel= document.getElementById('update-version-label');
const updateNotesEl     = document.getElementById('update-notes');
const updateProgressEl  = document.getElementById('update-progress-text');
const updateProgressWrap= document.getElementById('update-progress-wrap');
const updateProgressBar = document.getElementById('update-progress-bar');
const sidebarVersionTag = document.getElementById('sidebar-version-tag');
const settingsVersionLabel = document.getElementById('settings-version-label');
const sidebarSearch     = document.getElementById('sidebar-search');
const toastContainer    = document.getElementById('toast-container');
const cmdPaletteOverlay = document.getElementById('cmd-palette-overlay');
const cmdInput          = document.getElementById('cmd-input');
const cmdResultsEl      = document.getElementById('cmd-results');

// ════════════════════════════════════════
// STARTUP OVERLAY
// ════════════════════════════════════════
let backendReady = false;

// Restore conversations from localStorage immediately so the sidebar is
// populated before the backend connects (avoids a blank flash).
loadConvsFromStorage();

api.onBackendStart(() => {
  startupStatus.textContent = 'Starting backend';
});

api.onBackendLog((line) => {
  // keep last 8 lines
  const lines = startupLog.textContent.split('\n');
  lines.push(line);
  if (lines.length > 8) lines.splice(0, lines.length - 8);
  startupLog.textContent = lines.join('\n');
});

api.onPipeStatus((s) => {
  if (s.connected) {
    if (!backendReady) {
      backendReady = true;
      completeStep('step-backend');
      startupStatus.textContent = 'Connected';
      startupStatus.classList.add('connected');
      setTimeout(dismissStartup, 400);
    }
    // Resend saved settings on (re)connection — backend loses in-memory config on
    // restart. Skip if identical to what we already sent (avoids redundant IPC).
    const saved = JSON.parse(localStorage.getItem('rook-settings') || '{}');
    if (saved.apiUrl || saved.apiKey || saved.model || saved.platformApiKey) {
      const payload = JSON.stringify([saved.apiUrl, saved.apiKey, saved.model, saved.platformApiKey]);
      let h = 0; for (let i = 0; i < payload.length; i++) h = ((h << 5) - h + payload.charCodeAt(i)) | 0;
      const hash = h.toString(36);
      if (hash !== _lastSettingsHash) {
        api.send({ type: 'update_config', id: uid(), base_url: saved.apiUrl || '', api_key: saved.apiKey || '', model: saved.model || '', platform_url: PLATFORM_URL, platform_api_key: saved.platformApiKey || '' });
        _lastSettingsHash = hash;
      }
    }
    // Refresh platform profile + models from server (picks up plan changes, token resets, etc.)
    api.getPlatformUrl?.().then(url => {
      if (url) PLATFORM_URL = url;
      refreshPlatformSession();
    });
    // Sync conversation list from backend on every connect/reconnect
    api.send({ type: 'get_conversations', id: uid() });
  }

  // If we lost the connection mid-stream, unlock the input immediately
  if (!s.connected && streaming) {
    tokenQueue.length = 0; drainingTokens = false; pendingDoneMsg = null; streamThinking = '';
    sealStreamBubble({ tag: '(disconnected)' });
    finishCancelWatchdog();
    setStreaming(false);
    inFlightChatId = null;
  }

  // Connection dot + reconnect banner
  if (s.reconnecting) {
    connDot.className = 'reconnecting';
    connDot.title = `Reconnecting… (attempt ${s.attempt || 1})`;
    connDot.setAttribute('aria-label', `Reconnecting (attempt ${s.attempt || 1})`);
    reconnectBanner?.classList.remove('hidden');
    // Show a live countdown so the user knows exactly when the next attempt fires.
    if (s.retryDelayMs && s.retryDelayMs > 500) {
      const statusEl = document.getElementById('reconnect-status-text');
      if (statusEl) {
        let remaining = Math.ceil(s.retryDelayMs / 1000);
        statusEl.textContent = `Reconnecting in ${remaining}s (attempt ${s.attempt || 1})…`;
        const tick = setInterval(() => {
          remaining--;
          if (remaining <= 0 || !reconnectBanner || reconnectBanner.classList.contains('hidden')) {
            clearInterval(tick);
            if (statusEl) statusEl.textContent = 'Reconnecting…';
          } else {
            statusEl.textContent = `Reconnecting in ${remaining}s (attempt ${s.attempt || 1})…`;
          }
        }, 1000);
      }
    } else {
      const statusEl = document.getElementById('reconnect-status-text');
      if (statusEl) statusEl.textContent = `Reconnecting… (attempt ${s.attempt || 1})`;
    }
  } else if (s.connected) {
    connDot.className = 'online';
    connDot.title = 'Backend connected';
    connDot.setAttribute('aria-label', 'Backend connected');
    reconnectBanner?.classList.add('hidden');
    const statusEl = document.getElementById('reconnect-status-text');
    if (statusEl) statusEl.textContent = 'Reconnecting…';
    // Re-enable input after reconnect
    if (!streaming) {
      chatInput.disabled = false;
      chatInput.placeholder = 'Message Rook…';
    }
  } else {
    connDot.className = '';
    connDot.title = 'Backend offline';
    connDot.setAttribute('aria-label', 'Backend offline');
    reconnectBanner?.classList.remove('hidden');
    chatInput.placeholder = 'Backend offline — reconnecting…';
  }

  if (s.connected) {
    loadSkills();
    loadPluginsInstalled();
    loadPreviews();
    refreshBackendStatus();
  }
});

api.onUpdateState?.((state) => {
  currentUpdateState = state;
  renderUpdateBanner(state);
});

btnReconnectNow?.addEventListener('click', () => api.reconnectNow?.());

btnUpdateDismiss?.addEventListener('click', () => {
  if (!currentUpdateState?.latestVersion) return;
  const rec = JSON.stringify({ version: currentUpdateState.latestVersion, until: Date.now() + UPDATE_DISMISS_MS });
  localStorage.setItem(UPDATE_DISMISS_KEY, rec);
  renderUpdateBanner(currentUpdateState);
});

btnUpdateNow?.addEventListener('click', async () => {
  btnUpdateNow.disabled = true;
  try {
    await api.startUpdate?.();
  } catch (err) {
    btnUpdateNow.disabled = false;
    toast(`Update failed: ${err.message || err}`, 'error');
  }
});

// Ask the backend for status (mock mode) and memory capabilities (GNN status).
// Re-polled after settings changes so the UI stays accurate without a restart.
function refreshBackendStatus() {
  try {
    api.send({ type: 'health_check', id: uid() });
    api.send({ type: 'get_memory_capabilities', id: uid() });
  } catch (_) {}
}

function dismissStartup() {
  if (startupOverlay.classList.contains('fade-out')) return; // already dismissed
  startupOverlay.classList.add('fade-out');
  appEl.classList.remove('hidden');
  setTimeout(() => startupOverlay.remove(), 500);
  // Show onboarding prompt if user hasn't connected a platform account
  const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  if (!s.platformApiKey) {
    document.getElementById('onboarding-overlay')?.classList.remove('hidden');
  }
}

function dismissOnboarding() {
  document.getElementById('onboarding-overlay')?.classList.add('hidden');
}

// ── First-run setup display ──────────────────────────────────────────────────
const SETUP_STEPS = [
  { id: 'step-workspace', label: 'Creating workspace folder' },
  { id: 'step-config',    label: 'Writing default configuration' },
  { id: 'step-backend',   label: 'Starting backend engine' },
];

function renderSetupSteps() {
  const el = document.getElementById('startup-steps');
  if (!el) return;
  el.innerHTML = SETUP_STEPS.map(s =>
    `<div class="setup-step" id="${s.id}"><span class="step-icon">·</span><span>${s.label}</span></div>`
  ).join('');
  el.classList.remove('hidden');
}

function completeStep(id) {
  const el = document.getElementById(id);
  if (!el) return;
  el.classList.remove('active'); el.classList.add('done');
  el.querySelector('.step-icon').textContent = '✓';
}

function activateStep(id) {
  document.querySelectorAll('.setup-step').forEach(s => s.classList.remove('active'));
  const el = document.getElementById(id);
  if (el) { el.classList.add('active'); el.querySelector('.step-icon').textContent = '›'; }
}

api.onAppInit?.((data) => {
  if (!data?.firstRun) return;
  const marker = document.getElementById('startup-marker');
  if (marker) marker.textContent = '— FIRST-TIME SETUP';
  const statusText = document.getElementById('startup-status-text');
  if (statusText) statusText.textContent = 'Setting up Rook';

  renderSetupSteps();
  // Steps 1 & 2 are already done by main.js before window opened
  activateStep('step-workspace');
  setTimeout(() => { completeStep('step-workspace'); activateStep('step-config'); }, 300);
  setTimeout(() => { completeStep('step-config');    activateStep('step-backend'); }, 700);
  // step-backend completes when pipe connects (handled in onPipeStatus)
});

// Show the app after 1.5 s regardless — backend connects in background
setTimeout(dismissStartup, 1500);

document.getElementById('btn-onboard-signin')?.addEventListener('click', () => {
  dismissOnboarding();
  startPlatformSignIn(); // opens browser + starts UUID polling
});

document.getElementById('btn-onboard-skip')?.addEventListener('click', () => {
  dismissOnboarding();
  document.querySelector('[data-panel="settings"]')?.click();
});

// ════════════════════════════════════════
// WINDOW CONTROLS
// ════════════════════════════════════════
document.getElementById('btn-min').addEventListener('click',   () => api.minimize());
document.getElementById('btn-max').addEventListener('click',   () => api.maximize());
document.getElementById('btn-close').addEventListener('click', () => api.close());

Promise.all([
  api.getAppVersion?.(),
  api.isDevMode?.(),
  api.checkForUpdates?.(),
]).then(([version, isDev, updateState]) => {
  applyAppVersion(version, isDev);
  if (updateState) {
    currentUpdateState = updateState;
    renderUpdateBanner(updateState);
  }
}).catch(() => {});

// Mock badge is clickable — opens Settings so the user can enter an API key.
if (mockBadge) {
  mockBadge.style.cursor = 'pointer';
  mockBadge.title = 'Running without an API key — click to open Settings';
  mockBadge.addEventListener('click', () => switchPage('settings'));
}

// ════════════════════════════════════════
// PAGE NAVIGATION
// ════════════════════════════════════════
document.querySelectorAll('.ab-btn[data-page]').forEach(btn => {
  btn.addEventListener('click', () => switchPage(btn.dataset.page));
});

function switchPage(id) {
  document.querySelectorAll('.ab-btn[data-page]').forEach(b =>
    b.classList.toggle('active', b.dataset.page === id)
  );
  document.querySelectorAll('.page').forEach(p =>
    p.classList.toggle('active', p.id === `page-${id}`)
  );
  // Auto-load memory nodes when navigating to the memory page
  if (id === 'memory' && backendReady) {
    api.send({ type: 'query_memory', id: uid(), query: '', limit: 50 });
  }
}

function applyAppVersion(version, isDev) {
  currentAppVersion = version || currentAppVersion || '0.0.0';
  const shortVersion = `v${currentAppVersion}`;
  if (sidebarVersionTag) sidebarVersionTag.textContent = shortVersion;
  if (settingsVersionLabel) settingsVersionLabel.textContent = `${shortVersion} · ${isDev ? 'debug' : 'portable'}`;
}

function renderUpdateBanner(state) {
  if (!updateBanner || !state) return;

  const latestVersion = state.latestVersion || '';
  let isDismissed = false;
  try {
    const rec = JSON.parse(localStorage.getItem(UPDATE_DISMISS_KEY) || 'null');
    if (rec && rec.version === latestVersion && rec.until > Date.now()) isDismissed = true;
  } catch { localStorage.removeItem(UPDATE_DISMISS_KEY); }
  const isBusy = state.status === 'checking' || state.status === 'downloading' || state.status === 'installing';
  const shouldHide = !isBusy && (!state.updateAvailable || isDismissed);
  updateBanner.classList.toggle('hidden', shouldHide);
  if (shouldHide) return;

  if (updateVersionLabel) {
    updateVersionLabel.textContent = latestVersion ? `v${latestVersion}` : `v${state.currentVersion || currentAppVersion || '0.0.0'}`;
  }

  const notes = (state.notes || '').trim();
  if (updateNotesEl) {
    updateNotesEl.textContent = notes;
    updateNotesEl.classList.toggle('hidden', !notes);
  }

  let statusText = '';
  if (state.status === 'checking') statusText = 'Checking for updates…';
  else if (state.status === 'downloading') {
    const percent = state.progress?.percent;
    statusText = percent != null ? `Downloading update… ${percent}%` : 'Downloading update…';
  } else if (state.status === 'installing') statusText = 'Installing update… Rook will restart automatically.';
  else if (state.status === 'error' && state.error) statusText = `Update check failed: ${state.error}`;
  else if (state.updateAvailable && latestVersion) statusText = `Current version ${state.currentVersion || currentAppVersion || '0.0.0'} → ${latestVersion}`;

  if (updateProgressEl) {
    updateProgressEl.textContent = statusText;
    updateProgressEl.classList.toggle('hidden', !statusText);
  }

  const percent = state.progress?.percent;
  if (updateProgressWrap && updateProgressBar) {
    const showProgress = percent != null && (state.status === 'downloading' || state.status === 'installing');
    updateProgressWrap.classList.toggle('hidden', !showProgress);
    updateProgressBar.style.width = `${Math.max(0, Math.min(100, percent || 0))}%`;
  }

  if (btnUpdateNow) {
    btnUpdateNow.disabled = !state.updateAvailable || state.status === 'checking' || state.status === 'downloading' || state.status === 'installing';
    btnUpdateNow.textContent = state.status === 'installing' ? 'Restarting…'
      : state.status === 'downloading' ? 'Downloading…'
      : 'Update Now';
  }
  if (btnUpdateDismiss) {
    btnUpdateDismiss.disabled = state.status === 'downloading' || state.status === 'installing';
  }
}

// ════════════════════════════════════════
// BACKEND MESSAGES
// ════════════════════════════════════════
api.onMessage((msg) => {
  switch (msg.type) {
    case 'chat':                  finalizeChat(msg);              break;
    case 'context_curated':       renderMemoryPanel(msg.nodes || []); break;
    case 'user_facts_loaded':     renderUserFacts(msg.facts || []);   break;
    case 'chat_chunk':            handleChunk(msg.token);         break;
    case 'chat_thinking':         handleThinking(msg.thinking);   break;
    case 'chat_done':             finalizeChatDone(msg);          break;
    case 'session_todos':         renderSessionTodos(msg);        break;
    case 'pending_tool_approval': handleApproval(msg);            break;
    case 'memory_results':        renderMemoryList(msg.nodes);    break;
    case 'connected_nodes':       handleConnectedNodes(msg);      break;
    case 'skills_list':           renderSkills(msg.skills);       break;
    case 'previews':              handlePreviews(msg.previews);   break;
    case 'preview_created':       openArtifact(msg.name, msg.content || ''); break;
    case 'plugin_list':           handlePluginList(msg);          break;
    case 'plugin_action':         handlePluginAction(msg);        break;
    case 'gemma_launched':        handleGemmaLaunched(msg);       break;
    case 'config_updated':
      if (msg.success) appendMessage('system', '✓ API settings applied.');
      // Re-check mock-mode now that the key/model may have changed.
      refreshBackendStatus();
      break;
    case 'error':
      // Flush any pending stream state but PRESERVE whatever the user already
      // saw — silently dropping queued tokens after an error is the worst kind
      // of UX because users assume the response was complete.
      tokenQueue.length = 0; drainingTokens = false; pendingDoneMsg = null;
      sealStreamBubble({ tag: '(error — partial)' });
      appendMessage('system', formatError(msg.message));
      finishCancelWatchdog();
      setStreaming(false);
      inFlightChatId = null;
      break;
    case 'cancelled':
      // Backend has acknowledged the cancel. Keep the partial response in
      // the transcript with a clear "(cancelled)" tag instead of dropping it.
      tokenQueue.length = 0; drainingTokens = false; pendingDoneMsg = null;
      sealStreamBubble({ tag: '(cancelled — partial)' });
      finishCancelWatchdog();
      setStreaming(false);
      inFlightChatId = null;
      break;
    case 'conversation_list':
      handleConversationList(msg.conversations || []);
      break;
    case 'conversation_titled':
      updateConvTitle(msg.conversation_id, msg.title);
      break;
    case 'conversation_messages':
      handleConversationMessages(msg.conversation_id, msg.messages || []);
      break;
    case 'conversation_pinned': {
      const conv = conversations.find(c => c.id === msg.conversation_id);
      if (conv) { conv.pinned = msg.pinned; renderConvList(); saveConvsToStorage(); }
      break;
    }
    case 'conversation_deleted': {
      conversations = conversations.filter(c => c.id !== msg.conversation_id);
      if (currentConvId === msg.conversation_id) {
        currentConvId = null;
        clearMessages();
        chatTitle.textContent = 'New Conversation';
        ctxBarFill && (ctxBarFill.style.width = '0%');
      }
      renderConvList();
      saveConvsToStorage();
      break;
    }
    case 'conversation_renamed': {
      const conv = conversations.find(c => c.id === msg.conversation_id);
      if (conv) {
        conv.title = msg.title;
        renderConvList();
        saveConvsToStorage();
        if (msg.conversation_id === currentConvId) chatTitle.textContent = msg.title;
      }
      break;
    }
    case 'health_check': {
      // mock_mode is true when the backend has no API key. Surface a badge in
      // the header so users don't read placeholder text as a real model bug.
      if (mockBadge) {
        const wasMock = !mockBadge.classList.contains('hidden');
        mockBadge.classList.toggle('hidden', !msg.mock_mode);
        // First time transitioning into mock mode — prompt the user to add a key.
        if (msg.mock_mode && !wasMock) {
          toast('No API key set — responses are placeholders. Click MOCK or go to Settings to add a key.', 'warn', 10000);
        }
      }
      break;
    }
    case 'directory_indexed': {
      document.getElementById('index-folder-status')?.classList.add('done');
      const folderName = (msg.path || '').split(/[\\/]/).filter(Boolean).pop() || msg.path;
      toast(`Indexed ${msg.files_indexed} file${msg.files_indexed === 1 ? '' : 's'} in "${folderName}"`, 'info');
      break;
    }
    case 'memory_capabilities':
      if (gnnChip) {
        const on = !!msg.gnn_available;
        gnnChip.textContent = on ? 'GNN on' : 'GNN off';
        gnnChip.className   = on ? 'gnn-on' : 'gnn-off';
        gnnChip.title = on
          ? `GraphSAGE active · ${msg.node_count} nodes · ${msg.edge_count} edges · ${msg.embedding_count} embeddings`
          : `GraphSAGE unavailable (Python/script missing) · ${msg.node_count} nodes · semantic-only ranking`;
        gnnChip.classList.remove('hidden');
      }
      break;
  }
});

// Seal whatever the streaming bubble currently shows, mark it cancelled/errored,
// and detach it from the active stream so the next request starts a fresh one.
function sealStreamBubble({ tag }) {
  if (!streamBubble) return;
  const bubble = streamBubble.querySelector('.bubble');
  bubble.classList.remove('cursor', 'agent-running');
  bubble.classList.add('cancelled');
  bubble.innerHTML = safeMarkdown(streamContent || '');
  if (tag) {
    const tagEl = document.createElement('span');
    tagEl.className = 'cancelled-tag';
    tagEl.textContent = tag;
    bubble.appendChild(tagEl);
  }
  if (streamContent) saveAssistantMsg(streamContent + '\n\n_' + (tag || '') + '_', currentConvId);
  streamBubble = null;
  streamContent = '';
}

function finishCancelWatchdog() {
  if (cancelWatchdogTimer) {
    clearTimeout(cancelWatchdogTimer);
    cancelWatchdogTimer = null;
  }
  if (btnStop) {
    btnStop.disabled = false;
    btnStop.classList.add('hidden');
  }
  if (btnSend) btnSend.classList.remove('hidden');
}

// ════════════════════════════════════════
let tokenQueue = [];
let drainingTokens = false;
let pendingDoneMsg = null;

function renderSessionTodos(msg) {
  const widget = document.getElementById('todo-widget');
  const listEl = document.getElementById('todo-widget-list');
  const countEl = document.getElementById('todo-widget-count');
  if (!widget || !listEl) return;

  const todos = Array.isArray(msg.todos) ? msg.todos : [];
  if (todos.length === 0) {
    widget.classList.add('hidden');
    listEl.innerHTML = '';
    return;
  }
  widget.classList.remove('hidden');

  const done = todos.filter(t => t.status === 'completed').length;
  if (countEl) countEl.textContent = `${done}/${todos.length}`;

  listEl.innerHTML = todos.map(t => {
    const status = t.status || 'pending';
    const label = (status === 'in_progress' && t.activeForm) ? t.activeForm : (t.content || '');
    const icon = status === 'completed' ? '✓'
               : status === 'in_progress' ? '◌'
               : '·';
    return `<li class="todo-item todo-${esc(status)}"><span class="todo-icon">${icon}</span><span class="todo-text">${esc(label)}</span></li>`;
  }).join('');

  // Open memory panel automatically the first time we get a todo list so the
  // user actually notices the agent has started a plan.
  if (memoryPanel && memoryPanel.classList.contains('hidden')) {
    memoryPanel.classList.remove('hidden');
  }
}

function handleThinking(thinking) {
  // Ensure a stream bubble exists (thinking always precedes content)
  if (!streamBubble) {
    streamContent = '';
    streamThinking = '';
    streamBubble = appendMessage('assistant', '');
    streamBubble.querySelector('.bubble').classList.add('cursor');
    setStreaming(true);
  }
  // Accumulate thinking text — drainTokenQueue will prepend it each frame
  // so there is always exactly one thought card, never multiples.
  streamThinking += thinking;
  const bub = streamBubble.querySelector('.bubble');
  bub.innerHTML = safeMarkdown(buildThinkingCard(streamThinking) + renderMarkdown(streamContent));
  scrollBottom();
}

function handleChunk(token) {
  // If the user switched conversations while this stream is running, discard
  // the visual — _finalizeChatDone will still save the content to the right conv.
  if (streamingConvId !== null && streamingConvId !== currentConvId) return;
  tokenQueue.push(token);
  if (!drainingTokens) drainTokenQueue();
}

function drainTokenQueue() {
  if (tokenQueue.length === 0) {
    drainingTokens = false;
    if (pendingDoneMsg) {
      const msg = pendingDoneMsg;
      pendingDoneMsg = null;
      _finalizeChatDone(msg);
    }
    return;
  }
  drainingTokens = true;

  // Drain ALL queued tokens this tick — accumulating text is cheap, only
  // the DOM update (innerHTML) is expensive, so we want to batch.
  let appended = '';
  while (tokenQueue.length > 0) appended += tokenQueue.shift();

  if (!streamBubble) {
    streamContent = '';
    streamThinking = '';
    streamBubble = appendMessage('assistant', '');
    streamBubble.querySelector('.bubble').classList.add('cursor');
    setStreaming(true);
  }
  streamContent += appended;

  // Re-render the whole bubble. Thinking card is prepended if present so it
  // survives every re-render without spawning duplicates.
  const bub = streamBubble.querySelector('.bubble');
  bub.innerHTML = safeMarkdown((streamThinking ? buildThinkingCard(streamThinking) : '') + renderMarkdown(streamContent));

  // Pulsing dot: visible when the last streamed token closed a [[TOOL:...]] or [[TERR:...]]
  // card — i.e., the stream is now silent while the backend executes the tool.
  // trimEnd() removes any trailing \n so we only need one endsWith check.
  bub.classList.toggle('agent-running', streamContent.trimEnd().endsWith(']]'));

  scrollBottom();

  // Yield to the browser; if more tokens arrived while we were rendering,
  // drain them on the next frame.
  requestAnimationFrame(drainTokenQueue);
}

function finalizeChatDone(msg) {
  // If the queue still has tokens, defer until it's drained
  if (drainingTokens || tokenQueue.length > 0) {
    pendingDoneMsg = msg;
    return;
  }
  _finalizeChatDone(msg);
}

function applyHighlighting(el) {
  if (typeof hljs === 'undefined') return;
  el.querySelectorAll('pre code').forEach(block => {
    try { hljs.highlightElement(block); } catch (_) {}
  });
}

function _finalizeChatDone(msg) {
  const finalContent = streamContent;
  const wasForCurrentConv = (streamingConvId === null || streamingConvId === msg.conversation_id)
    && (currentConvId === null || currentConvId === msg.conversation_id);

  if (streamBubble) {
    if (wasForCurrentConv) {
      const b = streamBubble.querySelector('.bubble');
      b.classList.remove('cursor', 'agent-running');
      b.innerHTML = safeMarkdown((streamThinking ? buildThinkingCard(streamThinking) : '') + renderMarkdown(finalContent));
      applyHighlighting(b);
      if (msg.usage) { addTokenCount(streamBubble, msg.usage); updateCtxBar(msg.usage); }
      markLastAssistant();
    } else {
      // User switched conversations mid-stream — remove the orphaned bubble and
      // save the response quietly to the original conversation.
      streamBubble.remove();
      const origConv = conversations.find(c => c.id === msg.conversation_id);
      if (origConv) {
        showToast(`Response saved to "${origConv.title || 'conversation'}"`, 3500);
      }
    }
    streamBubble = null; streamThinking = '';
  }
  streamContent = '';

  // Always persist the assistant reply to the correct conversation.
  saveAssistantMsg(finalContent, msg.conversation_id);

  // Only take over currentConvId if the user hasn't navigated away.
  if (currentConvId === null || currentConvId === streamingConvId) {
    currentConvId = msg.conversation_id;
  }
  streamingConvId = null;

  // Clear any pending cancel watchdog — chat is done, no warning needed.
  finishCancelWatchdog();
  setStreaming(false);
}

function finalizeChat(msg) {
  tokenQueue.length = 0; drainingTokens = false; pendingDoneMsg = null;
  if (streamBubble) { streamBubble.querySelector('.bubble').classList.remove('cursor', 'agent-running'); streamBubble = null; streamContent = ''; streamThinking = ''; }

  // Only append to the UI if this response belongs to the currently viewed conv.
  const isCurrent = currentConvId === null || currentConvId === msg.conversation_id;
  if (isCurrent) {
    const wrap = appendMessage('assistant', msg.content || '');
    applyHighlighting(wrap);
    if (msg.usage) { addTokenCount(wrap, msg.usage); updateCtxBar(msg.usage); }
    markLastAssistant();
  }
  saveAssistantMsg(msg.content || '', msg.conversation_id);
  // Only take over currentConvId if the user hasn't navigated away.
  if (currentConvId === null || currentConvId === streamingConvId) {
    currentConvId = msg.conversation_id;
  }
  streamingConvId = null;
  // Clear any pending cancel watchdog — chat completed normally.
  finishCancelWatchdog();
  setStreaming(false);
}

/** Map raw backend error strings to user-friendly messages with actionable hints. */
function formatError(raw) {
  const s = String(raw || 'Unknown error');
  const lo = s.toLowerCase();

  if (/not signed in to svrn/i.test(s))
    return `⚠ Sign in to SVRN to use this model — open Settings and click Sign In.`;
  if (/401|unauthorized|invalid.{0,10}api.{0,5}key/i.test(s))
    return `⚠ API key rejected — check your key in Settings.`;
  if (/429|rate.?limit|too many requests/i.test(s))
    return `⚠ Rate limit reached — wait a moment, then retry.`;
  if (/timed? ?out|timeout|deadline/i.test(s))
    return `⚠ Request timed out — the model may be slow or your message too long.`;
  if (/connection refused|econnrefused/i.test(s) && lo.includes('ollama'))
    return `⚠ Ollama not running — start it with \`ollama serve\`, then retry.`;
  if (/connection refused|econnrefused/i.test(s))
    return `⚠ Connection refused — check that the backend is running.`;
  if (/context.{0,10}length|max.{0,5}tokens|too.{0,10}long/i.test(s))
    return `⚠ Message too long for this model — start a new conversation or shorten your input.`;
  if (/insufficient.{0,10}quota|billing|payment/i.test(s))
    return `⚠ Account quota exceeded — check your billing details with your API provider.`;
  if (/model.{0,10}not.{0,10}found|no such model/i.test(s))
    return `⚠ Model not found — check the model name in Settings.`;

  return `⚠ ${s}`;
}

function addTokenCount(wrap, usage) {
  const el = document.createElement('div');
  el.className = 'token-count';
  const fmt = n => n >= 1000 ? (n / 1000).toFixed(1) + 'k' : String(n);
  el.textContent = `↑ ${fmt(usage.prompt_tokens)} · ↓ ${fmt(usage.completion_tokens)}`;
  wrap.appendChild(el);
}

// Static hint text — restored when streaming ends
const HINT_DEFAULT = inputHint ? inputHint.innerHTML : 'Enter · send &nbsp; Shift+Enter · newline';

function setStreaming(v) {
  const wasStreaming = streaming;
  streaming = v;
  btnSend.disabled = v;
  chatInput.disabled = v;
  // Swap Send <-> Stop in the input bar so the user always has a working
  // affordance for the current state. Stop is hidden when not streaming so
  // it can't be clicked against a stale request id.
  if (btnStop) btnStop.classList.toggle('hidden', !v);
  if (btnSend) btnSend.classList.toggle('hidden', v);
  if (!v) {
    inFlightChatId = null;
    userScrolledUp = false; // re-enable auto-scroll for next turn
    // Restore hint text
    if (inputHint) inputHint.innerHTML = HINT_DEFAULT;
    // Desktop notification if window is not focused
    if (wasStreaming && !document.hasFocus()) {
      const conv  = conversations.find(c => c.id === currentConvId);
      const title = conv?.title || 'Rook';
      api.showNotification?.(title, 'Agent finished — click to view');
    }
  } else {
    // Show agent-working hint while streaming
    if (inputHint) inputHint.textContent = 'Agent working… Esc to stop';
  }
}

// Stop button: send a cancel_request targeting the in-flight chat id, lock the
// button immediately so the user can't double-click, and start a watchdog. The
// backend acknowledges with `cancelled` (handled above), which clears the
// watchdog. If the ack never arrives we surface a clear failure.
if (btnStop) {
  btnStop.addEventListener('click', () => {
    if (!inFlightChatId) return;
    btnStop.disabled = true;
    btnStop.title = 'Cancelling…';
    // Immediately drain the queue so rendering stops and the bubble freezes.
    // The backend cancel is still sent and the watchdog handles final cleanup.
    tokenQueue.length = 0;
    drainingTokens = false;
    if (streamBubble) {
      const bub = streamBubble.querySelector('.bubble');
      bub.classList.remove('agent-running');
      if (streamThinking || streamContent) {
        bub.innerHTML = safeMarkdown((streamThinking ? buildThinkingCard(streamThinking) : '') + renderMarkdown(streamContent)) + '<span class="cancel-pill">cancelling…</span>';
      }
      // Auto-clear the "cancelling…" pill after 2s even if backend hasn't acked yet
      setTimeout(() => {
        bub.querySelector?.('.cancel-pill')?.remove();
      }, 2000);
    }
    api.send({ type: 'cancel_request', id: uid(), target_id: inFlightChatId });
    if (cancelWatchdogTimer) clearTimeout(cancelWatchdogTimer);
    cancelWatchdogTimer = setTimeout(() => {
      // Backend never acknowledged. Tell the user honestly and let them
      // decide whether to keep waiting or treat the response as broken.
      appendMessage('system', '⚠ Cancel did not complete within 5s — backend may still be running. The next request will start fresh.');
      sealStreamBubble({ tag: '(cancel timed out — partial)' });
      setStreaming(false);
      inFlightChatId = null;
      cancelWatchdogTimer = null;
    }, CANCEL_WATCHDOG_MS);
  });
}

// ════════════════════════════════════════
// TOOL APPROVAL
// ════════════════════════════════════════
const AUTO_APPROVE_TOOLS = new Set([
  'file_read', 'list_files', 'get_cwd', 'glob', 'grep', 'search_in_file',
  'outline_file', 'file_diff', 'git_status', 'git_diff', 'git_log', 'git_branch',
  'shell_list', 'shell_read', 'todo_write',
]);

function handleApproval(msg) {
  const diffs = msg.diffs || [];
  const toolNames = diffs.map(d => d.tool_name || '');

  // Auto-approve if ALL pending tools are read-only
  const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  if (s.autoApproveReads !== false && toolNames.every(n => AUTO_APPROVE_TOOLS.has(n))) {
    api.send({ type: 'approve_pending_tools', id: uid(), conversation_id: msg.conversation_id, approved: true });
    return;
  }

  pendingApproval = { id: msg.id, conversation_id: msg.conversation_id };

  // Build diff preview
  const n = diffs.length;
  let html = `<span class="approval-count">${n} tool call${n !== 1 ? 's' : ''} awaiting approval</span>`;
  if (diffs.some(d => d.diff)) {
    html += '<details class="approval-diffs"><summary class="approval-diff-toggle">Show diffs</summary><div class="approval-diff-body">';
    for (const d of diffs) {
      if (d.diff) {
        html += `<div class="approval-diff-file"><div class="approval-diff-label">${escHtml(d.path || d.tool_name || 'unknown')}</div><pre class="approval-diff-pre">${escHtml(d.diff)}</pre></div>`;
      }
    }
    html += '</div></details>';
  }
  approvalText.innerHTML = html;
  approvalStrip.classList.remove('hidden');
  document.getElementById('approval-badge')?.classList.remove('hidden');
  setStreaming(false);
}
function clearApprovalBadge() {
  document.getElementById('approval-badge')?.classList.add('hidden');
}
btnApprove.addEventListener('click', () => {
  if (!pendingApproval) return;
  api.send({ type: 'approve_pending_tools', id: uid(), conversation_id: pendingApproval.conversation_id, approved: true });
  approvalStrip.classList.add('hidden');
  clearApprovalBadge();
  setStreaming(true);
  pendingApproval = null;
});
btnReject.addEventListener('click', () => {
  if (!pendingApproval) return;
  api.send({ type: 'approve_pending_tools', id: uid(), conversation_id: pendingApproval.conversation_id, approved: false });
  approvalStrip.classList.add('hidden');
  clearApprovalBadge();
  pendingApproval = null;
});

// ════════════════════════════════════════
// SEND MESSAGE
// ════════════════════════════════════════
function sendMessage() {
  const text = chatInput.value.trim();
  const hasImages = pendingImages.length > 0;
  if ((!text && !hasImages) || streaming) return;

  const displayText = text || (hasImages ? `[${pendingImages.length} image${pendingImages.length > 1 ? 's' : ''} attached]` : '');
  const sendText    = text || '(Describe the attached image)';

  switchPage('chat');

  // Append user bubble (with images if any)
  const userWrap = appendMessage('user', displayText);
  if (hasImages && userWrap) {
    const strip = document.createElement('div');
    strip.className = 'msg-img-strip';
    for (const img of pendingImages) {
      const el = document.createElement('img');
      el.className = 'msg-img-thumb'; el.src = img.dataUrl; el.alt = img.name; el.title = img.name;
      strip.appendChild(el);
    }
    userWrap.querySelector?.('.bubble')?.appendChild(strip);
  }

  chatInput.value = '';
  autoResize();

  // Capture and clear images
  const imageUrls = pendingImages.map(i => i.dataUrl);
  pendingImages = [];
  renderImgStrip();

  let conv = conversations.find(c => c.id === currentConvId);
  if (!conv) {
    conv = { id: null, title: (text || 'Image').slice(0, 48), messages: [] };
    conversations.unshift(conv);
    renderConvList();
  }
  conv.messages.push({ role: 'user', content: displayText });
  saveConvsToStorage();

  _convHasSentMessage = true;
  const chatId = uid();
  inFlightChatId = chatId;
  streamingConvId = currentConvId; // track which conv this stream belongs to
  const payload = { type: 'chat', id: chatId, conversation_id: currentConvId || undefined, message: sendText, model: modelSelect.value || undefined, agent_mode: currentMode };
  if (imageUrls.length) payload.images = imageUrls;
  // System prompt + model params from settings
  const sysPr = document.getElementById('s-system-prompt')?.value?.trim();
  if (sysPr) payload.system_prompt = sysPr;
  const tempSlider = document.getElementById('s-temperature');
  const tempVal = parseFloat(tempSlider?.value ?? '0.7');
  if (!isNaN(tempVal)) payload.temperature = tempVal;
  const maxTokSlider = document.getElementById('s-max-tokens');
  const maxTokVal = parseInt(maxTokSlider?.value ?? '4096', 10);
  if (!isNaN(maxTokVal)) payload.max_tokens = maxTokVal;
  api.send(payload);
  setStreaming(true);
}

btnSend.addEventListener('click', sendMessage);
chatInput.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendMessage(); } });
chatInput.addEventListener('input', autoResize);
// Safety: if the input is disabled but there's no active stream, re-enable on click
chatInput.addEventListener('mousedown', () => { if (chatInput.disabled && !streaming) { chatInput.disabled = false; chatInput.focus(); } });

function autoResize() {
  chatInput.style.height = 'auto';
  chatInput.style.height = Math.min(chatInput.scrollHeight, 160) + 'px';
}

// ════════════════════════════════════════
// AGENT MODE DROPDOWN
// ════════════════════════════════════════
btnModeToggle.addEventListener('click', (e) => {
  e.stopPropagation();
  modeMenu.classList.toggle('hidden');
  const open = !modeMenu.classList.contains('hidden');
  btnModeToggle.classList.toggle('open', open);
  btnModeToggle.setAttribute('aria-expanded', String(open));
});

// Restore persisted mode on startup
{
  const savedMode = localStorage.getItem('rook-mode') || 'Chat';
  const savedBtn = document.querySelector(`.mode-item[data-mode="${savedMode}"]`);
  if (savedBtn) {
    currentMode = savedMode;
    activeModeLabel.textContent = savedMode;
    document.querySelectorAll('.mode-icon').forEach(i => i.classList.remove('active'));
    const icon = document.getElementById('mode-icon-' + savedMode.toLowerCase());
    if (icon) icon.classList.add('active');
    document.querySelectorAll('.mode-item').forEach(b => b.classList.remove('active'));
    savedBtn.classList.add('active');
  }
}

document.querySelectorAll('.mode-item').forEach(btn => {
  btn.addEventListener('click', () => {
    currentMode = btn.dataset.mode;
    localStorage.setItem('rook-mode', currentMode);
    activeModeLabel.textContent = currentMode;
    // Update icon visibility in trigger button
    document.querySelectorAll('.mode-icon').forEach(i => i.classList.remove('active'));
    const icon = document.getElementById('mode-icon-' + currentMode.toLowerCase());
    if (icon) icon.classList.add('active');
    // Update active state in menu
    document.querySelectorAll('.mode-item').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    modeMenu.classList.add('hidden');
    btnModeToggle.classList.remove('open');
    btnModeToggle.setAttribute('aria-expanded', 'false');
    btnModeToggle.setAttribute('aria-label', `Switch agent mode: ${currentMode}`);
  });
});

document.addEventListener('click', (e) => {
  if (!e.target.closest('#mode-dropdown-wrap')) {
    modeMenu.classList.add('hidden');
    btnModeToggle.classList.remove('open');
    btnModeToggle.setAttribute('aria-expanded', 'false');
  }
});

// ════════════════════════════════════════
// CONVERSATIONS
// ════════════════════════════════════════
btnNewChat.addEventListener('click', () => {
  currentConvId = null;
  window._memoryPanelSeenConv = null;
  _convHasSentMessage = false;
  clearMessages();
  pendingImages = []; renderImgStrip();
  chatTitle.textContent = 'New Conversation';
  approvalStrip.classList.add('hidden');
  clearApprovalBadge?.();
  setStreaming(false);
  ctxBarFill && (ctxBarFill.style.width = '0%');
  chatInput.focus();
  document.querySelectorAll('#conversation-list li').forEach(l => l.classList.remove('active'));
  switchPage('chat');
});

function saveAssistantMsg(content, convId) {
  const conv = conversations.find(c => c.id === currentConvId || c.id === convId);
  if (conv) { conv.id = convId; conv.messages.push({ role: 'assistant', content }); currentConvId = convId; renderConvList(); saveConvsToStorage(); }
}

// renderConvList is defined at the bottom of this file (with pin/star support).

// ════════════════════════════════════════
// MESSAGES
// ════════════════════════════════════════
function appendMessage(role, content) {
  hideEmptyState();
  const wrap = document.createElement('div');
  wrap.className = `message ${role}`;
  if (role === 'assistant') {
    const row = document.createElement('div'); row.className = 'msg-row';
    const av  = document.createElement('div'); av.className = 'avatar'; /* mark via CSS ::before */
    const bub = document.createElement('div'); bub.className = 'bubble';
    if (content) {
      bub.innerHTML = safeMarkdown(content);
      // Apply syntax highlighting for pre-loaded / non-streaming messages
      applyHighlighting(bub);
    }
    row.appendChild(av); row.appendChild(bub); wrap.appendChild(row);
    // Regenerate + copy-message button row (shown on hover)
    const regenRow = document.createElement('div');
    regenRow.className = 'regen-row';
    regenRow.innerHTML = `
      <button class="copy-msg-btn" title="Copy response">⎘</button>
      <button class="regen-btn" title="Regenerate response">
        <svg viewBox="0 0 16 16" fill="none"><path d="M13 8A5 5 0 113.5 5M3 2v3h3" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"/></svg>
        Regenerate
      </button>`;
    regenRow.querySelector('.regen-btn').addEventListener('click', () => {
      if (!currentConvId || streaming) return;
      api.send({ type: 'regenerate_last_message', id: uid(), conversation_id: currentConvId });
      // Remove this message from the DOM — the regenerated one will take its place
      wrap.remove();
      setStreaming(true);
    });
    regenRow.querySelector('.copy-msg-btn').addEventListener('click', () => {
      const text = bub.innerText || bub.textContent || '';
      navigator.clipboard.writeText(text.trim()).then(() => {
        const btn = regenRow.querySelector('.copy-msg-btn');
        btn.textContent = '✓ Copied!';
        btn.classList.add('copied');
        setTimeout(() => { btn.textContent = '⎘'; btn.classList.remove('copied'); }, 1500);
      }).catch(() => {
        const btn = regenRow.querySelector('.copy-msg-btn');
        btn.textContent = '!';
        setTimeout(() => btn.textContent = '⎘', 1500);
      });
    });
    wrap.appendChild(regenRow);
  } else if (role === 'user') {
    const bub = document.createElement('div'); bub.className = 'bubble';
    if (content) bub.textContent = content;
    wrap.appendChild(bub);
    // Inline edit button
    const editBtn = document.createElement('button');
    editBtn.className = 'msg-edit-btn'; editBtn.title = 'Edit and resend';
    editBtn.innerHTML = '<svg viewBox="0 0 16 16" fill="none" width="12" height="12"><path d="M11 2l3 3-8 8H3v-3l8-8z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg>';
    editBtn.addEventListener('click', () => startEditUserMessage(wrap, bub, content));
    wrap.appendChild(editBtn);
  } else {
    const bub = document.createElement('div'); bub.className = 'bubble';
    if (content) bub.innerHTML = safeMarkdown(content);
    wrap.appendChild(bub);
  }
  messagesEl.appendChild(wrap);
  scrollBottom();
  return wrap;
}
// ── Scroll-lock: don't fight the user's scroll during streaming ──────────────
let userScrolledUp = false;

// Create the scroll-to-bottom anchor button
const scrollAnchorBtn = document.createElement('button');
scrollAnchorBtn.id = 'scroll-anchor';
scrollAnchorBtn.className = 'hidden';
scrollAnchorBtn.title = 'Jump to bottom';
scrollAnchorBtn.textContent = '↓';
document.body.appendChild(scrollAnchorBtn);

{ let _scrollRaf = false;
  messagesEl.addEventListener('scroll', () => {
    if (_scrollRaf) return;
    _scrollRaf = true;
    requestAnimationFrame(() => {
      _scrollRaf = false;
      const gap = messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight;
      const atBottom = gap < 80;
      if (streaming) userScrolledUp = !atBottom;
      scrollAnchorBtn.classList.toggle('hidden', atBottom);
    });
  });
}

scrollAnchorBtn.addEventListener('click', () => {
  userScrolledUp = false;
  messagesEl.scrollTop = messagesEl.scrollHeight;
  scrollAnchorBtn.classList.add('hidden');
});

function scrollBottom() {
  if (!userScrolledUp) {
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }
}

// Copy button for code blocks — delegated so it works on dynamically rendered content
messagesEl.addEventListener('click', e => {
  const btn = e.target.closest('.copy-btn');
  if (!btn) return;
  const code = btn.closest('.code-wrap')?.querySelector('code');
  if (!code) return;
  navigator.clipboard.writeText(code.textContent).then(() => {
    btn.textContent = '✓';
    setTimeout(() => btn.textContent = '⎘', 1800);
  }).catch(() => {
    btn.textContent = '!';
    setTimeout(() => btn.textContent = '⎘', 1800);
  });
});

document.getElementById('btn-export-chat')?.addEventListener('click', exportConversation);
function exportConversation() {
  const conv = conversations.find(c => c.id === currentConvId);
  if (!conv?.messages?.length) { toast('Nothing to export — start a conversation first.', 'warn'); return; }
  try {
    const lines = [`# ${conv.title || 'Conversation'}\n`];
    for (const m of conv.messages) {
      const who = m.role === 'user' ? 'You' : 'Rook';
      lines.push(`**${who}**\n\n${m.content}`);
    }
    const blob = new Blob([lines.join('\n\n---\n\n')], { type: 'text/markdown' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `${(conv.title || 'conversation').replace(/[^a-z0-9]/gi, '_').toLowerCase()}.md`;
    a.click();
    URL.revokeObjectURL(a.href);
    toast('Conversation exported.', 'info');
  } catch (err) {
    toast(`Export failed: ${err.message || err}`, 'error');
  }
}

// ════════════════════════════════════════
// MARKDOWN
// ════════════════════════════════════════


// ════════════════════════════════════════
// ARTIFACT VIEWER
// ════════════════════════════════════════
function openArtifact(name, content) {
  const id = uid();
  const type = /^\s*<(!DOCTYPE|html)/i.test(content) ? 'html' : 'text';
  artifactTabs.push({ id, name, content, type });
  renderArtifactTabs();
  setActiveTab(id);
  artifactPane.classList.remove('closed');
}

function renderArtifactTabs() {
  artifactTabs_el.innerHTML = '';
  for (const t of artifactTabs) {
    const btn = document.createElement('button');
    btn.className = `atab${t.id === activeTabId ? ' active' : ''}`;
    btn.innerHTML = `<span>${esc(t.name)}</span>
      <span class="close-tab" data-id="${t.id}" title="Close tab">&#10005;</span>`;
    btn.addEventListener('click', (e) => {
      if (e.target.classList.contains('close-tab')) closeTab(e.target.dataset.id);
      else setActiveTab(t.id);
    });
    artifactTabs_el.appendChild(btn);
  }
}

function setActiveTab(id) {
  activeTabId = id;
  // render tab content
  artifactBody.innerHTML = '';
  const tab = artifactTabs.find(t => t.id === id);
  if (!tab) return;
  renderArtifactTabs();

  if (tab.type === 'html') {
    const iframe = document.createElement('iframe');
    iframe.className = 'artifact-frame active';
    iframe.sandbox = 'allow-scripts allow-forms allow-modals';
    iframe.srcdoc = tab.content;
    artifactBody.appendChild(iframe);
  } else {
    const pre = document.createElement('div');
    pre.className = 'artifact-code active';
    pre.textContent = tab.content;
    artifactBody.appendChild(pre);
  }
}

function closeTab(id) {
  artifactTabs = artifactTabs.filter(t => t.id !== id);
  if (activeTabId === id) {
    activeTabId = artifactTabs.length ? artifactTabs[artifactTabs.length - 1].id : null;
    if (activeTabId) setActiveTab(activeTabId);
    else { artifactBody.innerHTML = ''; artifactPane.classList.add('closed'); }
  } else {
    renderArtifactTabs();
  }
  if (!artifactTabs.length) artifactPane.classList.add('closed');
}

btnCloseArtifact.addEventListener('click', () => {
  artifactPane.classList.add('closed');
});

// Handle previews from backend (each preview becomes an artifact tab)
function handlePreviews(previews) {
  if (!previews?.length) return;
  for (const p of previews) {
    if (!artifactTabs.find(t => t.name === p.name)) {
      openArtifact(p.name, p.content || '');
    }
  }
}

function loadPreviews() { api.send({ type: 'get_previews', id: uid() }); }

// ════════════════════════════════════════
// MEMORY PAGE
// ════════════════════════════════════════
document.getElementById('btn-mem-search').addEventListener('click', searchMemory);
document.getElementById('mem-query').addEventListener('keydown', e => { if (e.key === 'Enter') searchMemory(); });

function searchMemory() {
  const q = document.getElementById('mem-query').value.trim();
  const list = document.getElementById('memory-list');
  list.innerHTML = '<li class="mem-list-loading">Searching…</li>';
  api.send({ type: 'query_memory', id: uid(), query: q, limit: 50 });
}

// Per-type display tokens (color + icon)
const MEM_TYPE_META = {
  concept:  { color: '#818cf8', icon: '◈' },
  file:     { color: '#bc8cff', icon: '▤' },
  project:  { color: '#3fb950', icon: '▣' },
  task:     { color: '#d4a017', icon: '☑' },
  document: { color: '#58a6ff', icon: '▦' },
  website:  { color: '#58a6ff', icon: '◐' },
  user:     { color: '#f97316', icon: '◉' },
  fact:     { color: '#a78bfa', icon: '✦' },
};
function memTypeMeta(t) {
  return MEM_TYPE_META[String(t || '').toLowerCase()] || { color: '#7d8590', icon: '○' };
}

let _allMemoryNodes = [];
let _memoryTypeFilter = null; // null = all

function renderMemoryList(nodes) {
  _allMemoryNodes = Array.isArray(nodes) ? nodes : [];

  // Stats
  document.getElementById('mem-stat-total').textContent = _allMemoryNodes.length;
  const types = new Set(_allMemoryNodes.map(n => (n.node_type || 'node').toLowerCase()));
  document.getElementById('mem-stat-types').textContent = types.size;

  // Type filter chips
  renderTypeFilter(_allMemoryNodes);

  // Grouped view
  renderGraphView();
}

function renderTypeFilter(nodes) {
  const wrap = document.getElementById('mem-type-filter');
  if (!wrap) return;
  const counts = {};
  for (const n of nodes) {
    const t = (n.node_type || 'node').toLowerCase();
    counts[t] = (counts[t] || 0) + 1;
  }
  const types = Object.entries(counts).sort((a, b) => b[1] - a[1]);
  let html = `<button class="mem-type-chip ${_memoryTypeFilter === null ? 'active' : ''}" data-type="">All <span class="mem-type-count">${nodes.length}</span></button>`;
  for (const [t, c] of types) {
    const meta = memTypeMeta(t);
    const active = _memoryTypeFilter === t ? 'active' : '';
    html += `<button class="mem-type-chip ${active}" data-type="${escHtml(t)}" style="--chip-c:${meta.color}"><span class="mem-type-ico">${meta.icon}</span>${escHtml(t)}<span class="mem-type-count">${c}</span></button>`;
  }
  wrap.innerHTML = html;
  wrap.querySelectorAll('.mem-type-chip').forEach(btn => {
    btn.addEventListener('click', () => {
      const t = btn.dataset.type || null;
      _memoryTypeFilter = t === '' ? null : t;
      renderTypeFilter(_allMemoryNodes);
      renderGraphView();
    });
  });
}

function renderGraphView() {
  const container = document.getElementById('mem-graph-view');
  if (!container) return;
  let nodes = _allMemoryNodes;
  if (_memoryTypeFilter) {
    nodes = nodes.filter(n => (n.node_type || '').toLowerCase() === _memoryTypeFilter);
  }
  document.getElementById('mem-stat-shown').textContent = nodes.length;

  if (!nodes.length) {
    container.innerHTML = `<div class="mem-empty"><div class="mem-empty-icon">◎</div><div class="mem-empty-title">No memory nodes${_memoryTypeFilter ? ` of type "${_memoryTypeFilter}"` : ''}</div><div class="mem-empty-hint">${_memoryTypeFilter ? 'Try a different type filter.' : 'Start chatting — Rook will store what it learns here.'}</div></div>`;
    return;
  }

  // Group by type
  const groups = {};
  for (const n of nodes) {
    const t = (n.node_type || 'node').toLowerCase();
    (groups[t] = groups[t] || []).push(n);
  }
  const sortedTypes = Object.keys(groups).sort((a, b) => groups[b].length - groups[a].length);

  let html = '';
  for (const type of sortedTypes) {
    const meta = memTypeMeta(type);
    html += `<section class="mem-group" style="--gc:${meta.color}">
      <div class="mem-group-head">
        <span class="mem-group-ico">${meta.icon}</span>
        <span class="mem-group-name">${escHtml(type)}</span>
        <span class="mem-group-count">${groups[type].length}</span>
      </div>
      <div class="mem-group-body">`;
    for (const n of groups[type]) {
      const summary = n.summary || (n.metadata && typeof n.metadata === 'object' && n.metadata.summary) || '';
      const keyFacts = n.key_facts || [];
      const updated = n.updated_at ? new Date(n.updated_at * 1000).toLocaleDateString() : '';
      const conf = n.metadata && typeof n.metadata === 'object' && n.metadata.confidence;
      const meta2 = n.metadata && typeof n.metadata === 'object' ? n.metadata : null;
      const metaRows = meta2
        ? Object.entries(meta2).filter(([k]) => k !== 'summary').slice(0, 6).map(([k, v]) => {
            const val = typeof v === 'string' ? v : JSON.stringify(v);
            return `<div class="mem-meta-row"><span class="mem-meta-k">${escHtml(k)}</span><span class="mem-meta-v">${escHtml(val.length > 80 ? val.slice(0, 80) + '…' : val)}</span></div>`;
          }).join('')
        : '';
      html += `<details class="mem-node" data-node-id="${escHtml(n.id || '')}">
        <summary class="mem-node-head">
          <span class="mem-node-chev"></span>
          <span class="mem-node-title">${escHtml(n.title || n.id || 'Node')}</span>
          ${conf ? `<span class="mem-node-conf" title="Confidence">${(conf * 100).toFixed(0)}%</span>` : ''}
          ${updated ? `<span class="mem-node-date">${updated}</span>` : ''}
        </summary>
        <div class="mem-node-body">
          ${summary ? `<div class="mem-node-summary">${escHtml(summary)}</div>` : ''}
          ${keyFacts.length ? `<div class="mem-node-section"><div class="mem-node-section-label">Key facts</div><ul class="mem-node-facts">${keyFacts.slice(0, 6).map(f => `<li>${escHtml(f)}</li>`).join('')}</ul></div>` : ''}
          ${metaRows ? `<div class="mem-node-section"><div class="mem-node-section-label">Metadata</div>${metaRows}</div>` : ''}
          <div class="mem-node-section">
            <div class="mem-node-section-label">Connected nodes</div>
            <div class="mem-node-edges">click to load…</div>
          </div>
          <div class="mem-node-id">id: ${escHtml(n.id || '—')}</div>
        </div>
      </details>`;
    }
    html += `</div></section>`;
  }
  container.innerHTML = html;

  // Lazy-load connections when a node is opened
  container.querySelectorAll('details.mem-node').forEach(d => {
    d.addEventListener('toggle', () => {
      if (!d.open) return;
      const edgesEl = d.querySelector('.mem-node-edges');
      if (!edgesEl || edgesEl.dataset.loaded) return;
      edgesEl.dataset.loaded = '1';
      const nodeId = d.dataset.nodeId;
      if (!nodeId) { edgesEl.textContent = '(no id)'; return; }
      edgesEl.textContent = 'loading…';
      const reqId = uid();
      _pendingEdgeRequests.set(reqId, edgesEl);
      api.send({ type: 'get_connected_nodes', id: reqId, node_id: nodeId });
    });
  });
}

const _pendingEdgeRequests = new Map();
function handleConnectedNodes(msg) {
  const el = _pendingEdgeRequests.get(msg.id);
  if (!el) return;
  _pendingEdgeRequests.delete(msg.id);
  const nodes = msg.nodes || [];
  if (!nodes.length) { el.innerHTML = '<span class="mem-edges-empty">no connections</span>'; return; }
  el.innerHTML = nodes.map(n => {
    const meta = memTypeMeta(n.node_type);
    return `<span class="mem-edge-pill" style="--ec:${meta.color}"><span class="mem-edge-ico">${meta.icon}</span>${escHtml(n.title || n.id || 'node')}</span>`;
  }).join('');
}

document.getElementById('btn-mem-refresh')?.addEventListener('click', () => searchMemory());

// ════════════════════════════════════════
// SKILLS PAGE
// ════════════════════════════════════════
document.getElementById('btn-reload-skills').addEventListener('click', loadSkills);
function loadSkills() {
  const list = document.getElementById('skills-list');
  list.innerHTML = '<li style="color:var(--text-2);padding:10px 12px">Loading…</li>';
  api.send({ type: 'list_skills', id: uid() });
}

function renderSkills(skills) {
  const list = document.getElementById('skills-list');
  list.innerHTML = '';
  if (!skills?.length) { list.innerHTML = '<li style="color:var(--text-3);padding:10px 12px">No skills loaded</li>'; return; }
  for (const s of skills) {
    const li = document.createElement('li');
    li.innerHTML = `<span class="iname">${esc(s.name || s.id || 'Skill')}</span>
      <span class="idesc">${esc(s.description || '')}</span>
      <span class="ibadge">skill</span>`;
    li.addEventListener('click', () => {
      chatInput.value = `/skill ${s.name || s.id} `;
      chatInput.focus();
      switchPage('chat');
    });
    list.appendChild(li);
  }
}

// ════════════════════════════════════════
// PLUGINS PAGE
// ════════════════════════════════════════


// ════════════════════════════════════════
// GEMMA LAUNCH
// ════════════════════════════════════════
function handleGemmaLaunched(msg) {
  if (msg.success) {
    appendMessage('system', `✓ ${msg.message}`);
  } else {
    appendMessage('system', `⚠ ${msg.message}`);
    // Reset the selector back to the previous value so the user knows it didn't work
    modelSelect.value = modelSelect.dataset.lastGoodValue || '';
  }
}

modelSelect.addEventListener('change', () => {
  const val = modelSelect.value;
  if (val && (val.startsWith('gemma') || val.startsWith('kiwi_kiwi/')) && val.includes(':')) {
    appendMessage('system', `Launching ${val} — please wait…`);
    api.send({ type: 'launch_gemma', id: uid(), model: val });
  } else {
    modelSelect.dataset.lastGoodValue = val;
  }
});


// ════════════════════════════════════════
// ── Platform sign-in (device auth flow) ─
let PLATFORM_URL = 'https://svrnsys.com';
// Resolve platform URL + dev mode from main process
Promise.all([
  api.getPlatformUrl?.() ?? Promise.resolve(PLATFORM_URL),
  api.isDevMode?.()      ?? Promise.resolve(false),
]).then(([url, dev]) => {
  if (url) PLATFORM_URL = url;
  if (dev) {
    const simBtn = document.getElementById('btn-platform-simulate');
    if (simBtn) simBtn.hidden = false;
  }
});
const PLATFORM_MODELS_META = {
  'nova-fast':    'nova-fast — Amazon Nova Micro',
  'qwen-coder':   'qwen-coder — Qwen3 Coder 30B',
  'gemini-search':'gemini-search — Gemini 2.5 Flash Lite',
};

let _pollTimer = null;

function showPlatformState(state, data) {
  document.getElementById('platform-state-idle').hidden      = state !== 'idle';
  document.getElementById('platform-state-waiting').hidden   = state !== 'waiting';
  document.getElementById('platform-state-connected').hidden = state !== 'connected';
  if (state === 'connected' && data) {
    const info = document.getElementById('platform-account-info');
    if (info) info.textContent = `${data.plan?.charAt(0).toUpperCase() + data.plan?.slice(1)} plan · ${data.models?.length ?? 0} models available`;
  }
}

function populatePlatformModels(models) {
  const sel  = document.getElementById('s-model');
  const main = document.getElementById('model-select');
  [sel, main].forEach(s => {
    if (!s) return;
    // Remove old platform group
    s.querySelector('optgroup[data-platform]')?.remove();
    // Remove the "Loading…" placeholder if still present
    const placeholder = s.querySelector('option[value=""]');
    if (placeholder && placeholder.textContent === 'Loading…') placeholder.remove();

    if (!models?.length) {
      // No platform — restore a blank placeholder if select is empty
      if (!s.options.length) {
        const ph = document.createElement('option');
        ph.value = ''; ph.textContent = '— select a model —';
        s.appendChild(ph);
      }
      return;
    }
    const grp = document.createElement('optgroup');
    grp.label = 'SVRN Platform';
    grp.setAttribute('data-platform', '1');
    for (const m of models) {
      const opt = document.createElement('option');
      opt.value = m;
      opt.textContent = PLATFORM_MODELS_META[m] || m;
      grp.appendChild(opt);
    }
    s.insertBefore(grp, s.firstChild);
    // Auto-select first platform model if nothing is selected
    if (!s.value) s.value = models[0];
  });
}

function stopPolling() {
  if (_pollTimer) { clearInterval(_pollTimer); _pollTimer = null; }
}

function pollPlatformSession(sessionId) {
  stopPolling();
  let tries = 0;
  const maxTries = 150; // ~5 min at 2s intervals
  _pollTimer = setInterval(async () => {
    tries++;
    if (tries > maxTries) { stopPolling(); showPlatformState('idle'); return; }
    try {
      const r = await fetch(`${PLATFORM_URL}/api/auth/desktop?session=${sessionId}`);
      const d = await r.json();
      if (d.status === 'claimed') {
        stopPolling();
        dismissOnboarding();
        const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
        s.platformApiKey = d.api_key;
        s.platformPlan   = d.plan;
        s.platformModels = d.models;
        // Clear any saved custom API creds — platform handles routing now
        delete s.apiUrl; delete s.apiKey;
        localStorage.setItem('rook-settings', JSON.stringify(s));
        api.saveEnvSettings?.({ ...s, platformUrl: PLATFORM_URL, platformApiKey: d.api_key });
        api.send?.({ type: 'update_config', id: uid(), base_url: '', api_key: '', model: s.model || '', platform_url: PLATFORM_URL, platform_api_key: d.api_key });
        showPlatformState('connected', d);
        populatePlatformModels(d.models);
      } else if (d.status === 'expired') {
        stopPolling();
        showPlatformState('idle');
        toast('Sign-in session expired — please try again.', 'warn');
      }
    } catch (_) {
      // Network errors during polling are transient; don't spam toasts
    }
  }, 2000);
}

async function startPlatformSignIn() {
  const sessionId = crypto.randomUUID();
  sessionStorage.setItem('platform-session-id', sessionId);
  await api.openExternal(`${PLATFORM_URL}/auth/desktop?session=${sessionId}`);
  showPlatformState('waiting');
  pollPlatformSession(sessionId);
}

function platformSignOut() {
  stopPolling();
  const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  delete s.platformApiKey;
  delete s.platformPlan;
  delete s.platformModels;
  localStorage.setItem('rook-settings', JSON.stringify(s));
  api.saveEnvSettings?.({ ...s, platformApiKey: '', platformUrl: PLATFORM_URL });
  populatePlatformModels([]);
  showPlatformState('idle');
}

function syncPlatformAuthUI() {
  const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  const fetchBtn  = document.getElementById('btn-fetch-models');
  const modelHint = document.getElementById('s-model-hint');
  if (s.platformApiKey) {
    const data = { plan: s.platformPlan, models: s.platformModels, user: s.platformUser };
    showPlatformState('connected', data);
    populatePlatformModels(s.platformModels || []);
    updatePlatformStatus(data);
    if (fetchBtn)  fetchBtn.hidden = true;
    if (modelHint) modelHint.textContent = 'SVRN models included free — or bring your own API key below';
  } else {
    showPlatformState('idle');
    updatePlatformStatus(null);
    if (fetchBtn)  fetchBtn.hidden = false;
    if (modelHint) modelHint.textContent = 'Fetched from your API — select or type a model ID';
  }
}

// ── Dev simulate: calls platform's /api/auth/desktop/simulate (DEV_AUTH only) ─
async function simulatePlatformSignIn() {
  const btn = document.getElementById('btn-platform-simulate');
  if (btn) { btn.disabled = true; btn.textContent = 'Connecting…'; }
  try {
    const r = await fetch(`${PLATFORM_URL}/api/auth/desktop/simulate`, { method: 'POST' });
    if (!r.ok) {
      const err = await r.json().catch(() => ({}));
      console.warn('Platform simulate failed:', err.error ?? r.status);
      if (btn) { btn.disabled = false; btn.textContent = 'Test (dev) →'; }
      return;
    }
    const d = await r.json();
    const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
    s.platformApiKey = d.api_key;
    s.platformPlan   = d.plan;
    s.platformModels = d.models;
    s.platformUser   = d.user;
    delete s.apiUrl; delete s.apiKey;
    localStorage.setItem('rook-settings', JSON.stringify(s));
    api.saveEnvSettings?.({ ...s, platformUrl: d.platform_url ?? PLATFORM_URL, platformApiKey: d.api_key });
    api.send?.({ type: 'update_config', id: uid(), base_url: '', api_key: '', model: s.model || '', platform_url: d.platform_url ?? PLATFORM_URL, platform_api_key: d.api_key });
    showPlatformState('connected', d);
    populatePlatformModels(d.models);
    updatePlatformStatus(d);
  } catch (e) {
    console.warn('Platform simulate error:', e);
    if (btn) { btn.disabled = false; btn.textContent = 'Test (dev) →'; }
  }
}

// ── Auto-refresh profile + models using saved api key ────────────────────────
async function refreshPlatformSession() {
  const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  // Bootstrap from .env if localStorage doesn't have the key yet (e.g. first launch after manual .env setup)
  if (!s.platformApiKey) {
    const envKey = await api.getPlatformApiKey?.().catch(() => '');
    if (envKey) {
      s.platformApiKey = envKey;
      localStorage.setItem('rook-settings', JSON.stringify(s));
      api.send?.({ type: 'update_config', id: uid(), base_url: '', api_key: '', model: s.model || '', platform_url: PLATFORM_URL, platform_api_key: envKey });
      dismissOnboarding();
    }
  }
  if (!s.platformApiKey) return;
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 8000);
  try {
    const r = await fetch(`${PLATFORM_URL}/api/platform/me`, {
      headers: { 'Authorization': `Bearer ${s.platformApiKey}` },
      signal: ctrl.signal,
    });
    clearTimeout(timer);
    if (r.status === 401) { platformSignOut(); return; }
    if (!r.ok) return;
    const d = await r.json();
    s.platformPlan   = d.plan;
    s.platformModels = d.models;
    s.platformUser   = d.user;
    localStorage.setItem('rook-settings', JSON.stringify(s));
    showPlatformState('connected', d);
    populatePlatformModels(d.models);
    updatePlatformStatus(d);
  } catch (e) {
    clearTimeout(timer);
    if (e.name !== 'AbortError') console.warn('Platform refresh failed:', e);
  }
}

// ── Platform status pill shown in the main UI ─────────────────────────────────
function updatePlatformStatus(data) {
  // Show a small connection indicator in the sidebar / model select area if present
  const pill = document.getElementById('platform-status-pill');
  if (!pill) return;
  if (data) {
    const used = data.tokens?.used ?? 0;
    const lim  = data.tokens?.limit;
    pill.textContent = lim ? `${Math.round(used/1000)}K / ${Math.round(lim/1000)}K tokens` : `${data.plan} plan`;
    pill.style.display = '';
  } else {
    pill.style.display = 'none';
  }
}

document.getElementById('btn-platform-signin')?.addEventListener('click', startPlatformSignIn);
document.getElementById('btn-platform-simulate')?.addEventListener('click', simulatePlatformSignIn);
document.getElementById('btn-platform-cancel')?.addEventListener('click', () => { stopPolling(); showPlatformState('idle'); });
document.getElementById('btn-platform-signout')?.addEventListener('click', platformSignOut);

// ════════════════════════════════════════
function loadSettings() {
  try {
    const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
    // Always show custom API fields — users can bring their own key even when platform-connected
    document.getElementById('s-api-url').value              = s.apiUrl  || '';
    document.getElementById('s-api-key').value              = s.apiKey  || '';
    document.getElementById('s-backend-path').value         = s.backendPath  || '';
    document.getElementById('s-github-token').value         = s.githubToken  || '';
    document.getElementById('s-show-hint').checked          = s.showHint !== false;
    document.getElementById('s-preview-default').checked    = !!s.previewDefault;
    if (s.githubToken) api.setGithubToken(s.githubToken);
  } catch (_) {}
  syncPlatformAuthUI();
  renderProfiles();
}
function saveSettings() {
  const sSel = document.getElementById('s-model');
  const s    = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  Object.assign(s, {
    apiUrl:         document.getElementById('s-api-url').value.trim(),
    apiKey:         document.getElementById('s-api-key').value.trim(),
    model:          sSel ? sSel.value.trim() : '',
    backendPath:    document.getElementById('s-backend-path').value.trim(),
    githubToken:    document.getElementById('s-github-token').value.trim(),
    showHint:       document.getElementById('s-show-hint').checked,
    previewDefault: document.getElementById('s-preview-default').checked,
  });
  localStorage.setItem('rook-settings', JSON.stringify(s));
  api.saveEnvSettings?.({ apiUrl: s.apiUrl, apiKey: s.apiKey, model: s.model, platformApiKey: s.platformApiKey || '', platformUrl: PLATFORM_URL });
  applySettings(s);
  fetchModels({ silent: true });
  document.getElementById('btn-save-settings').textContent = 'Saved!';
  setTimeout(() => document.getElementById('btn-save-settings').textContent = 'Save settings', 1500);
}
function applySettings(s) {
  inputHint.style.display = s.showHint !== false ? '' : 'none';
  if (s.previewDefault && artifactTabs.length === 0) artifactPane.classList.remove('closed');
  if (s.githubToken) api.setGithubToken(s.githubToken);
  if (backendReady && (s.apiUrl || s.apiKey || s.model || s.platformApiKey)) {
    api.send({ type: 'update_config', id: uid(), base_url: s.apiUrl || '', api_key: s.apiKey || '', model: s.model || '', platform_url: PLATFORM_URL, platform_api_key: s.platformApiKey || '' });
  }
}

document.getElementById('btn-save-settings').addEventListener('click', saveSettings);
loadSettings();

// ── Model fetching ───────────────────────────────────────────────────────────
async function fetchModels({ silent = false } = {}) {
  const s = JSON.parse(localStorage.getItem('rook-settings') || '{}');
  // Platform-connected: models are already set by populatePlatformModels(), don't call external API
  if (s.platformApiKey) return;
  const base = (s.apiUrl || 'https://api.openai.com/v1').replace(/\/$/, '');
  const key  = s.apiKey || '';
  if (!key && !silent) { toast('No API key configured', 'warn'); return; }
  const sel  = document.getElementById('model-select');
  const sSel = document.getElementById('s-model');
  const btn  = document.getElementById('btn-refresh-models');
  const fBtn = document.getElementById('btn-fetch-models');
  if (btn)  btn.classList.add('spinning');
  if (fBtn) { fBtn.disabled = true; fBtn.textContent = '↻ Fetching…'; }
  try {
    const resp = await fetch(`${base}/models`, {
      headers: key ? { Authorization: `Bearer ${key}` } : {}
    });
    if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
    const data = await resp.json();
    const models = (data.data || data.models || [])
      .map(m => typeof m === 'string' ? m : (m.id || m.name || ''))
      .filter(Boolean)
      .sort();
    // Populate top-right model select
    if (sel) {
      const prev = sel.value;
      sel.innerHTML = '<option value="">— model —</option>';
      models.forEach(id => {
        const o = document.createElement('option');
        o.value = id; o.textContent = id;
        if (id === prev) o.selected = true;
        sel.appendChild(o);
      });
      if (!sel.value && s.model) sel.value = s.model;
    }
    // Populate settings model select
    if (sSel) {
      const prev = sSel.value || s.model || '';
      sSel.innerHTML = '<option value="">— select model —</option>';
      models.forEach(id => {
        const o = document.createElement('option');
        o.value = id; o.textContent = id;
        if (id === prev) o.selected = true;
        sSel.appendChild(o);
      });
      if (!sSel.value && prev) {
        // Model from settings not in list — add it
        const o = document.createElement('option');
        o.value = prev; o.textContent = prev + ' (custom)'; o.selected = true;
        sSel.appendChild(o);
      }
    }
    if (!silent) toast(`Loaded ${models.length} models`);
  } catch (err) {
    if (!silent) toast(`Could not fetch models: ${err.message}`, 'warn');
    // Fallback: populate with a blank + the saved default
    const fallback = s.model || '';
    [sel, sSel].forEach(el => {
      if (!el) return;
      if (el.options.length <= 1) {
        el.innerHTML = '<option value="">— model —</option>';
        if (fallback) {
          const o = document.createElement('option');
          o.value = fallback; o.textContent = fallback; o.selected = true;
          el.appendChild(o);
        }
      }
    });
  } finally {
    if (btn)  btn.classList.remove('spinning');
    if (fBtn) { fBtn.disabled = false; fBtn.textContent = '↻ Fetch'; }
  }
}

document.getElementById('btn-refresh-models')?.addEventListener('click', () => fetchModels());
document.getElementById('btn-fetch-models')?.addEventListener('click',   () => fetchModels());

// ── API Profiles ─────────────────────────────────────────────────────────────
function getProfiles() {
  try { return JSON.parse(localStorage.getItem('rook-profiles') || '[]'); }
  catch { return []; }
}
function saveProfiles(profiles) {
  localStorage.setItem('rook-profiles', JSON.stringify(profiles));
}
function renderProfiles() {
  const list = document.getElementById('profiles-list');
  if (!list) return;
  const profiles = getProfiles();
  if (profiles.length === 0) {
    list.innerHTML = '<span class="profiles-empty">No saved profiles yet.</span>';
    return;
  }
  list.innerHTML = '';
  profiles.forEach((p, i) => {
    const chip = document.createElement('div');
    chip.className = 'profile-chip';
    chip.innerHTML = `<button class="profile-chip-name" title="Apply profile">${escHtml(p.name)}</button><span class="profile-chip-url">${escHtml(p.apiUrl ? new URL(p.apiUrl).hostname : 'custom')}</span><button class="profile-chip-del" title="Delete profile" data-idx="${i}">×</button>`;
    chip.querySelector('.profile-chip-name').addEventListener('click', () => applyProfile(p));
    chip.querySelector('.profile-chip-del').addEventListener('click', () => {
      const ps = getProfiles(); ps.splice(i, 1); saveProfiles(ps); renderProfiles();
    });
    list.appendChild(chip);
  });
}
function applyProfile(p) {
  document.getElementById('s-api-url').value = p.apiUrl || '';
  document.getElementById('s-api-key').value = p.apiKey || '';
  if (p.model) {
    const sSel = document.getElementById('s-model');
    if (sSel) {
      // Try to select existing option, else add it
      let found = false;
      for (const o of sSel.options) { if (o.value === p.model) { sSel.value = p.model; found = true; break; } }
      if (!found) {
        const o = document.createElement('option');
        o.value = p.model; o.textContent = p.model; o.selected = true;
        sSel.appendChild(o);
      }
    }
  }
  toast(`Profile "${p.name}" applied — save to activate`);
}
document.getElementById('btn-save-profile')?.addEventListener('click', () => {
  const name = document.getElementById('profile-name-input')?.value.trim();
  if (!name) { toast('Enter a profile name first', 'warn'); return; }
  const apiUrl  = document.getElementById('s-api-url')?.value.trim() || '';
  const apiKey  = document.getElementById('s-api-key')?.value.trim() || '';
  const sSel    = document.getElementById('s-model');
  const model   = sSel?.value || '';
  const profiles = getProfiles();
  const existing = profiles.findIndex(p => p.name === name);
  const entry = { name, apiUrl, apiKey, model };
  if (existing >= 0) profiles[existing] = entry; else profiles.push(entry);
  saveProfiles(profiles);
  renderProfiles();
  document.getElementById('profile-name-input').value = '';
  toast(`Profile "${name}" saved`);
});

// ── Settings nav tab switching ───────────────────────────────────────────────
document.querySelectorAll('.snav-item[data-target]').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.snav-item').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.scontent').forEach(s => s.classList.remove('active'));
    btn.classList.add('active');
    document.getElementById(btn.dataset.target)?.classList.add('active');
  });
});

// Eye buttons — reveal/hide secret inputs
document.querySelectorAll('.sfield-eye[data-for]').forEach(btn => {
  btn.addEventListener('click', () => {
    const input = document.getElementById(btn.dataset.for);
    if (!input) return;
    input.type = input.type === 'password' ? 'text' : 'password';
    btn.title = input.type === 'password' ? 'Reveal' : 'Hide';
  });
});

// Quick-fill model chips
document.querySelectorAll('.sfield-ex[data-fill]').forEach(chip => {
  chip.addEventListener('click', () => {
    const el = document.getElementById(chip.dataset.fill);
    if (el) { el.value = chip.dataset.val; el.focus(); }
  });
});

// Wire plugin module with the deps it needs from app scope
initPlugins({ api, appendMessage, switchPage, toast });

// Kick off the browse list immediately (no backend needed)
loadBrowse(true);

// Fetch models on startup (silent — will silently skip if no key configured)
fetchModels({ silent: true });

// ════════════════════════════════════════
// HEARTBEAT
// ════════════════════════════════════════
setInterval(() => api.send({ type: 'health_check', id: uid() }), 30_000);


// ════════════════════════════════════════
const memoryFeedbackState = new Map();

function openMemoryPanel()  { memoryPanel.classList.remove('hidden'); btnMemoryToggle?.classList.add('active'); }
function closeMemoryPanel() { memoryPanel.classList.add('hidden');    btnMemoryToggle?.classList.remove('active'); }
function toggleMemoryPanel(){ memoryPanel.classList.contains('hidden') ? openMemoryPanel() : closeMemoryPanel(); }

btnMemoryToggle?.addEventListener('click', toggleMemoryPanel);
btnMemoryClose ?.addEventListener('click', closeMemoryPanel);
// Global keyboard shortcuts
document.addEventListener('keydown', (e) => {
  const mod = e.ctrlKey || e.metaKey;
  // Ctrl+M — toggle memory panel
  if (mod && e.key.toLowerCase() === 'm') { e.preventDefault(); toggleMemoryPanel(); return; }
  // Ctrl+K — command palette
  if (mod && e.key.toLowerCase() === 'k') { e.preventDefault(); cmdPaletteOverlay.classList.contains('hidden') ? openPalette() : closePalette(); return; }
  // Ctrl+N — new chat
  if (mod && e.key.toLowerCase() === 'n') { e.preventDefault(); btnNewChat.click(); return; }
  // Ctrl+/ — go to memory page
  if (mod && e.key === '/') { e.preventDefault(); switchPage('memory'); return; }
  // Ctrl+R — regenerate last response
  if (mod && e.key.toLowerCase() === 'r' && !streaming && currentConvId) {
    e.preventDefault();
    api.send({ type: 'regenerate_last_message', id: uid(), conversation_id: currentConvId });
    const lastAsst = [...document.querySelectorAll('.message.assistant')].pop();
    if (lastAsst) lastAsst.remove();
    setStreaming(true);
    return;
  }
  // Escape — stop streaming, or close palette / mode menu
  if (e.key === 'Escape') {
    if (!cmdPaletteOverlay.classList.contains('hidden')) { closePalette(); return; }
    if (!modeMenu.classList.contains('hidden')) { modeMenu.classList.add('hidden'); btnModeToggle.classList.remove('open'); return; }
    if (streaming && inFlightChatId && btnStop && !btnStop.disabled) { btnStop.click(); return; }
  }
});

// Build a plain-language explanation from the numeric scores. No LLM call —
// this is a template over the fields we already have, so it's free and
// deterministic. The point is to teach the user *why* a memory surfaced.
function explainMemoryNode(n) {
  const parts = [];
  const sem = n.embedding_score || 0;
  const grf = n.graph_score || 0;
  const rec = n.recency_score || 0;
  const con = n.confidence_score || 0;
  const scores = [
    ['semantic match', sem],
    ['graph relationship', grf],
    ['recent activity',   rec],
    ['confidence',        con],
  ].sort((a, b) => b[1] - a[1]);
  const [topLabel, topVal] = scores[0];
  if (topVal >= 0.6)      parts.push(`strong ${topLabel} (${topVal.toFixed(2)})`);
  else if (topVal >= 0.3) parts.push(`moderate ${topLabel} (${topVal.toFixed(2)})`);
  else                    parts.push(`weak signal overall — top factor was ${topLabel}`);
  const related = (n.related_titles || []).slice(0, 2);
  if (related.length) parts.push(`connected to ${related.join(', ')}`);
  return `Surfaced because of ${parts.join('; ')}.`;
}


// Node-type → accent colour mapping
const MEM_TYPE_COLORS = {
  concept:  '#C86E3D',
  file:     '#60a5fa',
  task:     '#fbbf24',
  tool:     '#34d399',
  website:  '#a78bfa',
  project:  '#f472b6',
  ui_state: '#94a3b8',
};
function memTypeColor(type) {
  return MEM_TYPE_COLORS[(type || '').toLowerCase()] || '#6b6560';
}

// Build signal-bar HTML for a 0-1 score (5 bars, each lights at .2 increments)
function buildSignalBars(score) {
  const lit = Math.round(Math.min(1, Math.max(0, score)) * 5);
  return Array.from({length: 5}, (_, i) =>
    `<span class="mem-signal-bar${i < lit ? ' lit' : ''}"></span>`
  ).join('');
}

function renderMemoryPanel(nodes) {
  if (!memoryPanel) return;
  memoryPanelList.innerHTML = '';
  const count = nodes.length;
  memoryPanelCount.textContent = count ? `${count} node${count === 1 ? '' : 's'}` : '';
  if (memoryPanelEmpty) {
    if (!_convHasSentMessage) {
      memoryPanelEmpty.innerHTML = '<strong>Memory context</strong>Send a message to activate memory retrieval for this conversation.';
    } else {
      memoryPanelEmpty.innerHTML = '<strong>No matches</strong>No relevant memories found for this conversation yet.';
    }
    memoryPanelEmpty.style.display = count ? 'none' : 'block';
  }

  // Auto-open when this conversation surfaces memory nodes
  if (count && memoryPanel.classList.contains('hidden') && !window._memoryPanelSeenConv) {
    openMemoryPanel();
    window._memoryPanelSeenConv = currentConvId;
  }

  for (const n of nodes) {
    const li = document.createElement('li');
    li.className = 'mem-card';
    const total = Math.min(1, Math.max(0, n.total_score       || 0));
    const sem   = Math.min(1, Math.max(0, n.embedding_score   || 0));
    const grf   = Math.min(1, Math.max(0, n.graph_score       || 0));
    const rec   = Math.min(1, Math.max(0, n.recency_score     || 0));
    const con   = Math.min(1, Math.max(0, n.confidence_score  || 0));
    const fbState = memoryFeedbackState.get(n.node_id) || null;
    const color = memTypeColor(n.node_type);

    // Set type colour as CSS custom property for stripe + signal + type label
    li.style.setProperty('--mcc', color);

    const why = explainMemoryNode(n);

    li.innerHTML = `
      <div class="mem-card-head">
        <span class="mem-card-title" title="${escHtml(n.title)}">${escHtml(n.title)}</span>
        <span class="mem-card-type">${escHtml(n.node_type || 'node')}</span>
        <div class="mem-signal" title="Relevance: ${(total*100).toFixed(0)}%">${buildSignalBars(total)}</div>
      </div>
      <div class="mem-score-row">
        <span class="mem-score-val">${total.toFixed(3)}</span>
        <span class="mem-score-subscores">
          <span class="mem-score-sub"><span class="mem-score-sub-label">sem·</span>${sem.toFixed(2)}</span>
          <span class="mem-score-sub"><span class="mem-score-sub-label">grf·</span>${grf.toFixed(2)}</span>
          <span class="mem-score-sub"><span class="mem-score-sub-label">rec·</span>${rec.toFixed(2)}</span>
          <span class="mem-score-sub"><span class="mem-score-sub-label">con·</span>${con.toFixed(2)}</span>
        </span>
      </div>
      <div class="mem-card-detail">
        ${why ? `<div class="mem-why"><span class="mem-why-prefix">›</span>${escHtml(why)}</div>` : ''}
        ${n.summary ? `<div class="mem-detail-row"><span class="mem-detail-label">Summary</span>${escHtml(n.summary)}</div>` : ''}
        ${(n.key_facts?.length) ? `<div class="mem-detail-row"><span class="mem-detail-label">Facts</span>${n.key_facts.map(f => `• ${escHtml(f)}`).join('<br>')}</div>` : ''}
        ${(n.related_titles?.length) ? `<div class="mem-detail-row"><span class="mem-detail-label">Related</span>${n.related_titles.map(escHtml).join(', ')}</div>` : ''}
        <div class="mem-fb-row">
          <button class="mem-fb-btn good ${fbState==='positive'?'active':''}" data-rating="positive">↑ Relevant</button>
          <button class="mem-fb-btn bad  ${fbState==='negative'?'active':''}" data-rating="negative">↓ Not relevant</button>
          ${fbState ? `<button class="mem-fb-btn clear" data-rating="neutral" title="Clear vote">✕</button>` : ''}
        </div>
      </div>
    `;

    li.addEventListener('click', (e) => {
      if (e.target.closest('.mem-fb-btn')) return;
      li.classList.toggle('expanded');
    });

    li.querySelectorAll('.mem-fb-btn').forEach(btn => {
      btn.addEventListener('click', (e) => {
        e.stopPropagation();
        const rating = btn.dataset.rating;
        const fbRow = li.querySelector('.mem-fb-row');
        if (rating === 'neutral') {
          // Clear existing vote
          memoryFeedbackState.delete(n.node_id);
          li.querySelectorAll('.mem-fb-btn').forEach(b => b.classList.remove('active'));
          fbRow.querySelector('.mem-fb-btn.clear')?.remove();
        } else {
          memoryFeedbackState.set(n.node_id, rating);
          li.querySelectorAll('.mem-fb-btn').forEach(b => b.classList.remove('active'));
          btn.classList.add('active');
          // Add clear button if not already present
          if (!fbRow.querySelector('.mem-fb-btn.clear')) {
            const clearBtn = document.createElement('button');
            clearBtn.className = 'mem-fb-btn clear';
            clearBtn.dataset.rating = 'neutral';
            clearBtn.title = 'Clear vote';
            clearBtn.textContent = '✕';
            fbRow.appendChild(clearBtn);
            clearBtn.addEventListener('click', (ev) => {
              ev.stopPropagation();
              memoryFeedbackState.delete(n.node_id);
              li.querySelectorAll('.mem-fb-btn').forEach(b => b.classList.remove('active'));
              clearBtn.remove();
              api.send({ type: 'submit_memory_feedback', id: uid(), node_id: n.node_id,
                query: undefined, rating: 'neutral', reason: null, source: 'ui',
                session_id: currentConvId || undefined });
            });
          }
        }
        api.send({
          type: 'submit_memory_feedback',
          id: uid(),
          node_id: n.node_id,
          query: undefined,
          rating,
          reason: null,
          source: 'ui',
          session_id: currentConvId || undefined,
        });
      });
    });

    memoryPanelList.appendChild(li);
  }
}

// Render user facts (global profile) as a collapsible section at the top of
// the memory panel — below the graph nodes but always visible if facts exist.
let _lastFactsJson = '';
function renderUserFacts(facts) {
  if (!memoryPanel) return;
  const json = JSON.stringify(facts);
  if (json === _lastFactsJson) return; // nothing changed
  _lastFactsJson = json;

  const existingSection = memoryPanel.querySelector('.user-facts-section');
  if (existingSection) existingSection.remove();
  if (!facts.length) return;

  const section = document.createElement('div');
  section.className = 'user-facts-section';
  section.innerHTML = `
    <details class="user-facts-details" open>
      <summary class="user-facts-summary">User profile (${facts.length} fact${facts.length !== 1 ? 's' : ''})</summary>
      <ul class="user-facts-list">
        ${facts.map(f => `<li><span class="uf-key">${escHtml(f.key)}</span><span class="uf-val">${escHtml(f.value)}</span></li>`).join('')}
      </ul>
    </details>`;

  // Insert before the node list so it sits at the top of the panel body
  const panelBody = memoryPanel.querySelector('#memory-panel-body') || memoryPanelList?.parentElement;
  if (panelBody) panelBody.insertBefore(section, panelBody.firstChild);

  // Auto-open panel if it's hidden and we now have facts
  if (memoryPanel.classList.contains('hidden') && !window._memoryPanelSeenConv) {
    openMemoryPanel();
    window._memoryPanelSeenConv = currentConvId;
  }
}

chatInput.focus();

// ════════════════════════════════════════
// EMPTY STATE HELPERS
// ════════════════════════════════════════
function showEmptyState() { emptyState?.classList.add('visible'); }
function hideEmptyState() { emptyState?.classList.remove('visible'); }

// Clear messages and re-attach the empty state element (innerHTML wipes it).
function clearMessages() {
  messagesEl.innerHTML = '';
  if (emptyState) { messagesEl.appendChild(emptyState); showEmptyState(); }
}

// Wire up shortcut chips inside the empty state
document.querySelectorAll('.shortcut-chip').forEach(btn => {
  btn.addEventListener('click', () => {
    const a = btn.dataset.action;
    if (a === 'new-chat')          btnNewChat.click();
    else if (a === 'open-palette') openPalette();
    else if (a === 'go-memory')    switchPage('memory');
  });
});

// ════════════════════════════════════════
// CONTEXT WINDOW BAR
// ════════════════════════════════════════
function updateCtxBar(usage) {
  if (!ctxBarFill || !usage) return;
  const pct = Math.min(100, ((usage.prompt_tokens || 0) / 128000) * 100);
  ctxBarFill.style.width = pct + '%';
  ctxBarFill.classList.toggle('warn',   pct > 55 && pct <= 80);
  ctxBarFill.classList.toggle('danger', pct > 80);
}

// ════════════════════════════════════════
// REGENERATE — mark last assistant message
// ════════════════════════════════════════
function markLastAssistant() {
  document.querySelectorAll('.message.assistant.last-msg').forEach(m => m.classList.remove('last-msg'));
  const all = document.querySelectorAll('.message.assistant');
  if (all.length) all[all.length - 1].classList.add('last-msg');
}

// ════════════════════════════════════════
// CONVERSATION PERSISTENCE
// ════════════════════════════════════════
function saveConvsToStorage() {
  // Progressively trim until the write fits within localStorage quota.
  const attempts = [
    { convs: 60, msgs: 30 },
    { convs: 40, msgs: 15 },
    { convs: 20, msgs: 5  },
    { convs: 10, msgs: 2  },
  ];
  for (const { convs, msgs } of attempts) {
    try {
      const slim = conversations.slice(0, convs).map(c => ({
        id: c.id, title: c.title, pinned: c.pinned,
        messages: (c.messages || []).slice(-msgs),
      }));
      localStorage.setItem('rook-conversations', JSON.stringify(slim));
      return; // success
    } catch (e) {
      if (e.name !== 'QuotaExceededError' && e.name !== 'NS_ERROR_DOM_QUOTA_REACHED') return;
      // else loop and try with fewer entries
    }
  }
}

function loadConvsFromStorage() {
  try {
    const raw = JSON.parse(localStorage.getItem('rook-conversations') || '[]');
    if (Array.isArray(raw) && raw.length) { conversations = raw; renderConvList(); }
  } catch (_) {}
}

// ════════════════════════════════════════
// CONVERSATION LIST HANDLERS (backend)
// ════════════════════════════════════════
function handleConversationList(backendConvs) {
  for (const bc of backendConvs) {
    const local = conversations.find(c => c.id === bc.id);
    if (local) {
      local.title  = bc.title || local.title;
      if (bc.pinned    !== undefined) local.pinned     = bc.pinned;
      if (bc.updated_at !== undefined) local.updated_at = bc.updated_at;
    } else {
      conversations.push({ id: bc.id, title: bc.title || 'Conversation', messages: [], pinned: !!bc.pinned, updated_at: bc.updated_at });
    }
  }
  conversations.sort((a, b) => (b.pinned ? 1 : 0) - (a.pinned ? 1 : 0));
  renderConvList();
  saveConvsToStorage();
}

function updateConvTitle(convId, title) {
  if (!title) return;
  const conv = conversations.find(c => c.id === convId);
  if (!conv) return;
  conv.title = title;
  renderConvList();
  saveConvsToStorage();
  if (convId === currentConvId) chatTitle.textContent = title;
  // No toast — title updates silently in the sidebar
}

function handleConversationMessages(convId, messages) {
  if (convId !== currentConvId) return;
  document.getElementById('conv-switch-spinner')?.remove();
  clearMessages();
  if (!messages.length) { showEmptyState(); return; }
  for (const m of messages) appendMessage(m.role === 'user' ? 'user' : 'assistant', m.content);
  markLastAssistant();
  const conv = conversations.find(c => c.id === convId);
  if (conv) { conv.messages = messages.map(m => ({ role: m.role, content: m.content })); saveConvsToStorage(); }
}

// ════════════════════════════════════════
// SIDEBAR CONVERSATION SEARCH
// ════════════════════════════════════════
let _searchTimer = null;
sidebarSearch?.addEventListener('input', () => {
  clearTimeout(_searchTimer);
  _searchTimer = setTimeout(() => {
    const q = sidebarSearch.value.trim();
    if (!q) { renderConvList(); return; }
    const ql = q.toLowerCase();
    const local = conversations.filter(c =>
      (c.title || '').toLowerCase().includes(ql) ||
      (c.messages || []).some(m => (m.content || '').toLowerCase().includes(ql))
    );
    renderConvList(local);
    if (q.length >= 2) api.send({ type: 'search_conversations', id: uid(), query: q });
  }, 200);
});

// ════════════════════════════════════════
// TOAST NOTIFICATIONS
// ════════════════════════════════════════
function toast(msg, type = 'info', duration = 4000) {
  if (!toastContainer) return;
  const el = document.createElement('div');
  el.className = `toast toast-${type}`;
  el.innerHTML = `<span class="toast-msg">${escHtml(msg)}</span><button class="toast-dismiss" aria-label="Dismiss">&#215;</button>`;
  el.querySelector('.toast-dismiss').addEventListener('click', () => dismissToast(el));
  toastContainer.appendChild(el);
  if (duration > 0 && type !== 'error') setTimeout(() => dismissToast(el), duration);
  return el;
}
function dismissToast(el) {
  el.classList.add('out');
  setTimeout(() => el.remove(), 220);
}

// ════════════════════════════════════════
// CONFIRM MODAL
// ════════════════════════════════════════
let _confirmResolve = null;

(function initConfirmModal() {
  const modal     = document.getElementById('confirm-modal');
  const cancelBtn = document.getElementById('confirm-cancel');
  const okBtn     = document.getElementById('confirm-ok');
  if (!modal) return;

  function close(result) {
    modal.classList.add('hidden');
    if (_confirmResolve) { _confirmResolve(result); _confirmResolve = null; }
  }

  cancelBtn?.addEventListener('click', () => close(false));
  okBtn?.addEventListener('click',     () => close(true));
  modal.addEventListener('click', (e) => { if (e.target === modal) close(false); });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !modal.classList.contains('hidden')) { e.stopPropagation(); close(false); }
  }, true);
})();

function showConfirmModal({ title = 'Are you sure?', body = '', okLabel = 'Delete', dangerous = true } = {}) {
  return new Promise((resolve) => {
    const modal   = document.getElementById('confirm-modal');
    const titleEl = document.getElementById('confirm-title');
    const bodyEl  = document.getElementById('confirm-body');
    const okBtn   = document.getElementById('confirm-ok');
    if (!modal) { resolve(window.confirm(body || title)); return; }
    if (titleEl) titleEl.textContent = title;
    if (bodyEl)  bodyEl.textContent  = body;
    if (okBtn)   okBtn.textContent   = okLabel;
    _confirmResolve = resolve;
    modal.classList.remove('hidden');
    document.getElementById('confirm-cancel')?.focus();
  });
}

// ════════════════════════════════════════
// COMMAND PALETTE  (Ctrl+K)
// ════════════════════════════════════════
const CMD_ITEMS = [
  { name: 'New chat',            desc: 'Start a fresh conversation',       kbd: 'Ctrl+N', action: () => btnNewChat.click() },
  { name: 'Memory',              desc: 'Browse the memory graph',           kbd: 'Ctrl+/', action: () => switchPage('memory') },
  { name: 'Settings',            desc: 'API keys and configuration',        action: () => switchPage('settings') },
  { name: 'Skills',              desc: 'View loaded skills',                action: () => switchPage('skills') },
  { name: 'Plugins',             desc: 'Browse and install plugins',        action: () => switchPage('plugins') },
  { name: 'Regenerate response', desc: 'Re-run the last AI response',       kbd: 'Ctrl+R', action: () => { if (currentConvId && !streaming) { api.send({ type: 'regenerate_last_message', id: uid(), conversation_id: currentConvId }); const la = [...document.querySelectorAll('.message.assistant')].pop(); if (la) la.remove(); setStreaming(true); } } },
  { name: 'Export conversation', desc: 'Save chat as Markdown file',        action: exportConversation },
  { name: 'Toggle memory panel', desc: 'Show/hide memory context sidebar',  kbd: 'Ctrl+M', action: toggleMemoryPanel },
];

function openPalette() {
  cmdPaletteOverlay.classList.remove('hidden');
  cmdInput.value = '';
  renderPaletteResults('');
  requestAnimationFrame(() => cmdInput.focus());
}
function closePalette() {
  cmdPaletteOverlay.classList.add('hidden');
}
function renderPaletteResults(q) {
  cmdResultsEl.innerHTML = '';
  const ql = q.toLowerCase();
  const cmds  = CMD_ITEMS.filter(c => !q || c.name.toLowerCase().includes(ql) || c.desc.toLowerCase().includes(ql));
  const convs = q ? conversations.filter(c => (c.title||'').toLowerCase().includes(ql)).slice(0, 5) : [];
  if (cmds.length) { _paletteSection('Actions'); cmds.forEach(i => _paletteItem(i)); }
  if (convs.length) {
    _paletteSection('Conversations');
    convs.forEach(c => _paletteItem({ name: c.title || 'Untitled', desc: 'Open conversation', action: () => {
      const li = convList.querySelector(`[data-conv-id="${c.id}"]`);
      if (li) li.click();
      else { currentConvId = c.id; clearMessages(); chatTitle.textContent = c.title || 'Conversation'; api.send({ type: 'get_conversation_messages', id: uid(), conversation_id: c.id }); switchPage('chat'); }
    }}));
  }
  if (!cmds.length && !convs.length) {
    const el = document.createElement('div');
    el.className = 'cmd-section-label';
    el.style.padding = '18px 10px';
    el.textContent = 'No results';
    cmdResultsEl.appendChild(el);
  }
  cmdResultsEl.querySelector('.cmd-item')?.classList.add('selected');
}
function _paletteSection(label) {
  const el = document.createElement('div');
  el.className = 'cmd-section-label';
  el.textContent = label;
  cmdResultsEl.appendChild(el);
}
function _paletteItem(item) {
  const el = document.createElement('div');
  el.className = 'cmd-item';
  el.innerHTML = `<svg viewBox="0 0 16 16" fill="none"><path d="M2 4h12M2 8h8M2 12h10" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/></svg>
    <div class="cmd-item-text"><div class="cmd-item-name">${escHtml(item.name)}</div><div class="cmd-item-desc">${escHtml(item.desc||'')}</div></div>
    ${item.kbd ? `<span class="cmd-item-kbd">${escHtml(item.kbd)}</span>` : ''}`;
  el.addEventListener('mouseenter', () => { cmdResultsEl.querySelectorAll('.cmd-item').forEach(i => i.classList.remove('selected')); el.classList.add('selected'); });
  el.addEventListener('click', () => { closePalette(); item.action?.(); });
  cmdResultsEl.appendChild(el);
}
cmdInput?.addEventListener('input', () => renderPaletteResults(cmdInput.value));
cmdInput?.addEventListener('keydown', (e) => {
  const items = [...cmdResultsEl.querySelectorAll('.cmd-item')];
  const sel   = cmdResultsEl.querySelector('.cmd-item.selected');
  const idx   = items.indexOf(sel);
  if (e.key === 'ArrowDown')  { e.preventDefault(); const n = items[Math.min(idx+1, items.length-1)]; items.forEach(i=>i.classList.remove('selected')); n?.classList.add('selected'); n?.scrollIntoView({block:'nearest'}); }
  else if (e.key === 'ArrowUp')   { e.preventDefault(); const p = items[Math.max(idx-1,0)]; items.forEach(i=>i.classList.remove('selected')); p?.classList.add('selected'); p?.scrollIntoView({block:'nearest'}); }
  else if (e.key === 'Enter')     { e.preventDefault(); sel?.click(); }
  else if (e.key === 'Escape')    { e.preventDefault(); closePalette(); }
});
cmdPaletteOverlay?.addEventListener('click', (e) => { if (e.target === cmdPaletteOverlay) closePalette(); });

// ════════════════════════════════════════
// TRAY INTEGRATION
// ════════════════════════════════════════
api.onTrayNewChat?.(() => btnNewChat.click());

// ════════════════════════════════════════
// FILE DRAG-AND-DROP
// ════════════════════════════════════════
const chatMain = document.getElementById('chat-main');

chatMain?.addEventListener('dragover', (e) => {
  e.preventDefault();
  e.stopPropagation();
  chatMain.classList.add('drag-over');
});

chatMain?.addEventListener('dragleave', (e) => {
  if (!chatMain.contains(e.relatedTarget)) chatMain.classList.remove('drag-over');
});

chatMain?.addEventListener('drop', async (e) => {
  e.preventDefault();
  e.stopPropagation();
  chatMain.classList.remove('drag-over');

  const files = [...(e.dataTransfer?.files || [])];
  if (!files.length) return;

  for (const file of files) {
    const isImage = file.type.startsWith('image/');
    if (isImage) {
      // Read image as data URL and show preview
      const reader = new FileReader();
      reader.onload = () => addPendingImage(file.name, reader.result);
      reader.readAsDataURL(file);
    } else {
      // Read text file content and inject into textarea
      try {
        const result = await api.readFileDrop(file.path || file.name);
        if (result?.error) {
          toast(`Could not read "${file.name}": ${result.error}`, 'error');
        } else {
          const ext  = file.name.split('.').pop() || '';
          const block = `\`\`\`${ext}\n// ${file.name}\n${result.content}\n\`\`\``;
          const cur  = chatInput.value;
          chatInput.value = cur ? cur + '\n\n' + block : block;
          autoResize();
          chatInput.focus();
        }
      } catch (_) {
        toast(`Failed to read "${file.name}"`, 'error');
      }
    }
  }
});

// ════════════════════════════════════════
// IMAGE PASTE & ATTACH
// ════════════════════════════════════════
let pendingImages = []; // [{name, dataUrl}]
const imgAttachStrip = document.getElementById('img-attach-strip');
const fileInput      = document.getElementById('file-input');
const btnAttach      = document.getElementById('btn-attach');

function addPendingImage(name, dataUrl) {
  const id = uid();
  pendingImages.push({ id, name, dataUrl });
  renderImgStrip();
  chatInput.focus();
}

function renderImgStrip() {
  if (!imgAttachStrip) return;
  if (!pendingImages.length) { imgAttachStrip.classList.add('hidden'); imgAttachStrip.innerHTML = ''; return; }
  imgAttachStrip.classList.remove('hidden');
  imgAttachStrip.innerHTML = '';
  for (const img of pendingImages) {
    const wrap = document.createElement('div');
    wrap.className = 'img-thumb-wrap';
    wrap.innerHTML = `<img class="img-thumb" src="${img.dataUrl}" alt="${escHtml(img.name)}" title="${escHtml(img.name)}"/>
      <button class="img-thumb-rm" data-id="${img.id}" title="Remove">&#10005;</button>`;
    wrap.querySelector('.img-thumb-rm').addEventListener('click', () => {
      pendingImages = pendingImages.filter(i => i.id !== img.id);
      renderImgStrip();
    });
    imgAttachStrip.appendChild(wrap);
  }
}

// Paste image from clipboard
chatInput?.addEventListener('paste', (e) => {
  const items = [...(e.clipboardData?.items || [])];
  const imageItems = items.filter(it => it.type.startsWith('image/'));
  if (!imageItems.length) return; // let normal text paste through
  e.preventDefault();
  for (const item of imageItems) {
    const file = item.getAsFile();
    if (!file) continue;
    const reader = new FileReader();
    reader.onload = () => addPendingImage(`pasted-image-${Date.now()}.png`, reader.result);
    reader.readAsDataURL(file);
  }
});

// Attach button → open file picker
btnAttach?.addEventListener('click', () => fileInput?.click());
fileInput?.addEventListener('change', async () => {
  const files = [...(fileInput.files || [])];
  fileInput.value = '';
  for (const file of files) {
    if (file.type.startsWith('image/')) {
      const reader = new FileReader();
      reader.onload = () => addPendingImage(file.name, reader.result);
      reader.readAsDataURL(file);
    } else {
      try {
        const result = await api.readFileDrop(file.path || file.name);
        if (result?.error) { toast(`Could not read "${file.name}": ${result.error}`, 'error'); continue; }
        const ext   = file.name.split('.').pop() || '';
        const block = `\`\`\`${ext}\n// ${file.name}\n${result.content}\n\`\`\``;
        const cur   = chatInput.value;
        chatInput.value = cur ? cur + '\n\n' + block : block;
        autoResize();
      } catch (_) { toast(`Failed to read "${file.name}"`, 'error'); }
    }
  }
});

// sendMessage is already updated above to handle pending images natively.

// Desktop notifications are handled inside setStreaming() directly (see above).

// ════════════════════════════════════════
// PIN / STAR CONVERSATIONS
// ════════════════════════════════════════
// renderConvList already exists — we extend it to show star button + pinned section

function renderConvList(list) {
  const src = list || conversations;
  convList.innerHTML = '';

  const pinned   = src.filter(c => c.pinned);
  const unpinned = src.filter(c => !c.pinned);

  // Date-group helpers
  const now   = Date.now();
  const DAY   = 86400000;
  const todayStart     = new Date(); todayStart.setHours(0,0,0,0);
  const yesterdayStart = new Date(todayStart - DAY);
  const week7Start     = new Date(todayStart - 6 * DAY);

  function convDate(c) {
    if (c.updated_at) return new Date(c.updated_at * 1000);
    return null;
  }
  function dateGroup(c) {
    const d = convDate(c);
    if (!d) return 'older';
    if (d >= todayStart)     return 'today';
    if (d >= yesterdayStart) return 'yesterday';
    if (d >= week7Start)     return 'week';
    return 'older';
  }
  const GROUP_LABELS = { today: 'Today', yesterday: 'Yesterday', week: 'Last 7 days', older: 'Older' };
  const GROUP_ORDER  = ['today', 'yesterday', 'week', 'older'];

  function buildItem(conv) {
    const li = document.createElement('li');
    li.dataset.convId = conv.id || '';
    if (conv.id && conv.id === currentConvId) li.classList.add('active');
    if (conv.pinned) li.classList.add('pinned');

    const titleSpan = document.createElement('span');
    titleSpan.className = 'conv-title';
    titleSpan.textContent = conv.title || 'Untitled';

    const starBtn = document.createElement('button');
    starBtn.className = `conv-star${conv.pinned ? ' pinned' : ''}`;
    starBtn.title = conv.pinned ? 'Unpin' : 'Pin conversation';
    starBtn.innerHTML = conv.pinned
      ? '<svg viewBox="0 0 16 16" fill="currentColor" width="11" height="11"><path d="M8 1l1.8 3.6 4 .6-2.9 2.8.7 4L8 10l-3.6 1.9.7-4L2.2 5.2l4-.6z"/></svg>'
      : '<svg viewBox="0 0 16 16" fill="none" width="11" height="11"><path d="M8 1l1.8 3.6 4 .6-2.9 2.8.7 4L8 10l-3.6 1.9.7-4L2.2 5.2l4-.6z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"/></svg>';

    starBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      if (!conv.id) return;
      conv.pinned = !conv.pinned;
      api.send({ type: 'pin_conversation', id: uid(), conversation_id: conv.id, pinned: conv.pinned });
      renderConvList();
      saveConvsToStorage();
    });

    li.appendChild(titleSpan);
    li.appendChild(starBtn);

    li.addEventListener('click', () => {
      currentConvId = conv.id;
      window._memoryPanelSeenConv = null;
      _convHasSentMessage = false;
      clearMessages();
      pendingImages = []; renderImgStrip();
      chatTitle.textContent = conv.title || 'Conversation';
      ctxBarFill && (ctxBarFill.style.width = '0%');
      if (conv.messages?.length) {
        for (const m of conv.messages) appendMessage(m.role === 'user' ? 'user' : 'assistant', m.content);
        markLastAssistant();
      } else {
        // Show spinner while loading from backend
        const spinner = document.createElement('div');
        spinner.className = 'messages-spinner';
        spinner.id = 'conv-switch-spinner';
        spinner.innerHTML = '<div class="spin-ring"></div>';
        messagesEl.appendChild(spinner);
        api.send({ type: 'get_conversation_messages', id: uid(), conversation_id: conv.id });
      }
      document.querySelectorAll('#conversation-list li').forEach(l => l.classList.remove('active'));
      li.classList.add('active');
      switchPage('chat');
    });

    // Right-click context menu
    li.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      e.stopPropagation();
      showConvContextMenu(e.clientX, e.clientY, conv, li);
    });

    return li;
  }

  function appendSection(label) {
    const hdr = document.createElement('li');
    hdr.className = 'conv-section-hdr';
    hdr.textContent = label;
    convList.appendChild(hdr);
  }

  // Pinned section
  if (pinned.length) {
    appendSection('Pinned');
    pinned.forEach(c => convList.appendChild(buildItem(c)));
  }

  // Date-grouped unpinned sections
  const groups = {};
  for (const c of unpinned) {
    const g = dateGroup(c);
    (groups[g] = groups[g] || []).push(c);
  }

  // If all convs lack updated_at, skip headers and just list them
  const hasDateInfo = unpinned.some(c => c.updated_at);
  if (hasDateInfo) {
    for (const g of GROUP_ORDER) {
      if (groups[g]?.length) {
        appendSection(GROUP_LABELS[g]);
        groups[g].forEach(c => convList.appendChild(buildItem(c)));
      }
    }
  } else {
    if (pinned.length && unpinned.length) appendSection('Recent');
    unpinned.forEach(c => convList.appendChild(buildItem(c)));
  }
}

// Handle backend pin ack (update pinned state from server)
// (The ConversationPinned response is handled in the backend message handler below)

// ════════════════════════════════════════
// AUTO-START SETTING
// ════════════════════════════════════════
const toggleAutostart = document.getElementById('s-autostart');
if (toggleAutostart) {
  // Get current state
  api.getLoginItem?.().then(enabled => { toggleAutostart.checked = !!enabled; }).catch(() => {});
  // Listen for state pushed on load
  api.onLoginItemState?.((enabled) => { toggleAutostart.checked = !!enabled; });
  toggleAutostart.addEventListener('change', () => {
    api.toggleLoginItem?.(toggleAutostart.checked);
  });
}

// ════════════════════════════════════════
// KEYBOARD SHORTCUT: Ctrl+, → Settings
// ════════════════════════════════════════
document.addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === ',') {
    e.preventDefault();
    switchPage('settings');
  }
});

// ════════════════════════════════════════
// PROMPT TEMPLATES  (type / in empty input)
// ════════════════════════════════════════
const TEMPLATES = [
  { trigger: '/explain',  label: 'Explain code',      text: 'Please explain the following code:\n\n```\n\n```' },
  { trigger: '/debug',    label: 'Debug error',        text: 'I\'m getting this error:\n\n```\n\n```\n\nPlease help me fix it.' },
  { trigger: '/tests',    label: 'Write tests',        text: 'Write comprehensive unit tests for:\n\n```\n\n```' },
  { trigger: '/review',   label: 'Code review',        text: 'Please review this code for bugs, style, and improvements:\n\n```\n\n```' },
  { trigger: '/summarize',label: 'Summarize',          text: 'Please summarize the following:\n\n' },
  { trigger: '/improve',  label: 'Improve writing',    text: 'Please improve the clarity and quality of this writing:\n\n' },
  { trigger: '/refactor', label: 'Refactor code',      text: 'Refactor the following code to be cleaner and more maintainable:\n\n```\n\n```' },
  { trigger: '/docs',     label: 'Generate docs',      text: 'Write clear documentation for the following:\n\n```\n\n```' },
];

let templateMenu = null;

function showTemplateMenu() {
  hideTemplateMenu();
  const menu = document.createElement('div');
  menu.id = 'template-menu';
  menu.className = 'template-menu';
  TEMPLATES.forEach(t => {
    const item = document.createElement('button');
    item.className = 'template-item';
    item.innerHTML = `<span class="tmpl-trigger">${escHtml(t.trigger)}</span><span class="tmpl-label">${escHtml(t.label)}</span>`;
    item.addEventListener('click', () => {
      chatInput.value = t.text;
      autoResize();
      chatInput.focus();
      // Place cursor at a sensible position (end of first line with placeholder)
      const pos = t.text.indexOf('\n\n') + 2;
      chatInput.setSelectionRange(pos, pos);
      hideTemplateMenu();
    });
    menu.appendChild(item);
  });
  // Position above the input
  const inputRect = document.getElementById('input-box').getBoundingClientRect();
  menu.style.bottom = (window.innerHeight - inputRect.top + 6) + 'px';
  menu.style.left   = inputRect.left + 'px';
  document.body.appendChild(menu);
  templateMenu = menu;
}

function hideTemplateMenu() {
  templateMenu?.remove();
  templateMenu = null;
}

chatInput?.addEventListener('input', () => {
  if (chatInput.value === '/') {
    showTemplateMenu();
  } else if (templateMenu && !chatInput.value.startsWith('/')) {
    hideTemplateMenu();
  }
});
document.addEventListener('click', (e) => {
  if (templateMenu && !templateMenu.contains(e.target) && e.target !== chatInput) hideTemplateMenu();
});
chatInput?.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && templateMenu) { e.stopPropagation(); hideTemplateMenu(); }
});

// ════════════════════════════════════════
// CONVERSATION CONTEXT MENU
// ════════════════════════════════════════
let _ctxMenu = null;

function closeConvContextMenu() {
  _ctxMenu?.remove();
  _ctxMenu = null;
}

function showConvContextMenu(x, y, conv, liEl) {
  closeConvContextMenu();
  const menu = document.createElement('div');
  menu.className = 'conv-ctx-menu';
  _ctxMenu = menu;

  function addItem(label, cls, action) {
    const btn = document.createElement('button');
    btn.className = `conv-ctx-item${cls ? ' ' + cls : ''}`;
    btn.textContent = label;
    btn.addEventListener('click', () => { closeConvContextMenu(); action(); });
    menu.appendChild(btn);
  }
  function addSep() {
    const sep = document.createElement('div');
    sep.className = 'conv-ctx-sep';
    menu.appendChild(sep);
  }

  addItem(conv.pinned ? 'Unpin' : 'Pin', '', () => {
    conv.pinned = !conv.pinned;
    api.send({ type: 'pin_conversation', id: uid(), conversation_id: conv.id, pinned: conv.pinned });
    renderConvList(); saveConvsToStorage();
  });

  addItem('Rename', '', () => {
    const newTitle = window.prompt('Rename conversation:', conv.title || '');
    if (newTitle === null || !newTitle.trim()) return;
    conv.title = newTitle.trim();
    api.send({ type: 'rename_conversation', id: uid(), conversation_id: conv.id, title: conv.title });
    renderConvList(); saveConvsToStorage();
    if (conv.id === currentConvId) chatTitle.textContent = conv.title;
  });

  addSep();

  addItem('Delete', 'danger', async () => {
    const confirmed = await showConfirmModal({
      title: 'Delete conversation?',
      body:  `"${conv.title || 'This conversation'}" will be permanently deleted. This cannot be undone.`,
      okLabel: 'Delete',
    });
    if (!confirmed) return;
    api.send({ type: 'delete_conversation', id: uid(), conversation_id: conv.id });
    conversations = conversations.filter(c => c.id !== conv.id);
    if (currentConvId === conv.id) {
      currentConvId = null;
      clearMessages();
      chatTitle.textContent = 'New Conversation';
      ctxBarFill && (ctxBarFill.style.width = '0%');
    }
    renderConvList(); saveConvsToStorage();
  });

  // Position near cursor, keep within viewport
  document.body.appendChild(menu);
  const mw = menu.offsetWidth  || 170;
  const mh = menu.offsetHeight || 120;
  menu.style.left = Math.min(x, window.innerWidth  - mw - 8) + 'px';
  menu.style.top  = Math.min(y, window.innerHeight - mh - 8) + 'px';
}

document.addEventListener('click',       closeConvContextMenu, true);
document.addEventListener('contextmenu', (e) => { if (!e.target.closest('.conv-ctx-menu')) closeConvContextMenu(); }, true);
document.addEventListener('keydown',     (e) => { if (e.key === 'Escape') closeConvContextMenu(); }, true);

// ════════════════════════════════════════
// EDIT USER MESSAGE + RESEND
// ════════════════════════════════════════
function startEditUserMessage(wrap, bub, originalText) {
  if (streaming) return;

  const textarea = document.createElement('textarea');
  textarea.className = 'msg-edit-input';
  textarea.value = originalText;
  textarea.rows = Math.max(2, (originalText.match(/\n/g) || []).length + 1);

  const actions = document.createElement('div');
  actions.className = 'msg-edit-actions';

  const saveBtn   = document.createElement('button');
  saveBtn.className = 'msg-edit-save';
  saveBtn.textContent = 'Submit';

  const cancelBtn = document.createElement('button');
  cancelBtn.className = 'msg-edit-cancel';
  cancelBtn.textContent = 'Cancel';

  actions.appendChild(cancelBtn);
  actions.appendChild(saveBtn);

  // Hide original bubble, show edit UI
  bub.style.display = 'none';
  wrap.insertBefore(textarea, bub);
  wrap.insertBefore(actions, bub);
  wrap.querySelector('.msg-edit-btn')?.style && (wrap.querySelector('.msg-edit-btn').style.display = 'none');
  textarea.focus();
  textarea.setSelectionRange(textarea.value.length, textarea.value.length);

  function cancelEdit() {
    textarea.remove();
    actions.remove();
    bub.style.display = '';
    wrap.querySelector('.msg-edit-btn') && (wrap.querySelector('.msg-edit-btn').style.display = '');
  }

  function submitEdit() {
    const newText = textarea.value.trim();
    if (!newText) { cancelEdit(); return; }

    // Remove this message wrap and everything after it from the DOM
    const allMsgs = [...messagesEl.querySelectorAll('.message')];
    const idx = allMsgs.indexOf(wrap);
    if (idx !== -1) {
      allMsgs.slice(idx).forEach(m => m.remove());
    }
    hideEmptyState();

    // Re-append the user bubble with new text
    const newWrap = appendMessage('user', newText);
    chatInput.value = '';
    autoResize();

    // Send as a new chat request in the same conversation
    const chatId = uid();
    inFlightChatId = chatId;
    streamingConvId = currentConvId;
    const payload = { type: 'chat', id: chatId, conversation_id: currentConvId || undefined, message: newText, model: modelSelect.value || undefined, agent_mode: currentMode };
    const sysPr = document.getElementById('s-system-prompt')?.value?.trim();
    if (sysPr) payload.system_prompt = sysPr;
    const tempVal = parseFloat(document.getElementById('s-temperature')?.value ?? '0.7');
    if (!isNaN(tempVal)) payload.temperature = tempVal;
    const maxTok = parseInt(document.getElementById('s-max-tokens')?.value ?? '4096', 10);
    if (!isNaN(maxTok)) payload.max_tokens = maxTok;
    api.send(payload);
    setStreaming(true);
  }

  cancelBtn.addEventListener('click', cancelEdit);
  saveBtn.addEventListener('click',   submitEdit);
  textarea.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); submitEdit(); }
    if (e.key === 'Escape')               { e.preventDefault(); cancelEdit(); }
  });
}

// ════════════════════════════════════════
// SETTINGS: system prompt + sliders
// ════════════════════════════════════════
(function initChatSettings() {
  const sysPromptEl  = document.getElementById('s-system-prompt');
  const tempEl       = document.getElementById('s-temperature');
  const tempNumEl    = document.getElementById('s-temperature-num');
  const maxTokEl     = document.getElementById('s-max-tokens');
  const maxTokNumEl  = document.getElementById('s-max-tokens-num');

  // Restore from localStorage
  const saved = JSON.parse(localStorage.getItem('rook-chat-settings') || '{}');
  if (sysPromptEl && saved.systemPrompt !== undefined) sysPromptEl.value = saved.systemPrompt;
  if (tempEl && saved.temperature !== undefined) {
    tempEl.value = saved.temperature;
    if (tempNumEl) tempNumEl.value = parseFloat(saved.temperature).toFixed(2);
  }
  if (maxTokEl && saved.maxTokens !== undefined) {
    maxTokEl.value = saved.maxTokens;
    if (maxTokNumEl) maxTokNumEl.value = saved.maxTokens;
  }

  function saveChatSettings() {
    localStorage.setItem('rook-chat-settings', JSON.stringify({
      systemPrompt: sysPromptEl?.value || '',
      temperature:  tempEl?.value    || '0.7',
      maxTokens:    maxTokEl?.value  || '4096',
    }));
  }

  sysPromptEl?.addEventListener('input', saveChatSettings);

  // Temperature — slider drives number input and vice versa
  tempEl?.addEventListener('input', () => {
    const v = parseFloat(tempEl.value).toFixed(2);
    if (tempNumEl) tempNumEl.value = v;
    saveChatSettings();
  });
  tempNumEl?.addEventListener('input', () => {
    const v = Math.min(2, Math.max(0, parseFloat(tempNumEl.value) || 0));
    if (tempEl) tempEl.value = v;
    saveChatSettings();
  });
  tempNumEl?.addEventListener('change', () => {
    // Clamp and format on blur/change
    const v = Math.min(2, Math.max(0, parseFloat(tempNumEl.value) || 0));
    tempNumEl.value = v.toFixed(2);
    if (tempEl) tempEl.value = v;
    saveChatSettings();
  });

  // Max tokens — slider drives number input and vice versa
  maxTokEl?.addEventListener('input', () => {
    if (maxTokNumEl) maxTokNumEl.value = maxTokEl.value;
    saveChatSettings();
  });
  maxTokNumEl?.addEventListener('input', () => {
    const v = Math.min(16384, Math.max(256, parseInt(maxTokNumEl.value, 10) || 256));
    if (maxTokEl) maxTokEl.value = v;
    saveChatSettings();
  });
  maxTokNumEl?.addEventListener('change', () => {
    const v = Math.round(Math.min(16384, Math.max(256, parseInt(maxTokNumEl.value, 10) || 256)) / 256) * 256;
    maxTokNumEl.value = v;
    if (maxTokEl) maxTokEl.value = v;
    saveChatSettings();
  });
})();

// ════════════════════════════════════════
// SIDEBAR KEYBOARD NAVIGATION
// ════════════════════════════════════════
(function initSidebarKeyNav() {
  let kbIdx = -1;

  function getItems() {
    return [...convList.querySelectorAll('li[data-conv-id]')];
  }
  function setKbFocus(idx) {
    const items = getItems();
    items.forEach(li => li.classList.remove('kb-focus'));
    kbIdx = Math.max(0, Math.min(idx, items.length - 1));
    const target = items[kbIdx];
    if (target) { target.classList.add('kb-focus'); target.scrollIntoView({ block: 'nearest' }); }
  }

  document.addEventListener('keydown', (e) => {
    // Only handle if focus is somewhere in the sidebar (or nowhere specific)
    const inSidebar = e.target.closest('#chat-sidebar');
    const inInput   = e.target === chatInput || e.target.closest('#input-area');
    if (inInput || e.target.closest('.conv-ctx-menu') || e.target.closest('#cmd-palette-overlay')) return;
    if (!inSidebar && e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;

    const items = getItems();
    if (!items.length) return;

    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setKbFocus(kbIdx < 0 ? 0 : kbIdx + 1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setKbFocus(kbIdx <= 0 ? 0 : kbIdx - 1);
    } else if (e.key === 'Enter' && kbIdx >= 0 && inSidebar) {
      e.preventDefault();
      items[kbIdx]?.click();
    }
  });

  // Reset kb index when user clicks a conversation
  convList.addEventListener('click', () => { kbIdx = -1; convList.querySelectorAll('.kb-focus').forEach(l => l.classList.remove('kb-focus')); });
})();

// ════════════════════════════════════════
// FOLDER INDEX BUTTON
// ════════════════════════════════════════
document.getElementById('btn-index-folder')?.addEventListener('click', async () => {
  const folder = await api.pickFolder?.();
  if (!folder) return;
  const name = folder.split(/[\\/]/).filter(Boolean).pop() || folder;
  const bar    = document.getElementById('index-folder-bar');
  const nameEl = document.getElementById('index-folder-name');
  const dot    = document.getElementById('index-folder-status');
  if (nameEl) nameEl.textContent = name;
  if (bar)    bar.classList.remove('hidden');
  if (dot)    dot.className = 'index-status-dot'; // pulsing
  api.send({ type: 'index_directory', id: uid(), path: folder });
});
