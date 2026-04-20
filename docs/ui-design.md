# Rook UI Design Notes

Written from the inside. Not a brand guide, not a pitch deck. Just the thinking
behind what's there and what's next.

## What we're going for

The design system is called **Quiet Cartography** in the code. That name does
a lot of work. Rook is a tool for mapping what you know and what you've said,
and the UI should feel like the quiet of a good studio, not the chrome of a
SaaS dashboard. No gradients that beg for attention. No pulsing badges. No
"AI sparkle" icons. If the interface is shouting, it's wrong.

Three rules I try to hold:

1. **The conversation is the product.** Everything else earns its pixels.
2. **Show state, don't describe it.** A dot beats a tooltip. A number beats a label.
3. **Typography does the work.** Instrument Serif for mood, Inter for content,
   JetBrains Mono for structure. We don't need a fourth font.

## The palette has intent

- **Paper** is the default theme. Warm off-white `#F7F2EA` with deep ink `#1A0F22`.
  Feels like a notebook. No harsh white, no true black.
- **Ink** is the night theme. Much darker after the last pass (`#05060B`). Cards
  sit at `#0F1220` with hairline borders. You can read it in a dark room without
  your eyes watering.
- **Signal pink** `#E6188A` is the only loud color and we use it sparingly. It
  marks selections, AI output attribution, and the live indicator. If you see
  pink somewhere else, it's a bug.
- **Node colors** (person, place, thing, fact, project, skill) are the second
  tier. They only show up in the memory graph and the node chips.

## What's in the shot right now

Looking at the app as of v0.1.7:

- Top bar has the brand, a context summary (`0 nodes - 1 sessions`), a pill-nav
  (`Chat / Skills / Settings`), a `live` pill, a menu, and the traffic lights.
- Chat is a single column, max 640px, vertically scrolled. "you" labels are
  `--ink-3`, "rook" labels are signal pink. Messages are flat paragraphs, no
  bubbles, no avatars.
- Input bar floats near the bottom with mode switches (`chat / coder / team`)
  and a send button.
- Memory panel floats top-right, draggable, currently showing a legend and
  a live indicator even when it has nothing to say.

The bones are good. Below is where it's weak.

## Problems I can see from one screenshot

### 1. The memory panel is loud even when it's empty
Empty state shows PERSON / THING / PROJECT chips and a LIVE pulse. If you haven't
talked to the model yet, that legend is selling a feature you haven't earned.
Worse: the real code has six node types (person, place, thing, fact, project,
skill) and the legend only shows three. It lies about the system.

**Fix:** hide the legend until the graph has at least one node. When empty, the
panel collapses to a 4px strip with the signal dot. Once nodes exist, the
legend shows only the types actually present in the current graph.

### 2. The `live` pill doesn't say what's alive
Is it the pipe to the Rust backend? The LLM connection? The auto-updater poll?
Right now it just means "the Electron IPC handshake succeeded."

**Fix:** split it into two dots in the top bar. Left dot = backend pipe.
Right dot = LLM reachability. Green / amber / red. Hover gives latency and
version. Remove the word "live" entirely.

### 3. Top-bar metadata reads like chrome noise
`v0.1.7  0 nodes - 1 sessions` is four pieces of information competing for the
same real estate as the brand. A user who just wants to chat does not care.

**Fix:** collapse it. Version moves behind the menu (`...`). Node and session
counts only show when the right panel is collapsed, as a single `12n / 3s`
string in mono, muted.

### 4. No conversation title anywhere
The sidebar (if you open it) has titles but the active pane doesn't. If I
take a screenshot of the chat, I can't tell what it's about.

**Fix:** center slot of the top bar currently holds the nav pill. Move the
nav pill inline-right, and put the conversation title (editable on click) in
the center. Paper-thin, italic serif. Reads like a document title.

### 5. "you" / "rook" label asymmetry
"rook" is in signal pink. "you" is in neutral ink-3. The asymmetry makes the
AI feel more important than the user, which is the opposite of the product
posture we want.

**Fix:** both labels are ink-3 by default. On the turn that streamed most
recently, the label gets a subtle signal underline. Active message, not active
speaker.

### 6. No streaming affordance
When rook is typing, nothing moves. You get text appearing, which is fine on
fast networks but reads as frozen on slow ones.

**Fix:** signal-pink caret that blinks at the insertion point during stream.
Disappears the instant the stream ends. Three lines of CSS.

### 7. Mode toggles are invisible
`chat / coder / team` sits under the input. Light gray on light gray. First-time
users miss it entirely. Then they're confused when the assistant acts different.

**Fix:** the active mode word goes to ink and gets a 1px underline in signal.
Inactive words stay ink-4. Add `Ctrl+1/2/3` shortcuts with a hint on hover.

### 8. No timestamps, ever
If you have a long conversation and want to reference "that thing we figured
out earlier," you cannot anchor it in time. Timestamps on every message would
be noisy. No timestamps is worse.

**Fix:** time clusters. When two messages are 10+ minutes apart, insert a
single faint centered timestamp divider between them. iMessage does this and
it just works.

### 9. Input doesn't show you the ceiling
You can type into the box without any idea how close to the context window
you are. For coder mode especially, people paste huge files and find out
after send.

**Fix:** a single character-count readout in the bottom-right of the input,
ink-4, only visible when you're above 2,000 chars. At 80% of the context budget
it goes amber. At 100% it goes red and the send button disables.

### 10. The chat column is too wide
640px max-width looks narrow in mockups but reads wide once you have real line
lengths with prose. Scanning becomes work.

**Fix:** 580px, and introduce a looser line-height (1.62) on assistant messages
specifically.

## What I'm not going to touch

- The Instrument Serif brand word. It's good.
- Pink as the signal color. It's distinctive and it works in both themes.
- The flat-message (no bubbles) layout. Bubbles make long-form reads hard.
- The pill-nav at the top. Works fine once we stop fighting it for space.

## Bigger moves, later

- **Command palette (`Ctrl+K`) is already wired up.** It's underused. Should
  be the primary navigation for everything, and the top bar should shrink
  accordingly.
- **Memory graph as a real full-screen mode.** The floating panel is a preview.
  Give it its own route and let the user drag nodes, pin them, delete them.
- **Split `index.html` out.** It's 1,500 lines of inlined babel-React. Move
  every component to its own file under `renderer/components/`. Keeps hot
  paths readable and makes PR diffs sane.
- **A real typing indicator per-mode.** Coder mode should show the file it's
  about to touch. Team mode should show which sub-agent is speaking.
- **Conversation search.** `Ctrl+F` inside a conversation, plus global search
  across all past chats. The memory system already has the embeddings.

## Order I'd ship them in

1. Live pill split + top-bar cleanup (one afternoon)
2. Streaming caret (one hour)
3. Conversation title in top bar (one afternoon)
4. Memory panel empty state fix (one afternoon)
5. Mode toggle affordance + shortcuts (one hour)
6. Timestamps on time gaps (two hours)
7. Input character counter (one hour)
8. Chat column width + line height (fifteen minutes, but verify in both themes)

Small wins first. Then do the `index.html` split on a slow weekend.

## Principles to violate only on purpose

- Don't add a sparkle icon to anything.
- Don't use gradient text for emphasis.
- Don't add hover animations that take more than 260ms.
- Don't add a notification toast for a user action they just took.
- Don't say "magic" or "intelligent" in any UI string.

Rook is a workshop, not a product launch. The design should feel like
somewhere you already live.
