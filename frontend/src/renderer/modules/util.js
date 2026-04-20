// Utility helpers 

/** Build a collapsible thinking/reasoning card. */
export function buildThinkingCard(thinking) {
  return `<details class="thought-block"><summary class="thought-summary"><span class="thought-label">Thought</span></summary><div class="thought-body">${esc(thinking.trim())}</div></details>`;
}

export function uid() { return crypto.randomUUID(); }

export function esc(s) { return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }

export function escHtml(s) {
  return String(s || '').replace(/[&<>"']/g, c => ({
    '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'
  }[c]));
}

/** Build a collapsible tool-call card (details/summary). */
export function buildToolCard(name, args) {
  const TOOL_META = {
    terminal_execute: { cls: 'tc--terminal', tag: 'SHL', label: 'exec',    primary: a => a.command ?? a.cmd },
    run_command:      { cls: 'tc--terminal', tag: 'SHL', label: 'run',     primary: a => a.command ?? a.cmd },
    bash:             { cls: 'tc--terminal', tag: 'SHL', label: 'bash',    primary: a => a.command ?? a.input },
    shell:            { cls: 'tc--terminal', tag: 'SHL', label: 'shell',   primary: a => a.command },
    code_edit:        { cls: 'tc--code',     tag: 'EDT', label: 'edit',    primary: a => a.path ?? a.file },
    edit_file:        { cls: 'tc--code',     tag: 'EDT', label: 'edit',    primary: a => a.path ?? a.file },
    write_file:       { cls: 'tc--file',     tag: 'WRT', label: 'write',   primary: a => a.path ?? a.file },
    read_file:        { cls: 'tc--file',     tag: 'RDR', label: 'read',    primary: a => a.path ?? a.file },
    web_search:       { cls: 'tc--search',   tag: 'WEB', label: 'search',  primary: a => a.query ?? a.q },
    search:           { cls: 'tc--search',   tag: 'WEB', label: 'search',  primary: a => a.query ?? a.q },
    store_memory:     { cls: 'tc--memory',   tag: 'MEM', label: 'store',   primary: a => a.key ?? a.id },
    save_memory:      { cls: 'tc--memory',   tag: 'MEM', label: 'save',    primary: a => a.key ?? (a.content ? String(a.content).slice(0, 60) : null) },
    git_status:       { cls: 'tc--terminal', tag: 'GIT', label: 'status',  primary: () => null },
    git_diff:         { cls: 'tc--terminal', tag: 'GIT', label: 'diff',    primary: a => a.file ?? null },
    git_log:          { cls: 'tc--terminal', tag: 'GIT', label: 'log',     primary: a => a.count ? `-${a.count}` : null },
    git_branch:       { cls: 'tc--terminal', tag: 'GIT', label: 'branch',  primary: () => null },
    fetch_url:        { cls: 'tc--search',   tag: 'WEB', label: 'fetch',   primary: a => a.url },
    browser_navigate: { cls: 'tc--search',   tag: 'WEB', label: 'nav',     primary: a => a.url },
    browser_click:    { cls: 'tc--code',     tag: 'DOM', label: 'click',   primary: a => a.selector },
    browser_type:     { cls: 'tc--code',     tag: 'DOM', label: 'type',    primary: a => a.text },
    browser_evaluate: { cls: 'tc--code',     tag: 'DOM', label: 'eval',    primary: a => a.js },
    store_user_fact:  { cls: 'tc--memory',   tag: 'MEM', label: 'fact',    primary: a => a.key ? `${a.key} = ${a.value}` : null },
    list_files:       { cls: 'tc--file',     tag: 'DIR', label: 'list',    primary: a => a.path || '.' },
    change_dir:       { cls: 'tc--file',     tag: 'DIR', label: 'cd',      primary: a => a.path },
    get_cwd:          { cls: 'tc--file',     tag: 'DIR', label: 'pwd',     primary: () => null },
    glob:             { cls: 'tc--search',   tag: 'GLB', label: 'glob',    primary: a => a.pattern },
    grep:             { cls: 'tc--search',   tag: 'GRP', label: 'grep',    primary: a => a.pattern },
    todo_write:       { cls: 'tc--memory',   tag: 'TSK', label: 'tasks',   primary: a => `${(a.todos || []).length} items` },
    file_read:        { cls: 'tc--file',     tag: 'RDR', label: 'read',    primary: a => a.path ?? a.file },
    // Persistent shells
    shell_spawn:      { cls: 'tc--terminal', tag: 'SHL', label: 'spawn',   primary: a => a.name },
    shell_exec:       { cls: 'tc--terminal', tag: 'SHL', label: 'exec',    primary: a => a.command },
    shell_read:       { cls: 'tc--terminal', tag: 'SHL', label: 'read',    primary: a => a.shell || 'default' },
    shell_kill:       { cls: 'tc--terminal', tag: 'SHL', label: 'kill',    primary: a => a.name || 'default' },
    shell_list:       { cls: 'tc--terminal', tag: 'SHL', label: 'shells',  primary: () => null },
    // Code intelligence
    outline_file:     { cls: 'tc--code',     tag: 'EDT', label: 'outline', primary: a => a.path },
    file_diff:        { cls: 'tc--file',     tag: 'DIF', label: 'diff',    primary: a => a.path },
    file_undo:        { cls: 'tc--file',     tag: 'UND', label: 'undo',    primary: a => a.path },
    search_in_file:   { cls: 'tc--search',   tag: 'GRP', label: 'search',  primary: a => a.pattern },
    apply_patch:      { cls: 'tc--code',     tag: 'EDT', label: 'patch',   primary: a => a.path },
  };

  const key = name.toLowerCase().replace(/-/g, '_');
  let meta = TOOL_META[key];
  if (!meta) {
    for (const [k, v] of Object.entries(TOOL_META)) {
      if (key.includes(k.split('_')[0])) { meta = v; break; }
    }
    meta = meta ?? { cls: '', tag: 'ACT', label: name.replace(/_/g, ' '), primary: () => null };
  }

  const primaryVal = meta.primary(args);
  const primaryStr = primaryVal != null ? String(primaryVal) : null;
  const displayPrimary = primaryStr
    ? (primaryStr.length > 68 ? primaryStr.slice(0, 68) + '…' : primaryStr)
    : null;

  // Body: all args as key/value rows
  const bodyRows = Object.entries(args).map(([k, v]) => {
    const val = typeof v === 'string' ? v : JSON.stringify(v, null, 2);
    return `<div class="tc-row"><span class="tc-key">${esc(k)}</span><pre class="tc-val">${esc(val)}</pre></div>`;
  }).join('');

  const hasBody = Object.keys(args).length > 0;

  return `<details class="tool-call ${meta.cls}"><summary class="tc-head"><span class="tc-tag">${esc(meta.tag)}</span><span class="tc-verb">${esc(meta.label)}</span>${displayPrimary ? `<span class="tc-primary">${esc(displayPrimary)}</span>` : ''}<span class="tc-chev">›</span></summary>${hasBody ? `<div class="tc-body">${bodyRows}</div>` : ''}</details>`;
}

/**
 * Extract [[TOOL:name:{json}]] and [[TERR:name]] markers using a brace-balanced
 * parser so nested JSON works. Returns { stripped, cards } where stripped has
 * placeholders that survive HTML-escaping (\x01 + index + \x01).
 */
function extractToolCalls(text) {
  const cards = [];
  let out = '';
  let i = 0;

  while (i < text.length) {
    // Find the next marker of either kind, whichever comes first.
    const nextTool = text.indexOf('[[TOOL:', i);
    const nextTerr = text.indexOf('[[TERR:', i);

    // Determine which marker (if any) comes first
    let nextPos = -1, isTerr = false;
    if (nextTool === -1 && nextTerr === -1) { out += text.slice(i); break; }
    else if (nextTool === -1)               { nextPos = nextTerr; isTerr = true; }
    else if (nextTerr === -1)               { nextPos = nextTool; isTerr = false; }
    else if (nextTerr < nextTool)           { nextPos = nextTerr; isTerr = true; }
    else                                    { nextPos = nextTool; isTerr = false; }

    // Flush text before this marker
    out += text.slice(i, nextPos);
    i = nextPos;

    if (isTerr) {
      // [[TERR:toolname]] - error badge for the immediately preceding tool card
      const match = text.slice(i).match(/^\[\[TERR:([^\]]*)\]\]/);
      if (match) {
        const toolName = match[1];
        const ph = `\x01TC${cards.length}\x01`;
        cards.push(`<span class="tc-err-pill" title="Tool '${esc(toolName)}' returned an error">⚠ error</span>`);
        out += ph;
        i += match[0].length;
      } else {
        // Incomplete marker (streaming) - leave rest as-is
        out += text.slice(i);
        break;
      }
    } else {
      // [[TOOL:name:{json}]] - brace-balanced JSON parse
      const nameStart = i + 7; // after '[[TOOL:'
      let nameEnd = nameStart;
      while (nameEnd < text.length && text[nameEnd] !== ':' && text[nameEnd] !== ']') nameEnd++;
      if (text[nameEnd] !== ':' || text[nameEnd + 1] !== '{') {
        out += text.slice(i, i + 7);
        i += 7;
        continue;
      }
      const name = text.slice(nameStart, nameEnd);
      // Walk brace-balanced JSON
      let depth = 0, inStr = false, escNext = false;
      let j = nameEnd + 1;
      let closed = false;
      for (; j < text.length; j++) {
        const c = text[j];
        if (escNext) { escNext = false; continue; }
        if (c === '\\') { escNext = true; continue; }
        if (c === '"') { inStr = !inStr; continue; }
        if (inStr) continue;
        if (c === '{') depth++;
        else if (c === '}') { depth--; if (depth === 0) { j++; closed = true; break; } }
      }
      if (!closed || text.slice(j, j + 2) !== ']]') {
        // Incomplete (still streaming) - stop scanning
        out += text.slice(i);
        break;
      }
      const argsStr = text.slice(nameEnd + 1, j);
      let html;
      try { html = buildToolCard(name, JSON.parse(argsStr)); }
      catch { html = `<details class="tool-call"><summary class="tc-head"><span class="tc-tag">ACT</span><span class="tc-verb">${esc(name)}</span><span class="tc-chev">›</span></summary></details>`; }
      const ph = `\x01TC${cards.length}\x01`;
      cards.push(html);
      out += ph;
      i = j + 2;
    }
  }
  return { stripped: out, cards };
}

/** DOMPurify config - allows formatting tags, code blocks, and our custom card elements */
const PURIFY_CONFIG = {
  ADD_TAGS: ['details', 'summary', 'del', 'blockquote', 'hr', 'table', 'thead', 'tbody', 'tr', 'th', 'td', 'input'],
  ADD_ATTR: ['class', 'title', 'open', 'data-lang', 'data-n', 'target', 'rel', 'href', 'type', 'checked', 'disabled'],
  FORBID_TAGS: ['script', 'style', 'iframe', 'object', 'embed', 'form'],
  FORBID_ATTR: ['onerror', 'onload', 'onclick', 'onmouseover', 'onfocus', 'onblur'],
};

/** Render markdown to HTML and sanitize to prevent XSS. */
export function safeMarkdown(text) {
  const raw = renderMarkdown(text);
  // DOMPurify loaded from CDN as a global
  if (typeof DOMPurify !== 'undefined') return DOMPurify.sanitize(raw, PURIFY_CONFIG);
  return raw; // fallback if CDN blocked (esc() still provides basic defense)
}

export function renderMarkdown(text) {
  // 0. Extract <thinking>...</thinking> blocks into collapsible cards.
  const thinkCards = [];
  text = text.replace(/<thinking>([\s\S]*?)<\/thinking>/gi, (_, inner) => {
    const idx = thinkCards.length;
    thinkCards.push(buildThinkingCard(inner));
    return `\x01TH${idx}\x01`;
  });

  // 0b. Convert LLM-generated HTML to markdown before esc() runs.
  //     Some models output HTML tags directly; without this they render as
  //     literal &lt;br&gt; etc. after escaping.
  if (/<(?:br|p|ol|ul|li|strong|em|h[1-6])\b/i.test(text)) {
    text = text
      .replace(/<br\s*\/?>/gi, '\n')
      .replace(/<strong>([\s\S]*?)<\/strong>/gi, '**$1**')
      .replace(/<em>([\s\S]*?)<\/em>/gi, '*$1*')
      .replace(/<li[^>]*data-n="(\d+)"[^>]*>([\s\S]*?)<\/li>/gi, '$1. $2\n')
      .replace(/<li[^>]*>([\s\S]*?)<\/li>/gi, '- $1\n')
      .replace(/<h([1-6])[^>]*>([\s\S]*?)<\/h\1>/gi, (_, l, c) => '#'.repeat(+l) + ' ' + c + '\n')
      .replace(/<\/?(?:ol|ul|p|div)[^>]*>/gi, '\n')
      .replace(/<[^>]+>/g, '');  // strip any remaining unknown tags
  }

  // 1. Extract [[TOOL:name:{...}]] and [[TERR:name]] markers BEFORE escaping.
  const { stripped, cards: toolCards } = extractToolCalls(text);
  text = stripped;

 // Point 5: esc() runs first - AI text is HTML-escaped before we inject our own tags 
  let h = esc(text);

 // Code blocks: extracted first so inner content is never transformed 
  // Point 3: language label uses an icon map instead of raw text
  const LANG_ICON = {
    js: '⬡ JS', javascript: '⬡ JS', ts: '⬡ TS', typescript: '⬡ TS',
    rust: '⚙ RS', rs: '⚙ RS', c: '© C', cpp: '© C++', 'c++': '© C++',
    py: '🐍 PY', python: '🐍 PY', go: '◈ GO', java: '☕ JAVA',
    sh: '$ SH', bash: '$ SH', shell: '$ SH', zsh: '$ ZSH',
    json: '{ } JSON', yaml: '- YAML', toml: '- TOML',
    html: '‹/› HTML', css: '# CSS', sql: '⊞ SQL',
    asm: '⊕ ASM', assembly: '⊕ ASM',
  };
  h = h.replace(/```(\w*)\n?([\s\S]*?)```/g, (_, l, c) => {
    const lang = l.toLowerCase();
    const langClass = l ? `lang-${l} language-${l}` : 'language-plaintext';
    const icon = l ? (LANG_ICON[lang] || l.toUpperCase()) : '';
    const label = icon ? `<span class="code-lang">${icon}</span>` : '';
    return `<div class="code-wrap">${label}<pre><code class="${langClass}">${c.trimEnd()}</code></pre><button class="copy-btn" title="Copy code">⎘</button></div>`;
  });

 // Inline code 
  h = h.replace(/`([^`\n]+)`/g, '<code>$1</code>');

 // Point 4: Hex/memory address highlighting (outside code blocks) 
  h = h.replace(/(0x[0-9a-fA-F]{2,})\b/g, '<span class="hex-addr">$1</span>');

 // Inline formatting 
  h = h.replace(/\*\*\*(.+?)\*\*\*/g, '<strong><em>$1</em></strong>');
  h = h.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
  h = h.replace(/\*(.+?)\*/g, '<em>$1</em>');
  h = h.replace(/~~(.+?)~~/g, '<del>$1</del>');
  // Named links before bare URLs to avoid double-wrapping
  h = h.replace(/\[([^\]]+)\]\((https?:\/\/[^\)]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
  h = h.replace(/(?<![="'>])(https?:\/\/[^\s<>"']+)/g, '<a href="$1" target="_blank" rel="noopener noreferrer">$1</a>');

 // Headings H1-H6 
  h = h.replace(/^#{6} (.+)$/gm, '<h6>$1</h6>');
  h = h.replace(/^#{5} (.+)$/gm, '<h5>$1</h5>');
  h = h.replace(/^#{4} (.+)$/gm, '<h4>$1</h4>');
  h = h.replace(/^### (.+)$/gm,  '<h3>$1</h3>');
  h = h.replace(/^## (.+)$/gm,   '<h2>$1</h2>');
  h = h.replace(/^# (.+)$/gm,    '<h1>$1</h1>');

 // Horizontal rule 
  h = h.replace(/^(?:-{3,}|\*{3,}|_{3,})$/gm, '<hr>');

 // Blockquotes - multi-line runs of &gt;-prefixed lines 
  h = h.replace(/(^&gt; [^\n]*(\n&gt; [^\n]*)*)/gm, m => {
    const inner = m.replace(/^&gt; /gm, '');
    return `<blockquote>${inner.trim()}</blockquote>`;
  });

 // Point 1: Checklist items - processed BEFORE generic list conversion 
  // [ ] unchecked  →  interactive checkbox li
  // [x] / [X] checked  →  checked checkbox li
  h = h.replace(/^[-*] \[( |x|X)\] (.+)$/gm, (_, state, content) => {
    const checked = state !== ' ' ? 'checked' : '';
    return `<li class="chk-item"><input type="checkbox" class="md-check" ${checked} disabled> <span class="${checked ? 'chk-done' : ''}">${content}</span></li>`;
  });
  h = h.replace(/(<li class="chk-item">.*<\/li>(\n)?)+/g, m => `<ul class="chk-list">${m}</ul>`);

 // Ordered lists 
  // Point 1 fix: use [^\n]+ (single line) instead of [\s\S]*? to prevent cross-list merging
  h = h.replace(/^(\d+)\. ([^\n]+)$/gm, '<li class="ol-item" data-n="$1">$2</li>');
  h = h.replace(/(<li class="ol-item"[^>]*>[^\n]*<\/li>\n?)+/g, m => {
    const count = (m.match(/<li class="ol-item"/g) || []).length;
    const list = `<ol>${m}</ol>`;
    return count >= 6
      ? `<details class="list-fold"><summary>${count} items</summary>${list}</details>`
      : list;
  });

 // Unordered lists 
  // Point 1 fix: [^\n]+ prevents cross-list merging across blank lines
  h = h.replace(/^[-*] ([^\n]+)$/gm, '<li>$1</li>');
  h = h.replace(/(<li>[^\n]*<\/li>\n?)+/g, m => {
    const lines = m.trim().split('\n').filter(l => l.trim());
    const count = lines.length;

    // Point 2: Hybrid KV-Grid - fire if ≥50% of items are Key: Value
    const kvBoldRe  = /^<li><strong>([^<]+)<\/strong>:\s*(.+)<\/li>$/;
    const kvPlainRe = /^<li>([^:<\n]{1,40}):\s+(.+)<\/li>$/;
    const kvCount = lines.filter(l => kvBoldRe.test(l) || kvPlainRe.test(l)).length;
    if (count >= 2 && kvCount / count >= 0.5) {
      const rows = lines.map(line => {
        const m2 = line.match(kvBoldRe) || line.match(kvPlainRe);
        if (m2) return `<div class="kv-key">${m2[1]}</div><div class="kv-val">${m2[2]}</div>`;
        // Non-KV item spans both columns
        const inner = line.replace(/^<li>/, '').replace(/<\/li>$/, '');
        return `<div class="kv-span">${inner}</div>`;
      }).join('');
      return `<div class="kv-grid">${rows}</div>`;
    }

    const list = `<ul>${m}</ul>`;
    return count >= 6
      ? `<details class="list-fold"><summary>${count} items</summary>${list}</details>`
      : list;
  });

 // Tables 
  h = h.replace(/(^\|.+\|\n\|[-| :]+\|\n(?:\|.+\|(?:\n|$))+)/gm, tableBlock => {
    const rows = tableBlock.trim().split('\n');
    const parseCells = row => row.split('|').filter((_, i, a) => i > 0 && i < a.length - 1).map(c => c.trim());
    const headers = parseCells(rows[0]).map(c => `<th>${c}</th>`).join('');
    const bodyRows = rows.slice(2).map(row =>
      `<tr>${parseCells(row).map(c => `<td>${c}</td>`).join('')}</tr>`
    ).join('');
    return `<div class="md-table-wrap"><table class="md-table"><thead><tr>${headers}</tr></thead><tbody>${bodyRows}</tbody></table></div>`;
  });

 // Line breaks - skip lines that already start a block element 
  h = h.replace(/\n(?!<(?:\/?(?:pre|ul|ol|li|h[1-6]|div|details|summary|blockquote|hr|table|thead|tbody|tr|th|td)))/g, '<br>');

  // 2. Restore tool-call cards and thinking cards.
  toolCards.forEach((card, i) => { h = h.replace(`\x01TC${i}\x01`, card); });
  thinkCards.forEach((card, i) => { h = h.replace(`\x01TH${i}\x01`, card); });

  // 3. Batch consecutive tool cards into a single collapsible group.
  //    Matches runs of 2+ <details class="tool-call ..."> blocks (with optional
  //    <br>, whitespace, or tc-err-pill badges between them).
  h = h.replace(
    /(<details class="tool-call[^"]*">[\s\S]*?<\/details>(?:\s*(?:<br>|<span class="tc-err-pill"[^>]*>[^<]*<\/span>)\s*)*)(<details class="tool-call[^"]*">[\s\S]*?<\/details>(?:\s*(?:<br>|<span class="tc-err-pill"[^>]*>[^<]*<\/span>)\s*)*)+/g,
    match => {
      // Count individual tool cards in the run
      const count = (match.match(/<details class="tool-call/g) || []).length;
      return `<details class="tc-batch"><summary class="tc-batch-head"><span class="tc-batch-tag">OPS</span><span class="tc-batch-label">Called ${count} tools</span><span class="tc-chev">›</span></summary><div class="tc-batch-body">${match}</div></details>`;
    }
  );

  return h;
}
