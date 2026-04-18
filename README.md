<p align="center">
  <img src="docs/hero.png" alt="Rook" width="560" />
</p>

<h1 align="center"><i>Rook</i></h1>

<p align="center">
  <b>The local-first AI that actually remembers you.</b><br/>
  <sub>Bring your own model. Your data stays on your machine. No accounts, no telemetry, no nonsense.</sub>
</p>

<p align="center">
  <a href="https://github.com/ayronny14-alt/Rook/releases/latest">
    <img src="https://img.shields.io/badge/Download-Rook%20for%20Windows-E6188A?style=for-the-badge&logo=windows&logoColor=white" alt="Download Rook for Windows" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/ayronny14-alt/Rook/releases">All releases</a> ·
  <a href="https://github.com/ayronny14-alt/Rook/issues">Report an issue</a>
</p>

---

## Why I made this

Every AI app I tried had the same problem. Brilliant for ten minutes, blank slate the next day. I would explain my project, my preferences, my whole setup, and the next session it would ask "so what are you working on?" again.

Rook is the answer to that. It runs entirely on your machine, plugs into whatever model you want, and keeps a real memory of you.

## Features

- **Local-first.** Everything (chat, memory, embeddings, indexing) runs on your machine. Your API key talks to your provider. Nothing in between.
- **Bring your own model.** OpenAI-compatible endpoints (OpenAI, Groq, OpenRouter, DeepInfra, Anthropic-via-proxy, Together, Mistral) and Ollama for fully offline use.
- **A memory that grows.** Every conversation feeds a structured graph of facts, people, places, projects, and skills. The model can reach back into it on demand.
- **Native Rust backend.** One small `rook.exe` does the heavy lifting. No Python, no Docker, no model downloads on first launch.
- **Built-in skills + MCP plugins.** Drop a skill into `.docu/skills/`, install an MCP server, done. Rook reaches for them when relevant.
- **Quiet design.** A floating chat with a memory map in the corner. Italic serif headers. Magenta where it matters. Looks like a journal, not a dashboard.
- **Auto-updates.** New release lands here, your Rook picks it up in the background. No admin prompt, no app store.

## Memory, in plain English

Most assistants treat your chat history like a sticky note they keep glancing at. Rook treats it like a city.

Every fact you share becomes <b>a place on the map</b>. <i>Aaron</i>, <i>Brisbane</i>, <i>Rust</i>, <i>that thing you said about not liking em-dashes</i>. Each one gets a coordinate.

Every connection between facts becomes <b>a road</b>. The more often two facts come up together, the wider the road gets.

Places you visit often grow into <b>landmarks</b> the model can see from across town. The corners you never come back to fade quietly into the background.

When you ask Rook something, it doesn't trawl your whole history. It looks at the map, picks the neighborhood your question belongs to, and walks the streets that lead there. That's the cartography part. Hence the name and the design.

The result is an AI that, after a week of talking to you, can answer a question with "right, you mentioned that on Tuesday in the context of the indexer thing" instead of "could you remind me what you do?"

## Install

1. Grab `Rook-Setup-<version>.exe` from the [latest release](https://github.com/ayronny14-alt/Rook/releases/latest).
2. Run it. Per-user install at `%LOCALAPPDATA%\Programs\Rook` (no admin needed). Start Menu + Desktop shortcuts.
3. Open Settings, paste your API key, send a message. That's it.

### About SmartScreen

Windows will probably say "Windows protected your PC" the first time. Rook isn't EV-signed yet (those certs cost about as much as a used car). Click **More info → Run anyway**, or build from source if you'd rather see the code first.

## Build from source

```sh
# you'll need: Rust (stable), Node 20+, Windows 10/11 x64
git clone https://github.com/ayronny14-alt/Rook.git
cd Rook
npm run build      # compiles rook.exe + bundles the installer
```

Output appears in `dist/Rook-Setup-<version>.exe`.

Dev loop:

```sh
npm run dev        # Electron against a cargo-run backend
```

## How it actually works

- **Frontend.** Electron + React 18 (no bundler, just Babel Standalone in the page). The UI is a single coherent surface called Quiet Cartography. Floating chat, ambient memory map, collapsible journal rail.
- **Backend.** Rust over a Windows named pipe (`\\.\pipe\rook`). Tokio runtime. SQLite for storage.
- **Memory.** Three layers in one DB: a graph of entities and relationships, an object store with summaries and facts, and an embedding index. A native-Rust GraphSAGE trainer runs in the background to smooth representations and predict new edges. No Python needed.
- **Embeddings.** Native Rook embedder by default (feature-hashed bag of word + char n-grams, 384-dim, deterministic, microseconds per call). Opt into OpenAI-compatible or Ollama if you want.
- **Installer.** NSIS via `electron-builder`. Auto-updates via `electron-updater` reading this repo's releases.

## Bring your own key

Your key lives in `%APPDATA%\Rook\.env` on your machine and is sent straight to the provider you chose. Rook does not phone home, does not proxy through a server, does not collect telemetry. Switch providers in Settings → Model.

## License

ISC. Take it, fork it, ship it.

---

<details>
<summary><sub>♜ a small thing</sub></summary>

<br/>

<pre align="center">
       ╭───╮
       │ ▣ │     hey.
       │   │     thanks for reading this far.
       │   │     i hope rook makes your computer
       ╭───┴───╮  feel a little more like yours.
       │ ░░░░░ │
       ╰───────╯
</pre>

<p align="center"><sub><i>~ aaron</i></sub></p>

</details>
